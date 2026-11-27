//! Private, bounded context delivery state for Claude Code and Codex sessions.

use crate::{capacity::HookBlock, ipc::owner_uid};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_SESSIONS: usize = 256;
const MAX_STATE_BYTES: u64 = 68 * 1024;

/// A prompt can be unscoped for callers without usable host input.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Prompt(Option<[u8; 32]>),
    Invalidate([u8; 32]),
    Ignore,
}

/// Reads only host routing metadata. Prompt contents are never retained.
pub(crate) fn event(host: &str, input: &[u8]) -> Event {
    #[derive(Deserialize)]
    struct Input {
        session_id: Option<String>,
        hook_event_name: Option<String>,
        source: Option<String>,
    }
    let Ok(input) = serde_json::from_slice::<Input>(input) else {
        return Event::Prompt(None);
    };
    let key = input
        .session_id
        .filter(|id| !id.trim().is_empty() && id.len() <= 1024)
        .map(|id| {
            let mut digest = Sha256::new();
            digest.update(host.as_bytes());
            digest.update([0]);
            digest.update(id.as_bytes());
            digest.finalize().into()
        });
    match input.hook_event_name.as_deref() {
        None | Some("UserPromptSubmit") => Event::Prompt(key),
        Some("SessionStart")
            if matches!(
                input.source.as_deref(),
                Some("compact" | "clear" | "resume")
            ) =>
        {
            key.map_or(Event::Ignore, Event::Invalidate)
        }
        _ => Event::Ignore,
    }
}

/// Resolves the per-UID state under the caller's XDG state home.
pub(crate) fn state_home() -> Option<PathBuf> {
    absolute_env("XDG_STATE_HOME")
        .or_else(|| absolute_env("HOME").map(|home| home.join(".local/state")))
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Opens a private directory, rejecting symlinks and shared access.
fn private_directory(path: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != owner_uid() || meta.mode() & 0o777 != 0o700 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

/// Opens a state file without following links, blocking on special files, or granting access.
fn private_file(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != owner_uid()
        || meta.mode() & 0o777 != 0o600
        || meta.nlink() != 1
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

/// Last delivered context. Existing ledger digests retain their serialized shape.
#[derive(serde::Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum Delivery {
    Quota([u8; 32]),
    Marker(DeliveryMarker),
}

/// A delivered block needs refresh after a lifecycle event or an outage.
#[derive(serde::Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DeliveryMarker {
    RefreshPending,
    Unavailable,
}

/// Oldest last-injected session comes first. Keys are SHA-256 digests.
type Sessions = Vec<([u8; 32], Delivery)>;

/// Runs a state operation under a nonblocking private file lock, then publishes by rename.
fn update(
    base: &Path,
    change: impl FnOnce(&mut Sessions) -> bool,
    emit: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(base)?;
    let metadata = fs::symlink_metadata(base)?;
    if !metadata.is_dir() || metadata.uid() != owner_uid() || metadata.mode() & 0o022 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let parent = base.join("agent-quota");
    private_directory(&parent)?;
    let dir = parent.join("hook");
    private_directory(&dir)?;
    let lock = private_file(&dir.join("lock"), true)?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let state = dir.join("sessions.json");
    let mut entries: Sessions = match private_file(&state, false) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_STATE_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_STATE_BYTES {
                return Err(io::ErrorKind::InvalidData.into());
            }
            serde_json::from_slice(&bytes)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    if !change(&mut entries) {
        return Ok(());
    }
    let excess = entries.len().saturating_sub(MAX_SESSIONS);
    entries.drain(..excess);
    let temporary = dir.join("sessions.new");
    // Any interrupted write is private and reserved to this lock-protected ledger.
    let mut file = private_file(&temporary, true)?;
    file.set_len(0)?;
    file.write_all(&serde_json::to_vec(&entries)?)?;
    if let Err(error) = emit() {
        let _ = fs::remove_file(temporary);
        return Err(error);
    }
    fs::rename(temporary, state)
}

/// Invalidates the digest while retaining delivery history without a service read.
pub(crate) fn invalidate(base: &Path, key: [u8; 32]) -> io::Result<()> {
    update(
        base,
        |entries| {
            let Some((_, delivery)) = entries.iter_mut().find(|(session, _)| session == &key)
            else {
                return false;
            };
            if !matches!(delivery, Delivery::Quota(_)) {
                return false;
            }
            *delivery = Delivery::Marker(DeliveryMarker::RefreshPending);
            true
        },
        || Ok(()),
    )
}

/// Emits only a changed block and records it after a successful write.
pub(crate) fn inject(
    base: &Path,
    key: [u8; 32],
    block: HookBlock,
    emit: impl FnOnce(&str) -> io::Result<()>,
) -> io::Result<()> {
    update(
        base,
        |entries| {
            if entries.iter().any(|(session, digest)| {
                session == &key && digest == &Delivery::Quota(block.digest)
            }) {
                return false;
            }
            entries.retain(|(session, _)| session != &key);
            entries.push((key, Delivery::Quota(block.digest)));
            true
        },
        || emit(&block.text),
    )
}

/// Reports the first unreachable read after a delivered block and records successful output.
pub(crate) fn unavailable(
    base: &Path,
    key: [u8; 32],
    emit: impl FnOnce(&str) -> io::Result<()>,
) -> io::Result<()> {
    update(
        base,
        |entries| {
            let Some(index) = entries.iter().position(|(session, delivery)| {
                session == &key
                    && !matches!(delivery, Delivery::Marker(DeliveryMarker::Unavailable))
            }) else {
                return false;
            };
            entries.remove(index);
            entries.push((key, Delivery::Marker(DeliveryMarker::Unavailable)));
            true
        },
        || {
            let minutes = std::time::UNIX_EPOCH
                .elapsed()
                .unwrap_or_default()
                .as_secs()
                / 60;
            emit(&format!(
                "Quota unavailable since {:02}:{:02}Z",
                minutes / 60 % 24,
                minutes % 60
            ))
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Explicit regression assertions")]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Session IDs are namespaced by host and a successful delivery alone suppresses repeats.
    #[test]
    fn sessions_prune_old_entries_and_keep_private_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let block = || HookBlock {
            text: "Quota update".into(),
            digest: [1; 32],
        };
        let run = |key| {
            let mut emitted = false;
            inject(dir.path(), key, block(), |_| {
                emitted = true;
                Ok(())
            })
            .unwrap();
            emitted
        };
        let input = br#"{"session_id":"test-session","prompt":"private prompt"}"#;
        let Event::Prompt(Some(claude)) = event("claude", input) else {
            panic!("missing Claude key")
        };
        let Event::Prompt(Some(codex)) = event("codex", input) else {
            panic!("missing Codex key")
        };
        assert_ne!(claude, codex);
        assert!(
            inject(dir.path(), claude, block(), |_| Err(
                io::ErrorKind::BrokenPipe.into()
            ))
            .is_err()
        );
        assert!(run(claude));
        assert!(!run(claude));
        assert!(run(codex));
        assert!(!run(codex));
        for index in 0..MAX_SESSIONS {
            let key: [u8; 32] = Sha256::digest(index.to_ne_bytes()).into();
            assert!(run(key));
        }
        assert!(run(claude));
        let hook = dir.path().join("agent-quota/hook");
        let state = hook.join("sessions.json");
        let data = fs::read_to_string(&state).unwrap();
        assert_eq!(
            serde_json::from_str::<Sessions>(&data).unwrap().len(),
            MAX_SESSIONS
        );
        assert!(!data.contains("private prompt") && !data.contains("test-session"));
        assert_eq!(fs::metadata(hook).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(state).unwrap().mode() & 0o777, 0o600);
    }
}
