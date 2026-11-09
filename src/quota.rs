//! Host-bound quota identities and version-1 collector normalization.
//!
//! Domain rules are carried over from agent-run; this module performs no I/O.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

/// Invalid quota identity, snapshot, or collector output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaError(String);

impl fmt::Display for QuotaError {
    /// Describes the validation failure.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}
impl std::error::Error for QuotaError {}

/// A validation result for identities, snapshots, and collector output.
pub type Result<T> = std::result::Result<T, QuotaError>;

/// Builds a validation error without binding this module to another crate.
fn invalid(message: impl Into<String>) -> QuotaError {
    QuotaError(message.into())
}

/// Rejects whitespace-only text and embedded NULs.
fn nonblank(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(invalid(format!(
            "{label} must be a nonblank NUL-free string"
        )));
    }
    Ok(())
}

/// A global, opaque account identity reusable across provider aliases.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    /// Returns the opaque account identity as its wire representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    /// Writes the opaque wire representation.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for AccountId {
    type Err = QuotaError;

    /// Parses a lowercase-led account id containing `a-z0-9_.-`, at most 96 bytes.
    fn from_str(value: &str) -> Result<Self> {
        if value.is_empty()
            || value.len() > 96
            || !value.as_bytes()[0].is_ascii_lowercase()
            || !value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
        {
            return Err(invalid(
                "account id must be 1..=96 ASCII characters starting lowercase",
            ));
        }
        Ok(Self(value.into()))
    }
}

impl<'de> Deserialize<'de> for AccountId {
    /// Deserializes and validates the transparent account wire value.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The provider-independent identity of one physical quota pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PhysicalQuotaKey(String);

impl PhysicalQuotaKey {
    /// Builds the shared pool identity for `account` on a nonblank lane up to 128 bytes.
    pub fn new(account: &AccountId, lane: &str) -> Result<Self> {
        nonblank("quota lane", lane)?;
        if lane.len() > 128 {
            return Err(invalid("quota lane exceeds 128 bytes"));
        }
        Ok(Self(format!("{}::{}", account.as_str(), lane)))
    }

    /// Returns the canonical stored key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns whether this key names a lane owned by the given global account.
    pub fn belongs_to(&self, account: &AccountId) -> bool {
        self.0
            .strip_prefix(account.as_str())
            .and_then(|rest| rest.strip_prefix("::"))
            .is_some_and(|lane| !lane.is_empty() && lane.len() <= 128 && !lane.contains('\0'))
    }
}

impl<'de> Deserialize<'de> for PhysicalQuotaKey {
    /// Validates account and lane syntax when a physical key crosses a wire.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let (account, lane) = text
            .split_once("::")
            .ok_or_else(|| serde::de::Error::custom("quota key lacks account and lane"))?;
        let account = AccountId::from_str(account).map_err(serde::de::Error::custom)?;
        Self::new(&account, lane).map_err(serde::de::Error::custom)
    }
}
/// One physical quota window. Unknown remaining or reset data stays absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Collector source identity, bounded to 128 bytes.
    pub source: String,
    /// Provider-native window name, bounded to 128 bytes.
    pub name: String,
    /// Remaining percentage in 0..=100, or unknown.
    pub remaining_percent: Option<f64>,
    /// Provider-reported Unix reset time in seconds, or unknown.
    pub reset_at: Option<f64>,
    /// Unix seconds when the collector observed this window.
    pub observed_at: f64,
    /// Unix seconds after which this observation is stale.
    pub valid_until: f64,
}

impl QuotaWindow {
    /// Rejects invented or invalid numeric facts and inverted observation time.
    pub fn validate(&self) -> Result<()> {
        nonblank("quota source", &self.source)?;
        nonblank("quota window", &self.name)?;
        if self.source.len() > 128
            || self.name.len() > 128
            || self
                .remaining_percent
                .is_some_and(|n| !n.is_finite() || !(0.0..=100.0).contains(&n))
            || self.reset_at.is_some_and(|n| !n.is_finite())
            || !self.observed_at.is_finite()
            || !self.valid_until.is_finite()
            || self.valid_until < self.observed_at
        {
            return Err(invalid("invalid quota window"));
        }
        Ok(())
    }
}

/// One physical pool and its directly attributable provider windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaPoolObservation {
    /// Global account plus provider-independent quota lane.
    pub key: PhysicalQuotaKey,
    /// Provider-reported windows; empty means this pool's capacity is unknown.
    pub windows: Vec<QuotaWindow>,
}

/// Explicit model membership and every physical pool it consumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaModelObservation {
    /// Catalog model id, never inferred by the collector.
    pub model: String,
    /// Nonempty physical pools consumed together by this model.
    pub pools: Vec<QuotaPoolObservation>,
}

/// One account-scoped collector handoff. No scoring or reservation lives here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NormalizedQuotaSnapshot {
    /// Validated host account that owns every physical key.
    pub account: AccountId,
    /// Explicit model memberships, bounded to 256 entries.
    pub models: Vec<QuotaModelObservation>,
}

impl NormalizedQuotaSnapshot {
    /// Checks account ownership, unique model/key/window identities and
    /// bounded facts before a quota producer scores the snapshot.
    ///
    /// Membership is per window, not per pool: models sharing a physical pool
    /// may each carry only the `(source, window)` facts that govern them, so
    /// their window sets can differ. One physical window
    /// `(key, source, window)` repeated under several models must carry
    /// identical facts.
    pub fn validate(&self) -> Result<()> {
        if self.models.len() > 256 {
            return Err(invalid("too many quota models"));
        }
        let mut models = BTreeSet::new();
        let mut physical_windows = BTreeMap::new();
        for model in &self.models {
            nonblank("quota model", &model.model)?;
            if model.model.len() > 256
                || !models.insert(&model.model)
                || model.pools.is_empty()
                || model.pools.len() > 32
            {
                return Err(invalid("invalid quota model membership"));
            }
            let mut keys = BTreeSet::new();
            for pool in &model.pools {
                if !pool.key.belongs_to(&self.account) || !keys.insert(&pool.key) {
                    return Err(invalid("quota key belongs to another account or repeats"));
                }
                if pool.windows.len() > 32 {
                    return Err(invalid("too many quota windows"));
                }
                let mut windows = BTreeSet::new();
                for window in &pool.windows {
                    window.validate()?;
                    if !windows.insert((&window.source, &window.name)) {
                        return Err(invalid("quota window repeats"));
                    }
                }
                // Membership is per window: models sharing a pool may each
                // observe only the windows that govern them, but one physical
                // window repeated under several models must agree exactly.
                for window in &pool.windows {
                    if physical_windows
                        .insert(
                            (pool.key.clone(), window.source.clone(), window.name.clone()),
                            window.clone(),
                        )
                        .is_some_and(|previous| previous != *window)
                    {
                        return Err(invalid("shared quota pool observations disagree"));
                    }
                }
            }
        }
        Ok(())
    }
}
/// Maximum windows accepted in one version-1 collector output.
pub const MAX_OUTPUT_WINDOWS: usize = 256;
/// Maximum model names accepted in one version-1 collector output.
pub const MAX_OUTPUT_MODELS: usize = 256;
/// Shelf-life seconds assumed when a window declares no `valid_until`.
///
/// Mirrors the capacity collector TTL: it bounds staleness only and never
/// fabricates a reset or remaining value.
pub const DEFAULT_WINDOW_TTL_SECONDS: f64 = 900.0;

/// The explicit Rust-side scope one collector invocation runs in.
///
/// `runtime` names the observing engine scope used for persistence, `source`
/// labels windows with the collector identity (never chosen by a script), and
/// `models` is the exhaustive set of configured models a script may report.
#[derive(Debug, Clone)]
pub struct CollectorScope {
    /// Engine scope that owns the persisted rows for this round.
    pub runtime: String,
    /// Stable collector source identity stamped on every emitted window.
    pub source: String,
    /// Configured model ids; output naming anything else is rejected.
    pub models: BTreeSet<String>,
}

/// Reads a finite JSON number, rejecting present-but-nonnumeric facts.
fn finite(value: Option<&Value>, code: &'static str) -> Result<Option<f64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| invalid(code)),
    }
}

/// Normalizes one version-1 collector output into a validated snapshot.
///
/// `raw` must be an object `{ "windows": [ ... ] }` whose entries carry
/// `pool` (physical lane), `window`, a nonempty explicit `models` list, an
/// optional `remaining_percent` (absent or null means unknown), optional
/// `reset_at`, required `observed_at`, and optional `valid_until` defaulting
/// to `observed_at + DEFAULT_WINDOW_TTL_SECONDS`. The envelope must declare
/// `version` exactly `1` and carry no other top-level key; entry keys are
/// closed the same way. `observed_at` may not lie in the future of `host_now`.
/// Foreign or unconfigured models, repeated model names, duplicate or
/// conflicting pool windows, nonfinite or out-of-range numbers, inverted
/// times, and outputs above [`MAX_OUTPUT_WINDOWS`]/[`MAX_OUTPUT_MODELS`] are
/// rejected before any fact is bound to `account`; the account identity
/// itself comes only from Rust. Returns a snapshot that already passed
/// [`NormalizedQuotaSnapshot::validate`].
pub fn normalize_collector_output(
    account: &AccountId,
    scope: &CollectorScope,
    raw: &Value,
    host_now: f64,
    max_windows: usize,
    max_models: usize,
) -> Result<NormalizedQuotaSnapshot> {
    if max_windows == 0 || max_models == 0 || max_windows > MAX_OUTPUT_WINDOWS {
        return Err(invalid("invalid collector output bound"));
    }
    if !host_now.is_finite() || host_now < 0.0 {
        return Err(invalid("quota_output_invalid_time"));
    }
    let envelope = raw
        .as_object()
        .ok_or_else(|| invalid("quota_output_malformed"))?;
    if !envelope.contains_key("version")
        || envelope.get("version").and_then(Value::as_i64) != Some(1)
        || envelope
            .keys()
            .any(|key| key != "version" && key != "windows")
    {
        return Err(invalid("quota_output_version"));
    }
    let entries = raw
        .get("windows")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("quota_output_malformed"))?;
    if entries.len() > max_windows {
        return Err(invalid("quota_output_overflow"));
    }
    // One fact per (lane, source, window); repeats are rejected as duplicates
    // whether or not their facts agree, so one physical window stays singular.
    let mut pools: BTreeMap<String, BTreeMap<(String, String), QuotaWindow>> = BTreeMap::new();
    let mut membership: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // The exact models each (pool, source, window) names.
    let mut window_members: BTreeMap<(String, String, String), BTreeSet<String>> = BTreeMap::new();
    let mut models_seen = BTreeSet::new();
    for entry in entries {
        let object = entry
            .as_object()
            .ok_or_else(|| invalid("quota_output_malformed"))?;
        for key in object.keys() {
            if !matches!(
                key.as_str(),
                "pool"
                    | "window"
                    | "models"
                    | "remaining_percent"
                    | "reset_at"
                    | "observed_at"
                    | "valid_until"
            ) {
                return Err(invalid("quota_output_malformed"));
            }
        }
        let pool = object
            .get("pool")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or_else(|| invalid("quota_output_malformed"))?;
        let window = object
            .get("window")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or_else(|| invalid("quota_output_malformed"))?;
        let listed = object
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("quota_output_malformed"))?;
        if listed.is_empty() {
            return Err(invalid("quota_output_malformed"));
        }
        // Unknown remaining is a valid fact; only present nonfinite or
        // out-of-range values fail.
        let remaining = match object.get("remaining_percent") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let n = v
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or_else(|| invalid("quota_output_invalid_number"))?;
                if !(0.0..=100.0).contains(&n) {
                    return Err(invalid("quota_output_invalid_number"));
                }
                Some(n)
            }
        };
        let reset_at = finite(object.get("reset_at"), "quota_output_invalid_number")?;
        let observed_at = object
            .get("observed_at")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0 && *n <= host_now)
            .ok_or_else(|| invalid("quota_output_invalid_time"))?;
        let valid_until = finite(object.get("valid_until"), "quota_output_invalid_time")?
            .unwrap_or(observed_at + DEFAULT_WINDOW_TTL_SECONDS);
        if valid_until < observed_at {
            return Err(invalid("quota_output_invalid_time"));
        }
        let mut window_models = BTreeSet::new();
        for model in listed {
            let model = model
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or_else(|| invalid("quota_output_malformed"))?;
            if !scope.models.contains(model) {
                return Err(invalid("quota_output_foreign_model"));
            }
            // A repeated model name inside one window is a malformed claim,
            // not an implicit second membership.
            if !window_models.insert(model.to_owned()) {
                return Err(invalid("quota_output_malformed"));
            }
        }
        models_seen.extend(window_models.iter().cloned());
        if models_seen.len() > max_models {
            return Err(invalid("quota_output_overflow"));
        }
        let fact = QuotaWindow {
            source: scope.source.clone(),
            name: window.to_owned(),
            remaining_percent: remaining,
            reset_at,
            observed_at,
            valid_until,
        };
        let windows = pools.entry(pool.to_owned()).or_default();
        if windows
            .insert((scope.source.clone(), window.to_owned()), fact)
            .is_some()
        {
            return Err(invalid("quota_output_duplicate_window"));
        }
        for model in &window_models {
            membership
                .entry(model.clone())
                .or_default()
                .insert(pool.to_owned());
        }
        window_members.insert(
            (pool.to_owned(), scope.source.clone(), window.to_owned()),
            window_models.clone(),
        );
    }
    let mut models = Vec::new();
    for (model, lanes) in &membership {
        let mut pools_observed = Vec::new();
        for lane in lanes {
            let key = PhysicalQuotaKey::new(account, lane)?;
            // Only the windows that name this model govern it.
            let mut windows: Vec<QuotaWindow> = pools[lane]
                .iter()
                .filter(|((source, name), _)| {
                    window_members
                        .get(&(lane.clone(), source.clone(), name.clone()))
                        .is_some_and(|members| members.contains(model))
                })
                .map(|(_, window)| window.clone())
                .collect();
            windows.sort_by(|a, b| (&a.source, &a.name).cmp(&(&b.source, &b.name)));
            pools_observed.push(QuotaPoolObservation { key, windows });
        }
        pools_observed.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
        models.push(QuotaModelObservation {
            model: model.clone(),
            pools: pools_observed,
        });
    }
    models.sort_by(|a, b| a.model.cmp(&b.model));
    let snapshot = NormalizedQuotaSnapshot {
        account: account.clone(),
        models,
    };
    snapshot.validate()?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The collector scope used by the synthetic normalization checks.
    fn scope() -> CollectorScope {
        CollectorScope {
            runtime: "runtime".into(),
            source: "collector".into(),
            models: BTreeSet::from(["a".into(), "b".into()]),
        }
    }

    #[test]
    fn normalizes_shared_windows_and_rejects_hostile_output()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let account: AccountId = "acct".parse()?;
        let normalize =
            |raw: &Value| normalize_collector_output(&account, &scope(), raw, 100.0, 256, 256);
        let good = json!({"version":1,"windows":[
            {"pool":"shared","window":"hour","models":["b","a"],"remaining_percent":0.0,"observed_at":99.0},
            {"pool":"shared","window":"week","models":["a"],"observed_at":100.0}
        ]});
        let result = normalize(&good)?;
        assert_eq!(result.models.len(), 2);
        assert_eq!(result.models[0].pools[0].key.as_str(), "acct::shared");
        assert_eq!(result.models[0].pools[0].windows.len(), 2);
        assert_eq!(result.models[1].pools[0].windows.len(), 1);
        assert_eq!(result.models[0].pools[0].windows[1].remaining_percent, None);
        assert_eq!(result.models[0].pools[0].windows[1].valid_until, 1000.0);
        let invalid = [
            json!({"windows":[]}),
            json!({"version":2,"windows":[]}),
            json!({"version":1,"windows":[],"foreign":true}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["a"],"observed_at":101.0}]}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["unknown"],"observed_at":10.0}]}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["a","a"],"observed_at":10.0}]}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["a"],"remaining_percent":101.0,"observed_at":10.0}]}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["a"],"remaining_percent":"10","observed_at":10.0}]}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["a"],"observed_at":10.0,"valid_until":9.0}]}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["a"],"observed_at":10.0,"extra":true}]}),
            json!({"version":1,"windows":[{"pool":"p","window":"w","models":["a"],"observed_at":10.0},{"pool":"p","window":"w","models":["b"],"observed_at":10.0}]}),
            json!({"version":1,"windows":[{"pool":"\u{0000}","window":"w","models":["a"],"observed_at":10.0}]}),
        ];
        for item in &invalid {
            assert!(normalize(item).is_err(), "accepted {item}");
        }
        assert!(normalize_collector_output(&account, &scope(), &good, 100.0, 1, 256).is_err());
        Ok(())
    }

    #[test]
    fn deserialization_and_snapshot_validation_enforce_account_identity()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        for invalid in ["", "Upper", "a::lane", "a\0", "a/b"] {
            assert!(serde_json::from_value::<AccountId>(json!(invalid)).is_err());
        }
        for invalid in ["foreign", "Acct::lane", "acct::", "acct::  ", "acct::x\0y"] {
            assert!(serde_json::from_value::<PhysicalQuotaKey>(json!(invalid)).is_err());
        }
        let foreign: NormalizedQuotaSnapshot =
            serde_json::from_value(json!({"account":"acct","models":[
                {"model":"a","pools":[{"key":"other::lane","windows":[]}]}
            ]}))?;
        assert!(foreign.validate().is_err());
        let conflicting: NormalizedQuotaSnapshot = serde_json::from_value(
            json!({"account":"acct","models":[
                {"model":"a","pools":[{"key":"acct::lane","windows":[{"source":"s","name":"w","remaining_percent":0.0,"reset_at":null,"observed_at":1.0,"valid_until":2.0}]}]},
                {"model":"b","pools":[{"key":"acct::lane","windows":[{"source":"s","name":"w","remaining_percent":20.0,"reset_at":null,"observed_at":1.0,"valid_until":2.0}]}]}
            ]}),
        )?;
        assert!(conflicting.validate().is_err());
        Ok(())
    }
}
