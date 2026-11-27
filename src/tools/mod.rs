//! Authoritative Rust tool registry. JSON discovery is an exported snapshot, not a second source.
use crate::{
    capacity::{self, RankingCredits, SnapshotWindow, WindowView},
    display,
    quota::{CreditBalance, ResetCreditSnapshot},
    response::Templates,
};
use mcp_presentation::Renderer;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
// xtask:modules

pub fn definitions() -> Vec<Value> {
    vec![
        json!({"name":"get_status", "description":"Report the product name, version and release qualification. Read-only; does not access files or external services.",
        "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}),
        json!({"name":"get_quota", "description":"Read stored per-account quota windows, reset times, risk reasons, reset credits, and Codex paid balances. Does not contact providers or start models. Narrow account or model scope if the result is too large.",
        "inputSchema":{"type":"object","properties":{"accounts":{"type":"array","items":{"type":"string"},"maxItems":256},"model":{"type":["string","null"],"maxLength":256}},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}),
        json!({"name":"get_capacity", "description":"Rank configured providers by stored window and credit capacity, applying provider and account multipliers. Shows model statuses and reasons, remaining percentages and resets for each window, stale evidence, and ranking credits without account or pool identities. Read-only; does not contact providers or start models.",
        "inputSchema":{"type":"object","properties":{"model":{"type":["string","null"],"maxLength":256}},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}),
        // xtask:definitions
    ]
}
pub fn templates() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "invalid_arguments",
            "ERROR invalid_arguments: get_status accepts an empty argument object.\n",
        ),
        (
            "quota",
            concat!(
                "Quota at {{ evaluated_at }} | coverage: {{ coverage }}\n",
                "{% for row in rows %}{{ row.account }} | {{ row.provider }} | {{ row.model }} | {{ row.details.status }}{% if row.details.reason %}: {{ row.details.reason }}{% endif %}{% if row.details.stale %} | {{ row.details.stale }}{% endif +%}\n",
                "{% for window in row.details.windows %}  {{ window.label }} | {{ window.remaining }}{% if window.burn %} | burn {{ window.burn }}{% endif %} | reset {{ window.reset }}{% if window.stale %} | {{ window.stale }}{% endif +%}\n{% endfor %}",
                "{% else %}No configured models in scope.\n{% endfor %}",
                "{% for row in reset_credits %}{{ row.account }} | reset credits: {% if row.known %}known{% else %}unknown{% endif %} | available count: {{ row.available_count }}{% if row.paid %} | paid credits: {{ row.paid }}{% endif +%}\n",
                "{% for credit in row.credits %}  {{ credit.id }} | {{ credit.provider_kind }} | {{ credit.label }} | {{ credit.scope }} | resets left: {{ credit.resets_left }} | usable now: {{ credit.usable_now }} | granted: {{ credit.granted_at }} | expires: {{ credit.expires_at }}\n{% endfor %}{% endfor %}",
            ),
        ),
        (
            "capacity",
            concat!(
                "Capacity order at {{ evaluated_at }} | coverage: {{ coverage }}\n",
                "{% for provider in providers %}{{ provider.rank }}. {{ provider.provider }} | status: {{ provider.status }} | score: {{ provider.score }}\n",
                "{% for model in provider.models %}   {{ model.model }} | {{ model.details.status }}{% if model.details.reason %}: {{ model.details.reason }}{% endif %}{% if model.details.stale %} | {{ model.details.stale }}{% endif %}{% for credit in model.credits %} | {{ credit }}{% endfor +%}\n",
                "{% for window in model.details.windows %}     {{ window.label }} | {{ window.remaining }}{% if window.burn %} | burn {{ window.burn }}{% endif %} | reset {{ window.reset }}{% if window.stale %} | {{ window.stale }}{% endif +%}\n{% endfor %}",
                "{% endfor %}{% else %}No configured providers in scope.\n{% endfor %}",
            ),
        ),
        // xtask:templates
    ]
}
pub fn incomplete() -> Vec<&'static str> {
    let statuses: &[(&str, bool)] = &[
        ("get_status", true),
        ("get_quota", true),
        ("get_capacity", true),
        // xtask:readiness
    ];
    statuses
        .iter()
        .filter(|(_, implemented)| !implemented)
        .map(|(name, _)| *name)
        .collect()
}
pub async fn call(
    name: &str,
    args: Value,
    identity: &Renderer,
    templates: &Templates,
    socket: Option<&Path>,
    server_uid: u32,
) -> Option<CallToolResult> {
    match name {
        "get_status" => {
            if !args.as_object().is_some_and(|a| a.is_empty()) {
                let text = templates
                    .render("invalid_arguments", &())
                    .unwrap_or_else(|_| {
                        "ERROR invalid_arguments: no effect performed. Presentation: degraded.\n"
                            .to_owned()
                    });
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.is_error = Some(true);
                return Some(result);
            }
            let reply = identity.identity(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                crate::qualification(),
            );
            let mut result =
                CallToolResult::success(vec![ContentBlock::text(reply.text().to_owned())]);
            result.is_error = Some(reply.is_error());
            Some(result)
        }
        "get_quota" => Some(quota_reply(args, templates, socket, server_uid).await),
        "get_capacity" => Some(capacity_reply(args, templates, socket, server_uid).await),
        // xtask:routes
        _ => None,
    }
}

/// Strict read scope; empty accounts selects every configured account.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuotaArgs {
    #[serde(default)]
    accounts: Vec<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Minimal decoded service result; unneeded quota history never reaches the template.
#[derive(Deserialize)]
struct Snapshot {
    schema_version: u8,
    evaluated_at: f64,
    coverage: Coverage,
    accounts: Vec<Account>,
}
#[derive(Deserialize)]
struct Coverage {
    state: String,
}
#[derive(Deserialize)]
struct Account {
    account: String,
    models: Vec<Model>,
    #[serde(default)]
    reset_credits: ResetCreditSnapshot,
    credit_balance: Option<CreditBalance>,
    #[serde(default)]
    windows: Vec<SnapshotWindow>,
}
#[derive(Deserialize)]
struct Model {
    provider: String,
    model: String,
    native_model: String,
    state: String,
    remaining_percent: Option<f64>,
    burn_percent_per_hour: Option<f64>,
    reset_at: Option<f64>,
    risk: String,
    limiting_window: Option<LimitingWindow>,
    newest_observed_at: Option<f64>,
    #[serde(default)]
    evidence: String,
}

#[derive(Deserialize)]
struct LimitingWindow {
    window: String,
}

/// Bounded and display-safe data handed to the embedded quota template.
#[derive(Serialize)]
struct QuotaView {
    evaluated_at: String,
    coverage: String,
    rows: Vec<QuotaRow>,
    reset_credits: Vec<ResetCreditsView>,
}

/// One account inventory with display-safe provider labels and identifiers.
#[derive(Serialize)]
struct ResetCreditsView {
    account: String,
    known: bool,
    available_count: String,
    paid: Option<String>,
    credits: Vec<ResetCreditView>,
}
/// Display-safe facts for one provider credit.
#[derive(Serialize)]
struct ResetCreditView {
    id: String,
    provider_kind: String,
    label: String,
    scope: String,
    resets_left: String,
    usable_now: String,
    granted_at: String,
    expires_at: String,
}
#[derive(Serialize)]
struct QuotaRow {
    account: String,
    provider: String,
    model: String,
    details: ModelDetails,
}

/// Status, explanation and display windows shared by both text views.
#[derive(Serialize)]
struct ModelDetails {
    status: String,
    reason: Option<String>,
    stale: Option<String>,
    windows: Vec<WindowRow>,
}

#[derive(Serialize)]
struct WindowRow {
    label: String,
    remaining: String,
    burn: Option<String>,
    reset: String,
    stale: Option<String>,
}

/// Names common windows and preserves the exact recoverable text of custom names.
fn window_label(window: &str) -> Result<String, &'static str> {
    let label = display::window_label(window);
    if matches!(label, "5h" | "7d" | "monthly") {
        Ok(label.to_owned())
    } else {
        quoted(label)
    }
}

/// Displays every governing counter and labels historical observations as stale.
fn window_rows(
    windows: impl IntoIterator<Item = WindowView>,
    now: f64,
) -> Result<Vec<WindowRow>, &'static str> {
    windows
        .into_iter()
        .map(|window| {
            let label = window_label(&window.window)?;
            let label = if window.model_specific {
                format!("{label} (model-specific)")
            } else {
                label
            };
            Ok(WindowRow {
                label,
                remaining: window
                    .last_remaining_percent
                    .filter(|_| !window.known)
                    .map_or_else(
                        || display::percent(window.remaining_percent),
                        |last| format!("last {last:.0}%"),
                    ),
                burn: window.burn_percent_per_hour.map(display::burn),
                reset: display::utc(window.reset_at, Some(now)),
                stale: window
                    .observed_at
                    .filter(|_| !window.known)
                    .map(|at| display::stale(Some(at), now)),
            })
        })
        .collect()
}

/// Adds the limiting counter for an older service, or for a latch without samples.
fn add_limiting_window(
    windows: &mut Vec<WindowRow>,
    limiting: Option<&str>,
    remaining: Option<f64>,
    burn: Option<f64>,
    reset: Option<f64>,
    now: f64,
) -> Result<(), &'static str> {
    if windows.is_empty()
        && let Some(limiting) = limiting
    {
        windows.push(WindowRow {
            label: window_label(limiting)?,
            remaining: display::percent(remaining),
            burn: burn.map(display::burn),
            reset: display::utc(reset, Some(now)),
            stale: None,
        });
    }
    Ok(())
}

/// Quotes labels and escapes directional controls without changing their recoverable text.
fn quoted(value: &str) -> Result<String, &'static str> {
    if value.len() > 256 {
        return Err("invalid_service_response");
    }
    let encoded = serde_json::to_string(value).map_err(|_| "invalid_service_response")?;
    let mut output = String::new();
    for character in encoded.chars() {
        if matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            output.push_str(&format!("\\u{:04x}", character as u32));
        } else {
            output.push(character);
        }
    }
    Ok(output)
}

/// Projects a bounded model page without dropping unseen rows or treating unknown as zero.
fn quota_view(value: Value, filter: Option<&str>) -> Result<QuotaView, &'static str> {
    let snapshot: Snapshot =
        serde_json::from_value(value).map_err(|_| "invalid_service_response")?;
    if snapshot.schema_version != 2
        || !snapshot.evaluated_at.is_finite()
        || !matches!(
            snapshot.coverage.state.as_str(),
            "unknown" | "partial" | "complete"
        )
    {
        return Err("invalid_service_response");
    }
    let mut rows = Vec::new();
    let mut reset_credits = Vec::new();
    let mut credit_count = 0;
    for account in snapshot.accounts {
        let account_name = quoted(&account.account)?;
        let credits = account.reset_credits;
        credits.validate().map_err(|_| "invalid_service_response")?;
        credit_count += credits.inventory.credits.len();
        if credit_count > 20 || reset_credits.len() >= 20 {
            return Err("response_too_large");
        }
        let rows_of_credits = credits
            .inventory
            .credits
            .into_iter()
            .map(|credit| {
                Ok(ResetCreditView {
                    id: quoted(&credit.id)?,
                    provider_kind: quoted(&credit.provider_kind)?,
                    label: quoted(&credit.label)?,
                    scope: quoted(&credit.scope)?,
                    resets_left: display_fact(credit.resets_left),
                    usable_now: display_fact(credit.usable_now),
                    granted_at: display::utc(credit.granted_at, None),
                    expires_at: display::utc(credit.expires_at, None),
                })
            })
            .collect::<Result<Vec<_>, &'static str>>()?;
        reset_credits.push(ResetCreditsView {
            account: account_name.clone(),
            known: credits.inventory.known,
            available_count: display_fact(credits.inventory.available_count),
            paid: account.credit_balance.as_ref().map(display::paid),
            credits: rows_of_credits,
        });
        for model in account.models {
            if filter.is_some_and(|wanted| wanted != model.model && wanted != model.native_model) {
                continue;
            }
            if !matches!(model.state.as_str(), "known" | "unknown" | "exhausted")
                || !matches!(model.risk.as_str(), "low" | "medium" | "high" | "unknown")
            {
                return Err("invalid_service_response");
            }
            let at = snapshot.evaluated_at;
            let status = match (model.state.as_str(), model.risk.as_str()) {
                ("known", "high") => "limited",
                ("known", _) => "available",
                (status, _) => status,
            };
            let limiting = model
                .limiting_window
                .as_ref()
                .map(|window| window.window.as_str());
            let label = limiting.map(window_label).transpose()?;
            let mut windows = window_rows(
                capacity::windows(&account.windows, &model.native_model, at, at),
                at,
            )?;
            add_limiting_window(
                &mut windows,
                limiting,
                model.remaining_percent,
                model.burn_percent_per_hour,
                model.reset_at,
                at,
            )?;
            rows.push(QuotaRow {
                account: account_name.clone(),
                provider: quoted(&model.provider)?,
                model: quoted(&model.model)?,
                details: ModelDetails {
                    status: status.to_owned(),
                    reason: display::reason(
                        status,
                        label.as_deref(),
                        model.remaining_percent,
                        model.burn_percent_per_hour,
                    ),
                    stale: (model.evidence == "stale")
                        .then(|| display::stale(model.newest_observed_at, at)),
                    windows,
                },
            });
            if rows.len() > 20 {
                return Err("response_too_large");
            }
        }
    }
    Ok(QuotaView {
        evaluated_at: display::utc(Some(snapshot.evaluated_at), None),
        coverage: snapshot.coverage.state,
        rows,
        reset_credits,
    })
}

/// Displays an optional fact while preserving zero and false.
fn display_fact(value: Option<impl ToString>) -> String {
    value.map_or_else(|| "unknown".into(), |value| value.to_string())
}

/// Gives the available filters even when the service cannot supply candidate examples.
pub(crate) async fn narrowing_message(
    socket: &Path,
    server_uid: u32,
    account_scope: bool,
    cli: bool,
) -> String {
    let request = crate::ipc::Request {
        version: 1,
        method: "scope_options".into(),
        ..Default::default()
    };
    let options = match crate::ipc::request(socket, &request, server_uid).await {
        Ok(crate::ipc::Reply::Result { result, .. }) => result,
        _ => Value::Null,
    };
    let candidates = |key: &str, more: &str| {
        let Some(values) = options[key].as_array() else {
            return "unavailable".into();
        };
        let parts: Vec<_> = values
            .iter()
            .take(crate::ipc::SCOPE_SAMPLE_SIZE)
            .filter_map(Value::as_str)
            .filter(|value| !value.chars().any(char::is_control))
            .filter_map(|value| quoted(value).ok())
            .collect();
        if parts.is_empty() {
            return "none".into();
        }
        let suffix = if options[more] == true || values.len() > crate::ipc::SCOPE_SAMPLE_SIZE {
            ", …"
        } else {
            ""
        };
        format!("{}{suffix}", parts.join(", "))
    };
    let (account, model) = if cli {
        ("--account", "--model")
    } else {
        ("accounts", "model")
    };
    let models = candidates("models", "more_models");
    if account_scope {
        let accounts = candidates("accounts", "more_accounts");
        format!(
            "response_too_large: narrow with {account} (candidates: {accounts}) or {model} (candidates: {models})."
        )
    } else {
        format!("response_too_large: narrow with {model} (candidates: {models}).")
    }
}

/// Reports oversized MCP results using the filters supported by the called tool.
async fn scoped_error(code: &str, socket: &Path, uid: u32, accounts: bool) -> CallToolResult {
    if code != "response_too_large" {
        return quota_error(code);
    }
    let message = narrowing_message(socket, uid, accounts, false).await;
    CallToolResult::error(vec![ContentBlock::text(format!("ERROR {message}\n"))])
}

/// Returns one fixed error without exposing socket paths or raw provider data.
fn quota_error(code: &str) -> CallToolResult {
    let text = match code {
        "invalid_arguments" => {
            "ERROR invalid_arguments: provide valid accounts and an optional model.\n"
        }
        "history_too_large" => {
            "ERROR history_too_large: the account history exceeds the service read budget; contact the operator.\n"
        }
        "unknown_account" => "ERROR unknown_account: the requested account is not configured.\n",
        "unknown_model" => {
            "ERROR unknown_model: the requested model is not configured in this scope.\n"
        }
        "invalid_scope" => "ERROR invalid_scope: the account scope is invalid.\n",
        "unsupported_method" => {
            "ERROR unsupported_method: the quota service is older than this client; capacity is unknown.\n"
        }
        "unavailable" => "ERROR unavailable: quota service is unavailable; quota is unknown.\n",
        "presentation_failed" => {
            "ERROR presentation_failed: the local read succeeded but the text could not be rendered. Narrow the account or model scope or use CLI JSON.\n"
        }
        _ => "ERROR quota_read_failed: no quota data was returned.\n",
    };
    CallToolResult::error(vec![ContentBlock::text(text)])
}

/// Reads a service snapshot once and renders only a validated bounded view.
async fn quota_reply(
    args: Value,
    templates: &Templates,
    socket: Option<&Path>,
    server_uid: u32,
) -> CallToolResult {
    let Ok(args) = serde_json::from_value::<QuotaArgs>(args) else {
        return quota_error("invalid_arguments");
    };
    if args.accounts.len() > 256
        || args.model.as_ref().is_some_and(|model| {
            model.is_empty() || model.len() > 256 || model.chars().any(char::is_control)
        })
    {
        return quota_error("invalid_arguments");
    }
    let Some(socket) = socket else {
        return quota_error("unavailable");
    };
    let request = crate::ipc::Request {
        version: 1,
        method: "snapshot".into(),
        accounts: args.accounts,
        model: args.model.clone(),
        observation: None,
        ..Default::default()
    };
    let value = match crate::ipc::request(socket, &request, server_uid).await {
        Ok(crate::ipc::Reply::Result { result, .. }) => result,
        Ok(crate::ipc::Reply::Error { error, .. }) | Err(error) => {
            return scoped_error(&error, socket, server_uid, true).await;
        }
    };
    let view = match quota_view(value, args.model.as_deref()) {
        Ok(view) => view,
        Err(code) => return scoped_error(code, socket, server_uid, true).await,
    };
    match templates.render("quota", &view) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(code) => scoped_error(code, socket, server_uid, true).await,
    }
}

/// Strict capacity scope: an optional configured or native model name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapacityArgs {
    #[serde(default)]
    model: Option<String>,
}

/// Minimal decoded capacity result; validated before rendering.
#[derive(Deserialize)]
struct Capacity {
    schema_version: u8,
    evaluated_at: f64,
    coverage: CapacityCoverage,
    providers: Vec<CapacityProvider>,
}
#[derive(Deserialize)]
struct CapacityCoverage {
    state: String,
    incomplete: bool,
}
#[derive(Deserialize)]
struct CapacityProvider {
    provider: String,
    score: Option<f64>,
    status: String,
    models: Vec<CapacityModel>,
}
#[derive(Deserialize)]
struct CapacityModel {
    model: String,
    status: String,
    evidence: String,
    newest_observed_at: Option<f64>,
    limiting_window: Option<String>,
    remaining_percent: Option<f64>,
    reset_at: Option<f64>,
    risk: String,
    burn_percent_per_hour: Option<f64>,
    #[serde(default)]
    windows: Vec<WindowView>,
    #[serde(default)]
    additional_windows: Vec<WindowView>,
    #[serde(default)]
    ranking_credits: RankingCredits,
}

/// Bounded display-safe capacity page.
#[derive(Serialize)]
struct CapacityPage {
    evaluated_at: String,
    /// Coverage state with an `(incomplete)` marker when any model is unknown.
    coverage: String,
    providers: Vec<CapacityProviderRow>,
}
#[derive(Serialize)]
struct CapacityProviderRow {
    rank: usize,
    provider: String,
    status: String,
    score: String,
    models: Vec<CapacityModelRow>,
}
#[derive(Serialize)]
struct CapacityModelRow {
    model: String,
    details: ModelDetails,
    credits: Vec<String>,
}

/// Maximum rendered model rows; a larger catalog must be narrowed by model.
const MAX_CAPACITY_ROWS: usize = 60;

/// Validates and projects a capacity result without silently dropping rows.
fn capacity_view(value: Value) -> Result<CapacityPage, &'static str> {
    let capacity: Capacity =
        serde_json::from_value(value).map_err(|_| "invalid_service_response")?;
    if capacity.schema_version != crate::capacity::SCHEMA_VERSION
        || !capacity.evaluated_at.is_finite()
        || !matches!(
            capacity.coverage.state.as_str(),
            "unknown" | "partial" | "complete"
        )
    {
        return Err("invalid_service_response");
    }
    let statuses = [
        "available",
        "limited",
        "unknown",
        "priority_overflow",
        "exhausted",
    ];
    let mut rows = 0usize;
    let mut providers = Vec::with_capacity(capacity.providers.len());
    for (index, provider) in capacity.providers.into_iter().enumerate() {
        if !statuses.contains(&provider.status.as_str()) {
            return Err("invalid_service_response");
        }
        let mut models = Vec::with_capacity(provider.models.len());
        for model in provider.models {
            if !statuses.contains(&model.status.as_str())
                || !matches!(model.evidence.as_str(), "fresh" | "stale" | "missing")
                || !matches!(model.risk.as_str(), "low" | "medium" | "high" | "unknown")
            {
                return Err("invalid_service_response");
            }
            rows += 1;
            if rows > MAX_CAPACITY_ROWS {
                return Err("response_too_large");
            }
            let at = capacity.evaluated_at;
            let limiting = model.limiting_window.as_deref();
            let label = limiting.map(window_label).transpose()?;
            let mut windows = window_rows(
                model.windows.into_iter().chain(model.additional_windows),
                at,
            )?;
            add_limiting_window(
                &mut windows,
                limiting,
                model.remaining_percent,
                model.burn_percent_per_hour,
                model.reset_at,
                at,
            )?;
            models.push(CapacityModelRow {
                model: quoted(&model.model)?,
                details: ModelDetails {
                    reason: display::reason(
                        &model.status,
                        label.as_deref(),
                        model.remaining_percent,
                        model.burn_percent_per_hour,
                    ),
                    stale: (model.evidence == "stale")
                        .then(|| display::stale(model.newest_observed_at, at)),
                    status: model.status,
                    windows,
                },
                credits: display::credit_badges(&model.ranking_credits),
            });
        }
        providers.push(CapacityProviderRow {
            rank: index + 1,
            provider: quoted(&provider.provider)?,
            status: provider.status,
            score: provider_score(provider.score),
            models,
        });
    }
    let coverage = if capacity.coverage.incomplete {
        format!("{} (incomplete)", capacity.coverage.state)
    } else {
        capacity.coverage.state
    };
    Ok(CapacityPage {
        evaluated_at: display::utc(Some(capacity.evaluated_at), None),
        coverage,
        providers,
    })
}

/// Shows an absent provider score as `none` (no known capacity), not zero.
fn provider_score(score: Option<f64>) -> String {
    score.map_or_else(|| "none".into(), |score| format!("{score:.2}"))
}

/// Reads the account-free capacity order once and renders a validated bounded page.
async fn capacity_reply(
    args: Value,
    templates: &Templates,
    socket: Option<&Path>,
    server_uid: u32,
) -> CallToolResult {
    let Ok(args) = serde_json::from_value::<CapacityArgs>(args) else {
        return quota_error("invalid_arguments");
    };
    if args.model.as_ref().is_some_and(|model| {
        model.is_empty() || model.len() > 256 || model.chars().any(char::is_control)
    }) {
        return quota_error("invalid_arguments");
    }
    let Some(socket) = socket else {
        return quota_error("unavailable");
    };
    let request = crate::ipc::Request {
        version: 1,
        method: "capacity".into(),
        accounts: Vec::new(),
        model: args.model,
        observation: None,
        ..Default::default()
    };
    let value = match crate::ipc::request(socket, &request, server_uid).await {
        Ok(crate::ipc::Reply::Result { result, .. }) => result,
        Ok(crate::ipc::Reply::Error { error, .. }) | Err(error) => {
            return scoped_error(&error, socket, server_uid, false).await;
        }
    };
    let page = match capacity_view(value) {
        Ok(page) => page,
        Err(code) => return scoped_error(code, socket, server_uid, false).await,
    };
    match templates.render("capacity", &page) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(code) => scoped_error(code, socket, server_uid, false).await,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Explicit regression assertions")]
mod tests {
    use super::*;

    /// Builds one synthetic model result without any credentials.
    fn model(name: &str) -> Value {
        json!({"provider":"codex","model":name,"native_model":name,"state":"unknown",
            "remaining_percent":null,"burn_percent_per_hour":null,"reset_at":null,"risk":"unknown"})
    }

    /// Unknown data remains unknown, and hostile labels cannot forge text lines.
    #[test]
    fn view_filters_models_and_escapes_display_controls() {
        let snapshot = json!({"schema_version":2,"evaluated_at":1.0,"coverage":{"state":"partial"},
            "accounts":[{"account":"acct","models":[model("a\u{202e}b"),model("other")]}]});
        let view = quota_view(snapshot.clone(), Some("a\u{202e}b")).unwrap();
        assert_eq!(view.rows.len(), 1);
        assert!(view.rows[0].model.contains("\\u202e"));
        assert_eq!(view.rows[0].details.status, "unknown");
        assert_eq!(view.coverage, "partial");
        let templates = Templates::new(&templates()).unwrap();
        assert!(templates.render("quota", &view).is_ok());
        for version in [1, 3] {
            let mut unsupported = snapshot.clone();
            unsupported["schema_version"] = json!(version);
            assert!(matches!(
                quota_view(unsupported, None),
                Err("invalid_service_response")
            ));
        }
    }

    /// Oversized pages refuse rather than silently omit quota rows.
    #[test]
    fn view_refuses_oversized_pages() {
        let models: Vec<_> = (0..21)
            .map(|number| model(&format!("model-{number}")))
            .collect();
        let snapshot = json!({"schema_version":2,"evaluated_at":1.0,"coverage":{"state":"complete"},
            "accounts":[{"account":"acct","models":models}]});
        assert!(matches!(
            quota_view(snapshot, None),
            Err("response_too_large")
        ));
    }

    /// Renders account-level credit details once, including explicit false and zero.
    #[test]
    fn quota_renders_reset_credits() {
        let snapshot = json!({"schema_version":2,"evaluated_at":1000.0,"coverage":{"state":"complete"},
        "accounts":[{"account":"acct","models":[model("a"), model("b")],
            "reset_credits":{"known":true,"available_count":0,"observed_at":999.0,"credits":[
                {"id":"fixture-credit","provider_kind":"GRANT","scope":"all","label":"Fixture credit",
                 "resets_left":2,"usable_now":false,"granted_at":null,"expires_at":4000.0}
            ]}}]});
        let view = quota_view(snapshot, None).unwrap();
        let templates = Templates::new(&templates()).unwrap();
        let text = templates.render("quota", &view).unwrap();
        assert!(text.contains(r#""acct" | reset credits: known | available count: 0"#));
        assert!(text.contains(r#""fixture-credit" | "GRANT" | "Fixture credit" | "all" | resets left: 2 | usable now: false | granted: unknown | expires: 01.01 01:06 UTC"#));
        assert!(!text.contains("observed:"));
        assert_eq!(text.matches("fixture-credit").count(), 1);
    }

    /// Both tools show every window, precise UTC resets and readable risk and credit evidence.
    #[test]
    fn quota_and_capacity_render_readable_multi_window_snapshots() {
        let at = 1795798740.0;
        let fast = json!({"window":"five_hour","lane":"codex","source":"fixture","known":true,
            "models":["gpt"],"observed_at":at,"valid_until":at+3600.0,
            "remaining_percent":64.4,"burn_percent_per_hour":6.545274455184143,"reset_at":1795803060.0,
            "last_reset_at":1795803060.0,"last_remaining_percent":64.4,
            "forecast":{"known":true,"remaining_percent":64.4,"burn_percent_per_hour":6.545274455184143}});
        let slow = json!({"window":"seven_day","lane":"codex","source":"fixture","known":true,
            "models":["gpt"],"observed_at":at,"valid_until":at+3600.0,
            "remaining_percent":63.0,"burn_percent_per_hour":20.0,"reset_at":1796331540.0,
            "last_reset_at":1796331540.0,"last_remaining_percent":63.0,
            "forecast":{"known":true,"remaining_percent":63.0,"burn_percent_per_hour":20.0}});
        let credit = json!({"has_credits":true,"unlimited":false,"balance":"42.50"});
        let snapshot = json!({"schema_version":2,"evaluated_at":at,"coverage":{"state":"complete"},
        "accounts":[{"account":"personal","credit_balance":credit,"windows":[fast, slow],"models":[
            {"provider":"codex","model":"gpt","native_model":"gpt","state":"known","risk":"high",
             "limiting_window":{"window":"seven_day"},"remaining_percent":63.0,"burn_percent_per_hour":20.0,
             "reset_at":1796331540.0}
        ]}]});
        let capacity = json!({"schema_version":1,"evaluated_at":at,"coverage":{"state":"complete","incomplete":false},
        "providers":[{"provider":"codex","status":"limited","score":0.5341,"models":[
            {"model":"gpt","status":"limited","risk":"high","evidence":"fresh",
             "limiting_window":"seven_day","remaining_percent":63.0,"burn_percent_per_hour":20.0,
             "reset_at":1796331540.0,"windows":[fast, slow],"ranking_credits":{"reset_uses":1,"paid":credit}}
        ]}]});
        let renderer = Templates::new(&templates()).unwrap();
        let quota = renderer
            .render("quota", &quota_view(snapshot, None).unwrap())
            .unwrap();
        assert_eq!(
            quota,
            concat!(
                "Quota at 27.11 16:59 UTC | coverage: complete\n",
                "\"personal\" | \"codex\" | \"gpt\" | limited: 7d window 63% left, burn 20.0%/h → runs out before reset\n",
                "  5h | 64% | burn 6.5%/h | reset 27.11 18:11 UTC (in 1h)\n",
                "  7d | 63% | burn 20.0%/h | reset 03.12 20:59 UTC (in 6d 4h)\n",
                "\"personal\" | reset credits: unknown | available count: unknown | paid credits: $42.50\n",
            )
        );
        let order = renderer
            .render("capacity", &capacity_view(capacity.clone()).unwrap())
            .unwrap();
        assert_eq!(
            order,
            concat!(
                "Capacity order at 27.11 16:59 UTC | coverage: complete\n",
                "1. \"codex\" | status: limited | score: 0.53\n",
                "   \"gpt\" | limited: 7d window 63% left, burn 20.0%/h → runs out before reset | +reset credit | +credits $42.50\n",
                "     5h | 64% | burn 6.5%/h | reset 27.11 18:11 UTC (in 1h)\n",
                "     7d | 63% | burn 20.0%/h | reset 03.12 20:59 UTC (in 6d 4h)\n",
            )
        );
        let mut old = capacity;
        let model = &mut old["providers"][0]["models"][0];
        model["status"] = json!("unknown");
        model["evidence"] = json!("stale");
        model["newest_observed_at"] = json!(at - 7200.0);
        let window = &mut model["windows"][0];
        window["known"] = json!(false);
        window["remaining_percent"] = Value::Null;
        window["burn_percent_per_hour"] = Value::Null;
        window["observed_at"] = json!(at - 7200.0);
        let stale = renderer
            .render("capacity", &capacity_view(old).unwrap())
            .unwrap();
        assert!(stale.contains("\"gpt\" | unknown | stale: 2h old"));
        assert!(stale.contains("5h | last 64% | reset 27.11 18:11 UTC (in 1h) | stale: 2h old"));
    }

    /// Capacity pages escape labels, keep unknown explicit and refuse invalid or oversized results.
    #[test]
    fn capacity_view_validates_and_bounds_rows() {
        let model = |name: &str, status: &str| {
            json!({"model":name,"status":status,"evidence":"missing","newest_observed_at":null,
                "limiting_window":null,"remaining_percent":null,"reset_at":null,"risk":"unknown"})
        };
        let capacity = json!({"schema_version":1,"evaluated_at":5.0,
            "coverage":{"state":"partial","incomplete":true},
            "providers":[{"provider":"p\u{202e}x","priority_multiplier":1.0,"score":null,
                "status":"unknown","models":[model("m","unknown")]}]});
        let page = capacity_view(capacity.clone()).unwrap();
        assert!(page.providers[0].provider.contains("\\u202e"));
        assert_eq!(page.providers[0].score, "none");
        assert_eq!(page.providers[0].models[0].details.status, "unknown");
        let templates = Templates::new(&templates()).unwrap();
        let text = templates.render("capacity", &page).unwrap();
        assert!(
            text.contains("coverage: partial (incomplete)\n1. "),
            "{text}"
        );
        assert!(!text.contains('\u{202e}'));
        let mut overflow = capacity.clone();
        overflow["providers"][0]["status"] = json!("priority_overflow");
        overflow["providers"][0]["models"][0]["status"] = json!("priority_overflow");
        let page = capacity_view(overflow).unwrap();
        let text = templates.render("capacity", &page).unwrap();
        assert_eq!(text.matches("priority_overflow").count(), 2);
        let mut invalid = capacity.clone();
        invalid["providers"][0]["models"][0]["status"] = json!("unsupported");
        assert!(matches!(
            capacity_view(invalid),
            Err("invalid_service_response")
        ));
        let mut future = capacity.clone();
        future["schema_version"] = json!(2);
        assert!(matches!(
            capacity_view(future),
            Err("invalid_service_response")
        ));
        let rows: Vec<_> = (0..=MAX_CAPACITY_ROWS)
            .map(|n| model(&format!("m{n}"), "unknown"))
            .collect();
        let mut large = capacity;
        large["providers"][0]["models"] = json!(rows);
        assert!(matches!(capacity_view(large), Err("response_too_large")));
    }
}
