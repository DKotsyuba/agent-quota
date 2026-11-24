//! Owner-requested redemption of one banked reset credit.
//!
//! The service only records requests and leaves a trigger file for a root-managed
//! launcher. The provider call runs under the credential owner's UID through an
//! operator-configured script that receives the private context on stdin. No
//! outcome is ever retried here: anything ambiguous is reported as `unknown`.

use crate::{
    collector::{self, CollectorBinding},
    config::{QuotaConfig, SourceConfig},
    credentials, delegated, refresh,
    store::RedeemClaim,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Upper bound of one encoded claim or outcome line.
pub(crate) const MAX_MESSAGE_BYTES: usize = 4096;

/// Operator-owned redemption command for one external source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RedeemBinding {
    /// Absolute executable path; arguments are passed literally without expansion.
    pub command: PathBuf,
    /// Literal arguments; never include credentials here.
    #[serde(default)]
    pub args: Vec<String>,
    /// Execution deadline in seconds, from 1 to 300; default 60.
    #[serde(default = "redeem_timeout")]
    pub timeout_seconds: u64,
    /// Additional host environment names explicitly granted to the command.
    #[serde(default)]
    pub env_from: Vec<String>,
    /// Launcher instance that names the trigger file; defaults to the account id.
    #[serde(default)]
    pub instance: Option<String>,
}

/// Returns the default redemption deadline in seconds.
fn redeem_timeout() -> u64 {
    60
}

impl RedeemBinding {
    /// Converts to the bounded process runner's binding.
    pub(crate) fn collector(&self) -> CollectorBinding {
        CollectorBinding {
            command: self.command.clone(),
            args: self.args.clone(),
            source: String::new(),
            timeout_seconds: self.timeout_seconds,
            env_from: self.env_from.clone(),
            exhaustion_windows: Default::default(),
        }
    }

    /// Rejects unsafe commands and trigger names without echoing configured values.
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.collector()
            .validate()
            .map_err(|_| "redeem_config_invalid".to_owned())?;
        if self
            .instance
            .as_deref()
            .is_some_and(|name| !instance_name(name))
        {
            return Err("redeem_config_invalid".into());
        }
        Ok(())
    }

    /// Returns the trigger name: the configured launcher instance or the account id.
    pub(crate) fn instance<'a>(&'a self, account: &'a str) -> &'a str {
        self.instance.as_deref().unwrap_or(account)
    }
}

/// A systemd-instance-safe file stem: 1–96 of `A-Za-z0-9_.-`, alphanumeric first.
fn instance_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Accepts only a canonical lowercase version-4 UUID.
pub(crate) fn valid_request_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            14 => *byte == b'4',
            19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

/// Applies the inventory's credit-id bounds: 1–128 bytes without control characters.
pub(crate) fn valid_credit_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

/// The complete set of verdicts an executor may report: for each final state, the
/// reason codes the supplied scripts, the executor and the launcher can emit.
/// Nothing else is accepted, so no provider text and no token-shaped value can be
/// stored as a reason. `outcome_unreported`, `executor_unavailable`,
/// `binding_changed`, `trigger_unavailable` and the admission refusals are set only
/// by the service itself and are deliberately absent here.
fn valid_verdict(state: &str, reason: Option<&str>) -> bool {
    match (state, reason) {
        ("redeemed", None) => true,
        (_, None) => false,
        ("redeemed", Some(code)) => code == "already_redeemed",
        ("rejected", Some(code)) => matches!(
            code,
            "already_used"
                | "not_limited"
                | "cooldown"
                | "ineligible"
                | "unavailable"
                | "nothing_to_reset"
                | "no_credit"
                | "owner_rate_limited"
        ),
        ("failed", Some(code)) => matches!(
            code,
            "auth_rejected"
                | "profile_unavailable"
                | "organization_unknown"
                | "provider_unreachable"
                | "unsupported_scope"
                | "codex_method_unsupported"
                | "codex_app_server_unavailable"
                | "redeem_request_invalid"
                | "redeem_script_unavailable"
                | "redeem_not_configured"
                | "config_unavailable"
                | "clock_invalid"
                | "owner_guard_unavailable"
                | "owner_binding_changed"
                | "credential_unavailable"
                | "credential_expired"
                | "credential_refresh_failed"
                | "credential_refresh_limited"
                | "credential_refresh_unsupported_host"
                | "credential_refresh_cleanup_unconfirmed"
        ),
        ("unknown", Some(code)) => matches!(
            code,
            "rate_limited"
                | "provider_conflict"
                | "provider_error"
                | "provider_timeout"
                | "network_error"
                | "provider_response_unrecognized"
                | "redeem_timeout"
                | "redeem_script_failed"
                | "redeem_output_invalid"
                | "redeem_output_contains_credential"
                | "executor_failed"
        ),
        _ => false,
    }
}

/// Generates a random version-4 UUID from the kernel's generator.
pub(crate) fn new_request_id() -> Result<String, &'static str> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| "random_unavailable")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// Final executor verdict for one claimed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Outcome {
    pub version: u8,
    pub request_id: String,
    pub account: String,
    /// Token of the claim this verdict belongs to, copied from the claim.
    pub claim_token: String,
    pub state: String,
    pub reason: Option<String>,
}

impl Outcome {
    /// Parses and bounds an outcome crossing the launcher pipe or the socket.
    pub(crate) fn parse(value: Value) -> Result<Self, &'static str> {
        let outcome: Self = serde_json::from_value(value).map_err(|_| "invalid_request")?;
        if outcome.version != 1
            || !valid_verdict(&outcome.state, outcome.reason.as_deref())
            || !valid_request_id(&outcome.request_id)
            || !valid_claim_token(&outcome.claim_token)
        {
            return Err("invalid_request");
        }
        Ok(outcome)
    }
}

/// Strict launcher handoff of one claimed request.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    version: u8,
    request_id: String,
    account: String,
    credit_id: String,
    scope: String,
    provider_kind: String,
    claim_token: String,
    config_revision: String,
}

impl Claim {
    /// Parses a claim and checks the fields every later step relies on.
    fn parse(value: Value) -> Result<Self, &'static str> {
        let claim: Self = serde_json::from_value(value).map_err(|_| "redeem_claim_invalid")?;
        if claim.version != 1
            || !valid_request_id(&claim.request_id)
            || !valid_credit_id(&claim.credit_id)
            || !valid_claim_token(&claim.claim_token)
        {
            return Err("redeem_claim_invalid");
        }
        Ok(claim)
    }

    /// Builds the verdict for this exact claim.
    fn outcome(&self, state: &str, reason: Option<&str>) -> Outcome {
        Outcome {
            version: 1,
            request_id: self.request_id.clone(),
            account: self.account.clone(),
            claim_token: self.claim_token.clone(),
            state: state.to_owned(),
            reason: reason.map(str::to_owned),
        }
    }
}

/// The store's claim token: 32 lowercase hexadecimal characters.
fn valid_claim_token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Encodes a claim for the launcher; contains no credential.
pub(crate) fn claim_value(claim: &RedeemClaim) -> Value {
    json!({"version":1,"request_id":claim.request_id,"account":claim.account,
        "credit_id":claim.credit_id,"scope":claim.scope,"provider_kind":claim.provider_kind,
        "claim_token":claim.claim_token,"config_revision":claim.config_revision})
}

/// Builds the launcher's own verdict for a claim whose executor returned nothing usable.
pub(crate) fn launcher_outcome(
    claim: Value,
    state: &str,
    reason: Option<&str>,
) -> Result<Value, &'static str> {
    let outcome = Claim::parse(claim)
        .map_err(|_| "invalid_request")?
        .outcome(state, reason);
    let value = serde_json::to_value(outcome).map_err(|_| "invalid_request")?;
    Outcome::parse(value.clone())?;
    Ok(value)
}

/// Selects the redemption binding of an account, if exactly one external source has it.
pub(crate) fn binding<'a>(
    sources: &'a [SourceConfig],
    account: &str,
) -> Option<(&'a SourceConfig, &'a RedeemBinding)> {
    sources
        .iter()
        .filter(|unit| unit.account.as_str() == account)
        .find_map(|unit| unit.redeem.as_ref().map(|redeem| (unit, redeem)))
}

/// Script verdict; the reason is a fixed code chosen by the script.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptOutcome {
    version: u8,
    state: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Performs one claimed redemption under this process's UID.
///
/// Returns `Err` only before the claim is understood. Afterwards every path yields
/// an outcome: `failed` when the provider was certainly not asked to redeem,
/// `unknown` whenever the script started and did not return a valid verdict.
pub(crate) async fn execute(
    config_path: &Path,
    home: &Path,
    account: &str,
    provider: &str,
    source: &str,
    claim: Value,
) -> Result<Outcome, &'static str> {
    let claim = Claim::parse(claim)?;
    if claim.account != account {
        return Err("redeem_claim_invalid");
    }
    let finish = |state: &str, reason: &str| claim.outcome(state, Some(reason));
    let Ok((config, revision)) = QuotaConfig::load_versioned(config_path) else {
        return Ok(finish("failed", "config_unavailable"));
    };
    // The request was admitted for one exact configuration. A different file here
    // could name another credential or script, so nothing is resolved or run.
    if revision != claim.config_revision {
        return Ok(finish("failed", "owner_binding_changed"));
    }
    let Ok(unit) = delegated::binding(&config, account, provider, source) else {
        return Ok(finish("failed", "redeem_not_configured"));
    };
    let Some(redeem) = unit.redeem.as_ref() else {
        return Ok(finish("failed", "redeem_not_configured"));
    };
    let Ok(at) = delegated::now() else {
        return Ok(finish("failed", "clock_invalid"));
    };
    if let Err(code) = refresh::renew_if_due(&unit, home, at).await {
        return Ok(finish("failed", credential_code(code)));
    }
    let mut context = match delegated::now()
        .map_err(|_| credentials::CREDENTIAL_UNAVAILABLE)
        .and_then(|at| unit.context(at))
    {
        Ok(context) => context,
        Err(code) => return Ok(finish("failed", credential_code(code))),
    };
    // Last gate before the provider: this user's own spending limit, timed after
    // any credential renewal.
    let Ok(at) = delegated::now() else {
        return Ok(finish("failed", "clock_invalid"));
    };
    match admit(home, account, at) {
        Ok(true) => {}
        Ok(false) => return Ok(finish("rejected", "owner_rate_limited")),
        Err(code) => return Ok(finish("failed", code)),
    }
    context["redeem"] = json!({"request_id":claim.request_id,"credit_id":claim.credit_id,
        "scope":claim.scope,"provider_kind":claim.provider_kind});
    // From here the provider may have been reached: no failure is definite.
    let output = match collector::run(&redeem.collector(), &context, home).await {
        Ok(output) => output,
        Err(code) if code == "collector_spawn_failed" || code == "collector_config_invalid" => {
            return Ok(finish("failed", "redeem_script_unavailable"));
        }
        Err(code) if code == "collector_timeout" => {
            return Ok(finish("unknown", "redeem_timeout"));
        }
        Err(_) => return Ok(finish("unknown", "redeem_script_failed")),
    };
    if credentials::output_contains_credential(&output, &context) {
        return Ok(finish("unknown", "redeem_output_contains_credential"));
    }
    let Ok(script) = serde_json::from_value::<ScriptOutcome>(output) else {
        return Ok(finish("unknown", "redeem_output_invalid"));
    };
    let outcome = Outcome {
        version: script.version,
        ..claim.outcome(&script.state, script.reason.as_deref())
    };
    let checked = serde_json::to_value(&outcome)
        .map_err(|_| "invalid_request")
        .and_then(Outcome::parse);
    Ok(checked.unwrap_or_else(|_| finish("unknown", "redeem_output_invalid")))
}

/// Minimum spacing of redemption executions for one account under one credential owner.
pub(crate) const OWNER_INTERVAL_SECONDS: f64 = 120.0;

/// Admits at most one execution per account per [`OWNER_INTERVAL_SECONDS`].
///
/// The credential owner keeps this record in its own private probe home, which the
/// service user cannot write. It bounds what a misbehaving service identity can spend
/// through the launcher. Read, decision and write happen under one exclusive file
/// lock. The time is recorded before the provider call; an unreadable or unsafe
/// record refuses the execution, as does a record dated in the future.
fn admit(home: &Path, account: &str, at: f64) -> Result<bool, &'static str> {
    use std::io::Read;
    const UNAVAILABLE: &str = "owner_guard_unavailable";
    if !instance_name(account) || !at.is_finite() {
        return Err(UNAVAILABLE);
    }
    let path = home.join(format!("redeem-{account}.last"));
    let staged = home.join(format!(".redeem-{account}.staged"));
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    // One exclusive lock covers read, decision and write, so concurrent executors of
    // this user cannot both pass. It is released when this function returns.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(home.join("redeem-guard.lock"))
        .map_err(|_| UNAVAILABLE)?;
    let locked = lock.metadata().map_err(|_| UNAVAILABLE)?;
    if !locked.is_file() || locked.uid() != uid || locked.mode() & 0o077 != 0 {
        return Err(UNAVAILABLE);
    }
    fs2::FileExt::lock_exclusive(&lock).map_err(|_| UNAVAILABLE)?;
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => {
            let metadata = file.metadata().map_err(|_| UNAVAILABLE)?;
            if !metadata.is_file()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(UNAVAILABLE);
            }
            let mut text = String::new();
            file.take(64)
                .read_to_string(&mut text)
                .map_err(|_| UNAVAILABLE)?;
            let last: f64 = text.trim().parse().map_err(|_| UNAVAILABLE)?;
            if !last.is_finite() {
                return Err(UNAVAILABLE);
            }
            if at - last < OWNER_INTERVAL_SECONDS {
                return Ok(false);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(UNAVAILABLE),
    }
    let _ = std::fs::remove_file(&staged);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staged)
        .and_then(|mut file| {
            file.write_all(format!("{at}\n").as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&staged, &path))
        .map_err(|_| {
            let _ = std::fs::remove_file(&staged);
            UNAVAILABLE
        })?;
    Ok(true)
}

/// Keeps the known credential diagnostics and hides every other cause.
fn credential_code(code: &str) -> &'static str {
    [
        credentials::CREDENTIAL_EXPIRED,
        credentials::CREDENTIAL_REFRESH_FAILED,
        credentials::CREDENTIAL_REFRESH_LIMITED,
        credentials::CREDENTIAL_REFRESH_UNSUPPORTED_HOST,
        credentials::CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED,
    ]
    .into_iter()
    .find(|known| *known == code)
    .unwrap_or(credentials::CREDENTIAL_UNAVAILABLE)
}

/// Returns the service-owned trigger directory under its private home.
pub(crate) fn spool(home: &Path) -> PathBuf {
    home.join("redeem")
}

/// Returns the trigger a launcher path unit watches for one instance.
fn trigger(home: &Path, instance: &str) -> Result<PathBuf, &'static str> {
    if !instance_name(instance) {
        return Err("trigger_unavailable");
    }
    Ok(spool(home).join(format!("{instance}.pending")))
}

/// Publishes the trigger atomically; its content is the nonsecret request id.
pub(crate) fn write_trigger(
    home: &Path,
    instance: &str,
    request_id: &str,
) -> Result<(), &'static str> {
    let destination = trigger(home, instance)?;
    let directory = spool(home);
    match DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("trigger_unavailable"),
    }
    let metadata = std::fs::symlink_metadata(&directory).map_err(|_| "trigger_unavailable")?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err("trigger_unavailable");
    }
    let staged = directory.join(format!(".{instance}.staged"));
    let _ = std::fs::remove_file(&staged);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staged)
        .and_then(|mut file| {
            file.write_all(request_id.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&staged, &destination));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
        return Err("trigger_unavailable");
    }
    Ok(())
}

/// Acknowledges a trigger so the path unit stops starting its service.
pub(crate) fn remove_trigger(home: &Path, instance: &str) {
    if let Ok(path) = trigger(home, instance) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Synthetic assertions")]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn request_ids_are_canonical_version_four_uuids() {
        for _ in 0..8 {
            assert!(valid_request_id(&new_request_id().unwrap()));
        }
        for bad in [
            "",
            "11111111-1111-1111-8111-111111111111",
            "11111111-1111-4111-7111-111111111111",
            "11111111-1111-4111-8111-11111111111",
            "11111111-1111-4111-8111-11111111111G",
            "11111111-1111-4111-8111-11111111111A",
            "11111111_1111-4111-8111-111111111111",
        ] {
            assert!(!valid_request_id(bad), "{bad}");
        }
        assert!(valid_credit_id("grant:abc 1"));
        assert!(!valid_credit_id(" "));
        assert!(!valid_credit_id("a\nb"));
        assert!(!valid_credit_id(&"x".repeat(129)));
    }

    #[test]
    fn outcomes_accept_only_final_states_and_fixed_reason_codes() {
        let base = json!({"version":1,"request_id":"11111111-1111-4111-8111-111111111111",
            "account":"fixture","claim_token":"0123456789abcdef0123456789abcdef",
            "state":"redeemed","reason":null});
        assert!(Outcome::parse(base.clone()).is_ok());
        for (field, value) in [
            ("version", json!(2)),
            ("state", json!("pending")),
            ("state", json!("done")),
            ("reason", json!("Bearer SYNTHETIC")),
            ("reason", json!("x".repeat(65))),
            // Syntactically harmless codes that are simply not on the list.
            ("reason", json!("provider_code_1234")),
            ("reason", json!("http_404")),
            ("reason", json!("0123456789abcdef0123456789abcdef")),
            // Codes only the service itself may set.
            ("reason", json!("outcome_unreported")),
            ("reason", json!("executor_unavailable")),
            ("reason", json!("binding_changed")),
            // A code of another state.
            ("reason", json!("already_used")),
            ("request_id", json!("nope")),
            ("claim_token", json!("short")),
            ("claim_token", json!("0123456789ABCDEF0123456789ABCDEF")),
        ] {
            let mut bad = base.clone();
            bad[field] = value;
            assert!(Outcome::parse(bad).is_err(), "{field}");
        }
        for (state, reason, accepted) in [
            ("redeemed", json!("already_redeemed"), true),
            ("rejected", json!("already_used"), true),
            ("rejected", json!("owner_rate_limited"), true),
            ("failed", json!("auth_rejected"), true),
            ("failed", json!("owner_binding_changed"), true),
            ("unknown", json!("provider_timeout"), true),
            ("unknown", json!("executor_failed"), true),
            ("rejected", Value::Null, false),
            ("failed", Value::Null, false),
            ("unknown", Value::Null, false),
            ("unknown", json!("auth_rejected"), false),
            ("failed", json!("provider_timeout"), false),
            ("redeemed", json!("provider_timeout"), false),
        ] {
            let mut verdict = base.clone();
            verdict["state"] = json!(state);
            verdict["reason"] = reason.clone();
            assert_eq!(
                Outcome::parse(verdict).is_ok(),
                accepted,
                "{state} {reason}"
            );
        }
        let mut extra = base.clone();
        extra["token"] = json!("x");
        assert!(Outcome::parse(extra).is_err());
        for field in ["request_id", "claim_token", "account"] {
            let mut missing = base.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(Outcome::parse(missing).is_err(), "{field}");
        }
        // The launcher's fallback verdict is bound to the claim it was given.
        let claim = json!({"version":1,"request_id":"11111111-1111-4111-8111-111111111111",
            "account":"fixture","credit_id":"c","scope":"five_hour","provider_kind":"GRANT",
            "claim_token":"0123456789abcdef0123456789abcdef","config_revision":"rev"});
        let fallback = launcher_outcome(claim.clone(), "unknown", Some("executor_failed")).unwrap();
        assert_eq!(fallback["claim_token"], claim["claim_token"]);
        assert_eq!(fallback["request_id"], claim["request_id"]);
        assert_eq!(fallback["state"], "unknown");
        assert!(launcher_outcome(claim.clone(), "pending", None).is_err());
        let mut tokenless = claim;
        tokenless.as_object_mut().unwrap().remove("claim_token");
        assert!(launcher_outcome(tokenless, "unknown", Some("executor_failed")).is_err());
    }

    #[test]
    fn owner_guard_spaces_executions_per_account_and_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path();
        assert_eq!(admit(home, "fixture", 1000.0), Ok(true));
        let record = home.join("redeem-fixture.last");
        assert_eq!(std::fs::metadata(&record).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            admit(home, "fixture", 1000.0 + OWNER_INTERVAL_SECONDS - 1.0),
            Ok(false)
        );
        // A refusal does not extend the interval, and accounts are independent.
        assert_eq!(admit(home, "other", 1001.0), Ok(true));
        assert_eq!(
            admit(home, "fixture", 1000.0 + OWNER_INTERVAL_SECONDS),
            Ok(true)
        );
        // A clock set back never opens the gate.
        assert_eq!(admit(home, "fixture", 500.0), Ok(false));
        for bad in ["not a time", "inf", ""] {
            std::fs::write(&record, bad).unwrap();
            assert_eq!(
                admit(home, "fixture", 9000.0),
                Err("owner_guard_unavailable")
            );
        }
        std::fs::write(&record, "1000\n").unwrap();
        std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            admit(home, "fixture", 9000.0),
            Err("owner_guard_unavailable")
        );
        std::fs::remove_file(&record).unwrap();
        std::os::unix::fs::symlink(home.join("elsewhere"), &record).unwrap();
        assert_eq!(
            admit(home, "fixture", 9000.0),
            Err("owner_guard_unavailable")
        );
        assert!(!home.join("elsewhere").exists());
        assert_eq!(
            admit(home, "../escape", 9000.0),
            Err("owner_guard_unavailable")
        );
    }

    #[test]
    fn owner_guard_admits_one_of_many_concurrent_executors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let home = root.path().to_owned();
                let start = std::sync::Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    admit(&home, "fixture", 1000.0)
                })
            })
            .collect();
        let verdicts: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(verdicts.iter().filter(|v| **v == Ok(true)).count(), 1);
        assert_eq!(verdicts.iter().filter(|v| **v == Ok(false)).count(), 15);
        let lock = root.path().join("redeem-guard.lock");
        assert_eq!(std::fs::metadata(lock).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn trigger_is_private_atomic_and_acknowledged() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        write_trigger(root.path(), "claude-main", id).unwrap();
        let path = root.path().join("redeem/claude-main.pending");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{id}\n"));
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(root.path().join("redeem"))
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
        // A repeated request replaces the trigger and leaves no staging file.
        write_trigger(root.path(), "claude-main", id).unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join("redeem"))
                .unwrap()
                .count(),
            1
        );
        for bad in ["", "../escape", ".hidden", "a/b", "a b"] {
            assert!(write_trigger(root.path(), bad, id).is_err(), "{bad}");
        }
        remove_trigger(root.path(), "claude-main");
        remove_trigger(root.path(), "claude-main");
        assert!(!path.exists());
    }
}
