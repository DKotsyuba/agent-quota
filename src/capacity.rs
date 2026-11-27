//! Account-free provider/model capacity ranking over one evaluated quota snapshot.
//!
//! This ports agent-run's provider capacity order (`provider_ranking.rs` and
//! `quota_projection.rs`) into Quota, which now owns provider and account
//! multipliers. Run keeps only its local admission read: account enablement,
//! cooldowns and active-attempt accounting are Run state and are not applied
//! here. The result never contains account identities, pools or credentials.

use crate::{
    config::{HarnessId, ProviderRanking, QuotaConfig},
    display,
    quota::{CreditBalance, ResetCreditSnapshot},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
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
const HOOK_HEADER: &str = "Quota update · provider capacity order (highest first, resets in UTC).";
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
    windows: Vec<SnapshotWindow>,
    #[serde(default)]
    reset_credits: ResetCreditSnapshot,
    credit_balance: Option<CreditBalance>,
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
    burn_percent_per_hour: Option<f64>,
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
pub(crate) struct SnapshotWindow {
    lane: String,
    source: String,
    window: String,
    models: Option<BTreeSet<String>>,
    observed_at: Option<f64>,
    valid_until: Option<f64>,
    last_reset_at: Option<f64>,
    last_remaining_percent: Option<f64>,
    forecast: WindowForecast,
}
/// The forecast fields needed for an account-free window row.
#[derive(Deserialize)]
struct WindowForecast {
    known: bool,
    remaining_percent: Option<f64>,
    burn_percent_per_hour: Option<f64>,
}

/// One account's standing on one model lane, as in agent-run's `LaneVerdict`.
enum Verdict<'a> {
    /// Usable capacity with `quota_score * account multiplier`.
    Known {
        priority: f64,
        raw: f64,
        model: &'a Model,
    },
    /// No current evidence.
    Unknown,
    /// Current evidence whose weighted priority exceeds the finite range.
    Overflow,
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
    /// True when coverage is not complete or an eligible lane is unknown or overflows.
    incomplete: bool,
}
/// One provider's account-free standing.
#[derive(Serialize)]
struct ProviderView {
    provider: String,
    priority_multiplier: f64,
    /// Best credit-adjusted model priority times the provider multiplier.
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
    /// `available`, `limited`, `unknown`, `priority_overflow` or `exhausted`.
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    additional_windows: Vec<WindowView>,
    burn_percent_per_hour: Option<f64>,
    /// Credit-adjusted model priority, including an exhausted plan with usable credits.
    ranking_priority: Option<f64>,
    /// Credits of the account that supplied the adjusted priority.
    ranking_credits: RankingCredits,
    /// Total applicable reset uses and count of paid-credit bindings for change detection.
    credit_availability: (u64, usize),
}
/// One governing window of the representative account, without pool or account identity.
#[derive(Deserialize, Serialize, PartialEq)]
pub(crate) struct WindowView {
    pub window: String,
    pub source: String,
    pub known: bool,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<f64>,
    pub observed_at: Option<f64>,
    valid_until: Option<f64>,
    pub last_remaining_percent: Option<f64>,
    pub burn_percent_per_hour: Option<f64>,
    #[serde(default)]
    pub model_specific: bool,
}

/// Credit capacity from one winning account, without credit IDs or account metadata.
#[derive(Default, Deserialize, Serialize)]
pub(crate) struct RankingCredits {
    pub reset_uses: u32,
    pub paid: Option<CreditBalance>,
}

impl RankingCredits {
    /// Historical reset multiplier, plus bounded capacity for a reset or paid fallback.
    fn adjusted(&self, raw: f64, ranking: &ProviderRanking) -> f64 {
        let uses = f64::from(self.reset_uses);
        let base = if self.reset_uses > 0 {
            raw.max(ranking.reset_credit_floor)
        } else {
            raw
        };
        base * (1.0 + uses / (uses + 1.0))
            + self
                .paid
                .as_ref()
                .map_or(0.0, |_| ranking.paid_credit_bonus)
    }
}

/// Count currently usable reset uses that cover this model's limiting window.
fn ranking_credits(account: &Account, model: &Model, codex: bool, at: f64) -> RankingCredits {
    let snapshot = &account.reset_credits;
    let window = model
        .limiting_window
        .as_ref()
        .and_then(|key| key["window"].as_str());
    let reset_uses =
        if snapshot.inventory.known && snapshot.observed_at.is_some_and(|time| time <= at) {
            if snapshot.inventory.credits.is_empty() {
                snapshot.inventory.available_count.unwrap_or(0)
            } else {
                snapshot
                    .inventory
                    .credits
                    .iter()
                    .filter(|credit| {
                        credit.usable_now == Some(true)
                            && credit.granted_at.is_none_or(|time| time <= at)
                            && credit.expires_at.is_none_or(|time| at < time)
                            && match credit.scope.as_str() {
                                "all" => true,
                                scope if Some(scope) == window => true,
                                scope => scope.strip_prefix("model:").is_some_and(|name| {
                                    name == model.model || name == model.native_model
                                }),
                            }
                    })
                    .map(|credit| credit.resets_left.unwrap_or(1))
                    .fold(0u32, u32::saturating_add)
            }
        } else {
            0
        };
    RankingCredits {
        reset_uses,
        paid: account
            .credit_balance
            .as_ref()
            .filter(|credit| codex && credit.available())
            .cloned(),
    }
}

/// Agent Run's status order for score ties. Both known display statuses share a rank.
fn status_rank(status: &str) -> u8 {
    match status {
        "available" | "limited" => 0,
        "unknown" => 1,
        "priority_overflow" => 2,
        "exhausted" => 3,
        _ => 4,
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
        Verdict::Known {
            priority,
            raw,
            model,
        }
    } else {
        Verdict::Overflow
    })
}

/// Account-free window details of the representative account's model lane.
pub(crate) fn windows(
    windows: &[SnapshotWindow],
    native: &str,
    at: f64,
    evaluated_at: f64,
) -> Vec<WindowView> {
    let mut rows: Vec<WindowView> = windows
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
                last_remaining_percent: window.last_remaining_percent,
                burn_percent_per_hour: window.forecast.burn_percent_per_hour.filter(|_| known),
                model_specific: window.lane.starts_with("model:"),
            }
        })
        .collect();
    rows.sort_by(|a, b| (&a.window, &a.source).cmp(&(&b.window, &b.source)));
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
/// in scope are omitted. A scored row takes its native model and detail fields
/// from the lane and account that supplied its best priority. Errors are fixed safe codes.
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
            let mut eligible: BTreeMap<(&str, &str), (f64, bool)> = BTreeMap::new();
            for source in &sources {
                if let Some(model) = source.models.iter().find(|model| model.id == model_id) {
                    let factor = if ranking.ordered_accounts {
                        1.0
                    } else {
                        source.priority_multiplier
                    };
                    let codex = source.harness.id == HarnessId::Codex;
                    eligible
                        .entry((source.account.as_str(), model.lane()))
                        .and_modify(|(value, supports_paid)| {
                            *value = value.max(factor);
                            *supports_paid |= codex;
                        })
                        .or_insert((factor, codex));
                }
            }
            let mut known: Vec<(f64, &Model, &Account)> = Vec::new();
            let mut exhausted: Vec<(&Model, &Account)> = Vec::new();
            let mut unknown = false;
            let mut overflow = false;
            let mut newest: Option<f64> = None;
            let mut native_model: Option<&str> = None;
            let mut adjusted = None;
            let mut ranking_credit = RankingCredits::default();
            let mut credit_availability = (0u64, 0usize);
            let mut fallback: Option<(&Model, &Account)> = None;
            for ((account_id, native), (factor, codex)) in &eligible {
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
                if fallback.is_none_or(|(previous, _)| {
                    view.newest_observed_at > previous.newest_observed_at
                }) {
                    fallback = Some((view, account));
                }
                if at >= snapshot.evaluated_at
                    && let Some(observed) = view
                        .newest_observed_at
                        .filter(|observed| observed.is_finite() && *observed <= at)
                {
                    newest = Some(newest.map_or(observed, |current: f64| current.max(observed)));
                }
                let credits = ranking_credits(account, view, *codex, at);
                credit_availability.0 += u64::from(credits.reset_uses);
                credit_availability.1 += usize::from(credits.paid.is_some());
                let raw = match verdict(view, *factor, at, snapshot.evaluated_at)? {
                    Verdict::Known {
                        priority,
                        raw,
                        model,
                    } => {
                        known.push((priority, model, account));
                        Some(raw)
                    }
                    Verdict::Exhausted { model } => {
                        exhausted.push((model, account));
                        (credits.reset_uses > 0 || credits.paid.is_some()).then_some(0.0)
                    }
                    Verdict::Unknown => {
                        unknown = true;
                        None
                    }
                    Verdict::Overflow => {
                        overflow = true;
                        None
                    }
                };
                if let Some(raw) = raw {
                    let adjusted_raw = credits.adjusted(raw, ranking);
                    if raw == 0.0 && adjusted_raw == 0.0 && view.state == "exhausted" {
                        continue;
                    }
                    let priority = (adjusted_raw * factor).min(f64::MAX);
                    if adjusted.is_none_or(|best| priority > best) {
                        adjusted = Some(priority);
                        ranking_credit = credits;
                    }
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
            } else if overflow {
                "priority_overflow"
            } else {
                "exhausted"
            };
            incomplete |= unknown || overflow;
            let best_priority = known.first().map(|(priority, _, _)| *priority);
            if let Some(priority) = adjusted {
                best = Some(best.map_or(priority, |current: f64| current.max(priority)));
            }
            let representative = match status {
                "available" | "limited" => {
                    known.first().map(|(_, model, account)| (*model, *account))
                }
                "exhausted" => exhausted.first().copied(),
                _ => None,
            };
            let evidence = if best_priority.is_some() || status == "exhausted" {
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
            let mut detail_windows =
                representative
                    .or(fallback)
                    .map_or_else(Vec::new, |(model, account)| {
                        windows(
                            &account.windows,
                            &model.native_model,
                            at,
                            snapshot.evaluated_at,
                        )
                    });
            let additional_windows = if representative.is_none() {
                std::mem::take(&mut detail_windows)
            } else if detail_windows.len() > MAX_MODEL_WINDOWS {
                detail_windows.split_off(MAX_MODEL_WINDOWS)
            } else {
                Vec::new()
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
                windows: detail_windows,
                additional_windows,
                burn_percent_per_hour: representative
                    .and_then(|(model, _)| model.burn_percent_per_hour),
                ranking_priority: adjusted,
                ranking_credits: ranking_credit,
                credit_availability,
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
            .min_by_key(|status| (status_rank(status), *status == "limited"))
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
        let a_score = a.score.unwrap_or(f64::NEG_INFINITY);
        let b_score = b.score.unwrap_or(f64::NEG_INFINITY);
        b_score
            .partial_cmp(&a_score)
            .unwrap_or(Ordering::Equal)
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
    evaluated_at: f64,
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
    limiting_window: Option<String>,
    remaining_percent: Option<f64>,
    reset_at: Option<f64>,
    burn_percent_per_hour: Option<f64>,
    newest_observed_at: Option<f64>,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    windows: Vec<WindowView>,
    #[serde(default)]
    additional_windows: Vec<WindowView>,
    #[serde(default)]
    ranking_credits: RankingCredits,
    #[serde(default)]
    credit_availability: (u64, usize),
}

impl HookModel {
    fn windows(&self) -> impl Iterator<Item = &WindowView> {
        self.windows.iter().chain(&self.additional_windows)
    }
}

/// Hook render and the digest of its significant quota facts.
pub(crate) struct HookBlock {
    pub text: String,
    pub digest: [u8; 32],
}

/// Keeps common labels short and quotes any text that could delimit a line.
fn hook_label(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        value.to_owned()
    } else {
        json_ascii(value)
    }
}

/// Renders shared counters once and names model-specific or differing counters.
fn provider_line(provider: &HookProvider, at: f64) -> String {
    let mut windows: Vec<(&WindowView, BTreeSet<&str>)> = Vec::new();
    let mut models = Vec::new();
    for model in &provider.models {
        for window in model.windows() {
            if let Some((_, names)) = windows.iter_mut().find(|(existing, _)| *existing == window) {
                names.insert(model.model.as_str());
            } else {
                windows.push((window, BTreeSet::from([model.model.as_str()])));
            }
        }
        let limiting = model
            .limiting_window
            .as_deref()
            .map(display::window_label)
            .map(hook_label);
        let name = hook_label(&model.model);
        let mut status = format!("{name} {}", model.status);
        if let Some(reason) = display::reason(
            &model.status,
            limiting.as_deref(),
            model.remaining_percent,
            model.burn_percent_per_hour,
        ) {
            status.push_str(&format!(": {reason}"));
        }
        if model.evidence == "stale" {
            status.push_str(&format!(
                " ({})",
                display::stale(model.newest_observed_at, at)
            ));
        }
        if model.windows().next().is_none()
            && let Some(window) = limiting
        {
            status.push_str(&format!(
                " · {window} {}",
                display::percent(model.remaining_percent)
            ));
            if let Some(reset) = model.reset_at.filter(|reset| *reset > at) {
                status.push_str(&format!(" (reset {})", display::short_reset(reset, at)));
            }
        }
        let badges = display::credit_badges(&model.ranking_credits);
        if !badges.is_empty() {
            status.push_str(&format!(" {}", badges.join(" ")));
        }
        models.push(status);
    }
    windows.sort_by_key(|(window, _)| (window.model_specific, window.window.as_str()));
    let next_reset = windows
        .iter()
        .filter_map(|(window, _)| window.reset_at)
        .filter(|reset| *reset > at)
        .min_by(f64::total_cmp);
    let mut facts = Vec::new();
    for (window, names) in &windows {
        let label = hook_label(display::window_label(&window.window));
        let differing = windows.iter().any(|(other, _)| {
            other.window == window.window && !other.model_specific && *other != *window
        });
        let label = if window.model_specific || differing {
            let names: Vec<_> = names.iter().map(|name| hook_label(name)).collect();
            format!("{} {label}", names.join("/"))
        } else {
            label
        };
        let mut fact = match (window.remaining_percent, window.last_remaining_percent) {
            (None, Some(last)) => format!("{label} last {last:.0}%"),
            (remaining, _) => format!("{label} {}", display::percent(remaining)),
        };
        if !window.known && window.observed_at.is_some() {
            fact.push_str(&format!(" ({})", display::stale(window.observed_at, at)));
        }
        if window.reset_at == next_reset
            && let Some(reset) = next_reset
        {
            fact.push_str(&format!(" (reset {})", display::short_reset(reset, at)));
        }
        facts.push(fact);
    }
    facts.extend(models);
    format!("{}: {}", hook_label(&provider.provider), facts.join(" · "))
}

/// Fingerprints the ordered providers and significant model/window/credit changes.
fn hook_digest(capacity: &HookCapacity) -> Option<[u8; 32]> {
    let bucket = |percent: Option<f64>| percent.map(|percent| (percent / 10.0).floor() as u8);
    let reset = |at: Option<f64>| at.map(|at| (at, at > capacity.evaluated_at));
    let providers: Vec<_> = capacity
        .providers
        .iter()
        .map(|provider| {
            let models: Vec<_> = provider
                .models
                .iter()
                .map(|model| {
                    let windows: Vec<_> = model
                        .windows()
                        .map(|window| {
                            (
                                &window.window,
                                &window.source,
                                window.model_specific,
                                window.known,
                                bucket(window.remaining_percent.or(window.last_remaining_percent)),
                                reset(window.reset_at),
                            )
                        })
                        .collect();
                    (
                        &model.model,
                        &model.status,
                        &model.evidence,
                        bucket(model.remaining_percent),
                        reset(model.reset_at),
                        windows,
                        model.credit_availability,
                        model.ranking_credits.reset_uses,
                        model
                            .ranking_credits
                            .paid
                            .as_ref()
                            .map(|paid| paid.unlimited),
                    )
                })
                .collect();
            (&provider.provider, models)
        })
        .collect();
    let bytes = serde_json::to_vec(&(&capacity.coverage.state, providers)).ok()?;
    Some(Sha256::digest(bytes).into())
}

/// Renders the compact UserPromptSubmit block within `limit` characters (capped at 2500).
///
/// Keeps the header and whole provider lines, adding an omission notice when
/// lines are dropped. Returns no block for an invalid result or a budget
/// too small for the header; the hook then prints nothing.
pub(crate) fn hook_block(capacity: Value, limit: usize) -> Option<HookBlock> {
    let capacity = serde_json::from_value::<HookCapacity>(capacity).ok()?;
    if capacity.schema_version != SCHEMA_VERSION
        || capacity.providers.iter().any(|provider| {
            provider.models.iter().any(|model| {
                !matches!(
                    model.status.as_str(),
                    "available" | "limited" | "unknown" | "priority_overflow" | "exhausted"
                )
            })
        })
    {
        return None;
    }
    let header = match capacity.coverage.state.as_str() {
        "complete" => HOOK_HEADER.to_owned(),
        "partial" | "unknown" => {
            format!("{HOOK_HEADER} Quota coverage: {}.", capacity.coverage.state)
        }
        _ => return None,
    };
    let mut lines = vec![header];
    if capacity.providers.is_empty() {
        lines.push("No configured providers.".into());
    }
    lines.extend(
        capacity
            .providers
            .iter()
            .map(|provider| provider_line(provider, capacity.evaluated_at)),
    );
    let text = truncate_lines(&lines, limit.min(HOOK_HARD_LIMIT_CHARS));
    if text.is_empty() {
        return None;
    }
    Some(HookBlock {
        text,
        digest: hook_digest(&capacity)?,
    })
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

    /// Agent Run breaks equal known scores by provider name regardless of quota risk.
    #[test]
    fn equal_provider_scores_sort_by_name() {
        let config = QuotaConfig::parse(
            &[
                source("a2", "beta", &["m1"], None),
                source("a1", "alpha", &["m1"], None),
            ]
            .concat(),
        )
        .unwrap();
        let view = rank(
            &config,
            snapshot(
                "complete",
                vec![
                    account("a2", vec![model("beta", "m1", "known", Some(1.0), "low")]),
                    account("a1", vec![model("alpha", "m1", "known", Some(1.0), "high")]),
                ],
            ),
            1000.0,
        )
        .unwrap();
        assert_eq!(view.providers[0].provider, "alpha");
        assert_eq!(view.providers[0].status, "limited");
        assert_eq!(view.providers[1].provider, "beta");
        assert_eq!(view.providers[1].status, "available");
        assert_eq!(view.providers[0].score, view.providers[1].score);
    }

    /// Agent Run ranks unknown above overflow and overflow above exhausted.
    #[test]
    fn overflow_is_distinct_from_unknown_and_exhausted() {
        let config = QuotaConfig::parse(
            &[
                source("a1", "alpha", &["m1"], None),
                source("a2", "beta", &["m1"], Some(2.0)),
                source("a3", "gamma", &["m1"], None),
            ]
            .concat(),
        )
        .unwrap();
        let view = rank(
            &config,
            snapshot(
                "complete",
                vec![
                    account("a1", vec![model("alpha", "m1", "exhausted", None, "high")]),
                    account(
                        "a2",
                        vec![model("beta", "m1", "known", Some(f64::MAX), "low")],
                    ),
                    account("a3", vec![model("gamma", "m1", "unknown", None, "unknown")]),
                ],
            ),
            1000.0,
        )
        .unwrap();
        let statuses: Vec<_> = view
            .providers
            .iter()
            .map(|provider| provider.status)
            .collect();
        assert_eq!(statuses, ["unknown", "priority_overflow", "exhausted"]);
        let overflow = &view.providers[1];
        assert_eq!(overflow.score, None);
        assert_eq!(overflow.models[0].status, "priority_overflow");
        assert_eq!(overflow.models[0].best_priority, None);
        assert_eq!(overflow.models[0].evidence, "stale");
        assert!(view.coverage.incomplete);
        assert!(
            hook_block(serde_json::to_value(view).unwrap(), HOOK_DEFAULT_CHARS)
                .unwrap()
                .text
                .contains("m1 priority_overflow")
        );
    }

    /// A shared alias publishes model and window details from the account and lane that scored.
    #[test]
    fn alias_details_follow_the_scoring_lane() {
        let config = QuotaConfig::parse(
            &[
                source("a1", "alpha", &["m1"], None),
                source("a2", "alpha", &["m1"], Some(3.0))
                    .replace(r#"id = "m1""#, r#"id = "m1", native_model = "native""#),
            ]
            .concat(),
        )
        .unwrap();
        let mut scoring_model = model("alpha", "m1", "known", Some(0.5), "high");
        scoring_model["native_model"] = json!("native");
        scoring_model["limiting_window"]["window"] = json!("weekly");
        scoring_model["remaining_percent"] = json!(5.0);
        let mut scoring_account = account("a2", vec![scoring_model]);
        let window = &mut scoring_account["windows"][0];
        window["models"] = json!(["native"]);
        window["window"] = json!("weekly");
        window["forecast"]["remaining_percent"] = json!(5.0);
        let view = rank(
            &config,
            snapshot(
                "complete",
                vec![
                    account("a1", vec![model("alpha", "m1", "known", Some(1.0), "low")]),
                    scoring_account,
                ],
            ),
            1000.0,
        )
        .unwrap();
        let row = &view.providers[0].models[0];
        assert_eq!(row.status, "available");
        assert_eq!(row.best_priority, Some(1.5));
        assert_eq!(row.native_model, "native");
        assert_eq!(row.risk, "high");
        assert_eq!(row.limiting_window.as_deref(), Some("weekly"));
        assert_eq!(row.remaining_percent, Some(5.0));
        assert_eq!(row.windows.len(), 1);
        assert_eq!(row.windows[0].window, "weekly");
        assert_eq!(row.windows[0].remaining_percent, Some(5.0));
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

    /// Credit-backed options improve the order while schema-1 window statuses and base scores survive.
    #[test]
    fn credits_boost_capacity_without_changing_plan_evidence() {
        let config = QuotaConfig::parse(
            &[
                source("a1", "alpha", &["m1"], None),
                source("a2", "beta", &["m1"], None),
            ]
            .concat(),
        )
        .unwrap();
        let beta = account("a2", vec![model("beta", "m1", "known", Some(0.4), "low")]);
        let rank_alpha = |alpha: &Value| {
            serde_json::to_value(
                rank(
                    &config,
                    snapshot("complete", vec![alpha.clone(), beta.clone()]),
                    1000.0,
                )
                .unwrap(),
            )
            .unwrap()
        };
        let mut alpha = account("a1", vec![model("alpha", "m1", "known", Some(0.3), "high")]);
        let legacy = rank_alpha(&alpha);
        assert_eq!(legacy["providers"][0]["provider"], "beta");
        alpha["reset_credits"] =
            json!({"known":true,"available_count":1,"observed_at":990.0,"credits":[]});
        let reset = rank_alpha(&alpha);
        assert_eq!(reset["schema_version"], 1);
        assert_eq!(reset["providers"][0]["provider"], "alpha");
        assert!((reset["providers"][0]["score"].as_f64().unwrap() - 0.45).abs() < 1e-9);
        let original_fields = legacy["providers"][1]["models"][0].as_object().unwrap();
        for (key, original) in original_fields {
            if !matches!(
                key.as_str(),
                "ranking_priority" | "ranking_credits" | "credit_availability"
            ) {
                assert_eq!(&reset["providers"][0]["models"][0][key], original, "{key}");
            }
        }
        assert_eq!(
            reset["providers"][0]["models"][0]["ranking_credits"]["reset_uses"],
            1
        );

        alpha["models"][0] = model("alpha", "m1", "exhausted", None, "high");
        alpha["reset_credits"]["available_count"] = json!(2);
        assert_eq!(rank_alpha(&alpha)["providers"][0]["provider"], "alpha");
        alpha["reset_credits"]["credits"] = json!([
            {"id":"wrong-scope","provider_kind":"GRANT","label":"reset","scope":"seven_day","usable_now":true},
            {"id":"expired","provider_kind":"GRANT","label":"reset","scope":"five_hour","usable_now":true,"expires_at":1000.0},
            {"id":"unusable","provider_kind":"GRANT","label":"reset","scope":"all","usable_now":false}
        ]);
        assert_eq!(rank_alpha(&alpha)["providers"][1]["score"], Value::Null);
        alpha["credit_balance"] = json!({"has_credits":false,"unlimited":true,"balance":null});
        let paid = rank_alpha(&alpha);
        assert_eq!(paid["providers"][0]["provider"], "alpha");
        assert_eq!(paid["providers"][0]["score"], 0.5);
        let row = &paid["providers"][0]["models"][0];
        assert_eq!(row["status"], "exhausted");
        assert_eq!(row["best_priority"], Value::Null);
        assert_eq!(row["exhausted_until"], 1500.0);
        assert_eq!(row["ranking_priority"], 0.5);
        alpha["models"][0] = model("alpha", "m1", "unknown", None, "unknown");
        assert_eq!(rank_alpha(&alpha)["providers"][1]["score"], Value::Null);
    }

    /// A shared window is rendered once and scoped weekly quota keeps its model label.
    #[test]
    fn hook_renders_shared_and_scoped_windows_and_tracks_significant_changes() {
        let at = 1795798740.0;
        let window = |name, remaining, reset, model_specific| {
            json!({"source":"fixture","window":name,"known":true,"remaining_percent":remaining,
                "reset_at":reset,"model_specific":model_specific})
        };
        let shared = vec![
            window("five_hour", 64.4, 1795803060.0, false),
            window("seven_day", 73.0, 1796331540.0, false),
        ];
        let mut fable = shared.clone();
        fable.push(window("seven_day", 63.0, 1796331540.0, true));
        let capacity = json!({"schema_version":1,"evaluated_at":at,"coverage":{"state":"complete"},
            "providers":[{"provider":"claude","models":[
                {"model":"sonnet","status":"available","windows":shared},
                {"model":"fable","status":"limited","limiting_window":"seven_day","remaining_percent":63.0,
                 "burn_percent_per_hour":20.0,"windows":fable}
            ]},{"provider":"glm","models":[{"model":"glm","status":"unknown"}]}]});
        let render = |value| hook_block(value, HOOK_DEFAULT_CHARS).unwrap();
        let original = render(capacity.clone());
        assert_eq!(
            original.text,
            concat!(
                "Quota update · provider capacity order (highest first, resets in UTC).\n",
                "claude: 5h 64% (reset 18:11) · 7d 73% · fable 7d 63% · sonnet available · ",
                "fable limited: 7d window 63% left, burn 20.0%/h → runs out before reset\n",
                "glm: glm unknown"
            )
        );
        let mut drift = capacity.clone();
        drift["evaluated_at"] = json!(at + 60.0);
        drift["providers"][0]["models"][1]["burn_percent_per_hour"] = json!(20.7);
        drift["providers"][0]["models"][1]["windows"][2]["remaining_percent"] = json!(69.9);
        assert_eq!(render(drift.clone()).digest, original.digest);
        drift["providers"][0]["models"][1]["windows"][2]["remaining_percent"] = json!(70.0);
        assert_ne!(render(drift).digest, original.digest);
        for (pointer, value) in [
            ("/providers/0/models/1/status", json!("exhausted")),
            (
                "/providers/0/models/1/windows/2/reset_at",
                json!(1796331541.0),
            ),
            ("/providers/0/models/1/credit_availability", json!([1, 0])),
            ("/providers/0/models/1/credit_availability", json!([0, 1])),
            ("/evaluated_at", json!(1795803060.0)),
        ] {
            let mut changed = capacity.clone();
            if pointer.ends_with("/credit_availability") {
                changed["providers"][0]["models"][1]["credit_availability"] = value;
            } else {
                *changed.pointer_mut(pointer).unwrap() = value;
            }
            assert_ne!(render(changed).digest, original.digest, "{pointer}");
        }
        let mut reordered = capacity;
        reordered["providers"].as_array_mut().unwrap().reverse();
        assert_ne!(render(reordered).digest, original.digest);
    }

    /// Hook lines escape labels and drop whole provider lines within budget.
    #[test]
    fn hook_text_is_compact_escaped_and_bounded() {
        let capacity = json!({"schema_version":1,"evaluated_at":0,"coverage":{"state":"complete","incomplete":false},
            "providers":[{"provider":"alpha","models":[{"model":"m\u{2028}1","status":"available"},{"model":"m2","status":"exhausted"}]},
                         {"provider":"beta","models":[{"model":"m1","status":"unknown"},{"model":"long-model-name-that-does-not-fit","status":"unknown"}]}]});
        let render =
            |value, limit| hook_block(value, limit).map_or_else(String::new, |block| block.text);
        let text = render(capacity.clone(), HOOK_DEFAULT_CHARS);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], HOOK_HEADER);
        assert_eq!(
            lines[1],
            "alpha: \"m\\u20281\" available · m2 exhausted: plan window has no usable quota"
        );
        assert_eq!(
            lines[2],
            "beta: m1 unknown · long-model-name-that-does-not-fit unknown"
        );
        let budget = HOOK_HEADER.chars().count()
            + 1
            + lines[1].chars().count()
            + 1
            + HOOK_OMITTED.chars().count();
        let short = render(capacity.clone(), budget);
        assert!(short.chars().count() <= budget);
        assert!(short.ends_with(HOOK_OMITTED));
        assert_eq!(short.lines().count(), 3);
        assert!(render(capacity.clone(), 10).is_empty());
        assert!(render(capacity.clone(), 100_000).chars().count() <= HOOK_HARD_LIMIT_CHARS);
        let mut partial = capacity.clone();
        partial["coverage"]["state"] = json!("partial");
        assert!(
            render(partial, HOOK_DEFAULT_CHARS)
                .lines()
                .next()
                .unwrap()
                .ends_with("Quota coverage: partial.")
        );
        let mut invalid = capacity;
        invalid["providers"][0]["models"][0]["status"] = json!("unsupported");
        assert!(render(invalid, HOOK_DEFAULT_CHARS).is_empty());
    }
}
