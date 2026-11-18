//! Account-free provider/model capacity ranking over one evaluated quota snapshot.
//!
//! This ports agent-run's provider capacity order (`provider_ranking.rs` and
//! `quota_projection.rs`) into Quota, which now owns provider and account
//! multipliers. Run keeps only its local admission read: account enablement,
//! cooldowns and active-attempt accounting are Run state and are not applied
//! here. The result never contains account identities, pools or credentials.

use crate::config::{ProviderRanking, QuotaConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Capacity result schema; independent of the snapshot schema it is derived from.
pub(crate) const SCHEMA_VERSION: u8 = 1;
/// Bounds per-model window details in the account-free result.
const MAX_MODEL_WINDOWS: usize = 8;
/// Absolute ceiling for the hook text, matching agent-run's context limit.
pub(crate) const HOOK_HARD_LIMIT_CHARS: usize = 2500;
/// Default hook budget: agent-run's priority slot (2500 - 600 active - 1 separator).
pub(crate) const HOOK_DEFAULT_CHARS: usize = 1899;
/// Header of the compact hook block; wording follows agent-run's former block.
const HOOK_HEADER: &str = "Provider capacity order (highest first; read-only). Pick provider, model, effort and profile yourself; agent-quota get_capacity has windows and resets. unknown = quota cannot be determined from available data, not a health claim; limited = known capacity at high quota risk.";
/// Omission instruction appended when complete provider lines do not fit.
const HOOK_OMITTED: &str = "More routes omitted; use agent-quota get_capacity if needed";

/// Minimal decoded snapshot; unrelated fields are ignored.
#[derive(Deserialize)]
struct Snapshot {
    schema_version: u8,
    source_epoch: String,
    revision: Option<i64>,
    config_revision: String,
    evaluated_at: f64,
    coverage: Coverage,
    accounts: Vec<Account>,
}
/// Quota collection coverage of the snapshot.
#[derive(Deserialize)]
struct Coverage {
    state: String,
}
/// One configured account's models and physical windows.
#[derive(Deserialize)]
struct Account {
    account: String,
    models: Vec<Model>,
    windows: Vec<Window>,
}
/// One account's quota-only model standing as computed by the snapshot.
#[derive(Deserialize)]
struct Model {
    provider: String,
    model: String,
    native_model: String,
    state: String,
    state_validity: Validity,
    quota_score: Option<f64>,
    limiting_window: Option<Value>,
    remaining_percent: Option<f64>,
    reset_at: Option<f64>,
    risk: String,
    newest_observed_at: Option<f64>,
    exhausted_until: Option<f64>,
}
/// State-class validity bound of one model.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Validity {
    Until { at: f64 },
    LatchedNoReset,
    Unknown,
}
/// One physical window with its exact model membership.
#[derive(Deserialize)]
struct Window {
    lane: String,
    source: String,
    window: String,
    models: Option<BTreeSet<String>>,
    observed_at: Option<f64>,
    valid_until: Option<f64>,
    last_reset_at: Option<f64>,
    forecast: WindowForecast,
}
/// The forecast fields needed for an account-free window row.
#[derive(Deserialize)]
struct WindowForecast {
    known: bool,
    remaining_percent: Option<f64>,
}

/// One account's standing on one model lane, as in agent-run's `LaneVerdict`.
enum Verdict<'a> {
    /// Usable capacity with `quota_score * account multiplier`.
    Known { priority: f64, model: &'a Model },
    /// No current evidence, or a non-finite weighted priority; never zero.
    Unknown,
    /// Fresh zero or active latch.
    Exhausted { model: &'a Model },
}

/// Public account-free capacity result.
#[derive(Serialize)]
pub(crate) struct CapacityView {
    schema_version: u8,
    source_epoch: String,
    revision: Option<i64>,
    config_revision: String,
    evaluated_at: f64,
    coverage: CoverageView,
    providers: Vec<ProviderView>,
}
/// Collection coverage plus whether any ranked model lacks usable evidence.
#[derive(Serialize)]
struct CoverageView {
    /// Quota collection coverage: `complete`, `partial` or `unknown`.
    state: String,
    /// True when coverage is not complete or any eligible account is unknown.
    incomplete: bool,
}
/// One provider's account-free standing.
#[derive(Serialize)]
struct ProviderView {
    provider: String,
    priority_multiplier: f64,
    /// Best available model priority times the provider multiplier; null without known capacity.
    score: Option<f64>,
    /// Best model status of this provider.
    status: &'static str,
    models: Vec<ModelView>,
}
/// One offered model's account-free standing.
#[derive(Serialize)]
struct ModelView {
    model: String,
    native_model: String,
    /// `available`, `limited`, `unknown` or `exhausted`.
    status: &'static str,
    /// Best weighted account priority; null without known capacity.
    best_priority: Option<f64>,
    /// `fresh`, `stale` or `missing`, as in agent-run.
    evidence: &'static str,
    newest_observed_at: Option<f64>,
    /// Earliest known reset among exhausted accounts when exhausted.
    exhausted_until: Option<f64>,
    /// Limiting window name of the representative account.
    limiting_window: Option<String>,
    remaining_percent: Option<f64>,
    reset_at: Option<f64>,
    risk: String,
    /// Exclusive state-class boundary of the representative account, when bounded.
    valid_until: Option<f64>,
    windows: Vec<WindowView>,
}
/// One governing window of the representative account, without pool or account identity.
#[derive(Serialize)]
struct WindowView {
    window: String,
    source: String,
    known: bool,
    remaining_percent: Option<f64>,
    reset_at: Option<f64>,
    observed_at: Option<f64>,
    valid_until: Option<f64>,
}

/// Status order used for provider tie-breaking: lower is better.
fn status_rank(status: &str) -> u8 {
    match status {
        "available" => 0,
        "limited" => 1,
        "unknown" => 2,
        _ => 3,
    }
}

/// Mirrors the snapshot's governing rule, including the legacy pool-equals-model fallback.
fn governs(membership: &Option<BTreeSet<String>>, lane: &str, model: &str) -> bool {
    match membership {
        Some(models) => models.contains(model),
        None => lane == model,
    }
}

/// Classifies one account's model view exactly as agent-run's quota adapter did.
fn verdict(
    model: &Model,
    factor: f64,
    at: f64,
    evaluated_at: f64,
) -> Result<Verdict<'_>, &'static str> {
    let clock_valid = evaluated_at.is_finite() && evaluated_at >= 0.0 && at >= evaluated_at;
    let state_valid = clock_valid
        && match (model.state.as_str(), &model.state_validity) {
            ("known" | "exhausted", Validity::Until { at: until }) => {
                until.is_finite() && *until >= 0.0 && at < *until
            }
            ("exhausted", Validity::LatchedNoReset) => true,
            _ => false,
        };
    if model.state == "exhausted" && state_valid {
        return Ok(Verdict::Exhausted { model });
    }
    if !state_valid || model.state != "known" {
        return Ok(Verdict::Unknown);
    }
    let raw = model
        .quota_score
        .filter(|score| score.is_finite() && *score >= 0.0)
        .ok_or("invalid_service_response")?;
    let priority = raw * factor;
    Ok(if priority.is_finite() {
        Verdict::Known { priority, model }
    } else {
        Verdict::Unknown
    })
}

/// Account-free window details of the representative account's model lane.
fn windows(account: &Account, native: &str, at: f64, evaluated_at: f64) -> Vec<WindowView> {
    let mut rows: Vec<WindowView> = account
        .windows
        .iter()
        .filter(|window| governs(&window.models, &window.lane, native))
        .map(|window| {
            let known = at >= evaluated_at
                && window.forecast.known
                && window.forecast.remaining_percent.is_some()
                && window.observed_at.is_some_and(|observed| observed <= at)
                && window.valid_until.is_some_and(|until| at <= until)
                && window.last_reset_at.is_none_or(|reset| at < reset);
            WindowView {
                window: window.window.clone(),
                source: window.source.clone(),
                known,
                remaining_percent: if known {
                    window.forecast.remaining_percent
                } else {
                    None
                },
                reset_at: window.last_reset_at,
                observed_at: window.observed_at,
                valid_until: window.valid_until,
            }
        })
        .collect();
    rows.sort_by(|a, b| (&a.window, &a.source).cmp(&(&b.window, &b.source)));
    rows.truncate(MAX_MODEL_WINDOWS);
    rows
}

/// Minimum of optional finite values.
fn min_option(values: impl Iterator<Item = f64>) -> Option<f64> {
    values.fold(None, |min: Option<f64>, value| {
        Some(min.map_or(value, |current| current.min(value)))
    })
}

/// Ranks configured providers and their offered models from one snapshot at `at`.
///
/// `snapshot` must be a schema-2 snapshot of every configured account, optionally
/// narrowed to one model, evaluated with `config`. A provider offers the models
/// of its sources in declaration order; a source's account is eligible for its
/// own models with the source's `priority_multiplier`. Providers with no model
/// in scope are omitted. Errors are fixed safe codes.
pub(crate) fn rank(
    config: &QuotaConfig,
    snapshot: Value,
    at: f64,
) -> Result<CapacityView, &'static str> {
    let snapshot: Snapshot =
        serde_json::from_value(snapshot).map_err(|_| "invalid_service_response")?;
    if snapshot.schema_version != 2
        || !snapshot.evaluated_at.is_finite()
        || !at.is_finite()
        || at < 0.0
        || !matches!(
            snapshot.coverage.state.as_str(),
            "unknown" | "partial" | "complete"
        )
    {
        return Err("invalid_service_response");
    }
    let accounts: BTreeMap<&str, &Account> = snapshot
        .accounts
        .iter()
        .map(|account| (account.account.as_str(), account))
        .collect();
    let mut provider_order: Vec<&str> = Vec::new();
    for source in &config.sources {
        if !provider_order.contains(&source.provider.as_str()) {
            provider_order.push(&source.provider);
        }
    }
    let neutral = ProviderRanking::default();
    let mut incomplete = snapshot.coverage.state != "complete";
    let mut providers = Vec::new();
    for provider in provider_order {
        let ranking = config.providers.get(provider).unwrap_or(&neutral);
        let sources: Vec<_> = config
            .sources
            .iter()
            .filter(|source| source.provider == provider)
            .collect();
        let mut offering: Vec<&str> = Vec::new();
        for source in &sources {
            for model in &source.models {
                if !offering.contains(&model.id.as_str()) {
                    offering.push(&model.id);
                }
            }
        }
        let mut models = Vec::new();
        let mut best: Option<f64> = None;
        for model_id in offering {
            // Effective account weight: the maximum of its eligible bindings, never a sum.
            // Ordered providers use weights as a fallback sequence, so ranking is neutral.
            let mut eligible: BTreeMap<(&str, &str), f64> = BTreeMap::new();
            for source in &sources {
                if let Some(model) = source.models.iter().find(|model| model.id == model_id) {
                    let factor = if ranking.ordered_accounts {
                        1.0
                    } else {
                        source.priority_multiplier
                    };
                    eligible
                        .entry((source.account.as_str(), model.lane()))
                        .and_modify(|value| *value = value.max(factor))
                        .or_insert(factor);
                }
            }
            let mut known: Vec<(f64, &Model, &Account)> = Vec::new();
            let mut exhausted: Vec<(&Model, &Account)> = Vec::new();
            let mut unknown = false;
            let mut newest: Option<f64> = None;
            let mut native_model: Option<&str> = None;
            for ((account_id, native), factor) in &eligible {
                let Some(account) = accounts.get(account_id) else {
                    continue;
                };
                let Some(view) = account.models.iter().find(|view| {
                    view.provider == provider
                        && view.model == model_id
                        && view.native_model == *native
                }) else {
                    continue;
                };
                native_model.get_or_insert(native);
                if at >= snapshot.evaluated_at
                    && let Some(observed) = view
                        .newest_observed_at
                        .filter(|observed| observed.is_finite() && *observed <= at)
                {
                    newest = Some(newest.map_or(observed, |current: f64| current.max(observed)));
                }
                match verdict(view, *factor, at, snapshot.evaluated_at)? {
                    Verdict::Known { priority, model } => known.push((priority, model, account)),
                    Verdict::Exhausted { model } => exhausted.push((model, account)),
                    Verdict::Unknown => unknown = true,
                }
            }
            let Some(native_model) = native_model else {
                continue;
            };
            known.sort_by(|a, b| {
                b.0.total_cmp(&a.0)
                    .then_with(|| a.2.account.cmp(&b.2.account))
            });
            exhausted.sort_by(|a, b| {
                let left = a.0.exhausted_until;
                let right = b.0.exhausted_until;
                left.is_none()
                    .cmp(&right.is_none())
                    .then_with(|| {
                        left.unwrap_or(f64::INFINITY)
                            .total_cmp(&right.unwrap_or(f64::INFINITY))
                    })
                    .then_with(|| a.1.account.cmp(&b.1.account))
            });
            let status = if !known.is_empty() {
                if known.iter().all(|(_, model, _)| model.risk == "high") {
                    "limited"
                } else {
                    "available"
                }
            } else if unknown {
                "unknown"
            } else {
                "exhausted"
            };
            incomplete |= unknown;
            let best_priority = known.first().map(|(priority, _, _)| *priority);
            if let Some(priority) = best_priority {
                best = Some(best.map_or(priority, |current: f64| current.max(priority)));
            }
            let representative = match status {
                "available" => known
                    .iter()
                    .find(|(_, model, _)| model.risk != "high")
                    .map(|(_, model, account)| (*model, *account)),
                "limited" => known.first().map(|(_, model, account)| (*model, *account)),
                "exhausted" => exhausted.first().copied(),
                _ => None,
            };
            let evidence = if status != "unknown" {
                "fresh"
            } else if newest.is_some() {
                "stale"
            } else {
                "missing"
            };
            let exhausted_until = if status == "exhausted" {
                min_option(
                    exhausted
                        .iter()
                        .filter_map(|(model, _)| model.exhausted_until),
                )
            } else {
                None
            };
            models.push(ModelView {
                model: model_id.to_owned(),
                native_model: representative
                    .map_or(native_model, |(model, _)| model.native_model.as_str())
                    .to_owned(),
                status,
                best_priority,
                evidence,
                newest_observed_at: newest,
                exhausted_until,
                limiting_window: representative.and_then(|(model, _)| {
                    model
                        .limiting_window
                        .as_ref()
                        .and_then(|window| window["window"].as_str())
                        .map(str::to_owned)
                }),
                remaining_percent: representative.and_then(|(model, _)| model.remaining_percent),
                reset_at: representative.and_then(|(model, _)| model.reset_at),
                risk: representative
                    .map_or_else(|| "unknown".to_owned(), |(model, _)| model.risk.clone()),
                valid_until: representative.and_then(|(model, _)| match model.state_validity {
                    Validity::Until { at } => Some(at),
                    _ => None,
                }),
                windows: representative.map_or_else(Vec::new, |(model, account)| {
                    windows(account, &model.native_model, at, snapshot.evaluated_at)
                }),
            });
        }
        if models.is_empty() {
            continue;
        }
        // The product can overflow; saturate so the order and its JSON stay representable.
        let score = best.map(|best| (best * ranking.priority_multiplier).min(f64::MAX));
        let status = models
            .iter()
            .map(|model| model.status)
            .min_by_key(|status| status_rank(status))
            .unwrap_or("unknown");
        providers.push(ProviderView {
            provider: provider.to_owned(),
            priority_multiplier: ranking.priority_multiplier,
            score,
            status,
            models,
        });
    }
    providers.sort_by(|a, b| {
        b.score
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&a.score.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| status_rank(a.status).cmp(&status_rank(b.status)))
            .then_with(|| a.provider.cmp(&b.provider))
    });
    Ok(CapacityView {
        schema_version: SCHEMA_VERSION,
        source_epoch: snapshot.source_epoch,
        revision: snapshot.revision,
        config_revision: snapshot.config_revision,
        evaluated_at: snapshot.evaluated_at,
        coverage: CoverageView {
            state: snapshot.coverage.state,
            incomplete,
        },
        providers,
    })
}

/// Encodes a JSON string with ASCII-only escaping so labels cannot split or reorder lines.
pub(crate) fn json_ascii(value: &str) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
    let mut output = String::with_capacity(encoded.len());
    for character in encoded.chars() {
        if character.is_ascii() {
            output.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0u16; 2]) {
                output.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    output
}

/// Decoded subset of a capacity result needed by the hook block.
#[derive(Deserialize)]
struct HookCapacity {
    schema_version: u8,
    coverage: Coverage,
    providers: Vec<HookProvider>,
}
/// One provider line of the hook block.
#[derive(Deserialize)]
struct HookProvider {
    provider: String,
    models: Vec<HookModel>,
}
/// One `model:status` entry of the hook block.
#[derive(Deserialize)]
struct HookModel {
    model: String,
    status: String,
}

/// Renders the compact UserPromptSubmit block within `limit` characters (capped at 2500).
///
/// Keeps the header and whole provider lines, adding an omission notice when
/// lines are dropped. Returns an empty string for an invalid result or a budget
/// too small for the header; the hook then prints nothing.
pub(crate) fn hook_text(capacity: Value, limit: usize) -> String {
    let Ok(capacity) = serde_json::from_value::<HookCapacity>(capacity) else {
        return String::new();
    };
    if capacity.schema_version != SCHEMA_VERSION
        || capacity.providers.iter().any(|provider| {
            provider.models.iter().any(|model| {
                !matches!(
                    model.status.as_str(),
                    "available" | "limited" | "unknown" | "exhausted"
                )
            })
        })
    {
        return String::new();
    }
    let header = match capacity.coverage.state.as_str() {
        "complete" => HOOK_HEADER.to_owned(),
        "partial" | "unknown" => {
            format!("{HOOK_HEADER} Quota coverage: {}.", capacity.coverage.state)
        }
        _ => return String::new(),
    };
    let mut lines = vec![header];
    if capacity.providers.is_empty() {
        lines.push("No configured providers.".into());
    }
    for (index, provider) in capacity.providers.iter().enumerate() {
        let models = provider
            .models
            .iter()
            .map(|model| format!("{}:{}", json_ascii(&model.model), model.status))
            .collect::<Vec<_>>()
            .join(",");
        lines.push(format!(
            "{}. provider={}; models=[{models}]",
            index + 1,
            json_ascii(&provider.provider)
        ));
    }
    truncate_lines(&lines, limit.min(HOOK_HARD_LIMIT_CHARS))
}

/// Keeps the header and whole following lines, appending the omission notice when needed.
fn truncate_lines(lines: &[String], limit: usize) -> String {
    let full = lines.join("\n");
    if full.chars().count() <= limit {
        return full;
    }
    let Some(header) = lines.first() else {
        return String::new();
    };
    let notice = HOOK_OMITTED.chars().count();
    let mut used = header.chars().count();
    if used + 1 + notice > limit {
        return String::new();
    }
    let mut kept = vec![header.as_str()];
    for line in &lines[1..] {
        let next = used + 1 + line.chars().count();
        if next + 1 + notice > limit {
            break;
        }
        used = next;
        kept.push(line);
    }
    kept.push(HOOK_OMITTED);
    kept.join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Explicit regression assertions")]
mod tests {
    use super::*;
    use serde_json::json;

    /// One unobserved source; no credential is resolved while parsing.
    fn source(account: &str, provider: &str, models: &[&str], multiplier: Option<f64>) -> String {
        let models = models
            .iter()
            .map(|model| format!("{{ id = \"{model}\" }}"))
            .collect::<Vec<_>>()
            .join(", ");
        let multiplier = multiplier.map_or_else(String::new, |value| {
            format!("priority_multiplier = {value:?}\n")
        });
        format!(
            "[[sources]]\naccount = \"{account}\"\nprovider = \"{provider}\"\nauth_family = \"openai\"\ncredential = \"native:codex\"\nnative_directory = \"/synthetic\"\nlimits_source = \"none\"\nmodels = [{models}]\n{multiplier}[sources.harness]\nid = \"codex\"\nbinary = \"/bin/true\"\n"
        )
    }

    /// A schema-2 model view with an explicit state, score and risk.
    fn model(provider: &str, name: &str, state: &str, score: Option<f64>, risk: &str) -> Value {
        let validity = match state {
            "unknown" => json!({"kind":"unknown"}),
            _ => json!({"kind":"until","at":2000.0}),
        };
        json!({"provider":provider,"model":name,"native_model":name,"state":state,
            "state_validity":validity,"quota_score":score,
            "limiting_window":if state == "unknown" { Value::Null } else { json!({"pool":"secret-acct::primary","source":"src","window":"five_hour"}) },
            "remaining_percent":if state == "known" { json!(42.0) } else if state == "exhausted" { json!(0.0) } else { Value::Null },
            "reset_at":if state == "unknown" { Value::Null } else { json!(1500.0) },
            "risk":risk,"newest_observed_at":if state == "unknown" { json!(900.0) } else { json!(990.0) },
            "exhausted_until":if state == "exhausted" { json!(1500.0) } else { Value::Null }})
    }

    /// One account with one governing window for every listed model.
    fn account(name: &str, models: Vec<Value>) -> Value {
        json!({"account":name,"models":models,"windows":[{"lane":"primary","source":"src","window":"five_hour",
            "models":["m1","m2","m3"],"observed_at":990.0,"valid_until":1900.0,"last_reset_at":1500.0,
            "forecast":{"known":true,"remaining_percent":42.0}}]})
    }

    /// Wraps account views in a snapshot evaluated at 1000.
    fn snapshot(coverage: &str, accounts: Vec<Value>) -> Value {
        json!({"schema_version":2,"source_epoch":"e","revision":3,"config_revision":"c",
            "evaluated_at":1000.0,"coverage":{"state":coverage},"accounts":accounts})
    }

    /// Account and provider multipliers order providers as agent-run did, with max-not-sum weights.
    #[test]
    fn multipliers_rank_providers_and_never_expose_accounts() {
        let text = [
            source("secret-acct", "alpha", &["m1"], Some(2.0)),
            source("secret-acct", "alpha", &["m1"], Some(3.0)),
            source("other-acct", "beta", &["m1"], None),
            "[providers.beta]\npriority_multiplier = 4.0\n".to_owned(),
        ]
        .concat();
        let config = QuotaConfig::parse(&text).unwrap();
        let view = rank(
            &config,
            snapshot(
                "complete",
                vec![
                    account(
                        "secret-acct",
                        vec![model("alpha", "m1", "known", Some(1.5), "low")],
                    ),
                    account(
                        "other-acct",
                        vec![model("beta", "m1", "known", Some(1.25), "low")],
                    ),
                ],
            ),
            1000.0,
        )
        .unwrap();
        let value = serde_json::to_value(&view).unwrap();
        // alpha: 1.5 * max(2, 3) = 4.5; beta: 1.25 * 1 * 4 = 5.
        assert_eq!(value["providers"][0]["provider"], "beta");
        assert_eq!(value["providers"][0]["score"], 5.0);
        assert_eq!(value["providers"][1]["models"][0]["best_priority"], 4.5);
        assert_eq!(value["providers"][1]["models"][0]["status"], "available");
        assert_eq!(
            value["providers"][1]["models"][0]["windows"][0]["remaining_percent"],
            42.0
        );
        assert_eq!(value["coverage"]["incomplete"], false);
        let wire = value.to_string();
        assert!(!wire.contains("secret-acct") && !wire.contains("other-acct"));
    }

    /// Ordered providers rank with neutral weights; their multipliers only choose fallback order.
    #[test]
    fn ordered_accounts_use_neutral_weights() {
        let text = [
            source("a1", "alpha", &["m1"], Some(10.0)),
            "[providers.alpha]\nordered_accounts = true\n".to_owned(),
        ]
        .concat();
        let config = QuotaConfig::parse(&text).unwrap();
        let view = rank(
            &config,
            snapshot(
                "complete",
                vec![account(
                    "a1",
                    vec![model("alpha", "m1", "known", Some(1.5), "low")],
                )],
            ),
            1000.0,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(view).unwrap()["providers"][0]["score"],
            1.5
        );
    }

    /// Limited, unknown and exhausted states keep their distinct meanings and order.
    #[test]
    fn statuses_distinguish_limited_unknown_and_exhausted() {
        let text = [
            source("a1", "alpha", &["m1", "m2", "m3"], None),
            source("a2", "beta", &["m1"], None),
        ]
        .concat();
        let config = QuotaConfig::parse(&text).unwrap();
        let view = rank(
            &config,
            snapshot(
                "partial",
                vec![
                    account(
                        "a1",
                        vec![
                            model("alpha", "m1", "known", Some(1.1), "high"),
                            model("alpha", "m2", "unknown", None, "unknown"),
                            model("alpha", "m3", "exhausted", None, "high"),
                        ],
                    ),
                    account("a2", vec![model("beta", "m1", "unknown", None, "unknown")]),
                ],
            ),
            1000.0,
        )
        .unwrap();
        let value = serde_json::to_value(view).unwrap();
        let alpha = &value["providers"][0];
        assert_eq!(alpha["provider"], "alpha");
        assert_eq!(alpha["status"], "limited");
        assert_eq!(alpha["models"][0]["status"], "limited");
        assert_eq!(alpha["models"][1]["status"], "unknown");
        assert_eq!(alpha["models"][1]["evidence"], "stale");
        assert_eq!(alpha["models"][1]["remaining_percent"], Value::Null);
        assert_eq!(alpha["models"][2]["status"], "exhausted");
        assert_eq!(alpha["models"][2]["exhausted_until"], 1500.0);
        assert_eq!(value["providers"][1]["score"], Value::Null);
        assert_eq!(value["coverage"]["incomplete"], true);
        // An expired validity bound is unknown at a later clock, never available.
        let later = rank(
            &config,
            snapshot(
                "complete",
                vec![account(
                    "a1",
                    vec![model("alpha", "m1", "known", Some(1.1), "low")],
                )],
            ),
            2000.0,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(later).unwrap()["providers"][0]["models"][0]["status"],
            "unknown"
        );
    }

    /// Unsupported snapshots and malformed known scores are refused.
    #[test]
    fn invalid_snapshots_are_refused() {
        let config = QuotaConfig::parse(&source("a1", "alpha", &["m1"], None)).unwrap();
        let mut old = snapshot("complete", vec![]);
        old["schema_version"] = json!(1);
        assert!(rank(&config, old, 1000.0).is_err());
        let bad = snapshot(
            "complete",
            vec![account(
                "a1",
                vec![model("alpha", "m1", "known", None, "low")],
            )],
        );
        assert!(rank(&config, bad, 1000.0).is_err());
    }

    /// Hook lines keep agent-run's shape, escape labels and drop whole lines within budget.
    #[test]
    fn hook_text_is_compact_escaped_and_bounded() {
        let capacity = json!({"schema_version":1,"coverage":{"state":"complete","incomplete":false},
            "providers":[{"provider":"alpha","models":[{"model":"m\u{2028}1","status":"available"},{"model":"m2","status":"exhausted"}]},
                         {"provider":"beta","models":[{"model":"m1","status":"unknown"},{"model":"long-model-name-that-does-not-fit","status":"unknown"}]}]});
        let text = hook_text(capacity.clone(), HOOK_DEFAULT_CHARS);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("Provider capacity order (highest first; read-only"));
        assert_eq!(
            lines[1],
            "1. provider=\"alpha\"; models=[\"m\\u20281\":available,\"m2\":exhausted]"
        );
        assert_eq!(
            lines[2],
            "2. provider=\"beta\"; models=[\"m1\":unknown,\"long-model-name-that-does-not-fit\":unknown]"
        );
        let budget = HOOK_HEADER.chars().count()
            + 1
            + lines[1].chars().count()
            + 1
            + HOOK_OMITTED.chars().count();
        let short = hook_text(capacity.clone(), budget);
        assert!(short.chars().count() <= budget);
        assert!(short.ends_with(HOOK_OMITTED));
        assert_eq!(short.lines().count(), 3);
        assert!(hook_text(capacity.clone(), 10).is_empty());
        assert!(hook_text(capacity.clone(), 100_000).chars().count() <= HOOK_HARD_LIMIT_CHARS);
        let mut partial = capacity.clone();
        partial["coverage"]["state"] = json!("partial");
        assert!(
            hook_text(partial, HOOK_DEFAULT_CHARS)
                .lines()
                .next()
                .unwrap()
                .ends_with("Quota coverage: partial.")
        );
        let mut invalid = capacity;
        invalid["providers"][0]["models"][0]["status"] = json!("priority_overflow");
        assert!(hook_text(invalid, HOOK_DEFAULT_CHARS).is_empty());
    }
}
