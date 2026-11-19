//! Directory-anchored single-flight and durable renewal admission per Claude store.

use crate::credentials::{CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED, CREDENTIAL_REFRESH_LIMITED};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    ffi::CStr,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

/// Most renewal attempts allowed per store in a trailing day.
pub(super) const MAX_RENEWALS_PER_DAY: usize = 6;
/// Length of the attempt-cap window in seconds.
pub(super) const DAY_SECONDS: f64 = 86_400.0;
/// Files inside the store directory are shared even when workers use different Quota homes.
pub(super) const LOCK: &CStr = c".agent-quota-oauth-renewal.lock";
pub(super) const LEDGER: &CStr = c".agent-quota-oauth-renewals.json";
const TEMPORARY: &CStr = c".agent-quota-oauth-renewals.tmp";
const LEGACY: &CStr = c"oauth-renewals.json";
const MAX_LEDGER_BYTES: u64 = 64 * 1024;

/// Canonical Claude configuration directory and its open device/inode identity.
/// Use the directory identity because Claude can atomically replace its credential file.
pub(super) struct Store {
    path: PathBuf,
    directory: File,
}

impl Store {
    /// Canonical path of the open store, after the caller checks its trusted ancestry.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Opens the canonical directory and rejects a foreign or writable-by-others store.
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let path = path.canonicalize()?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = directory.metadata()?;
        if metadata.uid() != uid() || metadata.mode() & 0o022 != 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let store = Self { path, directory };
        store.verify_path(&store.path)?;
        Ok(store)
    }

    /// Refuses to pass a path to the CLI if it no longer denotes this store.
    pub(super) fn verify_path(&self, selected: &Path) -> io::Result<()> {
        let actual = selected.canonicalize()?;
        let metadata = actual.symlink_metadata()?;
        let expected = self.directory.metadata()?;
        if actual != self.path
            || (metadata.dev(), metadata.ino()) != (expected.dev(), expected.ino())
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(())
    }

    /// Requires the Linux credential file to belong to this one store.
    pub(super) fn verify_credential_file(&self) -> io::Result<()> {
        self.open_file(c".credentials.json", libc::O_RDONLY)
            .map(drop)
    }

    /// Takes a nonblocking store lock. Closing the private descriptor releases it.
    pub(super) fn try_lock(&self) -> io::Result<Option<File>> {
        let file = self.open_file(LOCK, libc::O_RDWR | libc::O_CREAT)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }

    /// Opens a private regular file anchored to this verified directory descriptor.
    fn open_file(&self, name: &CStr, flags: libc::c_int) -> io::Result<File> {
        let directory = self.directory.as_raw_fd();
        let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        // SAFETY: the directory fd stays open and name is NUL-terminated. O_CREAT uses mode 0600.
        let fd = unsafe { libc::openat(directory, name.as_ptr(), flags, 0o600 as libc::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor and transfers ownership to File.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(file)
    }

    /// Only ENOENT means a ledger is absent. Refuses symlinks, bad permissions and malformed data.
    fn read<T: DeserializeOwned>(&self, name: &CStr) -> io::Result<Option<T>> {
        let file = match self.open_file(name, libc::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(MAX_LEDGER_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Replaces the ledger atomically under the store lock and syncs both data and directory.
    fn write(&self, ledger: &Ledger) -> io::Result<()> {
        let bytes = serde_json::to_vec(ledger)?;
        let directory = self.directory.as_raw_fd();
        // SAFETY: the directory is open, and both fixed names are NUL-terminated.
        let remove_temp = || unsafe { libc::unlinkat(directory, TEMPORARY.as_ptr(), 0) };
        // A previous process can leave a temporary file behind after a crash.
        remove_temp();
        let result = (|| {
            let mut file =
                self.open_file(TEMPORARY, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            // SAFETY: the directory is open and both fixed entry names are NUL-terminated.
            if unsafe { libc::renameat(directory, TEMPORARY.as_ptr(), directory, LEDGER.as_ptr()) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
            self.directory.sync_all()
        })();
        if result.is_err() {
            remove_temp();
        }
        result
    }
}

/// Effective owner of the store and its state.
fn uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory effects.
    unsafe { libc::geteuid() }
}

/// Attempt times and a crash-persistent fence for a launch whose cleanup is unproven.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ledger {
    version: u8,
    pub(super) attempts: Vec<f64>,
    pub(super) pending: bool,
}

impl Ledger {
    /// Loads this store's state without resetting on failed validation.
    pub(super) fn load(store: &Store) -> Result<Self, &'static str> {
        match store.read::<Self>(LEDGER) {
            Ok(Some(ledger))
                if ledger.version == 2
                    && ledger.attempts.len() <= MAX_RENEWALS_PER_DAY
                    && ledger
                        .attempts
                        .iter()
                        .all(|time| time.is_finite() && *time >= 0.0) =>
            {
                Ok(ledger)
            }
            Ok(None) => Ok(Self {
                version: 2,
                attempts: Vec::new(),
                pending: false,
            }),
            _ => Err(CREDENTIAL_REFRESH_LIMITED),
        }
    }

    /// An abandoned or concurrent launch must not be followed by another credential operation.
    pub(super) fn require_clean(&self) -> Result<(), &'static str> {
        if self.pending {
            return Err(CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED);
        }
        Ok(())
    }

    /// Checks the cap without charging a turn. Count recent legacy entries because version 1 used
    /// raw path hashes and could represent this store under any number of aliases.
    pub(super) fn available(&self, home: &Path, at: f64) -> Result<bool, &'static str> {
        if !at.is_finite() || at < 0.0 {
            return Err(CREDENTIAL_REFRESH_LIMITED);
        }
        let recent = self
            .attempts
            .iter()
            .filter(|time| **time > at - DAY_SECONDS)
            .count();
        let legacy_count = legacy_attempts(home, at).map_err(|_| CREDENTIAL_REFRESH_LIMITED)?;
        Ok(recent + legacy_count < MAX_RENEWALS_PER_DAY)
    }

    /// Charges an admitted print turn using its own start time.
    pub(super) fn charge(&mut self, at: f64) {
        self.attempts.retain(|time| *time > at - DAY_SECONDS);
        self.attempts.push(at);
    }

    /// Commits the current fence and attempt history.
    pub(super) fn save(&self, store: &Store) -> Result<(), &'static str> {
        store.write(self).map_err(|_| CREDENTIAL_REFRESH_LIMITED)
    }

    /// Clears only the pending fence, retaining the attempt against the daily cap.
    pub(super) fn complete(&mut self, store: &Store) -> Result<(), &'static str> {
        self.pending = false;
        self.save(store)
    }
}

/// Version-1 Quota-home ledger used only to honor pre-upgrade attempts.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyLedger {
    version: u8,
    logins: BTreeMap<String, Vec<f64>>,
}

/// Returns the count of unexpired legacy attempts. I/O and schema failures refuse renewal.
fn legacy_attempts(home: &Path, at: f64) -> io::Result<usize> {
    let home = Store::open(home)?;
    match home.read::<LegacyLedger>(LEGACY)? {
        None => Ok(0),
        Some(legacy) if legacy.version == 1 => Ok(legacy
            .logins
            .values()
            .flatten()
            .filter(|time| **time > at - DAY_SECONDS)
            .count()),
        Some(_) => Err(io::ErrorKind::InvalidData.into()),
    }
}
