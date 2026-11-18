//! Opt-in renewal of Claude OAuth logins by the configured official Claude CLI.
//!
//! Quota never exchanges, copies or writes OAuth tokens. When a collection is due and the
//! selected login's recorded access-token expiry is within Claude Code's own renewal window,
//! the configured harness binary runs one minimal print-mode turn: a small model, no tools,
//! no MCP servers, no hooks, no settings sources, no slash commands and no saved session.
//! Claude Code awaits its own OAuth renewal before that request, under the cross-process
//! refresh lock its agents use, and writes its own store. Quota then re-reads only the expiry.
//!
//! Runs are single-flight per private home and capped per login to
//! [`MAX_RENEWALS_PER_DAY`] in any trailing 24 hours by a private attempt ledger.

use crate::{
    collector,
    config::SourceConfig,
    credentials::{
        CREDENTIAL_EXPIRED, CREDENTIAL_REFRESH_FAILED, CREDENTIAL_REFRESH_LIMITED,
        CREDENTIAL_UNAVAILABLE, ClaudeLogin,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    time::Duration,
};

/// Deadline for one CLI run; renewal precedes its single small model request.
const RENEWAL_TIMEOUT: Duration = Duration::from_secs(90);
/// Most CLI renewal attempts per login in any trailing 24 hours.
pub(crate) const MAX_RENEWALS_PER_DAY: usize = 6;
/// Length of the attempt-cap window in seconds.
const DAY_SECONDS: f64 = 86_400.0;
/// Private attempt ledger and single-flight lock names inside the Quota home.
const LEDGER: &str = "oauth-renewals.json";
const LOCK: &str = "oauth-renewal.lock";
/// Byte ceiling and login-count ceiling of the attempt ledger.
const MAX_LEDGER_BYTES: u64 = 64 * 1024;
const MAX_LEDGER_LOGINS: usize = 64;
/// Fixed prompt; the reply is discarded.
const PROMPT: &str = "Reply with OK.";

/// Fixed CLI arguments: one print-mode turn with every optional extension source disabled.
/// `--setting-sources ""` skips user, project and local settings (and so their hooks,
/// MCP servers and plugins); `disableAllHooks` and `--strict-mcp-config` also refuse hooks
/// and MCP servers from any remaining source.
fn cli_arguments() -> [&'static str; 16] {
    [
        "-p",
        "--model",
        "haiku",
        "--tools",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--disable-slash-commands",
        "--setting-sources",
        "",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--system-prompt",
        PROMPT,
        "--",
        PROMPT,
    ]
}

/// Outcome of one renewal check; failures are fixed credential codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Renewal {
    /// The source did not opt in; nothing was read or started.
    NotConfigured,
    /// The token has no recorded expiry or is outside the renewal window; no CLI was started.
    NotDue,
    /// After renewal, the store holds a token outside the renewal window.
    Renewed,
    /// The token was not renewed but has not expired yet; a later collection tries again.
    StillValid,
}

/// Renews an opted-in Claude login through its CLI when its token is near or past expiry.
///
/// `at` is the collection time in Unix seconds and `home` the private Quota home used as the
/// working and temporary directory and for the lock and ledger. Errors are fixed codes:
/// `credential_expired` when an expired login has no refresh token, `credential_refresh_limited`
/// when the daily cap (or an unusable ledger) prevented an attempt on an expired login,
/// `credential_refresh_failed` when the login is still expired or unreadable afterwards, and
/// `credential_unavailable` when the store cannot be read before renewal.
pub(crate) async fn renew_if_due(
    source: &SourceConfig,
    home: &Path,
    at: f64,
) -> Result<Renewal, &'static str> {
    renew(source, home, at, RENEWAL_TIMEOUT).await
}

/// Implements [`renew_if_due`] with an explicit CLI deadline.
async fn renew(
    source: &SourceConfig,
    home: &Path,
    at: f64,
    timeout: Duration,
) -> Result<Renewal, &'static str> {
    if !source.harness.refresh_oauth {
        return Ok(Renewal::NotConfigured);
    }
    let login = source
        .credential
        .claude_login(
            source.app_home.as_deref(),
            source.harness.home.as_deref(),
            source.native_directory.as_deref(),
        )?
        .ok_or(CREDENTIAL_UNAVAILABLE)?;
    let before = login.state()?;
    if !before.renewal_due(at) {
        return Ok(Renewal::NotDue);
    }
    if !before.refreshable {
        return if before.expired(at) {
            Err(CREDENTIAL_EXPIRED)
        } else {
            Ok(Renewal::StillValid)
        };
    }
    let mut limited = false;
    // A busy lock means another renewal is running; the store below is the only evidence.
    if let Ok(Some(_lock)) = RenewalLock::try_acquire(home) {
        // Another renewal may have finished while this collection was waiting.
        if !login.state()?.renewal_due(at) {
            return Ok(Renewal::Renewed);
        }
        if admit(home, &ledger_key(&login), at) {
            run_cli(source, &login, home, timeout).await;
        } else {
            limited = true;
        }
    }
    match login.state() {
        Ok(after) if !after.renewal_due(at) => Ok(Renewal::Renewed),
        Ok(after) if !after.expired(at) => Ok(Renewal::StillValid),
        _ if limited => Err(CREDENTIAL_REFRESH_LIMITED),
        _ => Err(CREDENTIAL_REFRESH_FAILED),
    }
}

/// Runs the CLI once with null stdio, a restricted environment and the selected store.
/// Its exit status is ignored: the model reply is irrelevant and the store is the evidence.
async fn run_cli(source: &SourceConfig, login: &ClaudeLogin, home: &Path, timeout: Duration) {
    let env_from = source
        .collector
        .as_ref()
        .map(|collector| collector.env_from.as_slice())
        .unwrap_or_default();
    // An unusable environment skips the CLI.
    let Ok(mut environment) = collector::restricted_environment(env_from, home) else {
        return;
    };
    // Name exactly the selected store, as Agent Run does for its Claude agents.
    environment.remove("CLAUDE_CONFIG_DIR");
    environment.insert(
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
        "1".into(),
    );
    let mut command = tokio::process::Command::new(&source.harness.binary);
    command
        .args(cli_arguments())
        .current_dir(home)
        .env_clear()
        .envs(environment);
    if login.overridden {
        command.env("CLAUDE_CONFIG_DIR", &login.directory);
    }
    let _ = collector::run_silent(command, timeout).await;
}

/// Exclusive non-blocking single-flight lock in the private home, released on drop.
/// The descriptor is close-on-exec, so a lingering CLI descendant cannot keep it.
struct RenewalLock {
    /// Open lock file; closing it releases the lock.
    _file: File,
}

impl RenewalLock {
    /// Returns `None` when another renewal holds the lock; errors refuse to run the CLI.
    fn try_acquire(home: &Path) -> Result<Option<Self>, ()> {
        let file = private_file(&home.join(LOCK), true)?;
        // SAFETY: `file` owns a valid open descriptor for the duration of this call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(Some(Self { _file: file }))
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(())
        }
    }
}

/// Opens a regular, private, current-user file without following a final symlink.
fn private_file(path: &Path, create: bool) -> Result<File, ()> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(drop)?;
    let metadata = file.metadata().map_err(drop)?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(());
    }
    Ok(file)
}

/// Nonsecret ledger key: a digest of the selected login directory path.
fn ledger_key(login: &ClaudeLogin) -> String {
    let digest = Sha256::digest(login.directory.as_os_str().as_encoded_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Version-1 attempt ledger: recent CLI start times per login key.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u8,
    logins: BTreeMap<String, Vec<f64>>,
}

/// Records an attempt for `key` at `at` unless the trailing-day cap is reached.
/// The caller holds the renewal lock. An unreadable or unwritable ledger refuses the attempt,
/// so the cap cannot be bypassed by corrupting it.
fn admit(home: &Path, key: &str, at: f64) -> bool {
    let path = home.join(LEDGER);
    let mut ledger = match private_file(&path, false) {
        Ok(file) => {
            let mut bytes = Vec::new();
            if file
                .take(MAX_LEDGER_BYTES + 1)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() as u64 > MAX_LEDGER_BYTES
            {
                return false;
            }
            match serde_json::from_slice::<Ledger>(&bytes) {
                Ok(ledger) if ledger.version == 1 => ledger,
                _ => return false,
            }
        }
        // Only a truly absent path starts a new ledger; a dangling symlink is refused.
        Err(()) if path.symlink_metadata().is_err() => Ledger {
            version: 1,
            logins: BTreeMap::new(),
        },
        Err(()) => return false,
    };
    // Future entries (a clock step back) stay counted until a day after them has passed.
    let recent = |time: &f64| time.is_finite() && *time > at - DAY_SECONDS;
    ledger.logins.retain(|_, times| {
        times.retain(recent);
        !times.is_empty()
    });
    let times = ledger.logins.entry(key.to_owned()).or_default();
    if times.len() >= MAX_RENEWALS_PER_DAY || !at.is_finite() {
        return false;
    }
    times.push(at);
    if ledger.logins.len() > MAX_LEDGER_LOGINS {
        return false;
    }
    let Ok(bytes) = serde_json::to_vec(&ledger) else {
        return false;
    };
    let temporary = home.join(format!("{LEDGER}.tmp"));
    let _ = std::fs::remove_file(&temporary);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(&bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::QuotaConfig;
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};

    /// Fixed collection time; expiries in fixtures are relative to it.
    const AT: f64 = 1_000_000.0;

    /// Synthetic login, private home and mock CLI; no real login, CLI or network is used.
    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        login: PathBuf,
        source: SourceConfig,
    }

    impl Fixture {
        /// Writes a mock `claude` whose behavior is selected by the `mode` file.
        fn new(refresh_oauth: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_owned();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            let home = root.join("quota-home");
            let login = root.join("claude-config");
            std::fs::create_dir(&home).unwrap();
            std::fs::create_dir(&login).unwrap();
            let cli = root.join("claude");
            let script = format!(
                r#"#!/bin/sh
root='{root}'
printf '%s|%s|%s\n' "${{CLAUDE_CONFIG_DIR:-}}" "${{CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:-}}" "$(pwd)" >> "$root/calls"
printf '%s\n' "$@" > "$root/arguments"
/usr/bin/env | /usr/bin/cut -d= -f1 | /usr/bin/sort > "$root/environment"
/bin/ls -l /proc/self/fd 2>/dev/null | /bin/grep -q 'oauth-renewal.lock' && : > "$root/descriptor-leaked"
printf '%s\n' 'SYNTHETIC_CLI_STDOUT'
printf '%s\n' 'SYNTHETIC_CLI_STDERR' >&2
case "$(cat "$root/mode")" in
renew) cat "$root/renewed.json" > "$CLAUDE_CONFIG_DIR/.credentials.json" ;;
clear) printf '%s' '{{"claudeAiOauth":{{}}}}' > "$CLAUDE_CONFIG_DIR/.credentials.json" ;;
hang) exec sleep 30 ;;
fail) exit 1 ;;
esac
exit 0
"#,
                root = root.display()
            );
            std::fs::write(&cli, script).unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(root.join("mode"), "noop").unwrap();
            let config = format!(
                r#"
[[sources]]
account = "account-one"
provider = "claude"
auth_family = "anthropic"
credential = "native:claude-code"
native_directory = "{login}"
execution = "external"
models = [{{ id = "model-one" }}]
[sources.harness]
id = "claude-code"
binary = "{cli}"
refresh_oauth = {refresh_oauth}
[sources.collector]
command = "/synthetic/bin/quota"
source = "claude-usage"
"#,
                login = login.display(),
                cli = cli.display(),
            );
            let source = QuotaConfig::parse(&config).unwrap().sources.remove(0);
            Self {
                _directory: directory,
                root,
                home,
                login,
                source,
            }
        }

        /// Stores a private synthetic OAuth document expiring `offset` seconds after [`AT`].
        fn store(&self, offset: i64, refresh: bool) {
            let path = self.login.join(".credentials.json");
            std::fs::write(&path, document("SYNTHETIC_OLD_ACCESS", offset, refresh)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        /// Selects the mock CLI behavior and the document a `renew` run writes.
        fn mode(&self, mode: &str, renewed_offset: i64) {
            std::fs::write(self.root.join("mode"), mode).unwrap();
            std::fs::write(
                self.root.join("renewed.json"),
                document("SYNTHETIC_NEW_ACCESS", renewed_offset, true),
            )
            .unwrap();
        }

        /// Returns the recorded CLI invocations, one line each.
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.root.join("calls"))
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }

        async fn renew(&self) -> Result<Renewal, &'static str> {
            self.renew_at(AT).await
        }

        async fn renew_at(&self, at: f64) -> Result<Renewal, &'static str> {
            renew(&self.source, &self.home, at, Duration::from_secs(5)).await
        }
    }

    /// Builds a synthetic Claude OAuth document; tokens are fixed canaries.
    fn document(access: &str, offset: i64, refresh: bool) -> String {
        let expiry = (AT as i64 + offset) * 1000;
        let refresh = if refresh {
            r#","refreshToken":"SYNTHETIC_REFRESH_SECRET""#
        } else {
            ""
        };
        format!(r#"{{"claudeAiOauth":{{"accessToken":"{access}","expiresAt":{expiry}{refresh}}}}}"#)
    }

    #[tokio::test]
    async fn unconfigured_or_fresh_logins_never_start_the_cli() {
        let fixture = Fixture::new(false);
        fixture.store(-60, true);
        assert_eq!(fixture.renew().await, Ok(Renewal::NotConfigured));
        let fixture = Fixture::new(true);
        fixture.store(301, true);
        assert_eq!(fixture.renew().await, Ok(Renewal::NotDue));
        std::fs::write(
            fixture.login.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"SYNTHETIC_OLD_ACCESS","refreshToken":"SYNTHETIC_REFRESH_SECRET"}}"#,
        )
        .unwrap();
        assert_eq!(fixture.renew().await, Ok(Renewal::NotDue));
        assert!(fixture.calls().is_empty());
        assert!(!fixture.home.join(LEDGER).exists());
    }

    #[tokio::test]
    async fn due_login_is_renewed_by_one_isolated_cli_turn() {
        let fixture = Fixture::new(true);
        fixture.store(299, true);
        fixture.mode("renew", 28_800);
        assert_eq!(fixture.renew().await, Ok(Renewal::Renewed));
        assert_eq!(
            fixture.calls(),
            [format!(
                "{}|1|{}",
                fixture.login.display(),
                fixture.home.display()
            )]
        );
        let arguments = std::fs::read_to_string(fixture.root.join("arguments")).unwrap();
        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            [
                "-p",
                "--model",
                "haiku",
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--disable-slash-commands",
                "--setting-sources",
                "",
                "--settings",
                r#"{"disableAllHooks":true}"#,
                "--system-prompt",
                "Reply with OK.",
                "--",
                "Reply with OK.",
            ]
        );
        let allowed = [
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CONFIG_DIR",
            "HOME",
            "LANG",
            "LC_ALL",
            "LOGNAME",
            "OLDPWD",
            "PATH",
            "PWD",
            "SHLVL",
            "TEMP",
            "TMP",
            "TMPDIR",
            "USER",
            "_",
        ];
        let environment = std::fs::read_to_string(fixture.root.join("environment")).unwrap();
        for name in environment.lines() {
            assert!(allowed.contains(&name), "unexpected variable {name}");
        }
        // The renewed store is used and Quota left its mode alone.
        let context = fixture.source.context(AT).unwrap();
        assert_eq!(context["auth"]["token"], "SYNTHETIC_NEW_ACCESS");
        assert!(!context.to_string().contains("SYNTHETIC_REFRESH_SECRET"));
        let mode = std::fs::metadata(fixture.login.join(".credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // The ledger records one attempt time, privately, and nothing secret.
        let ledger_path = fixture.home.join(LEDGER);
        let ledger = std::fs::read_to_string(&ledger_path).unwrap();
        assert!(!ledger.contains("SYNTHETIC"));
        let ledger: Ledger = serde_json::from_str(&ledger).unwrap();
        assert_eq!(ledger.logins.values().collect::<Vec<_>>(), [&vec![AT]]);
        let mode = std::fs::metadata(&ledger_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn unrenewed_logins_report_fixed_codes_by_expiry() {
        // Near expiry but still valid: collection proceeds and a later round retries.
        let fixture = Fixture::new(true);
        fixture.store(120, true);
        assert_eq!(fixture.renew().await, Ok(Renewal::StillValid));
        assert_eq!(fixture.calls().len(), 1);
        assert_eq!(
            fixture.source.context(AT).unwrap()["auth"]["token"],
            "SYNTHETIC_OLD_ACCESS"
        );
        // Expired and not renewed, whatever the CLI exit status.
        for mode in ["noop", "fail", "clear"] {
            let fixture = Fixture::new(true);
            fixture.store(-1, true);
            fixture.mode(mode, 0);
            assert_eq!(
                fixture.renew().await,
                Err(CREDENTIAL_REFRESH_FAILED),
                "{mode}"
            );
            assert_eq!(fixture.calls().len(), 1);
        }
        // A renewal that writes an already-expired token is not success either.
        let fixture = Fixture::new(true);
        fixture.store(-1, true);
        fixture.mode("renew", -5);
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_FAILED));
    }

    #[tokio::test]
    async fn attempts_are_capped_per_login_per_trailing_day() {
        let fixture = Fixture::new(true);
        fixture.store(120, true);
        for index in 0..MAX_RENEWALS_PER_DAY + 2 {
            let at = AT + index as f64 * 60.0;
            fixture.store(120 + index as i64 * 60, true);
            assert_eq!(fixture.renew_at(at).await, Ok(Renewal::StillValid));
        }
        assert_eq!(fixture.calls().len(), MAX_RENEWALS_PER_DAY);
        // Once expired, the cap is reported distinctly and the CLI is not started.
        fixture.store(-1, true);
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_LIMITED));
        assert_eq!(fixture.calls().len(), MAX_RENEWALS_PER_DAY);
        // A day after the first attempt, one more is admitted.
        let later = AT + DAY_SECONDS + 1.0;
        fixture.store((DAY_SECONDS as i64) - 1, true);
        assert_eq!(
            fixture.renew_at(later).await,
            Err(CREDENTIAL_REFRESH_FAILED)
        );
        assert_eq!(fixture.calls().len(), MAX_RENEWALS_PER_DAY + 1);
    }

    #[tokio::test]
    async fn corrupt_or_unsafe_ledger_refuses_the_attempt() {
        let fixture = Fixture::new(true);
        fixture.store(-1, true);
        let ledger = fixture.home.join(LEDGER);
        std::fs::write(&ledger, "SYNTHETIC_CORRUPT").unwrap();
        std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_LIMITED));
        std::fs::write(&ledger, r#"{"version":1,"logins":{}}"#).unwrap();
        std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_LIMITED));
        std::fs::remove_file(&ledger).unwrap();
        std::os::unix::fs::symlink(fixture.root.join("elsewhere"), &ledger).unwrap();
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_LIMITED));
        assert!(!fixture.root.join("elsewhere").exists());
        assert!(fixture.calls().is_empty());
    }

    #[tokio::test]
    async fn renewal_is_single_flight_and_does_not_leak_its_lock() {
        let fixture = Fixture::new(true);
        fixture.store(-1, true);
        let held = RenewalLock::try_acquire(&fixture.home).unwrap().unwrap();
        assert!(RenewalLock::try_acquire(&fixture.home).unwrap().is_none());
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_FAILED));
        assert!(fixture.calls().is_empty());
        drop(held);
        fixture.mode("renew", 28_800);
        assert_eq!(fixture.renew().await, Ok(Renewal::Renewed));
        assert_eq!(fixture.calls().len(), 1);
        assert!(!fixture.root.join("descriptor-leaked").exists());
        // The lock is released after the run.
        assert!(RenewalLock::try_acquire(&fixture.home).unwrap().is_some());
    }

    #[tokio::test]
    async fn expired_login_without_refresh_token_skips_the_cli() {
        let fixture = Fixture::new(true);
        fixture.store(0, false);
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_EXPIRED));
        fixture.store(100, false);
        assert_eq!(fixture.renew().await, Ok(Renewal::StillValid));
        assert!(fixture.calls().is_empty());
        assert_eq!(
            fixture.source.context(AT).unwrap()["auth"]["token"],
            "SYNTHETIC_OLD_ACCESS"
        );
        fixture.store(0, false);
        assert_eq!(fixture.source.context(AT).unwrap_err(), CREDENTIAL_EXPIRED);
    }

    #[tokio::test]
    async fn hung_cli_is_bounded_and_reported() {
        let fixture = Fixture::new(true);
        fixture.store(-1, true);
        fixture.mode("hang", 0);
        let started = std::time::Instant::now();
        let result = renew(
            &fixture.source,
            &fixture.home,
            AT,
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(result, Err(CREDENTIAL_REFRESH_FAILED));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn missing_store_or_cli_fail_closed() {
        let fixture = Fixture::new(true);
        assert_eq!(fixture.renew().await, Err(CREDENTIAL_UNAVAILABLE));
        fixture.store(-1, true);
        let mut source = fixture.source.clone();
        source.harness.binary = fixture.root.join("absent-cli");
        assert_eq!(
            renew(&source, &fixture.home, AT, Duration::from_secs(5)).await,
            Err(CREDENTIAL_REFRESH_FAILED)
        );
    }

    #[test]
    fn refresh_is_limited_to_claude_logins() {
        let fixture = Fixture::new(true);
        assert!(fixture.source.validate().is_ok());
        let mut token = fixture.source.clone();
        token.credential = "file:/synthetic/token".parse().unwrap();
        token.native_directory = None;
        assert_eq!(token.validate().unwrap_err(), "oauth_refresh_unsupported");
        token.harness.refresh_oauth = false;
        assert!(token.validate().is_ok());
    }
}
