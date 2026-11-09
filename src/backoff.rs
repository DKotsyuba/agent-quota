//! Durable, account-scoped collection backoff ported from agent-run.
use crate::quota::AccountId;
use std::{
    collections::BTreeMap,
    fs::{DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const CREDENTIAL_UNAVAILABLE: &str = "credential_unavailable";
const STORE_FAILED: &str = "store_failed";

/// Bounded failure backoff shared across every alias of one account.
///
/// The key is `(global account id, stable collector source)`, so two provider
/// labels over one physical account suppress duplicate remote requests after
/// failures together. The delay grows exponentially from `BASE_DELAY_SECONDS`
/// and is capped at [`MAX_DELAY_SECONDS`]; a success clears it. State is durable in
/// `capacity/backoff.json` under the service home so suppression survives
/// the process boundary between polling rounds. The latest failure code is retained
/// through suppressed rounds and cleared on success. This is a cooperative bound
/// only — it never invents quota facts, and skipped rounds leave the previous
/// samples and durable exhaustion latch untouched.
#[derive(Default)]
pub struct AccountBackoff {
    entries: BTreeMap<(String, String), (u32, f64, Option<String>)>,
}

/// First failed-round delay; later rounds double it up to the cap.
const BASE_DELAY_SECONDS: f64 = 60.0;
/// Hard ceiling for one suppressed round, matching the sample TTL horizon.
pub const MAX_DELAY_SECONDS: f64 = 900.0;
/// Failure count at which the cap is reached; further failures stay there.
const MAX_FAILURES: u32 = 5;
const MAX_LEDGER_BYTES: u64 = 1024 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Checks the private ledger directory without following a symlink.
fn ledger_dir(home: &Path, create: bool) -> io::Result<Option<PathBuf>> {
    let path = home.join("capacity");
    if create {
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe ledger directory",
        ));
    }
    Ok(Some(path))
}

/// Rejects links, foreign ownership and shared access before reading or replacing a ledger.
fn valid_ledger(metadata: &std::fs::Metadata) -> bool {
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    metadata.file_type().is_file()
        && metadata.uid() == uid
        && metadata.mode() & 0o077 == 0
        && metadata.nlink() == 1
}

impl AccountBackoff {
    /// Returns whether `account`'s `source` round is suppressed at `at`.
    pub fn suppressed(&self, account: &AccountId, source: &str, at: f64) -> bool {
        self.entries
            .get(&(account.as_str().to_owned(), source.to_owned()))
            .is_some_and(|&(_, until, _)| at < until)
    }

    /// Records one failed round with its safe issue code and returns the new suppressed-until epoch.
    /// The code is retained durably but is allowlisted before it appears in a report.
    pub fn record_failure(
        &mut self,
        account: &AccountId,
        source: &str,
        at: f64,
        issue: String,
    ) -> f64 {
        let key = (account.as_str().to_owned(), source.to_owned());
        let failures = self
            .entries
            .get(&key)
            .map_or(0, |(n, _, _)| *n)
            .saturating_add(1);
        let delay = BASE_DELAY_SECONDS
            * 2_f64
                .powi((failures.min(MAX_FAILURES) - 1) as i32)
                .min(MAX_DELAY_SECONDS / BASE_DELAY_SECONDS);
        let until = at + delay.min(MAX_DELAY_SECONDS);
        self.entries.insert(
            key,
            (failures, until, safe_failure_issue(&issue).then_some(issue)),
        );
        until
    }

    /// Returns the last safe failure classification, if one is retained.
    pub fn last_failure(&self, account: &AccountId, source: &str) -> Option<&str> {
        self.entries
            .get(&(account.as_str().to_owned(), source.to_owned()))
            .and_then(|(_, _, issue)| issue.as_deref())
    }

    /// Clears any suppression after one successful round.
    pub fn record_success(&mut self, account: &AccountId, source: &str) {
        self.entries
            .remove(&(account.as_str().to_owned(), source.to_owned()));
    }

    /// Loads the durable backoff ledger under `home`; a missing or malformed
    /// ledger starts clean rather than blocking collection.
    pub fn load(home: &Path) -> io::Result<Self> {
        let Some(dir) = ledger_dir(home, false)? else {
            return Ok(Self::default());
        };
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(dir.join("backoff.json"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !valid_ledger(&metadata) || metadata.len() > MAX_LEDGER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsafe backoff ledger",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_LEDGER_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backoff ledger too large",
            ));
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => return Ok(Self::default()),
        };
        Ok(
            serde_json::from_str::<Vec<((String, String), (u32, f64, Option<String>))>>(text)
                .map(|rows| Self {
                    entries: rows
                        .into_iter()
                        .map(|(key, (count, until, issue))| {
                            (key, (count, until, issue.filter(|s| safe_failure_issue(s))))
                        })
                        .collect(),
                })
                .or_else(|_| {
                    serde_json::from_str::<Vec<((String, String), (u32, f64))>>(text).map(|rows| {
                        Self {
                            entries: rows
                                .into_iter()
                                .map(|(key, (count, until))| (key, (count, until, None)))
                                .collect(),
                        }
                    })
                })
                .unwrap_or_default(),
        )
    }

    /// Persists the ledger with expired entries pruned; write failures are
    /// returned so a read-only home is reported, not silently ignored.
    pub fn save(&self, home: &Path, at: f64) -> std::io::Result<()> {
        let dir =
            ledger_dir(home, true)?.ok_or_else(|| io::Error::other("ledger directory missing"))?;
        let rows: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, (_, until, _))| *until > at)
            .map(|(key, (count, until, issue))| {
                (
                    key,
                    (
                        *count,
                        *until,
                        issue.as_ref().filter(|s| safe_failure_issue(s)).cloned(),
                    ),
                )
            })
            .collect();
        let bytes = serde_json::to_vec(&rows).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "backoff ledger too large",
            ));
        }
        let destination = dir.join("backoff.json");
        let previous = match std::fs::symlink_metadata(&destination) {
            Ok(metadata) if valid_ledger(&metadata) => Some(metadata),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe backoff ledger",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let temporary = dir.join(format!(
            ".backoff-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            match (previous, std::fs::symlink_metadata(&destination)) {
                (Some(before), Ok(after))
                    if valid_ledger(&after)
                        && before.dev() == after.dev()
                        && before.ino() == after.ino() =>
                {
                    std::fs::rename(&temporary, &destination)?;
                }
                (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                    crate::ipc::publish(&temporary, &destination).map_err(io::Error::other)?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "backoff ledger changed",
                    ));
                }
            }
            File::open(&dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

/// Accepts only known classifications or numeric process termination evidence.
fn safe_failure_issue(issue: &str) -> bool {
    const KNOWN: &[&str] = &[
        "collector_config_invalid",
        "collector_input_invalid",
        "collector_input_too_large",
        "collector_spawn_failed",
        "collector_io_failed",
        "collector_output_too_large",
        "collector_wait_failed",
        "collector_timeout",
        "collector_pipe_timeout",
        "collector_pipe_unavailable",
        "collector_process_identity_unavailable",
        "collector_temp_unavailable",
        "collector_exit_unknown",
        "collector_cleanup_unverified",
        "collector_output_invalid",
        "collector_output_contains_credential",
        CREDENTIAL_UNAVAILABLE,
        STORE_FAILED,
    ];
    KNOWN.contains(&issue)
        || ["collector_exit_status:", "collector_exit_signal:"]
            .iter()
            .any(|prefix| {
                issue.strip_prefix(prefix).is_some_and(|n| {
                    !n.is_empty()
                        && n.bytes().all(|b| b.is_ascii_digit())
                        && n.parse::<u32>().is_ok()
                })
            })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Isolated ledger regression assertions")]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// Creates a private home in the configured task temporary directory.
    fn home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    /// A saved delay and its safe issue survive a service restart.
    #[test]
    fn backoff_roundtrip_and_legacy_format() {
        let dir = home();
        let account: AccountId = "acct".parse().unwrap();
        let mut backoff = AccountBackoff::default();
        assert_eq!(
            backoff.record_failure(&account, "s", 100.0, "collector_timeout".into()),
            160.0
        );
        backoff.save(dir.path(), 100.0).unwrap();
        let loaded = AccountBackoff::load(dir.path()).unwrap();
        assert!(loaded.suppressed(&account, "s", 120.0));
        assert_eq!(
            loaded.last_failure(&account, "s"),
            Some("collector_timeout")
        );
        std::fs::write(
            dir.path().join("capacity/backoff.json"),
            "[[[\"acct\",\"s\"],[1,160.0]]]",
        )
        .unwrap();
        assert!(
            AccountBackoff::load(dir.path())
                .unwrap()
                .suppressed(&account, "s", 120.0)
        );
    }

    /// An unsafe ledger or directory never truncates the referenced file.
    #[test]
    fn refuses_links_and_preserves_their_targets() {
        let dir = home();
        let outside = home();
        let target = outside.path().join("data");
        std::fs::write(&target, b"preserve").unwrap();
        symlink(outside.path(), dir.path().join("capacity")).unwrap();
        assert!(AccountBackoff::default().save(dir.path(), 1.0).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
        std::fs::remove_file(dir.path().join("capacity")).unwrap();
        std::fs::create_dir(dir.path().join("capacity")).unwrap();
        std::fs::set_permissions(
            dir.path().join("capacity"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        symlink(&target, dir.path().join("capacity/backoff.json")).unwrap();
        assert!(AccountBackoff::default().save(dir.path(), 1.0).is_err());
        assert!(AccountBackoff::load(dir.path()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
        std::fs::remove_file(dir.path().join("capacity/backoff.json")).unwrap();
        std::fs::hard_link(&target, dir.path().join("capacity/backoff.json")).unwrap();
        assert!(AccountBackoff::default().save(dir.path(), 1.0).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
    }

    /// A FIFO ledger is rejected without waiting for a writer.
    #[test]
    fn fifo_ledger_is_nonblocking() {
        use std::os::unix::ffi::OsStrExt;
        let dir = home();
        std::fs::create_dir(dir.path().join("capacity")).unwrap();
        std::fs::set_permissions(
            dir.path().join("capacity"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let fifo = dir.path().join("capacity/backoff.json");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path inside this test's private home.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(AccountBackoff::load(dir.path()).is_err());
        assert!(AccountBackoff::default().save(dir.path(), 1.0).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
