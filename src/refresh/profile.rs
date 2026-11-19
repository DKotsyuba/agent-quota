//! The narrowly supported Claude CLI invocation and its managed-policy checks.

use super::ledger::Store;
use crate::{
    collector::{self, HelperError},
    config::SourceConfig,
    credentials::{
        CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED, CREDENTIAL_REFRESH_UNSUPPORTED_HOST, ClaudeLogin,
    },
};
use std::{
    fs,
    io::ErrorKind,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

/// Scope the renewal contract to the exact CLI version reviewed for this profile.
const VERSION: &[u8] = b"2.1.322 (Claude Code)";
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const VERSION_BYTES: usize = 128;
const SETTINGS: &str = r#"{"disableAllHooks":true,"disableClaudeAiConnectors":true}"#;
const EMPTY_MCP: &str = r#"{"mcpServers":{}}"#;
const PROMPT: &str = "Reply with OK.";

#[cfg(target_os = "linux")]
const MANAGED_ROOT: &str = "/etc/claude-code";
#[cfg(target_os = "macos")]
const MANAGED_ROOT: &str = "/Library/Application Support/ClaudeCode";
const MANAGED_DOMAIN: &str = "com.anthropic.claudecode.plist";

/// Fixed print-mode flags. Managed policy must already be excluded by the host check.
pub(super) fn cli_arguments() -> [&'static str; 18] {
    [
        "-p",
        "--model",
        "haiku",
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        EMPTY_MCP,
        "--no-session-persistence",
        "--disable-slash-commands",
        "--setting-sources",
        "",
        "--settings",
        SETTINGS,
        "--system-prompt",
        PROMPT,
        "--",
        PROMPT,
    ]
}

/// Fixed OS policy locations and the credential owner's home.
pub(super) struct Host {
    pub(super) home: PathBuf,
    pub(super) managed: PathBuf,
    pub(super) preferences: Option<PathBuf>,
    pub(super) unsupported: bool,
    pub(super) keychain_only: bool,
}

impl Host {
    /// Uses platform paths; no environment variable can redirect the policy check.
    pub(super) fn current() -> Self {
        #[cfg(target_os = "linux")]
        let unsupported = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map_or(true, |release| {
                release.to_ascii_lowercase().contains("microsoft")
            });
        #[cfg(target_os = "macos")]
        let unsupported = false;
        Self {
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            managed: PathBuf::from(MANAGED_ROOT),
            preferences: cfg!(target_os = "macos").then(|| "/Library/Managed Preferences".into()),
            unsupported,
            keychain_only: cfg!(target_os = "macos"),
        }
    }

    /// Refuses any managed input, including dangling links and inaccessible policy locations.
    fn check(&self, login: &ClaudeLogin, store: &Store) -> Result<(), &'static str> {
        if self.unsupported || !self.home.is_absolute() {
            return Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST);
        }
        // An active Anthropic profile can take precedence over the subscription or
        // serve as a fallback when it expires. The macOS profile uses Keychain only
        // so a leftover plaintext document cannot identify a different account.
        require_absent(&self.home.join(".config/anthropic"))?;
        if self.keychain_only {
            require_absent(&login.directory.join(".credentials.json"))?;
        } else {
            store
                .verify_credential_file()
                .map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)?;
        }
        // Check both locations of Claude's cached remote settings.
        for name in [
            "managed-settings.json",
            "managed-settings.d",
            "managed-mcp.json",
            "CLAUDE.md",
        ] {
            require_absent(&self.managed.join(name))?;
        }
        for path in [&login.directory, &self.home.join(".claude")] {
            require_absent(&path.join("remote-settings.json"))?;
        }
        if let Some(root) = &self.preferences {
            self.check_preferences(root)?;
        }
        store
            .verify_path(&login.directory)
            .map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    }

    /// Checks the macOS system and per-user managed preferences for Claude Code's domain.
    fn check_preferences(&self, root: &Path) -> Result<(), &'static str> {
        require_absent(&root.join(MANAGED_DOMAIN))?;
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST),
        };
        for entry in entries {
            let entry = entry.map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)?;
            let kind = entry
                .file_type()
                .map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)?;
            if kind.is_dir() || kind.is_symlink() {
                require_absent(&entry.path().join(MANAGED_DOMAIN))?;
            }
        }
        Ok(())
    }

    /// Validates the host and prepares the command without spawning any process.
    pub(super) fn prepare<'a>(
        &'a self,
        source: &SourceConfig,
        login: &'a ClaudeLogin,
        store: &Store,
        home: &'a Path,
    ) -> Result<Cli<'a>, &'static str> {
        self.check(login, store)?;
        let path = source
            .harness
            .binary
            .canonicalize()
            .map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)?;
        let identity = executable_identity(&path)?;
        let temp = collector::temp_dir(home).map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)?;
        Ok(Cli {
            path,
            identity,
            host: self,
            login,
            home,
            temp,
        })
    }
}

/// A verified absence is the only allowed result at a policy filename.
fn require_absent(path: &Path) -> Result<(), &'static str> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        _ => Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST),
    }
}

/// Stable device/inode of the canonical executable used for both CLI invocations.
fn executable_identity(path: &Path) -> Result<(u64, u64), &'static str> {
    let meta = path
        .symlink_metadata()
        .map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)?;
    if !meta.is_file() {
        return Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST);
    }
    Ok((meta.dev(), meta.ino()))
}

/// Command allowlist for both invocations. Clearing the whole environment drops collector
/// grants, auth overrides, provider routing, proxies and loader injection variables.
fn command(binary: &Path, os_home: &Path, work: &Path, temp: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .current_dir(work)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", os_home)
        .env("TMPDIR", temp)
        .env("LANG", "C")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1")
        .env("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1")
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false");
    command
}

/// Verified CLI and the exact environment needed for one OAuth-enabled run.
pub(super) struct Cli<'a> {
    path: PathBuf,
    identity: (u64, u64),
    host: &'a Host,
    login: &'a ClaudeLogin,
    home: &'a Path,
    temp: PathBuf,
}

impl Cli<'_> {
    /// Checks the exact accepted version using a bounded probe with a synthetic home.
    pub(super) async fn qualify(&self) -> Result<(), &'static str> {
        let mut command = command(&self.path, &self.temp, &self.temp, &self.temp);
        command.arg("--version");
        match collector::run_helper(command, VERSION_TIMEOUT, Some(VERSION_BYTES)).await {
            Ok(bytes) if bytes.trim_ascii() == VERSION => Ok(()),
            Err(HelperError::CleanupUnconfirmed) => Err(CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED),
            _ => Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST),
        }
    }

    /// Return cleanup uncertainty even when the CLI updated the token or exited successfully.
    pub(super) async fn run(&self, store: &Store, timeout: Duration) -> Result<(), &'static str> {
        self.host.check(self.login, store)?;
        if executable_identity(&self.path)? != self.identity {
            return Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST);
        }
        if !self.login.state()?.personal_subscription {
            return Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST);
        }
        let mut command = command(&self.path, &self.host.home, self.home, &self.temp);
        command.args(cli_arguments());
        if self.login.overridden {
            // The exact path also selects the macOS Keychain service for explicit logins.
            command.env("CLAUDE_CONFIG_DIR", &self.login.directory);
        }
        match collector::run_helper(command, timeout, None).await {
            Err(HelperError::CleanupUnconfirmed) => Err(CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED),
            // The credential store is the only evidence of renewal.
            _ => Ok(()),
        }
    }
}
