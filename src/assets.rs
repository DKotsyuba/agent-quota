//! Export and verify the collector and Linux launcher files embedded in this binary.
use serde::Serialize;
use std::{
    collections::BTreeSet,
    ffi::{CStr, CString, OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    ptr::NonNull,
};

/// One fixed file whose bytes travel inside the verified executable.
struct Asset {
    directory: &'static str,
    name: &'static str,
    bytes: &'static [u8],
    executable: bool,
}

const ASSETS: &[Asset] = &[
    Asset {
        directory: "collectors",
        name: "codex.sh",
        bytes: include_bytes!("../scripts/collectors/codex.sh"),
        executable: true,
    },
    Asset {
        directory: "collectors",
        name: "codex.jq",
        bytes: include_bytes!("../scripts/collectors/codex.jq"),
        executable: false,
    },
    Asset {
        directory: "collectors",
        name: "claude.sh",
        bytes: include_bytes!("../scripts/collectors/claude.sh"),
        executable: true,
    },
    Asset {
        directory: "collectors",
        name: "claude.jq",
        bytes: include_bytes!("../scripts/collectors/claude.jq"),
        executable: false,
    },
    Asset {
        directory: "collectors",
        name: "glm.sh",
        bytes: include_bytes!("../scripts/collectors/glm.sh"),
        executable: true,
    },
    Asset {
        directory: "collectors",
        name: "glm.jq",
        bytes: include_bytes!("../scripts/collectors/glm.jq"),
        executable: false,
    },
    Asset {
        directory: "collectors",
        name: "common.jq",
        bytes: include_bytes!("../scripts/collectors/common.jq"),
        executable: false,
    },
    Asset {
        directory: "systemd",
        name: "agent-quota-owner-collect.sh",
        bytes: include_bytes!("../scripts/systemd/agent-quota-owner-collect.sh"),
        executable: true,
    },
    Asset {
        directory: "systemd",
        name: "agent-quota-owner-collect@.service",
        bytes: include_bytes!("../scripts/systemd/agent-quota-owner-collect@.service"),
        executable: false,
    },
    Asset {
        directory: "systemd",
        name: "agent-quota-owner-collect@.timer",
        bytes: include_bytes!("../scripts/systemd/agent-quota-owner-collect@.timer"),
        executable: false,
    },
    Asset {
        directory: "systemd",
        name: "owner-collect.env.example",
        bytes: include_bytes!("../scripts/systemd/owner-collect.env.example"),
        executable: false,
    },
];

/// Successful asset operation; the listed files are tied to this binary version.
#[derive(Serialize)]
pub(crate) struct Report {
    product: &'static str,
    version: &'static str,
    directory: PathBuf,
    files: usize,
    verified: bool,
}

/// Checks an opened directory's owner and privacy rather than trusting its pathname.
fn check_private_directory(directory: &File) -> Result<(), &'static str> {
    let metadata = directory
        .metadata()
        .map_err(|_| "assets_directory_unavailable")?;
    if !metadata.is_dir()
        || metadata.uid() != crate::ipc::owner_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err("assets_directory_not_private");
    }
    Ok(())
}

/// Opens and pins an existing owner-private directory without following its final symlink.
fn private_directory(path: &Path) -> Result<File, &'static str> {
    if !path.is_absolute()
        || path.to_str().is_none()
        || path.components().any(|part| part == Component::ParentDir)
    {
        return Err("assets_path_invalid");
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "assets_directory_unavailable")?;
    check_private_directory(&directory)?;
    Ok(directory)
}

/// Encodes one path component for the descriptor-relative system calls.
fn component(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid component",
        ));
    }
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid component"))
}

/// Opens one component relative to a pinned directory; the returned file owns its descriptor.
fn open_at(parent: &File, name: &OsStr, flags: libc::c_int, mode: libc::c_int) -> io::Result<File> {
    let name = component(name)?;
    // SAFETY: parent owns an open descriptor and name is a live NUL-terminated component.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, mode) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor whose ownership is transferred to File.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

/// Creates one new directory relative to its pinned parent.
fn mkdir_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let name = component(name)?;
    // SAFETY: parent owns an open descriptor and name is a live NUL-terminated component.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Opens and validates one child directory without following a symlink.
fn directory_at(parent: &File, name: &OsStr) -> Result<File, &'static str> {
    let directory = open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    )
    .map_err(|_| "assets_directory_unavailable")?;
    check_private_directory(&directory)?;
    Ok(directory)
}

/// Closes the DIR stream and its owned descriptor on every path.
struct DirectoryStream(NonNull<libc::DIR>);
impl Drop for DirectoryStream {
    /// Closes the stream and the descriptor transferred into it by fdopendir.
    fn drop(&mut self) {
        // SAFETY: this guard uniquely owns the successful fdopendir result.
        unsafe {
            libc::closedir(self.0.as_ptr());
        }
    }
}

/// Clears thread-local errno before readdir so EOF and read failure differ.
fn clear_errno() {
    #[cfg(target_os = "linux")]
    // SAFETY: libc exposes a writable errno slot for the calling thread.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: libc exposes a writable errno slot for the calling thread.
    unsafe {
        *libc::__error() = 0;
    }
}

/// Reads at most `limit` entries from a fresh stream over the pinned directory.
fn names(directory: &File, limit: usize) -> Result<BTreeSet<OsString>, &'static str> {
    let descriptor = open_at(
        directory,
        OsStr::new("."),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| "assets_verify_failed")?
    .into_raw_fd();
    // SAFETY: descriptor is an open directory and becomes owned by DIR only on success.
    let stream = unsafe { libc::fdopendir(descriptor) };
    let Some(stream) = NonNull::new(stream) else {
        // SAFETY: fdopendir failed, so the caller still owns this descriptor.
        unsafe {
            libc::close(descriptor);
        }
        return Err("assets_verify_failed");
    };
    let stream = DirectoryStream(stream);
    let mut result = BTreeSet::new();
    loop {
        clear_errno();
        // SAFETY: stream owns a live DIR, used only on this thread until the next call.
        let entry = unsafe { libc::readdir(stream.0.as_ptr()) };
        if entry.is_null() {
            if io::Error::last_os_error().raw_os_error().unwrap_or(0) != 0 {
                return Err("assets_verify_failed");
            }
            return Ok(result);
        }
        // SAFETY: readdir returned a live dirent with a NUL-terminated name; copy it now.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if result.len() == limit || !result.insert(OsString::from_vec(name.to_vec())) {
            return Err("assets_inventory_mismatch");
        }
    }
}

/// Compares pinned filesystem identities without trusting a path's spelling.
fn same_directory(left: &File, right: &File) -> bool {
    match (left.metadata(), right.metadata()) {
        (Ok(left), Ok(right)) => {
            left.dev() == right.dev() && left.ino() == right.ino() && left.uid() == right.uid()
        }
        _ => false,
    }
}

/// Verifies the fixed inventory and exact bytes through already pinned directory descriptors.
fn verify_tree(root: &File) -> Result<(), &'static str> {
    let top = names(root, 2)?;
    if top != BTreeSet::from([OsString::from("collectors"), OsString::from("systemd")]) {
        return Err("assets_inventory_mismatch");
    }
    for directory in ["collectors", "systemd"] {
        let pinned = directory_at(root, OsStr::new(directory))?;
        let expected: BTreeSet<OsString> = ASSETS
            .iter()
            .filter(|asset| asset.directory == directory)
            .map(|asset| OsString::from(asset.name))
            .collect();
        let actual = names(&pinned, expected.len())?;
        if actual != expected {
            return Err("assets_inventory_mismatch");
        }
        for asset in ASSETS.iter().filter(|asset| asset.directory == directory) {
            let file = open_at(
                &pinned,
                OsStr::new(asset.name),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0,
            )
            .map_err(|_| "assets_verify_failed")?;
            let metadata = file.metadata().map_err(|_| "assets_verify_failed")?;
            if !metadata.is_file()
                || metadata.uid() != crate::ipc::owner_uid()
                || metadata.nlink() != 1
                || metadata.mode() & 0o077 != 0
                || (asset.executable && metadata.mode() & 0o100 == 0)
                || metadata.len() != asset.bytes.len() as u64
            {
                return Err("assets_integrity_mismatch");
            }
            let mut bytes = Vec::with_capacity(asset.bytes.len());
            file.take(asset.bytes.len() as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "assets_verify_failed")?;
            if bytes != asset.bytes {
                return Err("assets_integrity_mismatch");
            }
        }
    }
    Ok(())
}

/// Creates a new private export; existing destinations are never modified.
/// A failed export can leave an incomplete new directory for the owner to inspect.
pub(crate) fn export(output: &Path) -> Result<Report, &'static str> {
    export_with_hooks(output, || {}, || {})
}

/// Keeps all writes descriptor-relative; hooks exercise rename races in tests.
fn export_with_hooks(
    output: &Path,
    after_parent: impl FnOnce(),
    before_check: impl FnOnce(),
) -> Result<Report, &'static str> {
    if !output.is_absolute()
        || output.to_str().is_none()
        || output.components().any(|part| part == Component::ParentDir)
    {
        return Err("assets_path_invalid");
    }
    let name = output
        .file_name()
        .filter(|name| *name != OsStr::new("."))
        .ok_or("assets_path_invalid")?;
    let parent_path = output.parent().ok_or("assets_path_invalid")?;
    let parent = private_directory(parent_path)?;
    after_parent();
    match mkdir_at(&parent, name) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err("assets_target_exists");
        }
        Err(_) => return Err("assets_directory_unavailable"),
    }
    let target = directory_at(&parent, name).map_err(|_| "assets_export_incomplete")?;
    for directory in ["collectors", "systemd"] {
        mkdir_at(&target, OsStr::new(directory)).map_err(|_| "assets_export_incomplete")?;
        let pinned =
            directory_at(&target, OsStr::new(directory)).map_err(|_| "assets_export_incomplete")?;
        for asset in ASSETS.iter().filter(|asset| asset.directory == directory) {
            let mode = if asset.executable { 0o700 } else { 0o600 };
            let mut file = open_at(
                &pinned,
                OsStr::new(asset.name),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
            .map_err(|_| "assets_export_incomplete")?;
            file.write_all(asset.bytes)
                // Both possible modes are positive permission bits representable as u32.
                .and_then(|()| file.set_permissions(std::fs::Permissions::from_mode(mode as u32)))
                .and_then(|()| file.sync_all())
                .map_err(|_| "assets_export_incomplete")?;
        }
    }
    verify_tree(&target).map_err(|_| "assets_export_incomplete")?;
    before_check();
    let current_parent = private_directory(parent_path).map_err(|_| "assets_output_uncertain")?;
    let current_target = directory_at(&parent, name).map_err(|_| "assets_output_uncertain")?;
    if !same_directory(&parent, &current_parent) || !same_directory(&target, &current_target) {
        return Err("assets_output_uncertain");
    }
    Ok(report(output))
}

/// Verifies exact embedded bytes and inventory in an owner-private export.
pub(crate) fn verify(directory: &Path) -> Result<Report, &'static str> {
    let root = private_directory(directory)?;
    verify_tree(&root)?;
    let current = private_directory(directory).map_err(|_| "assets_directory_changed")?;
    if !same_directory(&root, &current) {
        return Err("assets_directory_changed");
    }
    Ok(report(directory))
}

/// Builds a response without resolving or reopening an untrusted path.
fn report(directory: &Path) -> Report {
    Report {
        product: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        directory: directory.to_path_buf(),
        files: ASSETS.len(),
        verified: true,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "Synthetic race assertions"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A replaced parent cannot redirect writes, and the result is reported uncertain.
    #[test]
    fn renamed_parent_does_not_redirect_export() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = root.path().join("parent");
        let moved = root.path().join("moved");
        let decoy = root.path().join("decoy");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&decoy).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = parent.join("assets");
        let result = export_with_hooks(
            &output,
            || {
                std::fs::rename(&parent, &moved).unwrap();
                symlink(&decoy, &parent).unwrap();
            },
            || {},
        );
        assert!(matches!(result, Err("assets_output_uncertain")));
        assert!(!decoy.join("assets").exists());
        assert_eq!(
            std::fs::read(moved.join("assets/collectors/codex.sh")).unwrap(),
            include_bytes!("../scripts/collectors/codex.sh")
        );
    }

    /// A replacement target is never modified or reported as the completed export.
    #[test]
    fn replaced_target_is_not_reported_or_overwritten() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = root.path().join("assets");
        let moved = root.path().join("moved");
        let result = export_with_hooks(
            &output,
            || {},
            || {
                std::fs::rename(&output, &moved).unwrap();
                std::fs::create_dir(&output).unwrap();
                std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700)).unwrap();
                std::fs::write(output.join("keep"), "sentinel").unwrap();
            },
        );
        assert!(matches!(result, Err("assets_output_uncertain")));
        assert_eq!(
            std::fs::read_to_string(output.join("keep")).unwrap(),
            "sentinel"
        );
        assert!(moved.join("collectors/codex.sh").is_file());
    }
}
