//! Authoritative Rust tool registry. JSON discovery is an exported snapshot, not a second source.
use crate::response::Templates;
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
        json!({"name":"get_quota", "description":"Read stored quota metrics from the local service. Does not contact providers or start models. Narrow account or model scope if the result is too large.",
        "inputSchema":{"type":"object","properties":{"accounts":{"type":"array","items":{"type":"string"},"maxItems":256},"model":{"type":["string","null"],"maxLength":256}},"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}),
        json!({"name":"get_capacity", "description":"Rank configured providers and their models by stored quota capacity (highest first), applying Quota's provider and account multipliers. Shows each model's status (available, limited, unknown, priority_overflow, exhausted), remaining percentage, limiting window, reset and evidence freshness. Account-free: no account or pool identities. Read-only; does not contact providers or start models.",
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
            "Quota at {{ evaluated_at }} | coverage: {{ coverage }}\n{% for row in rows %}{{ row.account }} | {{ row.provider }} | {{ row.model }} | {{ row.state }} | remaining: {{ row.remaining }} | burn/hour: {{ row.burn }} | reset: {{ row.reset }} | risk: {{ row.risk }}\n{% else %}No configured models in scope.\n{% endfor %}",
        ),
        (
            "capacity",
            "Capacity order at {{ evaluated_at }} | coverage: {{ coverage }}\n{% for provider in providers %}{{ provider.rank }}. {{ provider.provider }} | status: {{ provider.status }} | score: {{ provider.score }} | multiplier: {{ provider.multiplier }}\n{% for model in provider.models %}   {{ model.model }} | {{ model.status }} | remaining: {{ model.remaining }}{% if model.window %} ({{ model.window }}){% endif %} | reset: {{ model.reset }} | risk: {{ model.risk }} | evidence: {{ model.evidence }} | observed: {{ model.observed }}\n{% endfor %}{% else %}No configured providers in scope.\n{% endfor %}",
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
}

/// Bounded and display-safe data handed to the embedded quota template.
#[derive(Serialize)]
struct QuotaView {
    evaluated_at: f64,
    coverage: String,
    rows: Vec<QuotaRow>,
}
#[derive(Serialize)]
struct QuotaRow {
    account: String,
    provider: String,
    model: String,
    state: String,
    remaining: String,
    burn: String,
    reset: String,
    risk: String,
}

/// Quotes labels and escapes directional controls without changing their recoverable text.
fn quoted(value: &str) -> Result<String, &'static str> {
    if value.len() > 256 {
        return Err("invalid_service_response");
    }
    let encoded = serde_json::to_string(value).map_err(|_| "invalid_service_response")?;
    let mut output = String::new();
    for character in encoded.chars() {
        if matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
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
    for account in snapshot.accounts {
        let account_name = quoted(&account.account)?;
        for model in account.models {
            if filter.is_some_and(|wanted| wanted != model.model && wanted != model.native_model) {
                continue;
            }
            if !matches!(model.state.as_str(), "known" | "unknown" | "exhausted")
                || !matches!(model.risk.as_str(), "low" | "medium" | "high" | "unknown")
            {
                return Err("invalid_service_response");
            }
            rows.push(QuotaRow {
                account: account_name.clone(),
                provider: quoted(&model.provider)?,
                model: quoted(&model.model)?,
                state: model.state,
                remaining: model
                    .remaining_percent
                    .map_or_else(|| "unknown".into(), |value| format!("{value}%")),
                burn: model
                    .burn_percent_per_hour
                    .map_or_else(|| "unknown".into(), |value| format!("{value}%")),
                reset: model
                    .reset_at
                    .map_or_else(|| "unknown".into(), |value| value.to_string()),
                risk: model.risk,
            });
            if rows.len() > 20 {
                return Err("response_too_large");
            }
        }
    }
    Ok(QuotaView {
        evaluated_at: snapshot.evaluated_at,
        coverage: snapshot.coverage.state,
        rows,
    })
}

/// Returns one fixed error without exposing socket paths or raw provider data.
fn quota_error(code: &str) -> CallToolResult {
    let text = match code {
        "invalid_arguments" => {
            "ERROR invalid_arguments: provide valid accounts and an optional model.\n"
        }
        "response_too_large" => "ERROR response_too_large: narrow the account or model scope.\n",
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
    };
    let value = match crate::ipc::request(socket, &request, server_uid).await {
        Ok(crate::ipc::Reply::Result { result, .. }) => result,
        Ok(crate::ipc::Reply::Error { error, .. }) | Err(error) => return quota_error(&error),
    };
    let view = match quota_view(value, args.model.as_deref()) {
        Ok(view) => view,
        Err(code) => return quota_error(code),
    };
    match templates.render("quota", &view) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(_) => quota_error("presentation_failed"),
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
    priority_multiplier: f64,
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
}

/// Bounded display-safe capacity page.
#[derive(Serialize)]
struct CapacityPage {
    evaluated_at: f64,
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
    multiplier: f64,
    models: Vec<CapacityModelRow>,
}
#[derive(Serialize)]
struct CapacityModelRow {
    model: String,
    status: String,
    remaining: String,
    window: Option<String>,
    reset: String,
    risk: String,
    evidence: String,
    observed: String,
}

/// Maximum rendered model rows; a larger catalog must be narrowed by model.
const MAX_CAPACITY_ROWS: usize = 60;

/// Formats an optional number, keeping absent values explicitly unknown.
fn unknown_or(value: Option<f64>, suffix: &str) -> String {
    value.map_or_else(|| "unknown".into(), |value| format!("{value}{suffix}"))
}

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
            models.push(CapacityModelRow {
                model: quoted(&model.model)?,
                status: model.status,
                remaining: unknown_or(model.remaining_percent, "%"),
                window: model.limiting_window.as_deref().map(quoted).transpose()?,
                reset: unknown_or(model.reset_at, ""),
                risk: model.risk,
                evidence: model.evidence,
                observed: unknown_or(model.newest_observed_at, ""),
            });
        }
        providers.push(CapacityProviderRow {
            rank: index + 1,
            provider: quoted(&provider.provider)?,
            status: provider.status,
            score: provider_score(provider.score),
            multiplier: provider.priority_multiplier,
            models,
        });
    }
    let coverage = if capacity.coverage.incomplete {
        format!("{} (incomplete)", capacity.coverage.state)
    } else {
        capacity.coverage.state
    };
    Ok(CapacityPage {
        evaluated_at: capacity.evaluated_at,
        coverage,
        providers,
    })
}

/// Shows an absent provider score as `none` (no known capacity), not zero.
fn provider_score(score: Option<f64>) -> String {
    score.map_or_else(|| "none".into(), |score| format!("{score:.4}"))
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
    };
    let value = match crate::ipc::request(socket, &request, server_uid).await {
        Ok(crate::ipc::Reply::Result { result, .. }) => result,
        Ok(crate::ipc::Reply::Error { error, .. }) | Err(error) => return quota_error(&error),
    };
    let page = match capacity_view(value) {
        Ok(page) => page,
        Err(code) => return quota_error(code),
    };
    match templates.render("capacity", &page) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(_) => quota_error("presentation_failed"),
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
        assert_eq!(view.rows[0].remaining, "unknown");
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
        assert_eq!(page.providers[0].models[0].remaining, "unknown");
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
