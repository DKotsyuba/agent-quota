#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::config::QuotaConfig;
use ledger::{DAY_SECONDS, LEDGER, MAX_RENEWALS_PER_DAY};
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
    host: Host,
}

impl Fixture {
    /// Writes a mock `claude` whose behavior is selected by the `mode` file.
    fn new(refresh_oauth: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("quota-home");
        let login = root.join("claude-config");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(&login).unwrap();
        let cli = root.join("claude");
        let script = format!(
            r#"#!/bin/sh
root='{root}'
if [ "$1" = --version ]; then
    [ -z "${{CLAUDE_CONFIG_DIR+x}}" ] || exit 1
    printf '%s\n' version >> "$root/version-calls"
    if [ -f "$root/cache-during-version" ]; then
        cat "$root/cache-during-version" > "$root/claude-config/remote-settings.json"
    fi
    cat "$root/version"
    exit 0
fi
printf '%s|%s|%s\n' "${{CLAUDE_CONFIG_DIR:-}}" "${{CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:-}}" "$(pwd)" >> "$root/calls"
printf '%s\n' "$@" > "$root/arguments"
/usr/bin/env | /usr/bin/cut -d= -f1 | /usr/bin/sort > "$root/environment"
/bin/ls -l /proc/self/fd 2>/dev/null | /bin/grep -q 'oauth-renewal.lock' && : > "$root/descriptor-leaked"
if [ -e "$root/managed/managed-settings.json" ] || [ -d "$root/managed/managed-settings.d" ]; then
    : > "$root/managed-hook-ran"
fi
if [ -e "$root/managed/managed-mcp.json" ]; then
    : > "$root/managed-mcp-loaded"
    exit 1
fi
if [ -e "$CLAUDE_CONFIG_DIR/settings.json" ]; then
    /bin/grep -Fxq -- '{{"disableAllHooks":true,"disableClaudeAiConnectors":true}}' "$root/arguments" &&
    /bin/grep -A1 -Fx -- '--setting-sources' "$root/arguments" | /bin/grep -qx '' ||
    : > "$root/user-hook-ran"
fi
if [ -e .mcp.json ]; then
    /bin/grep -Fxq -- '--strict-mcp-config' "$root/arguments" &&
    /bin/grep -Fxq -- '{{"mcpServers":{{}}}}' "$root/arguments" &&
    [ "$ENABLE_CLAUDEAI_MCP_SERVERS" = false ] ||
    : > "$root/user-mcp-loaded"
fi
printf '%s\n' 'SYNTHETIC_CLI_STDOUT'
printf '%s\n' 'SYNTHETIC_CLI_STDERR' >&2
case "$(cat "$root/mode")" in
renew) cat "$root/renewed.json" > "$CLAUDE_CONFIG_DIR/.credentials.json" ;;
clear) printf '%s' '{{"claudeAiOauth":{{}}}}' > "$CLAUDE_CONFIG_DIR/.credentials.json" ;;
hang) printf '%s\n' "$$" > "$root/hanging-leader"; exec sleep 30 ;;
orphan) sleep 15 & printf '%s\n' "$!" > "$root/descendant" ;;
fail) exit 1 ;;
esac
exit 0
"#,
            root = root.display()
        );
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.join("mode"), "noop").unwrap();
        std::fs::write(root.join("version"), "2.1.322 (Claude Code)\n").unwrap();
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
        let mut source = QuotaConfig::parse(&config).unwrap().sources.remove(0);
        source.collector.as_mut().unwrap().env_from = [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "https_proxy",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NODE_EXTRA_CA_CERTS",
        ]
        .map(str::to_owned)
        .to_vec();
        let host = Host {
            home: root.join("os-home"),
            managed: root.join("managed"),
            ancestry_root: root.clone(),
            unsupported: false,
        };
        Self {
            _directory: directory,
            root,
            home,
            login,
            source,
            host,
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
        renew(
            &self.source,
            &self.home,
            at,
            &self.host,
            Duration::from_secs(5),
            || Ok(at),
        )
        .await
    }

    /// Name of this fixture's store-local ledger.
    fn ledger_path(&self) -> PathBuf {
        self.login.join(LEDGER.to_str().unwrap())
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
    format!(
        r#"{{"claudeAiOauth":{{"accessToken":"{access}","subscriptionType":"max","expiresAt":{expiry}{refresh}}}}}"#
    )
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
    assert!(!fixture.ledger_path().exists());
    assert!(!fixture.root.join("version-calls").exists());
}

#[tokio::test]
async fn due_login_is_renewed_by_one_isolated_cli_turn() {
    let fixture = Fixture::new(true);
    fixture.store(299, true);
    fixture.mode("renew", 28_800);
    std::fs::write(
        fixture.login.join("settings.json"),
        r#"{"hooks":{"SessionStart":[]}}"#,
    )
    .unwrap();
    std::fs::write(
        fixture.home.join(".mcp.json"),
        r#"{"mcpServers":{"canary":{"command":"false"}}}"#,
    )
    .unwrap();
    assert_eq!(fixture.renew().await, Ok(Renewal::Renewed));
    assert!(!fixture.root.join("user-hook-ran").exists());
    assert!(!fixture.root.join("user-mcp-loaded").exists());
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
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--no-session-persistence",
            "--disable-slash-commands",
            "--setting-sources",
            "",
            "--settings",
            r#"{"disableAllHooks":true,"disableClaudeAiConnectors":true}"#,
            "--system-prompt",
            "Reply with OK.",
            "--",
            "Reply with OK.",
        ]
    );
    let allowed = [
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL",
        "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
        "CLAUDE_CONFIG_DIR",
        "ENABLE_CLAUDEAI_MCP_SERVERS",
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
    let ledger_path = fixture.ledger_path();
    let ledger = std::fs::read_to_string(&ledger_path).unwrap();
    assert!(!ledger.contains("SYNTHETIC"));
    let ledger: Ledger = serde_json::from_str(&ledger).unwrap();
    assert_eq!(ledger.attempts, [AT]);
    assert!(!ledger.pending);
    let mode = std::fs::metadata(&ledger_path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Exercises the cache exception through the full synthetic renewal path.
#[tokio::test]
async fn benign_remote_settings_cache_allows_renewal() {
    let fixture = Fixture::new(true);
    fixture.store(120, true);
    fixture.mode("renew", 28_800);
    let cache = fixture.login.join("remote-settings.json");
    std::fs::write(&cache, r#"{"channelsEnabled":true}"#).unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
    // Policy limits and their sidecars are a separate Claude Code input.
    for (name, content) in [
        (
            "remote-settings.json.signature.json",
            r#"{"jws":"synthetic"}"#,
        ),
        ("remote-settings.json.signature-iat.json", "{}"),
        ("policy-limits.json", r#"{"restrictions":{}}"#),
        (
            "policy-limits.json.signature.json",
            r#"{"jws":"synthetic"}"#,
        ),
        ("policy-limits.json.signature-iat.json", "{}"),
        ("policy-limits.json.stamp.json", "{}"),
    ] {
        std::fs::write(fixture.login.join(name), content).unwrap();
    }
    let fallback = fixture.host.home.join(".claude");
    std::fs::create_dir_all(&fallback).unwrap();
    std::fs::write(
        fallback.join("remote-settings.json"),
        r#"{"channelsEnabled":false}"#,
    )
    .unwrap();
    assert_eq!(fixture.renew().await, Ok(Renewal::Renewed));
    assert_eq!(fixture.calls().len(), 1);
}

/// The default location remains subject to policy checks with a custom login.
#[tokio::test]
async fn unsafe_default_remote_settings_cache_refuses_launch() {
    let fixture = Fixture::new(true);
    fixture.store(120, true);
    let fallback = fixture.host.home.join(".claude");
    std::fs::create_dir_all(&fallback).unwrap();
    std::fs::write(fallback.join("remote-settings.json"), r#"{"env":{}}"#).unwrap();

    assert_eq!(
        fixture.renew().await,
        Err(crate::credentials::CREDENTIAL_REFRESH_POLICY_CACHE_UNKNOWN)
    );
    assert!(!fixture.root.join("version-calls").exists());
    assert!(fixture.calls().is_empty());
    assert!(!fixture.ledger_path().exists());
}

/// Qualification can change the cache, so the print turn must recheck it.
#[tokio::test]
async fn remote_settings_changed_during_qualification_refuses_print_turn() {
    let fixture = Fixture::new(true);
    fixture.store(120, true);
    std::fs::write(
        fixture.login.join("remote-settings.json"),
        r#"{"channelsEnabled":true}"#,
    )
    .unwrap();
    std::fs::write(fixture.root.join("cache-during-version"), r#"{"hooks":{}}"#).unwrap();

    assert_eq!(
        fixture.renew().await,
        Err(crate::credentials::CREDENTIAL_REFRESH_POLICY_CACHE_UNKNOWN)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("version-calls")).unwrap(),
        "version\n"
    );
    assert!(fixture.calls().is_empty());
    let ledger: Ledger =
        serde_json::from_slice(&std::fs::read(fixture.ledger_path()).unwrap()).unwrap();
    assert!(ledger.attempts.is_empty());
    assert!(!ledger.pending);
}

/// Rejects untrusted cache content and file shapes before invoking Claude.
#[tokio::test]
async fn unsafe_remote_settings_cache_refuses_launch() {
    use crate::credentials::CREDENTIAL_REFRESH_POLICY_CACHE_UNKNOWN;

    let fixture = Fixture::new(true);
    fixture.store(120, true);
    let cache = fixture.login.join("remote-settings.json");
    let refusal = Err(CREDENTIAL_REFRESH_POLICY_CACHE_UNKNOWN);
    let oversized = format!("{{{}}}", " ".repeat(4095));
    for content in [
        r#"{"futureSetting":false}"#,
        r#"{"channelsEnabled":true,"permissions":{}}"#,
        r#"{"channelsEnabled":true,"channelsEnabled":false}"#,
        r#"{"channelsEnabled":null}"#,
        r#"[true]"#,
        "{",
        &oversized,
    ] {
        std::fs::write(&cache, content).unwrap();
        assert_eq!(fixture.renew().await, refusal);
    }
    std::fs::write(&cache, "{}").unwrap();
    for mode in [0o620, 0o602] {
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(fixture.renew().await, refusal);
    }
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
    let other = fixture.root.join("other");
    std::fs::hard_link(&cache, &other).unwrap();
    assert_eq!(fixture.renew().await, refusal);
    std::fs::remove_file(&cache).unwrap();
    std::os::unix::fs::symlink(&other, &cache).unwrap();
    assert_eq!(fixture.renew().await, refusal);
    std::fs::remove_file(&cache).unwrap();
    std::fs::remove_file(&other).unwrap();
    std::os::unix::fs::symlink(&other, &cache).unwrap();
    assert_eq!(fixture.renew().await, refusal);
    assert!(!fixture.root.join("version-calls").exists());
    assert!(fixture.calls().is_empty());
    assert!(!fixture.ledger_path().exists());
}

#[tokio::test]
async fn managed_hooks_mcp_and_alternate_profile_refuse_launch() {
    let fixture = Fixture::new(true);
    fixture.store(120, true);
    std::fs::create_dir(&fixture.host.managed).unwrap();
    for (name, content) in [
        (
            "managed-settings.json",
            r#"{"enabledPlugins":{"canary@managed":true},"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"false"}]}]}}"#,
        ),
        (
            "managed-mcp.json",
            r#"{"mcpServers":{"canary":{"command":"false"}}}"#,
        ),
    ] {
        let path = fixture.host.managed.join(name);
        std::fs::write(&path, content).unwrap();
        assert_eq!(
            fixture.renew().await,
            Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
        );
        std::fs::remove_file(path).unwrap();
    }
    let drop_ins = fixture.host.managed.join("managed-settings.d");
    std::fs::create_dir(&drop_ins).unwrap();
    assert_eq!(
        fixture.renew().await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );
    std::fs::remove_dir(drop_ins).unwrap();

    let profile = fixture.host.home.join(".config/anthropic");
    std::fs::create_dir_all(&profile).unwrap();
    assert_eq!(
        fixture.renew().await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );
    std::fs::remove_dir(&profile).unwrap();
    std::fs::write(
        fixture.login.join(".credentials.json"),
        document("canary", 120, true).replace(r#""max""#, r#""team""#),
    )
    .unwrap();
    assert_eq!(
        fixture.renew().await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );
    for marker in [
        "managed-hook-ran",
        "managed-mcp-loaded",
        "version-calls",
        "calls",
    ] {
        assert!(!fixture.root.join(marker).exists());
    }
    assert!(!fixture.ledger_path().exists());
}

#[tokio::test]
async fn unsupported_platform_refuses_before_accessing_the_store() {
    let mut fixture = Fixture::new(true);
    fixture.host.unsupported = true;
    assert_eq!(
        fixture.renew().await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );
    assert!(!fixture.root.join("version-calls").exists());
}

#[tokio::test]
async fn store_paths_with_symlinks_or_writable_ancestors_cannot_launch() {
    let fixture = Fixture::new(true);
    fixture.store(-1, true);
    let alias = fixture.root.join("alias");
    std::os::unix::fs::symlink(&fixture.login, &alias).unwrap();
    let mut source = fixture.source.clone();
    source.native_directory = Some(alias);
    assert_eq!(
        renew(
            &source,
            &fixture.home,
            AT,
            &fixture.host,
            RENEWAL_TIMEOUT,
            || Ok(AT)
        )
        .await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );

    let shared = fixture.root.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
    let nested = shared.join("login");
    std::fs::rename(&fixture.login, &nested).unwrap();
    source.native_directory = Some(nested);
    assert_eq!(
        renew(
            &source,
            &fixture.home,
            AT,
            &fixture.host,
            RENEWAL_TIMEOUT,
            || Ok(AT)
        )
        .await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );
    assert!(!fixture.root.join("version-calls").exists());
}

#[tokio::test]
async fn qualification_is_fenced_and_print_turn_uses_its_later_timestamp() {
    let fixture = Fixture::new(true);
    fixture.store(120, true);
    let store = Store::open(&fixture.login).unwrap();
    let observed = std::cell::Cell::new(false);
    let now = || {
        if !fixture.root.join("version-calls").exists() {
            return Ok(AT);
        }
        if !observed.replace(true) {
            let ledger = Ledger::load(&store).unwrap();
            assert!(ledger.pending);
            assert!(ledger.attempts.is_empty());
        }
        Ok(AT + 30.0)
    };
    assert_eq!(
        renew(
            &fixture.source,
            &fixture.home,
            AT,
            &fixture.host,
            RENEWAL_TIMEOUT,
            now
        )
        .await,
        Ok(Renewal::StillValid)
    );
    assert!(observed.get());
    let ledger = Ledger::load(&store).unwrap();
    assert_eq!(ledger.attempts, [AT + 30.0]);
    assert!(!ledger.pending);
}

#[tokio::test]
async fn refuses_cli_versions_outside_the_pinned_profile() {
    let fixture = Fixture::new(true);
    fixture.store(-1, true);
    for version in ["2.1.321", "2.1.323", "2.1.322-dev", "unexpected"] {
        std::fs::write(
            fixture.root.join("version"),
            format!("{version} (Claude Code)\n"),
        )
        .unwrap();
        assert_eq!(
            fixture.renew().await,
            Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
        );
    }
    assert!(fixture.calls().is_empty());
    let ledger = Ledger::load(&Store::open(&fixture.login).unwrap()).unwrap();
    assert!(ledger.attempts.is_empty());
    assert!(!ledger.pending);
}

#[tokio::test]
async fn expiry_during_cli_is_evaluated_at_the_new_wall_clock() {
    let fixture = Fixture::new(true);
    fixture.store(1, true);
    let calls = fixture.root.join("calls");
    let now = || Ok(if calls.exists() { AT + 2.0 } else { AT });
    let result = renew(
        &fixture.source,
        &fixture.home,
        AT,
        &fixture.host,
        Duration::from_secs(2),
        now,
    )
    .await;
    assert_eq!(result, Err(CREDENTIAL_REFRESH_FAILED));
    assert_eq!(fixture.calls().len(), 1);
    assert_eq!(
        fixture.source.context(AT + 2.0).unwrap_err(),
        CREDENTIAL_EXPIRED
    );
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
    let alternate_home = fixture.root.join("alternate-quota-home");
    std::fs::create_dir(&alternate_home).unwrap();
    assert_eq!(
        renew(
            &fixture.source,
            &alternate_home,
            AT,
            &fixture.host,
            Duration::from_secs(2),
            || Ok(AT)
        )
        .await,
        Err(CREDENTIAL_REFRESH_LIMITED)
    );
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
async fn recent_legacy_attempts_under_aliases_still_count_after_upgrade() {
    let fixture = Fixture::new(true);
    fixture.store(-1, true);
    let old = fixture.home.join("oauth-renewals.json");
    std::fs::write(&old, format!(r#"{{"version":1,"logins":{{"old-key":[{AT},{AT},{AT}],"other-key":[{AT},{AT},{AT}]}}}}"#)).unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_LIMITED));
    assert!(fixture.calls().is_empty());
    assert_eq!(
        fixture.renew_at(AT + DAY_SECONDS + 1.0).await,
        Err(CREDENTIAL_REFRESH_FAILED)
    );
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn ledger_replace_stays_with_the_open_store_after_path_replacement() {
    let fixture = Fixture::new(true);
    let store = Store::open(&fixture.login).unwrap();
    let _lock = store.try_lock().unwrap().unwrap();
    let moved = fixture.root.join("moved");
    std::fs::rename(&fixture.login, &moved).unwrap();
    std::fs::create_dir(&fixture.login).unwrap();
    assert!(store.verify_path(&fixture.login).is_err());
    let mut ledger = Ledger::load(&store).unwrap();
    ledger.pending = true;
    ledger.charge(AT);
    ledger.save(&store).unwrap();
    assert!(!fixture.ledger_path().exists());
    assert!(moved.join(LEDGER.to_str().unwrap()).exists());
}

#[tokio::test]
async fn credential_file_links_cannot_split_one_login_across_store_directories() {
    let fixture = Fixture::new(true);
    fixture.store(-1, true);
    let credentials = fixture.login.join(".credentials.json");
    let other = fixture.root.join("other-credentials.json");
    std::fs::hard_link(&credentials, &other).unwrap();
    assert_eq!(
        fixture.renew().await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );
    std::fs::remove_file(&credentials).unwrap();
    std::os::unix::fs::symlink(&other, &credentials).unwrap();
    assert_eq!(
        fixture.renew().await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
    );
    assert!(!fixture.root.join("version-calls").exists());
}

#[tokio::test]
async fn corrupt_or_unsafe_ledger_refuses_the_attempt() {
    let fixture = Fixture::new(true);
    fixture.store(-1, true);
    let ledger = fixture.ledger_path();
    std::fs::write(&ledger, "SYNTHETIC_CORRUPT").unwrap();
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_LIMITED));
    std::fs::write(&ledger, r#"{"version":2,"attempts":[],"pending":false}"#).unwrap();
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
    let store = Store::open(&fixture.login).unwrap();
    let alias = fixture.root.join("alias");
    std::os::unix::fs::symlink(&fixture.login, &alias).unwrap();
    let using_alias = Store::open(&alias).unwrap();
    let held = store.try_lock().unwrap().unwrap();
    assert!(using_alias.try_lock().unwrap().is_none());
    assert_eq!(fixture.renew().await, Err(CREDENTIAL_REFRESH_FAILED));
    assert!(fixture.calls().is_empty());
    drop(held);
    fixture.mode("renew", 28_800);
    assert_eq!(fixture.renew().await, Ok(Renewal::Renewed));
    assert_eq!(fixture.calls().len(), 1);
    assert!(!fixture.root.join("descriptor-leaked").exists());
    // The lock is released after the run.
    assert!(using_alias.try_lock().unwrap().is_some());
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
        &fixture.host,
        Duration::from_millis(300),
        || Ok(AT),
    )
    .await;
    assert_eq!(result, Err(CREDENTIAL_REFRESH_FAILED));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cli_child_that_outlives_its_leader_is_cleaned_and_releases_admission() {
    let fixture = Fixture::new(true);
    fixture.store(120, true);
    fixture.mode("orphan", 0);
    assert_eq!(fixture.renew().await, Ok(Renewal::StillValid));
    let pid = std::fs::read_to_string(fixture.root.join("descendant"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_stopped(pid));
    assert!(
        !Ledger::load(&Store::open(&fixture.login).unwrap())
            .unwrap()
            .pending
    );
}

#[tokio::test]
async fn interrupted_renewal_fences_later_attempts_even_after_the_cap_window() {
    let fixture = Fixture::new(true);
    fixture.store(120, true);
    fixture.mode("hang", 0);
    let leader = fixture.root.join("hanging-leader");
    tokio::select! {
        result = fixture.renew() => panic!("renewal returned early: {result:?}"),
        () = async {
            while !std::fs::read_to_string(&leader).is_ok_and(|pid| pid.trim().parse::<i32>().is_ok()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => {}
    }
    let pid = std::fs::read_to_string(leader)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_stopped(pid));
    fixture.store(100_000, true);
    assert_eq!(
        fixture.renew_at(AT + DAY_SECONDS + 1.0).await,
        Err(CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED)
    );
    assert_eq!(fixture.calls().len(), 1);
    let ledger = Ledger::load(&Store::open(&fixture.login).unwrap()).unwrap();
    assert_eq!(ledger.attempts, [AT]);
    assert!(ledger.pending);
}

/// Checks actual kernel state instead of treating PID reuse, denial or output as cleanup proof.
fn process_stopped(pid: i32) -> bool {
    match crate::process::inspect(pid) {
        Ok(process) => process.zombie,
        Err(error) => matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)),
    }
}

#[tokio::test]
async fn missing_store_path_component_is_unavailable() {
    let mut fixture = Fixture::new(true);
    fixture.source.native_directory = Some(fixture.root.join("missing").join("claude-config"));
    assert_eq!(fixture.renew().await, Err(CREDENTIAL_UNAVAILABLE));
    assert!(!fixture.root.join("version-calls").exists());
}

#[tokio::test]
async fn missing_store_or_cli_fail_closed() {
    let fixture = Fixture::new(true);
    assert_eq!(fixture.renew().await, Err(CREDENTIAL_UNAVAILABLE));
    fixture.store(-1, true);
    let mut source = fixture.source.clone();
    source.harness.binary = fixture.root.join("absent-cli");
    assert_eq!(
        renew(
            &source,
            &fixture.home,
            AT,
            &fixture.host,
            Duration::from_secs(5),
            || Ok(AT)
        )
        .await,
        Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST)
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
