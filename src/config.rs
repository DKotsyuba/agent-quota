//! Strict, bounded independent quota configuration; loading never resolves credentials.
//!
//! A source uses `account`, `provider`, `auth_family`, a typed `credential` string,
//! `models = [{ id = "model", native_model = "native-lane" }]`, `[sources.harness]`
//! with `id`/absolute `binary`, and `[sources.collector]` with an absolute command.
//! `limits_source` defaults to `"exec"`; `"none"` forbids a collector and retains
//! the account/models as configured but unobserved, without resolving credentials.
//! Optional source `app_home` locates named logins; Claude named logins also need
//! `harness.home`. `native_directory` explicitly overrides a native login's home.
//! Without that override only context creation consults the harness's native
//! environment convention. Unknown fields and literal/untyped secrets are rejected.

pub use crate::credentials::HarnessId;
use crate::{
    collector::CollectorBinding,
    credentials::{CREDENTIAL_UNAVAILABLE, CredentialRef},
    quota::{AccountId, MAX_OUTPUT_MODELS},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_SOURCES: usize = 256;

/// Poll cadence and source declarations, containing references rather than tokens.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaConfig {
    /// Poll interval in seconds, default 300; accepted range 1–86400.
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    /// Per-store retained sample limit, default 1000; accepted range 1–1000000.
    #[serde(default = "default_retention")]
    pub sample_retention: usize,
    /// At most 256 declarations; an empty list is valid and performs no collection.
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
}

/// Whether quota is collected by an external executable or remains unobserved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitsSource {
    /// Execute the required configured collector; default for existing configurations.
    #[default]
    Exec,
    /// Do not collect this source; configured models retain unknown quota.
    None,
}

/// Which process owns collection; external probes run under their credential owner's UID.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// The service resolves credentials and runs the collector on its polling schedule.
    #[default]
    Service,
    /// A separately launched probe resolves credentials; the service only verifies ingestion.
    External,
}

/// One provider alias for an explicitly identified physical account and quota policy.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Stable account identity used for persistence and alias deduplication.
    pub account: AccountId,
    /// Display scope of the configured provider alias.
    pub provider: String,
    /// Account authentication family; native references must match their harness.
    pub auth_family: String,
    /// Validated existing store selector, never credential bytes.
    pub credential: CredentialRef,
    /// Harness metadata supplied to the collector over stdin.
    pub harness: HarnessConfig,
    /// Existing application's home for named-account conventions; never created here.
    #[serde(default)]
    pub app_home: Option<PathBuf>,
    /// Optional explicit native directory; Claude treats it as a Keychain override.
    #[serde(default)]
    pub native_directory: Option<PathBuf>,
    /// Native lane definitions; empty declarations retain the original no-model behavior.
    pub models: Vec<ModelConfig>,
    /// Quota acquisition mode; omitted means executable collection.
    #[serde(default)]
    pub limits_source: LimitsSource,
    /// Execution owner; defaults to service collection and never changes OS identity.
    #[serde(default)]
    pub execution: ExecutionMode,
    /// Exact invocation policy, required for `exec` and forbidden for `none`.
    #[serde(default)]
    pub collector: Option<CollectorBinding>,
}

/// Harness metadata only; a harness is never started while loading configuration.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Supported harness protocol identity.
    pub id: HarnessId,
    /// Absolute harness executable path passed to the configured collector.
    pub binary: PathBuf,
    /// Existing runtime home required to resolve legacy named Claude logins.
    #[serde(default)]
    pub home: Option<PathBuf>,
}

/// Provider model alias and optional native quota lane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// Provider model identifier.
    pub id: String,
    /// Native quota lane; defaults to `id` when absent.
    #[serde(default)]
    pub native_model: Option<String>,
}

impl fmt::Debug for QuotaConfig {
    /// Prevents accidental dumping of rejected input or configured process arguments.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuotaConfig")
            .field("source_count", &self.sources.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SourceConfig {
    /// Omits all user-provided fields, including mistyped inline credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceConfig(<redacted>)")
    }
}

/// Returns the default five-minute polling cadence.
fn default_interval() -> u64 {
    300
}

/// Returns the default retained sample ceiling.
fn default_retention() -> usize {
    1000
}

impl QuotaConfig {
    /// Reads a regular configuration file with a 1 MiB ceiling; errors never quote contents.
    #[cfg(test)]
    pub fn load(path: &Path) -> Result<Self, String> {
        Self::load_versioned(path).map(|(config, _)| config)
    }

    /// Reads and hashes the exact accepted bytes so callers can identify configuration changes.
    pub fn load_versioned(path: &Path) -> Result<(Self, String), String> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|_| "config_unavailable".to_owned())?;
        let metadata = file
            .metadata()
            .map_err(|_| "config_unavailable".to_owned())?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES as u64 {
            return Err("config_too_large_or_not_regular".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "config_unavailable".to_owned())?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err("config_too_large".into());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "config_invalid".to_owned())?;
        let config = Self::parse(text)?;
        let digest = Sha256::digest(&bytes);
        let revision = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok((config, revision))
    }

    /// Parses TOML without echoing parser excerpts; validates bounds and all alias bindings.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err("config_too_large".into());
        }
        let config: Self = toml::from_str(text).map_err(|_| "config_invalid".to_owned())?;
        config.validate()?;
        Ok(config)
    }

    /// Checks numeric/resource bounds and account/source consistency without reading stores.
    pub fn validate(&self) -> Result<(), String> {
        self.plan().map(|_| ())
    }

    /// Plans one invocation per account/stable source with the union of native model lanes.
    /// Contradictory aliases fail before credential reads, subprocesses or state writes.
    /// Sources with `limits_source = "none"` remain in configuration but are not scheduled.
    pub fn plan(&self) -> Result<Vec<SourceConfig>, String> {
        if !(1..=86_400).contains(&self.interval_seconds)
            || !(1..=1_000_000).contains(&self.sample_retention)
            || self.sources.len() > MAX_SOURCES
        {
            return Err("config_bounds_invalid".into());
        }
        let mut accounts: BTreeMap<&AccountId, &SourceConfig> = BTreeMap::new();
        let mut units: BTreeMap<(AccountId, String), SourceConfig> = BTreeMap::new();
        for source in &self.sources {
            source.validate()?;
            if let Some(previous) = accounts.insert(&source.account, source)
                && !same_account(previous, source)
            {
                return Err("conflicting_account_alias".into());
            }
            let Some(identity) = source.source() else {
                continue;
            };
            let key = (source.account.clone(), identity);
            if let Some(unit) = units.get_mut(&key) {
                if unit.execution != source.execution {
                    return Err("conflicting_execution_alias".into());
                }
                if unit.collector != source.collector {
                    return Err("conflicting_collector_alias".into());
                }
                let mut models: BTreeMap<String, ModelConfig> = unit
                    .models
                    .iter()
                    .map(|model| (model.lane().to_owned(), model.clone()))
                    .collect();
                for model in &source.models {
                    // Original collector planning uses the later provider alias's metadata.
                    models.insert(model.lane().to_owned(), model.clone());
                }
                if models.len() > MAX_OUTPUT_MODELS {
                    return Err("too_many_native_models".into());
                }
                unit.models = models.into_values().collect();
            } else {
                units.insert(key, source.clone());
            }
        }
        Ok(units.into_values().collect())
    }
}

/// Prevents one global account id from silently selecting different authentication contexts.
fn same_account(left: &SourceConfig, right: &SourceConfig) -> bool {
    left.auth_family == right.auth_family
        && left.credential == right.credential
        && left.harness == right.harness
        && left.app_home == right.app_home
        && left.native_directory == right.native_directory
}

impl SourceConfig {
    /// Validates the public constructor fields without accessing credentials or executables.
    pub fn validate(&self) -> Result<(), String> {
        if !provider_identifier(&self.provider)
            || self.auth_family.is_empty()
            || self.auth_family.len() > 32
            || !self
                .auth_family
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("source_identity_invalid".into());
        }
        if !valid_path(&self.harness.binary)
            || [&self.harness.home, &self.app_home, &self.native_directory]
                .into_iter()
                .flatten()
                .any(|path| !valid_path(path))
        {
            return Err("source_path_invalid".into());
        }
        self.credential
            .validate_binding(
                &self.auth_family,
                self.harness.id,
                self.app_home.as_deref(),
                self.harness.home.as_deref(),
                self.native_directory.as_deref(),
            )
            .map_err(str::to_owned)?;
        if self.limits_source == LimitsSource::None && self.execution == ExecutionMode::External {
            return Err("none_external_forbidden".into());
        }
        match (self.limits_source, &self.collector) {
            (LimitsSource::Exec, Some(collector)) => collector
                .validate()
                .map_err(|_| "collector_config_invalid".to_owned())?,
            (LimitsSource::None, None) => {}
            (LimitsSource::Exec, None) => return Err("exec_collector_required".into()),
            (LimitsSource::None, Some(_)) => return Err("none_collector_forbidden".into()),
        }
        if self.models.len() > MAX_OUTPUT_MODELS {
            return Err("too_many_native_models".into());
        }
        let mut ids = BTreeSet::new();
        for model in &self.models {
            if !model_text(&model.id) || !model_text(model.lane()) || !ids.insert(&model.id) {
                return Err("model_config_invalid".into());
            }
        }
        Ok(())
    }

    /// Returns an executable source's configured identity or compatible command/args digest.
    /// Returns `None` for unobserved sources or invalid missing collectors; validate first.
    pub fn source(&self) -> Option<String> {
        if self.limits_source == LimitsSource::None {
            return None;
        }
        let collector = self.collector.as_ref()?;
        if !collector.source.is_empty() {
            return Some(collector.source.clone());
        }
        // Only command and literal arguments participate; no secret is resolved here.
        let value = json!({"args":collector.args,"command":collector.command});
        let json = value.to_string();
        let mut canonical = String::with_capacity(json.len());
        for character in json.chars() {
            if character > '~' {
                for unit in character.encode_utf16(&mut [0u16; 2]) {
                    canonical.push_str(&format!("\\u{unit:04x}"));
                }
            } else {
                canonical.push(character);
            }
        }
        let digest = Sha256::digest(canonical.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Some(format!("exec:{hex}"))
    }

    /// Returns native quota lane names for output normalization.
    pub fn model_ids(&self) -> BTreeSet<String> {
        self.models
            .iter()
            .map(|model| model.lane().to_owned())
            .collect()
    }

    /// Builds version-1 private collector stdin; never log, persist or return this value to users.
    /// Native Codex passes a home directory; all other supported stores pass only their token.
    /// Refuses unobserved sources before any credential lookup.
    pub fn context(&self, at: f64) -> Result<Value, &'static str> {
        if !at.is_finite() || at < 0.0 {
            return Err("collector_input_invalid");
        }
        self.validate().map_err(|_| "collector_config_invalid")?;
        if self.limits_source == LimitsSource::None {
            return Err("collector_config_invalid");
        }
        let auth = self
            .credential
            .auth(
                self.app_home.as_deref(),
                self.harness.home.as_deref(),
                self.native_directory.as_deref(),
            )
            .map_err(|_| CREDENTIAL_UNAVAILABLE)?;
        let models: BTreeMap<_, _> = self
            .models
            .iter()
            .map(|model| {
                (
                    model.lane(),
                    json!({"id":model.id,"native_model":model.lane()}),
                )
            })
            .collect();
        Ok(
            json!({"version":1,"account":{"id":self.account,"auth_family":self.auth_family},"models":models,"now":at,"auth":auth,"harness":{"id":self.harness.id.as_str(),"command":self.harness.binary}}),
        )
    }
}

impl ModelConfig {
    /// Returns the native lane or the model id when no native alias is configured.
    pub fn lane(&self) -> &str {
        self.native_model.as_deref().unwrap_or(&self.id)
    }
}

/// Accepts agent-run provider IDs exactly, preserving their case.
fn provider_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

/// Requires absolute bounded UTF-8 paths free of control characters.
fn valid_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| text.len() <= 4096 && !text.chars().any(char::is_control))
}

/// Bounds the model text which reaches collector context and normalization scope.
fn model_text(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Supplies a native directory explicitly, so tests never inspect host environment/auth.
    fn fixture() -> &'static str {
        r#"
[[sources]]
account = "account-one"
provider = "openai"
auth_family = "openai"
credential = "native:codex"
native_directory = "/synthetic/codex"
models = [{ id = "alias", native_model = "native-lane" }]
[sources.harness]
id = "codex"
binary = "/synthetic/bin/codex"
[sources.collector]
command = "/synthetic/bin/quota"
source = "provider-quota"
"#
    }

    #[test]
    fn defaults_and_native_context_need_no_credentials() {
        let config = QuotaConfig::parse(fixture()).unwrap();
        assert_eq!(config.interval_seconds, 300);
        assert_eq!(config.sample_retention, 1000);
        let context = config.sources[0].context(1234.0).unwrap();
        assert_eq!(context["version"], 1);
        assert_eq!(
            context["auth"],
            json!({"kind":"native_login","directory":"/synthetic/codex"})
        );
        assert_eq!(context["models"]["native-lane"]["id"], "alias");
        assert_eq!(
            config.sources[0].context(f64::NAN),
            Err("collector_input_invalid")
        );
        assert!(QuotaConfig::parse("").unwrap().sources.is_empty());
    }

    /// Provider names follow Run's case-sensitive grammar; account IDs keep their own grammar.
    #[test]
    fn provider_identifiers_match_agent_run() {
        let config = QuotaConfig::parse(fixture()).unwrap();
        for valid in ["Claude", "claude", "1gateway", "A-_.9", &"A".repeat(64)] {
            let mut source = config.sources[0].clone();
            source.provider = valid.into();
            assert!(source.validate().is_ok(), "valid provider: {valid}");
        }
        for invalid in ["", "-bad", "_bad", ".bad", "bad/name", "é", &"a".repeat(65)] {
            let mut source = config.sources[0].clone();
            source.provider = invalid.into();
            assert_eq!(source.validate().unwrap_err(), "source_identity_invalid");
        }
        assert!("Account".parse::<AccountId>().is_err());
        assert!("1account".parse::<AccountId>().is_err());
    }

    #[test]
    fn limits_source_defaults_to_exec_and_explicit_exec_is_compatible() {
        let default = QuotaConfig::parse(fixture()).unwrap();
        let explicit = QuotaConfig::parse(
            &fixture().replace("[[sources]]", "[[sources]]\nlimits_source = \"exec\""),
        )
        .unwrap();
        assert_eq!(default.sources[0].limits_source, LimitsSource::Exec);
        assert!(default.sources[0].collector.is_some());
        assert_eq!(default.plan().unwrap().len(), 1);
        assert_eq!(explicit.plan().unwrap().len(), 1);
        assert_eq!(default.sources[0].source(), explicit.sources[0].source());
    }

    #[test]
    fn execution_defaults_to_service_and_external_remains_in_plan() {
        let default = QuotaConfig::parse(fixture()).unwrap();
        assert_eq!(default.sources[0].execution, ExecutionMode::Service);
        for (text, expected) in [
            ("service", ExecutionMode::Service),
            ("external", ExecutionMode::External),
        ] {
            let config = QuotaConfig::parse(&fixture().replace(
                "[[sources]]",
                &format!("[[sources]]\nexecution = \"{text}\""),
            ))
            .unwrap();
            assert_eq!(config.plan().unwrap()[0].execution, expected);
        }
        assert_eq!(
            QuotaConfig::parse(
                &fixture().replace("[[sources]]", "[[sources]]\nexecution = \"sudo\"",)
            )
            .unwrap_err(),
            "config_invalid"
        );
    }

    #[test]
    fn external_execution_rejects_unobserved_sources_and_conflicting_aliases() {
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let mut alias = config.sources[0].clone();
        alias.provider = "another-alias".into();
        alias.execution = ExecutionMode::External;
        config.sources.push(alias);
        assert_eq!(config.plan().unwrap_err(), "conflicting_execution_alias");
        config.sources[0].execution = ExecutionMode::External;
        assert_eq!(config.plan().unwrap().len(), 1);
        config.sources[1].limits_source = LimitsSource::None;
        config.sources[1].collector = None;
        assert_eq!(config.plan().unwrap_err(), "none_external_forbidden");
    }

    #[test]
    fn none_retains_models_but_never_builds_credential_context() {
        let without_collector = fixture().split_once("[sources.collector]").unwrap().0;
        let text = without_collector
            .replace("[[sources]]", "[[sources]]\nlimits_source = \"none\"")
            .replace("native:codex", "file:/synthetic/absent-credential")
            .replace("native_directory = \"/synthetic/codex\"", "");
        let config = QuotaConfig::parse(&text).unwrap();
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].limits_source, LimitsSource::None);
        assert!(config.sources[0].collector.is_none());
        assert_eq!(config.sources[0].source(), None);
        assert_eq!(
            config.sources[0].model_ids(),
            BTreeSet::from(["native-lane".into()])
        );
        assert!(config.plan().unwrap().is_empty());
        // No credential lookup: the result is configuration refusal, not credential_unavailable.
        assert_eq!(
            config.sources[0].context(1234.0),
            Err("collector_config_invalid")
        );
    }

    #[test]
    fn limits_source_rejects_missing_forbidden_or_unknown_collectors() {
        let without_collector = fixture().split_once("[sources.collector]").unwrap().0;
        assert_eq!(
            QuotaConfig::parse(without_collector).unwrap_err(),
            "exec_collector_required"
        );
        let explicit_exec =
            without_collector.replace("[[sources]]", "[[sources]]\nlimits_source = \"exec\"");
        assert_eq!(
            QuotaConfig::parse(&explicit_exec).unwrap_err(),
            "exec_collector_required"
        );
        let contradictory =
            fixture().replace("[[sources]]", "[[sources]]\nlimits_source = \"none\"");
        assert_eq!(
            QuotaConfig::parse(&contradictory).unwrap_err(),
            "none_collector_forbidden"
        );
        let unknown = fixture().replace("[[sources]]", "[[sources]]\nlimits_source = \"lua\"");
        assert_eq!(QuotaConfig::parse(&unknown).unwrap_err(), "config_invalid");
    }

    #[test]
    fn none_alias_does_not_extend_an_exec_plan_or_bypass_account_consistency() {
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let mut alias = config.sources[0].clone();
        alias.provider = "unobserved-alias".into();
        alias.limits_source = LimitsSource::None;
        alias.collector = None;
        alias.models = vec![ModelConfig {
            id: "unobserved-model".into(),
            native_model: None,
        }];
        config.sources.push(alias);
        assert_eq!(config.sources.len(), 2);
        let plan = config.plan().unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].model_ids(), BTreeSet::from(["native-lane".into()]));
        assert_eq!(
            config.sources[1].model_ids(),
            BTreeSet::from(["unobserved-model".into()])
        );
        config.sources[1].native_directory = Some("/synthetic/different-account".into());
        assert_eq!(config.plan().unwrap_err(), "conflicting_account_alias");
    }

    #[test]
    fn invalid_unknown_or_mismatched_config_never_echoes_input() {
        for text in [
            fixture().replace("native:codex", "SYNTHETIC_SECRET_CANARY"),
            fixture().replace("auth_family = \"openai\"", "auth_family = \"anthropic\""),
            fixture().replace("id = \"codex\"", "id = \"claude-code\""),
            fixture().replace("credential = \"native:codex\"", ""),
            fixture().replace("/synthetic/bin/quota", "relative"),
            format!("interval_seconds = 0\n{}", fixture()),
            format!("sample_retention = 0\n{}", fixture()),
            format!("token = \"SYNTHETIC_SECRET_CANARY\"\n{}", fixture()),
            fixture().replace(
                "source = \"provider-quota\"",
                "token = \"SYNTHETIC_SECRET_CANARY\"",
            ),
        ] {
            let error = QuotaConfig::parse(&text).unwrap_err();
            assert!(!error.contains("SYNTHETIC_SECRET_CANARY"));
        }
    }

    #[test]
    fn aliases_deduplicate_and_union_models_but_conflicts_fail() {
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let mut alias = config.sources[0].clone();
        alias.provider = "other-alias".into();
        alias.models.push(ModelConfig {
            id: "another".into(),
            native_model: None,
        });
        config.sources.push(alias);
        let units = config.plan().unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(
            units[0].model_ids(),
            BTreeSet::from(["another".into(), "native-lane".into()])
        );
        config.sources[1]
            .collector
            .as_mut()
            .unwrap()
            .timeout_seconds += 1;
        assert_eq!(config.plan().unwrap_err(), "conflicting_collector_alias");
        config.sources[1]
            .collector
            .as_mut()
            .unwrap()
            .timeout_seconds -= 1;
        config.sources[1].native_directory = Some("/different/account".into());
        assert_eq!(config.plan().unwrap_err(), "conflicting_account_alias");
    }

    #[test]
    fn config_load_is_bounded_and_regular() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, fixture()).unwrap();
        assert!(QuotaConfig::load(&path).is_ok());
        std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES + 1]).unwrap();
        assert!(QuotaConfig::load(&path).is_err());
        assert!(QuotaConfig::load(directory.path()).is_err());
    }

    #[test]
    fn source_identity_is_stable_under_deadline_change() {
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let source = &mut config.sources[0];
        source.collector.as_mut().unwrap().source.clear();
        let identity = source.source().unwrap();
        source.collector.as_mut().unwrap().timeout_seconds += 1;
        assert_eq!(source.source().as_deref(), Some(identity.as_str()));
        source
            .collector
            .as_mut()
            .unwrap()
            .args
            .push("different".into());
        assert_ne!(source.source().as_deref(), Some(identity.as_str()));
        assert!(identity.starts_with("exec:"));
        assert_eq!(identity.len(), 69);
    }

    #[test]
    fn claude_native_and_named_context_read_only_the_selected_private_store() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let app_home = directory.path().join("app");
        let selected = app_home.join("accounts/claude/team/claude-config");
        std::fs::create_dir_all(&selected).unwrap();
        let credentials = selected.join(".credentials.json");
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"SYNTHETIC_CLAUDE_CANARY","refreshToken":"UNRELATED_REFRESH_CANARY"}}"#,
        ).unwrap();
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let source = &mut config.sources[0];
        source.auth_family = "anthropic".into();
        source.harness.id = HarnessId::ClaudeCode;
        source.credential = "native:claude-code".parse().unwrap();
        source.native_directory = Some(selected);
        let native = source.context(1234.0).unwrap();
        assert_eq!(
            native["auth"],
            json!({"kind":"token", "token":"SYNTHETIC_CLAUDE_CANARY"})
        );
        assert_eq!(native["account"]["auth_family"], "anthropic");
        assert_eq!(native["harness"]["id"], "claude-code");
        assert!(!native.to_string().contains("UNRELATED_REFRESH_CANARY"));

        source.credential = "named:claude-code:team".parse().unwrap();
        source.native_directory = None;
        source.app_home = Some(app_home);
        source.harness.home = Some(directory.path().join("claude-runtime"));
        assert_eq!(source.context(1234.0).unwrap(), native);
        assert!(!format!("{source:?}").contains("SYNTHETIC_CLAUDE_CANARY"));
    }

    #[test]
    fn named_codex_passes_the_exact_account_directory_without_reading_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let source = &mut config.sources[0];
        source.credential = "named:codex:team".parse().unwrap();
        source.native_directory = None;
        source.app_home = Some(directory.path().to_owned());
        let context = source.context(1234.0).unwrap();
        let expected = directory.path().join("accounts/codex/team");
        assert!(!expected.exists());
        assert_eq!(
            context["auth"],
            json!({"kind":"native_login", "directory":expected})
        );
        assert!(context["auth"].get("token").is_none());
    }

    #[test]
    fn source_digest_matches_agent_run_ascii_escaping_and_surrogate_pairs() {
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let source = &mut config.sources[0];
        let collector = source.collector.as_mut().unwrap();
        collector.source.clear();
        collector.command = "/synthetic/é/🦀".into();
        collector.args = vec!["héllo".into(), "😀".into(), "\u{7f}".into()];
        // Pinned agent-run canonical::dumps(value, true) sorts args before command
        // and escapes DEL/non-ASCII with lowercase UTF-16 surrogate sequences.
        // Independently calculated SHA-256 of these exact ASCII bytes:
        // {"args":["h\u00e9llo","\ud83d\ude00","\u007f"],"command":"/synthetic/\u00e9/\ud83e\udd80"}
        assert_eq!(
            source.source().unwrap(),
            "exec:f20fb0a82809bed34b10a88a505dc64b702352d35226323c87a6dd61fea5f365"
        );
    }
}
