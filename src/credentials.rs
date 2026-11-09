//! Read-only credential resolution for private collector stdin, never diagnostics.
//!
//! References name existing stores. This module neither provisions credentials nor
//! changes permissions; files must belong to the effective user and be private.

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    str::FromStr,
};

/// Fixed error shared by unavailable, malformed and inaccessible credential stores.
pub const CREDENTIAL_UNAVAILABLE: &str = "credential_unavailable";
const MAX_SECRET_BYTES: usize = 64 * 1024;

/// Harness identity used by the version-1 collector protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum HarnessId {
    /// Codex native login is consumed by its app-server, not exported as a token.
    #[serde(rename = "codex")]
    Codex,
    /// Claude native OAuth may be passed to the configured quota executable.
    #[serde(rename = "claude-code")]
    ClaudeCode,
    /// Explicit token collectors may use the OpenCode harness metadata.
    #[serde(rename = "opencode")]
    OpenCode,
}

impl HarnessId {
    /// Returns the version-1 wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::OpenCode => "opencode",
        }
    }
}

/// Validated nonsecret location; private variants prevent bypassing its grammar.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialRef(Location);

/// Existing store selectors; no variant holds a resolved credential.
#[derive(Clone, PartialEq, Eq)]
enum Location {
    Native(HarnessId),
    Named(HarnessId, String),
    Environment(String),
    File(PathBuf),
    Keychain { service: String, account: String },
}

impl fmt::Debug for CredentialRef {
    /// Redacts even store labels and paths from diagnostic formatting.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialRef(<redacted>)")
    }
}

impl FromStr for CredentialRef {
    type Err = &'static str;

    /// Parses a bounded canonical reference without resolving or echoing its value.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        const INVALID: &str = "invalid credential reference";
        if value.is_empty()
            || value.len() > 256
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(INVALID);
        }
        let location = if let Some(harness) = value.strip_prefix("native:") {
            Location::Native(native_harness(harness).ok_or(INVALID)?)
        } else if let Some(named) = value.strip_prefix("named:") {
            let (harness, label) = named.split_once(':').ok_or(INVALID)?;
            if !(1..=32).contains(&label.len())
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
            {
                return Err(INVALID);
            }
            Location::Named(native_harness(harness).ok_or(INVALID)?, label.into())
        } else if let Some(name) = value.strip_prefix("env:") {
            if name.is_empty()
                || !name.as_bytes()[0].is_ascii_uppercase() && name.as_bytes()[0] != b'_'
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(INVALID);
            }
            Location::Environment(name.into())
        } else if let Some(path) = value.strip_prefix("file:") {
            if !Path::new(path).is_absolute() {
                return Err(INVALID);
            }
            Location::File(path.into())
        } else if let Some(names) = value.strip_prefix("keychain:") {
            let (service, account) = names.split_once(':').ok_or(INVALID)?;
            if [service, account].iter().any(|name| {
                name.is_empty()
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            }) {
                return Err(INVALID);
            }
            Location::Keychain {
                service: service.into(),
                account: account.into(),
            }
        } else {
            return Err(INVALID);
        };
        Ok(Self(location))
    }
}

impl<'de> Deserialize<'de> for CredentialRef {
    /// Keeps serde diagnostics independent of possibly secret rejected input.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Only the two supported native login conventions may select a native store.
fn native_harness(value: &str) -> Option<HarnessId> {
    match value {
        "codex" => Some(HarnessId::Codex),
        "claude-code" => Some(HarnessId::ClaudeCode),
        _ => None,
    }
}

impl CredentialRef {
    /// Checks native account family/harness and required named-home metadata without I/O.
    pub fn validate_binding(
        &self,
        family: &str,
        harness: HarnessId,
        app_home: Option<&Path>,
        runtime_home: Option<&Path>,
        native_directory: Option<&Path>,
    ) -> Result<(), &'static str> {
        let login = match &self.0 {
            Location::Native(id) | Location::Named(id, _) => Some(*id),
            _ => None,
        };
        if let Some(id) = login {
            let expected = match id {
                HarnessId::Codex => "openai",
                HarnessId::ClaudeCode => "anthropic",
                HarnessId::OpenCode => return Err("invalid native harness"),
            };
            if harness != id || family != expected {
                return Err("credential authentication binding mismatch");
            }
        }
        if let Location::Named(id, _) = &self.0 {
            if app_home.is_none() || (*id == HarnessId::ClaudeCode && runtime_home.is_none()) {
                return Err("named credential home missing");
            }
            if native_directory.is_some() {
                return Err("named credential cannot use native directory");
            }
        } else if login.is_none() && native_directory.is_some() {
            return Err("token credential cannot use native directory");
        }
        Ok(())
    }

    /// Resolves exactly this account for private stdin; native Codex only returns its directory.
    /// The caller validates the binding before invoking this read-only operation.
    pub(crate) fn auth(
        &self,
        app_home: Option<&Path>,
        runtime_home: Option<&Path>,
        native_directory: Option<&Path>,
    ) -> Result<Value, &'static str> {
        let token = match &self.0 {
            Location::Native(HarnessId::Codex) => {
                let (directory, _) = native_config(HarnessId::Codex, native_directory)?;
                return Ok(json!({"kind":"native_login", "directory":directory}));
            }
            Location::Named(HarnessId::Codex, label) => {
                let directory = app_home
                    .ok_or(CREDENTIAL_UNAVAILABLE)?
                    .join("accounts/codex")
                    .join(label);
                return Ok(json!({"kind":"native_login", "directory":directory}));
            }
            Location::Native(HarnessId::ClaudeCode) => {
                let (directory, overridden) =
                    native_config(HarnessId::ClaudeCode, native_directory)?;
                claude_oauth(&directory, overridden)?
            }
            Location::Named(HarnessId::ClaudeCode, label) => {
                let directory = claude_account_config(
                    app_home.ok_or(CREDENTIAL_UNAVAILABLE)?,
                    runtime_home.ok_or(CREDENTIAL_UNAVAILABLE)?,
                    label,
                )?;
                claude_oauth(&directory, true)?
            }
            Location::Environment(name) => {
                checked_secret(std::env::var(name).map_err(|_| CREDENTIAL_UNAVAILABLE)?)?
            }
            Location::File(path) => read_private_file(path)?,
            Location::Keychain { service, account } => {
                keychain_lookup(service, Some(account)).ok_or(CREDENTIAL_UNAVAILABLE)?
            }
            Location::Native(HarnessId::OpenCode) | Location::Named(HarnessId::OpenCode, _) => {
                return Err(CREDENTIAL_UNAVAILABLE);
            }
        };
        Ok(json!({"kind":"token", "token":token}))
    }
}

/// Bounds token bytes and refuses an absent credential; never returns rejected content.
fn checked_secret(value: String) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > MAX_SECRET_BYTES {
        Err(CREDENTIAL_UNAVAILABLE)
    } else {
        Ok(value)
    }
}

/// Reads a regular private current-user file with a byte ceiling and no FIFO blocking.
fn read_private_file(path: &Path) -> Result<String, &'static str> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| CREDENTIAL_UNAVAILABLE)?;
    let metadata = file.metadata().map_err(|_| CREDENTIAL_UNAVAILABLE)?;
    // SAFETY: geteuid has no pointer or ownership preconditions and only reads process identity.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != uid
        || metadata.len() > MAX_SECRET_BYTES as u64
    {
        return Err(CREDENTIAL_UNAVAILABLE);
    }
    let mut bytes = Vec::new();
    file.take(MAX_SECRET_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CREDENTIAL_UNAVAILABLE)?;
    if bytes.len() > MAX_SECRET_BYTES {
        return Err(CREDENTIAL_UNAVAILABLE);
    }
    let value = String::from_utf8(bytes).map_err(|_| CREDENTIAL_UNAVAILABLE)?;
    checked_secret(value.trim_end_matches(['\r', '\n']).to_owned())
}

/// Selects the explicit native directory or the harness's host environment convention.
fn native_config(
    harness: HarnessId,
    explicit: Option<&Path>,
) -> Result<(PathBuf, bool), &'static str> {
    if let Some(directory) = explicit {
        return Ok((directory.to_owned(), true));
    }
    let (variable, suffix) = match harness {
        HarnessId::Codex => ("CODEX_HOME", ".codex"),
        HarnessId::ClaudeCode => ("CLAUDE_CONFIG_DIR", ".claude"),
        HarnessId::OpenCode => return Err(CREDENTIAL_UNAVAILABLE),
    };
    let result = if let Some(directory) = std::env::var_os(variable).filter(|v| !v.is_empty()) {
        (PathBuf::from(directory), true)
    } else {
        (
            PathBuf::from(
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .ok_or(CREDENTIAL_UNAVAILABLE)?,
            )
            .join(suffix),
            false,
        )
    };
    if !result.0.is_absolute() {
        return Err(CREDENTIAL_UNAVAILABLE);
    }
    Ok(result)
}

/// Resolves canonical/legacy named Claude homes; distinct existing homes are ambiguous.
fn claude_account_config(
    app_home: &Path,
    runtime_home: &Path,
    label: &str,
) -> Result<PathBuf, &'static str> {
    let canonical = app_home
        .join("accounts/claude")
        .join(label)
        .join("claude-config");
    let name = runtime_home
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(CREDENTIAL_UNAVAILABLE)?;
    let legacy = runtime_home
        .with_file_name(format!("{name}@{label}"))
        .join("claude-config");
    match (canonical.is_dir(), legacy.is_dir()) {
        (true, true) => {
            let canonical_real = canonical
                .canonicalize()
                .map_err(|_| CREDENTIAL_UNAVAILABLE)?;
            let legacy_real = legacy.canonicalize().map_err(|_| CREDENTIAL_UNAVAILABLE)?;
            if canonical_real != legacy_real {
                Err(CREDENTIAL_UNAVAILABLE)
            } else {
                Ok(canonical)
            }
        }
        (false, true) => Ok(legacy),
        _ => Ok(canonical),
    }
}

/// Reads a Claude token from its private JSON store or that same login's Keychain service.
fn claude_oauth(directory: &Path, overridden: bool) -> Result<String, &'static str> {
    if let Ok(document) = read_private_file(&directory.join(".credentials.json"))
        && let Some(token) = access_token(&document)
    {
        return Ok(token);
    }
    keychain_lookup(&keychain_service(directory, overridden), None)
        .as_deref()
        .and_then(access_token)
        .ok_or(CREDENTIAL_UNAVAILABLE)
}

/// Matches Claude Code's service name; explicit directories append eight SHA-256 hex digits.
fn keychain_service(directory: &Path, overridden: bool) -> String {
    if !overridden {
        return "Claude Code-credentials".into();
    }
    let digest = Sha256::digest(directory.to_string_lossy().as_bytes());
    let suffix: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("Claude Code-credentials-{suffix}")
}

/// Extracts only a bounded nonempty access token; parse failures never expose the document.
fn access_token(document: &str) -> Option<String> {
    let value: Value = serde_json::from_str(document).ok()?;
    checked_secret(
        value
            .pointer("/claudeAiOauth/accessToken")
            .or_else(|| value.get("accessToken"))?
            .as_str()?
            .to_owned(),
    )
    .ok()
}

/// Detects echoes of the context's direct token in JSON keys or values.
/// Native-login directory references do not supply a token for this comparison.
pub fn output_contains_credential(output: &Value, context: &Value) -> bool {
    context
        .pointer("/auth/token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .is_some_and(|token| contains_secret(output, token))
}

/// Searches only strings and keys, recursively, without serializing or logging the token.
fn contains_secret(value: &Value, token: &str) -> bool {
    match value {
        Value::String(text) => text.contains(token),
        Value::Array(items) => items.iter().any(|item| contains_secret(item, token)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(token) || contains_secret(value, token)),
        _ => false,
    }
}

/// Keychain is unavailable on Linux; no fallback, credential copy or privilege change occurs.
#[cfg(not(target_os = "macos"))]
fn keychain_lookup(_service: &str, _account: Option<&str>) -> Option<String> {
    None
}

/// Resolves the effective user's login Keychain and bounds command/output to five seconds.
#[cfg(target_os = "macos")]
fn keychain_lookup(service: &str, account: Option<&str>) -> Option<String> {
    use std::{
        ffi::CStr,
        mem::MaybeUninit,
        process::{Command, Stdio},
        time::Duration,
    };
    let mut entry = MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0u8; MAX_SECRET_BYTES];
    let mut result = std::ptr::null_mut();
    // SAFETY: all writable pointers refer to live storage of their documented sizes.
    // getpwuid_r owns no returned memory; pw_dir remains valid while buffer is live.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: successful getpwuid_r initialized entry and its strings in our live buffer.
    let entry = unsafe { entry.assume_init() };
    if entry.pw_dir.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r supplies a terminated pw_dir in buffer, which is still alive.
    let home = unsafe { CStr::from_ptr(entry.pw_dir) }.to_str().ok()?;
    let keychain = Path::new(home).join("Library/Keychains/login.keychain-db");
    let mut command = Command::new("/usr/bin/security");
    command.arg("find-generic-password").arg("-s").arg(service);
    if let Some(account) = account {
        command.arg("-a").arg(account);
    }
    command
        .arg("-w")
        .arg(keychain)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    keychain_command(command, Duration::from_secs(5))
}

/// Drains stdout while waiting, bounding both bytes and EOF even if a descendant holds the pipe.
#[cfg(target_os = "macos")]
fn keychain_command(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> Option<String> {
    use std::{os::fd::AsRawFd, process::Stdio, time::Instant};
    let deadline = Instant::now() + timeout;
    let mut child = command.stdout(Stdio::piped()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let mut bytes = Vec::new();
    let mut eof = false;
    let result = loop {
        if Instant::now() >= deadline {
            break None;
        }
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(_) => break None,
        };
        if let Some(status) = status {
            if !status.success() {
                break None;
            }
            if eof {
                break String::from_utf8(bytes)
                    .ok()
                    .and_then(|value| checked_secret(value.trim().to_owned()).ok());
            }
        }
        if eof {
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        let mut descriptor = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor is one live pollfd and stdout owns its valid descriptor.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 10) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break None;
        }
        if ready == 0 {
            continue;
        }
        let mut chunk = [0u8; 4096];
        match stdout.read(&mut chunk) {
            Ok(0) => eof = true,
            Ok(count) => {
                if bytes.len() + count > MAX_SECRET_BYTES {
                    break None;
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break None,
        }
    };
    if result.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Creates a synthetic credential with explicit permission bits.
    fn secret_file(directory: &Path, content: &[u8], mode: u32) -> PathBuf {
        let path = directory.join("synthetic-secret");
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn private_files_are_bounded_and_errors_hide_content() {
        let directory = tempfile::tempdir().unwrap();
        let token = b"SYNTHETIC_SECRET_CANARY\r\n";
        let path = secret_file(directory.path(), token, 0o600);
        assert_eq!(read_private_file(&path).unwrap(), "SYNTHETIC_SECRET_CANARY");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(read_private_file(&path), Err(CREDENTIAL_UNAVAILABLE));
        let path = secret_file(directory.path(), &vec![b'x'; MAX_SECRET_BYTES], 0o600);
        assert_eq!(read_private_file(&path).unwrap().len(), MAX_SECRET_BYTES);
        let path = secret_file(directory.path(), &vec![b'x'; MAX_SECRET_BYTES + 1], 0o600);
        assert_eq!(read_private_file(&path), Err(CREDENTIAL_UNAVAILABLE));
        let path = secret_file(directory.path(), b"\xffSYNTHETIC_SECRET_CANARY", 0o600);
        assert_eq!(read_private_file(&path), Err(CREDENTIAL_UNAVAILABLE));
        assert_eq!(
            read_private_file(directory.path()),
            Err(CREDENTIAL_UNAVAILABLE)
        );
        assert_eq!(
            read_private_file(&directory.path().join("absent")),
            Err(CREDENTIAL_UNAVAILABLE)
        );
    }

    #[test]
    fn references_are_strict_and_redacted() {
        for text in [
            "native:codex",
            "named:codex:team_2",
            "native:claude-code",
            "named:claude-code:team-2",
            "env:EXAMPLE_TOKEN",
            "file:/fixture/token",
            "keychain:service:account",
        ] {
            let reference = text.parse::<CredentialRef>().unwrap();
            assert_eq!(format!("{reference:?}"), "CredentialRef(<redacted>)");
        }
        for text in [
            "SYNTHETIC_SECRET_CANARY",
            "env:lower",
            "env:1BAD",
            "file:relative",
            "native:opencode",
            "named:codex:../escape",
            "keychain:a:b:c",
            "native:codex\n",
        ] {
            assert_eq!(
                text.parse::<CredentialRef>(),
                Err("invalid credential reference")
            );
        }
    }

    #[test]
    fn named_claude_selection_preserves_isolation() {
        let directory = tempfile::tempdir().unwrap();
        let app = directory.path().join("app");
        let runtime = directory.path().join("runtime");
        let canonical = app.join("accounts/claude/team/claude-config");
        let legacy = directory.path().join("runtime@team/claude-config");
        assert_eq!(
            claude_account_config(&app, &runtime, "team").unwrap(),
            canonical
        );
        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(
            claude_account_config(&app, &runtime, "team").unwrap(),
            legacy
        );
        std::fs::create_dir_all(&canonical).unwrap();
        assert_eq!(
            claude_account_config(&app, &runtime, "team"),
            Err(CREDENTIAL_UNAVAILABLE)
        );
        assert_ne!(
            keychain_service(&canonical, true),
            keychain_service(&legacy, true)
        );
        assert_eq!(
            keychain_service(&canonical, false),
            "Claude Code-credentials"
        );
    }

    #[test]
    fn claude_json_and_echo_detection_are_narrow() {
        assert_eq!(
            access_token(r#"{"claudeAiOauth":{"accessToken":"SYNTHETIC_SECRET_CANARY"}}"#)
                .as_deref(),
            Some("SYNTHETIC_SECRET_CANARY")
        );
        assert_eq!(
            access_token(r#"{"refreshToken":"SYNTHETIC_SECRET_CANARY"}"#),
            None
        );
        assert_eq!(access_token("SYNTHETIC_SECRET_CANARY"), None);
        let context = json!({"auth":{"token":"SYNTHETIC_SECRET_CANARY"}});
        assert!(output_contains_credential(
            &json!({"rows":[{"prefix-SYNTHETIC_SECRET_CANARY":1}]}),
            &context
        ));
        assert!(output_contains_credential(
            &json!(["prefix-SYNTHETIC_SECRET_CANARY-suffix"]),
            &context
        ));
        assert!(!output_contains_credential(
            &json!({"available":0}),
            &context
        ));
        assert!(!output_contains_credential(
            &json!(""),
            &json!({"auth":{"kind":"native_login"}})
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_keychain_and_missing_named_login_fail_closed() {
        assert_eq!(
            keychain_lookup("synthetic-service", Some("synthetic-account")),
            None
        );
        let directory = tempfile::tempdir().unwrap();
        let reference: CredentialRef = "named:claude-code:absent".parse().unwrap();
        assert_eq!(
            reference.auth(
                Some(directory.path()),
                Some(&directory.path().join("runtime")),
                None
            ),
            Err(CREDENTIAL_UNAVAILABLE)
        );
    }
}
