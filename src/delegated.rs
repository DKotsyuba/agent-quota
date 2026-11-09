//! Credential-owner probes and broker-side verification of bounded, revision-fenced observations.
//!
//! This module never changes UID or filesystem permissions and never writes the quota store.
//! The operator launches the probe as the credential owner with the same nonsecret config bytes.

use crate::{
    collector,
    config::{ExecutionMode, QuotaConfig, SourceConfig},
    credentials,
    quota::{
        CollectorScope, MAX_OUTPUT_MODELS, MAX_OUTPUT_WINDOWS, NormalizedQuotaSnapshot,
        normalize_collector_output,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Maximum encoded observation envelope; transport callers must also bound bytes before decoding.
pub(crate) const MAX_ENVELOPE_BYTES: usize = 2 * 1024 * 1024;

/// Strict worker handoff; all scope claims must match the broker's current configuration.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u8,
    config_revision: String,
    account: String,
    provider: String,
    source: String,
    observation: Value,
}

/// Verified quota facts and the configured runtime that owns their persistence.
pub(crate) struct Verified {
    /// Canonical provider from the broker's deduplicated collection plan.
    pub runtime: String,
    /// Stable source selected from the broker's validated configuration.
    pub source: String,
    /// Validated nonempty snapshot scoped exclusively by broker configuration.
    pub snapshot: NormalizedQuotaSnapshot,
}

/// Counts encoded JSON bytes without allocating another buffer or retaining rejected content.
struct ByteBudget(usize);

impl Write for ByteBudget {
    /// Rejects the first chunk that would exceed the remaining output budget.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or(io::ErrorKind::InvalidData)?;
        Ok(bytes.len())
    }

    /// Counting has no buffered data to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Selects one canonical planned external source without resolving any credential.
fn binding(
    config: &QuotaConfig,
    account: &str,
    provider: &str,
    source: &str,
) -> Result<SourceConfig, String> {
    let mut matches = config
        .plan()
        .map_err(|_| "external_config_invalid")?
        .into_iter()
        .filter(|unit| {
            unit.execution == ExecutionMode::External
                && unit.account.as_str() == account
                && unit.provider == provider
                && unit.source().as_deref() == Some(source)
        });
    let unit = matches.next().ok_or("external_source_unavailable")?;
    if matches.next().is_some() {
        return Err("external_source_ambiguous".into());
    }
    Ok(unit)
}

/// Returns epoch seconds, refusing a host clock before the epoch without exposing OS errors.
fn now() -> Result<f64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .map_err(|_| "external_clock_invalid".into())
}

/// Runs one configured external collector under this process's UID and returns only verified facts.
///
/// Credentials exist only in private stdin; raw diagnostics and credential echoes never escape.
/// The existing collector owns bounded execution and child cleanup. The returned revision hashes
/// the exact config bytes read before collection, fencing changes while the worker is running.
pub(crate) async fn probe(
    config_path: &Path,
    home: &Path,
    account: &str,
    provider: &str,
    source: &str,
) -> Result<Value, String> {
    let (config, revision) = QuotaConfig::load_versioned(config_path)?;
    let unit = binding(&config, account, provider, source)?;
    let context = unit.context(now()?).map_err(|_| "credential_unavailable")?;
    let collector = unit
        .collector
        .as_ref()
        .ok_or("external_source_unavailable")?;
    let observation = collector::run(collector, &context, home)
        .await
        .map_err(|_| "external_probe_failed")?;
    if credentials::output_contains_credential(&observation, &context) {
        return Err("collector_output_contains_credential".into());
    }
    let envelope = json!({
        "version": 1, "config_revision": revision, "account": unit.account,
        "provider": unit.provider, "source": source, "observation": observation,
    });
    // Validate on the credential-owning side too: arbitrary script fields must not cross the pipe.
    verify(&config, &revision, envelope.clone(), now()?)?;
    Ok(envelope)
}

/// Verifies a bounded worker envelope against current broker configuration without reading secrets.
///
/// Revision, account, provider, source and execution owner must all agree. Model membership and
/// physical-account identity come from the broker; worker-supplied scope fields are rejected.
/// This function performs no persistence and does not establish replay/idempotency semantics.
pub(crate) fn verify(
    config: &QuotaConfig,
    revision: &str,
    envelope: Value,
    now: f64,
) -> Result<Verified, String> {
    // Reserve one byte for the newline emitted by the probe CLI.
    serde_json::to_writer(ByteBudget(MAX_ENVELOPE_BYTES - 1), &envelope)
        .map_err(|_| "external_envelope_too_large")?;
    let envelope: Envelope =
        serde_json::from_value(envelope).map_err(|_| "external_envelope_invalid")?;
    if envelope.version != 1 {
        return Err("external_envelope_version".into());
    }
    if envelope.config_revision != revision {
        return Err("external_revision_mismatch".into());
    }
    let unit = binding(
        config,
        &envelope.account,
        &envelope.provider,
        &envelope.source,
    )?;
    let scope = CollectorScope {
        runtime: unit.provider.clone(),
        source: unit.source().ok_or("external_source_unavailable")?,
        models: unit.model_ids(),
    };
    let source = scope.source.clone();
    let snapshot = normalize_collector_output(
        &unit.account,
        &scope,
        &envelope.observation,
        now,
        MAX_OUTPUT_WINDOWS,
        MAX_OUTPUT_MODELS,
    )
    .map_err(|_| "collector_output_invalid")?;
    if snapshot.models.is_empty() {
        return Err("collector_output_empty".into());
    }
    Ok(Verified {
        runtime: unit.provider,
        source,
        snapshot,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Defines a synthetic external source that never resolves host credentials.
    fn fixture() -> &'static str {
        r#"
[[sources]]
account = "account-one"
provider = "openai"
auth_family = "openai"
credential = "native:codex"
native_directory = "/synthetic/codex"
execution = "external"
models = [{ id = "alias", native_model = "native-lane" }]
[sources.harness]
id = "codex"
binary = "/synthetic/bin/codex"
[sources.collector]
command = "/synthetic/bin/quota"
source = "provider-quota"
"#
    }

    /// Returns an in-scope observation whose time is controlled by the test.
    fn observation() -> Value {
        json!({"version":1,"windows":[{"pool":"shared","window":"hour","models":["native-lane"],"remaining_percent":42,"observed_at":100}]})
    }

    /// Wraps a synthetic observation with an exact configured binding and revision.
    fn envelope() -> Value {
        json!({"version":1,"config_revision":"revision-one","account":"account-one","provider":"openai","source":"provider-quota","observation":observation()})
    }

    #[test]
    fn verification_uses_broker_scope_and_unions_only_configured_aliases() {
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        let mut alias = config.sources[0].clone();
        alias.provider = "other-alias".into();
        alias.models[0].native_model = Some("second-lane".into());
        config.sources.push(alias);
        let mut raw = envelope();
        raw["observation"]["windows"][0]["models"] = json!(["native-lane", "second-lane"]);
        let verified = verify(&config, "revision-one", raw, 100.0).unwrap();
        assert_eq!(verified.runtime, "openai");
        assert_eq!(verified.snapshot.account.as_str(), "account-one");
        assert_eq!(verified.snapshot.models.len(), 2);
        for model in verified.snapshot.models {
            assert_eq!(model.pools[0].windows[0].source, "provider-quota");
            assert!(model.pools[0].key.belongs_to(&verified.snapshot.account));
        }
    }

    #[test]
    fn revision_binding_and_execution_are_fenced() {
        let mut config = QuotaConfig::parse(fixture()).unwrap();
        for field in ["config_revision", "account", "provider", "source"] {
            let mut raw = envelope();
            raw[field] = json!("SYNTHETIC_UNTRUSTED_INPUT");
            let error = verify(&config, "revision-one", raw, 100.0).err().unwrap();
            assert!(!error.contains("SYNTHETIC_UNTRUSTED_INPUT"));
        }
        config.sources[0].execution = ExecutionMode::Service;
        assert_eq!(
            verify(&config, "revision-one", envelope(), 100.0)
                .err()
                .unwrap(),
            "external_source_unavailable"
        );
    }

    #[test]
    fn rejects_malformed_untrusted_empty_and_oversized_envelopes() {
        let config = QuotaConfig::parse(fixture()).unwrap();
        let mut invalid = vec![Value::Null];
        for version in [json!(0), json!(2), json!(1.5), json!("1")] {
            let mut raw = envelope();
            raw["version"] = version;
            invalid.push(raw);
        }
        let mut raw = envelope();
        raw.as_object_mut().unwrap().remove("account");
        invalid.push(raw);
        let mut raw = envelope();
        raw["scope"] = json!({"models":["foreign"]});
        invalid.push(raw);
        let mut raw = envelope();
        raw["observation"]["windows"][0]["models"] = json!(["foreign"]);
        invalid.push(raw);
        let mut raw = envelope();
        raw["observation"]["account"] = json!("foreign");
        invalid.push(raw);
        let mut raw = envelope();
        raw["observation"]["windows"][0]["source"] = json!("foreign");
        invalid.push(raw);
        let mut raw = envelope();
        raw["observation"]["windows"][0]["observed_at"] = json!(101);
        invalid.push(raw);
        let mut raw = envelope();
        raw["observation"]["windows"] = json!([]);
        invalid.push(raw);
        let mut raw = envelope();
        raw["observation"]["windows"] = json!(vec![
            observation()["windows"][0].clone();
            MAX_OUTPUT_WINDOWS + 1
        ]);
        invalid.push(raw);
        for raw in invalid {
            assert!(verify(&config, "revision-one", raw, 100.0).is_err());
        }
        let mut raw = envelope();
        raw["observation"] = json!("x".repeat(MAX_ENVELOPE_BYTES));
        assert_eq!(
            verify(&config, "revision-one", raw, 100.0).err().unwrap(),
            "external_envelope_too_large"
        );
        assert!(verify(&config, "revision-one", envelope(), f64::NAN).is_err());
    }

    /// Creates private synthetic inputs and a small shell collector, using no real login or env token.
    fn probe_fixture(directory: &Path, output: &Value, exit: u8) -> std::path::PathBuf {
        let token = directory.join("token");
        std::fs::write(&token, "SYNTHETIC_PROBE_SECRET").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let script = directory.join("probe.sh");
        std::fs::write(&script, format!("#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s' '{}'\nprintf '%s' 'SYNTHETIC_STDERR_SECRET' >&2\nexit {exit}\n", output.to_string().replace('\'', "'\\''"))).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = fixture()
            .replace(
                "credential = \"native:codex\"",
                &format!("credential = \"file:{}\"", token.display()),
            )
            .replace("native_directory = \"/synthetic/codex\"", "")
            .replace("/synthetic/bin/quota", script.to_str().unwrap());
        let path = directory.join("config.toml");
        std::fs::write(&path, config).unwrap();
        path
    }

    #[tokio::test]
    async fn probe_returns_only_a_fenced_observation_and_never_private_context() {
        let directory = tempfile::tempdir().unwrap();
        let path = probe_fixture(directory.path(), &observation(), 0);
        let raw = probe(
            &path,
            directory.path(),
            "account-one",
            "openai",
            "provider-quota",
        )
        .await
        .unwrap();
        let (config, revision) = QuotaConfig::load_versioned(&path).unwrap();
        assert_eq!(raw["config_revision"], revision);
        assert_eq!(raw["observation"], observation());
        assert!(!raw.to_string().contains("SECRET"));
        assert!(verify(&config, &revision, raw, 100.0).is_ok());
        let error = probe(
            &path,
            directory.path(),
            "foreign",
            "openai",
            "provider-quota",
        )
        .await
        .unwrap_err();
        assert_eq!(error, "external_source_unavailable");
    }

    #[tokio::test]
    async fn probe_rejects_credential_echo_invalid_output_and_script_failure() {
        for (output, exit, expected) in [
            (
                json!({"echo":"SYNTHETIC_PROBE_SECRET"}),
                0,
                "collector_output_contains_credential",
            ),
            (
                json!({"raw":"SYNTHETIC_UNTRUSTED_INPUT"}),
                0,
                "collector_output_invalid",
            ),
            (observation(), 7, "external_probe_failed"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = probe_fixture(directory.path(), &output, exit);
            assert_eq!(
                probe(
                    &path,
                    directory.path(),
                    "account-one",
                    "openai",
                    "provider-quota"
                )
                .await
                .unwrap_err(),
                expected
            );
        }
    }
}
