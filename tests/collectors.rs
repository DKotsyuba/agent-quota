//! Synthetic local HTTP checks for provider collectors exported from the binary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Explicit fixture assertions"
)]

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

/// Resolves an explicitly supplied shipped binary or the Cargo-built product.
fn binary() -> PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_agent-quota")))
}

/// Exports the collector files from the exact executable under test.
fn exported_assets() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(binary())
        .arg("export-assets")
        .arg("--directory")
        .arg(root.path().join("assets"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", root.path())
        .env("TMP", root.path())
        .env("TEMP", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    root
}

const OPENCODE_GO_OK: &str = r#"{"usage":{"rolling":{"status":"ok","percent":0,"resetsAt":"2026-11-14T02:18:33.000Z"},"weekly":{"status":"ok","percent":12.5,"resetsAt":"2026-11-16T00:00:00.000Z"},"monthly":{"status":"ok","percent":99,"resetsAt":"2026-12-07T21:18:54.000Z"}}}"#;

/// Serves exactly one local 200 fixture response and returns the captured HTTP request.
fn endpoint(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    endpoint_with_status("200 OK", body)
}

/// Serves exactly one local fixture response with the given status line.
fn endpoint_with_status(
    status: &'static str,
    body: &'static str,
) -> (String, std::thread::JoinHandle<String>) {
    endpoints(vec![(status, body)])
}

/// Serves sequential fixture responses from one origin and records their requests.
fn endpoints(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}/quota", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        responses.into_iter().map(|(status, body)| {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            assert!(
                Instant::now() < deadline,
                "collector did not reach local fixture"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 1024];
        while !bytes.windows(4).any(|value| value == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0 && bytes.len() < 65_536);
            bytes.extend_from_slice(&chunk[..count]);
        }
        write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        String::from_utf8(bytes).unwrap()
    }).collect::<Vec<_>>().join("\n")
    });
    (address, handle)
}

/// Runs the actual GLM and Claude scripts against loopback-only fixtures.
#[tokio::test]
async fn http_collectors_preserve_auth_and_normalized_windows() {
    let assets = exported_assets();
    for (name, body, authorization, expected_windows) in [
        (
            "glm",
            r#"{"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":100,"currentValue":25,"percentage":25},{"type":"CREDIT_LIMIT","unit":6,"number":1,"percentage":12},{"type":"TIME_LIMIT","percentage":3}]}}"#,
            "authorization: synthetic-fixture-token",
            2,
        ),
        (
            "claude",
            r#"{"limits":[{"kind":"session","percent":25,"is_active":false},{"kind":"weekly_all","percent":12},{"kind":"weekly_scoped","percent":75,"scope":{"model":{"display_name":"Sonnet","id":null}},"is_active":true}]}"#,
            "authorization: bearer synthetic-fixture-token",
            3,
        ),
        (
            "opencode-go",
            OPENCODE_GO_OK,
            "authorization: bearer synthetic-fixture-token",
            3,
        ),
    ] {
        let (url, server) = if name == "glm" {
            endpoints(vec![("200 OK", body), ("503 Unavailable", "{}")])
        } else {
            endpoint(body)
        };
        let script = assets.path().join(format!("assets/collectors/{name}.sh"));
        let mut child = tokio::process::Command::new("/bin/bash")
            .arg(script)
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let context = json!({"version":1,"auth":{"kind":"token","token":"synthetic-fixture-token"},
            "models":{"sonnet":{"id":"sonnet","native_model":"sonnet"}},"now":1000});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(context.to_string().as_bytes())
            .await
            .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success(), "{name} collector failed");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["version"], 1);
        assert_eq!(
            result["windows"].as_array().unwrap().len(),
            expected_windows
        );
        assert!(!result.to_string().contains("synthetic-fixture-token"));
        let requests = server.join().unwrap().to_ascii_lowercase();
        assert!(requests.contains(authorization));
        if name == "glm" {
            assert!(
                requests.contains("get /api/biz/customer-package-reset/list?targettype=personal")
            );
            assert!(requests.contains("authorization: bearer synthetic-fixture-token"));
            assert_eq!(result["reset_credits"]["known"], false);
        } else if name == "claude" {
            assert!(requests.contains("get /quota?cedar_ember=1 "));
        }
    }
}

/// Contradictory provider data fails before becoming quota evidence.
#[tokio::test]
async fn supplied_parsers_reject_invalid_provider_data() {
    let assets = exported_assets();
    for (name, body) in [
        (
            "glm",
            r#"{"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":100,"currentValue":80,"percentage":25}]}}"#,
        ),
        ("claude", r#"{"limits":[{"kind":"session","percent":101}]}"#),
    ] {
        let directory = assets.path().join("assets/collectors");
        let mut child = tokio::process::Command::new("jq")
            .args(["-e", "-L"])
            .arg(&directory)
            .args([
                "--argjson",
                "ctx",
                r#"{"models":{"sonnet":{}},"now":1000}"#,
                "-f",
            ])
            .arg(directory.join(format!("{name}.jq")))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success(), "{name} accepted invalid provider data");
    }
}

/// Runs the exported OpenCode Go filter over one body; returns success and stdout.
async fn opencode_go_filter(body: &str) -> (bool, Value) {
    let assets = exported_assets();
    let directory = assets.path().join("assets/collectors");
    collector_filter(&directory, "opencode-go", body).await
}

/// Runs a shipped jq parser on synthetic input, including GLM's two-document input.
async fn collector_filter(directory: &std::path::Path, name: &str, body: &str) -> (bool, Value) {
    let mut child = tokio::process::Command::new("jq")
        .args(["-c", "-e", "-L"])
        .arg(directory)
        .args(if name == "glm-credits" {
            &["-s"][..]
        } else {
            &[]
        })
        .args([
            "--argjson",
            "ctx",
            r#"{"models":{"deepseek":{},"muse":{}},"now":1000}"#,
            "-f",
        ])
        .arg(directory.join(format!("{name}.jq")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.success(), value)
}

/// Optional inventories preserve quota windows across absent, malformed and empty data.
#[tokio::test]
async fn reset_credit_fixtures_preserve_known_zero_and_provider_fields() {
    let assets = exported_assets();
    let directory = assets.path().join("assets/collectors");
    for (name, base, key, present, empty) in [
        (
            "claude",
            json!({"limits":[{"kind":"session","percent":25}]}),
            "cedar_ember",
            json!({"grants":[{"id":"fixture-credit","label":"Fixture credit","resets_left":2,
                "usable_now":true,"clears":["session","weekly_all"],"starts_at":"1970-01-01T00:10:00Z",
                "ends_at":"1970-01-01T01:00:00Z"}]}),
            json!({"grants":[]}),
        ),
        (
            "codex",
            json!({"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}}}),
            "rateLimitResetCredits",
            json!({"availableCount":2,"credits":[{"id":"fixture-credit","resetType":"FIVE_HOUR","status":"available",
                "title":"Fixture credit","grantedAt":600,"expiresAt":3600}]}),
            json!({"credits":[]}),
        ),
        (
            "glm-credits",
            json!({"version":1,"windows":[{"window":"five_hour"}]}),
            "data",
            json!({"fiveHourResets":[{"recordId":"fixture-credit","grantType":"PACKAGE","available":true,
                "packageName":"Fixture credit","expireTime":"1970-01-01 01:00:00"}],"weekResets":[]}),
            json!({"fiveHourResets":[],"weekResets":[]}),
        ),
    ] {
        for (payload, count) in [
            (None, None),
            (Some(json!("invalid")), None),
            (Some(empty), Some(0)),
            (
                Some(present),
                Some(if name == "glm-credits" { 1 } else { 2 }),
            ),
        ] {
            let mut document = if name == "glm-credits" {
                json!({"success":true,"code":0})
            } else {
                base.clone()
            };
            if let Some(payload) = payload {
                document[key] = payload;
            }
            let input = if name == "glm-credits" {
                format!("{base}\n{document}")
            } else {
                document.to_string()
            };
            let (ok, result) = collector_filter(&directory, name, &input).await;
            assert!(ok, "{name}: {input}");
            assert_eq!(result["windows"].as_array().unwrap().len(), 1);
            let credits = &result["reset_credits"];
            assert_eq!(credits["known"], count.is_some(), "{name}: {input}");
            assert_eq!(credits["available_count"], json!(count), "{name}: {input}");
            if count.is_some_and(|value| value > 0) {
                let credit = &credits["credits"][0];
                assert_eq!(credit["id"], "fixture-credit");
                assert_eq!(credit["label"], "Fixture credit");
                assert_eq!(credit["expires_at"], 3600);
                assert_eq!(credit["usable_now"], true);
                assert_eq!(
                    credit["scope"],
                    if name == "claude" { "all" } else { "five_hour" }
                );
            } else {
                assert_eq!(credits["credits"], json!([]));
            }
        }
    }
    let (ok, result) = collector_filter(&directory, "codex", r#"{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}},"rate_limit_reset_credits":{"available_count":3}}"#).await;
    assert!(ok);
    assert_eq!(
        result["reset_credits"],
        json!({"known":true,"available_count":3,"credits":[]})
    );
}

/// Covers paid balances independently of banked resets and quota windows.
#[tokio::test]
async fn codex_paid_credit_balance_fixtures() {
    let assets = exported_assets();
    let directory = assets.path().join("assets/collectors");
    for (credits, expected) in [
        (None, Value::Null),
        (Some(Value::Null), Value::Null),
        (Some(json!("invalid")), Value::Null),
        (
            Some(json!({"hasCredits":"true","unlimited":false,"balance":"1"})),
            Value::Null,
        ),
        (
            Some(json!({"hasCredits":true,"unlimited":false,"balance":"9007199254740993.012300"})),
            json!({"has_credits":true,"unlimited":false,"balance":"9007199254740993.012300"}),
        ),
        (
            Some(json!({"hasCredits":false,"unlimited":false,"balance":"0"})),
            json!({"has_credits":false,"unlimited":false,"balance":"0"}),
        ),
        (
            Some(json!({"hasCredits":true,"unlimited":true,"balance":null})),
            json!({"has_credits":true,"unlimited":true,"balance":null}),
        ),
        (
            Some(json!({"hasCredits":true,"unlimited":false,"balance":10})),
            json!({"has_credits":true,"unlimited":false,"balance":null}),
        ),
        (
            Some(json!({"hasCredits":true,"unlimited":false,"balance":"12\n"})),
            json!({"has_credits":true,"unlimited":false,"balance":null}),
        ),
    ] {
        let bucket = json!({"primary":{"usedPercent":25,"windowDurationMins":300}});
        let mut input = json!({"result":{"rateLimitsByLimitId":{"codex":bucket,"dedicated":{
            "credits":{"hasCredits":true,"unlimited":false,"balance":"999"}
        }}}});
        if let Some(credits) = credits {
            input["result"]["rateLimitsByLimitId"]["codex"]["credits"] = credits;
        }
        let (ok, result) = collector_filter(&directory, "codex", &input.to_string()).await;
        assert!(ok, "{input}");
        assert_eq!(result["windows"].as_array().unwrap().len(), 1);
        assert_eq!(result["credit_balance"], expected, "{input}");
    }
    let primary = json!({"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300},
        "credits":{"hasCredits":true,"unlimited":false,"balance":"42.50"}}});
    let (ok, result) = collector_filter(&directory, "codex", &primary.to_string()).await;
    assert!(ok);
    assert_eq!(result["credit_balance"]["balance"], "42.50");
}

/// Builds an OpenCode Go usage body from per-window (status, percent) JSON fragments.
fn opencode_go_body(rolling: &str, weekly: &str, monthly: &str) -> String {
    format!(r#"{{"usage":{{"rolling":{rolling},"weekly":{weekly},"monthly":{monthly}}}}}"#)
}

/// `percent` is percent used; windows map to five_hour, seven_day and monthly for every model.
#[tokio::test]
async fn opencode_go_reports_remaining_for_every_model() {
    let (ok, result) = opencode_go_filter(OPENCODE_GO_OK).await;
    assert!(ok);
    let windows = result["windows"].as_array().unwrap();
    let names: Vec<_> = windows
        .iter()
        .map(|w| w["window"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["five_hour", "seven_day", "monthly"]);
    let remaining: Vec<_> = windows
        .iter()
        .map(|w| w["remaining_percent"].as_f64().unwrap())
        .collect();
    assert_eq!(remaining, [100.0, 87.5, 1.0]);
    assert_eq!(windows[0]["reset_at"], 1_794_622_713);
    for window in windows {
        assert_eq!(window["pool"], "primary");
        assert_eq!(window["models"], json!(["deepseek", "muse"]));
        assert_eq!(window["observed_at"], 1000);
    }
}

/// A non-ok status is never read as zero use: exhausted is 0 remaining, unknown is null.
#[tokio::test]
async fn opencode_go_status_is_conservative() {
    let reset = r#""resetsAt":"2026-11-14T02:18:33.000Z""#;
    let body = opencode_go_body(
        &format!(r#"{{"status":"exhausted","percent":100,{reset}}}"#),
        &format!(r#"{{"status":"limited",{reset}}}"#),
        &format!(r#"{{"status":"surprise","percent":0,{reset}}}"#),
    );
    let (ok, result) = opencode_go_filter(&body).await;
    assert!(ok);
    let windows = result["windows"].as_array().unwrap();
    assert_eq!(windows[0]["remaining_percent"], 0);
    assert_eq!(windows[1]["remaining_percent"], 0);
    assert!(windows[2]["remaining_percent"].is_null());
}

/// Schema drift fails the whole observation instead of manufacturing quota evidence.
#[tokio::test]
async fn opencode_go_rejects_schema_drift() {
    let good = r#"{"status":"ok","percent":10,"resetsAt":"2026-11-14T02:18:33.000Z"}"#;
    let mut bodies = vec![
        // percent above 100, below 0, or not a number
        opencode_go_body(
            r#"{"status":"ok","percent":100.5,"resetsAt":"2026-11-14T02:18:33Z"}"#,
            good,
            good,
        ),
        opencode_go_body(
            r#"{"status":"ok","percent":-1,"resetsAt":"2026-11-14T02:18:33Z"}"#,
            good,
            good,
        ),
        opencode_go_body(
            r#"{"status":"ok","percent":"10","resetsAt":"2026-11-14T02:18:33Z"}"#,
            good,
            good,
        ),
        // missing percent, status, reset or reset of the wrong shape
        opencode_go_body(
            r#"{"status":"ok","resetsAt":"2026-11-14T02:18:33Z"}"#,
            good,
            good,
        ),
        opencode_go_body(
            r#"{"percent":10,"resetsAt":"2026-11-14T02:18:33Z"}"#,
            good,
            good,
        ),
        opencode_go_body(r#"{"status":"ok","percent":10}"#, good, good),
        opencode_go_body(
            r#"{"status":"ok","percent":10,"resetsAt":1794622713}"#,
            good,
            good,
        ),
        opencode_go_body(
            r#"{"status":"ok","percent":10,"resetsAt":"tomorrow"}"#,
            good,
            good,
        ),
        // a window that is not an object
        opencode_go_body("null", good, good),
        // missing, extra or absent usage windows
        format!(r#"{{"usage":{{"rolling":{good},"weekly":{good}}}}}"#),
        format!(
            r#"{{"usage":{{"rolling":{good},"weekly":{good},"monthly":{good},"daily":{good}}}}}"#
        ),
        r#"{"usage":{}}"#.to_owned(),
        r#"{"usage":null}"#.to_owned(),
        r#"{"error":"nope"}"#.to_owned(),
        "[]".to_owned(),
    ];
    bodies.push(good.to_owned());
    for body in bodies {
        let (ok, result) = opencode_go_filter(&body).await;
        assert!(!ok, "accepted {body}");
        assert!(result.is_null(), "emitted output for {body}");
    }
}

/// An unauthorized response fails the script without output or a token in stderr.
#[tokio::test]
async fn opencode_go_fails_closed_on_unauthorized() {
    let assets = exported_assets();
    let (url, server) = endpoint_with_status("401 Unauthorized", r#"{"error":"unauthorized"}"#);
    let script = assets.path().join("assets/collectors/opencode-go.sh");
    let mut child = tokio::process::Command::new("/bin/bash")
        .arg(script)
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let context = json!({"version":1,"auth":{"kind":"token","token":"synthetic-fixture-token"},
        "models":{"sonnet":{"id":"sonnet","native_model":"sonnet"}},"now":1000});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(context.to_string().as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-fixture-token"));
    server.join().unwrap();
}

/// Live-shaped Claude grants map extra clear targets and reject an ineligible surface.
#[tokio::test]
async fn claude_reset_grants_follow_live_shape() {
    let assets = exported_assets();
    let directory = assets.path().join("assets/collectors");
    let grant = |clears: Value| {
        json!({"id":"full-reset","label":"One full limit reset","type":"full","clears":clears,
            "resets_left":1,"resets_total":1,"usable_now":true,"unavailable_reason":null,
            "starts_at":"1970-01-01T00:10:00+00:00","ends_at":"1970-01-01T01:00:00+00:00"})
    };
    let body = json!({"limits":[{"kind":"session","percent":25}],"cedar_ember":{"eligible":true,
        "grants":[grant(json!(["five_hour","seven_day","seven_day_overage_included"])),
                  grant(json!(["five_hour","seven_day_overage_included"]))]}});
    let (ok, result) = collector_filter(&directory, "claude", &body.to_string()).await;
    assert!(ok);
    let credits = &result["reset_credits"];
    assert_eq!(credits["known"], true);
    assert_eq!(credits["available_count"], 2);
    assert_eq!(credits["credits"][0]["scope"], "all");
    assert_eq!(credits["credits"][0]["provider_kind"], "full");
    assert_eq!(credits["credits"][1]["scope"], "five_hour");

    let ineligible = json!({"limits":[{"kind":"session","percent":25}],
        "cedar_ember":{"eligible":false,"ineligible_reason":"surface","grants":[]}});
    let (ok, result) = collector_filter(&directory, "claude", &ineligible.to_string()).await;
    assert!(ok);
    assert_eq!(result["reset_credits"]["known"], false);
    assert_eq!(result["windows"].as_array().unwrap().len(), 1);
}

/// Codex full resets refill both the five-hour and weekly windows.
#[tokio::test]
async fn codex_rate_limit_credits_cover_all_windows() {
    let assets = exported_assets();
    let directory = assets.path().join("assets/collectors");
    let body = r#"{"rateLimits":{"primary":{"usedPercent":25,"windowDurationMins":300}},"rateLimitResetCredits":{"availableCount":1,"credits":[{"id":"c1","resetType":"codexRateLimits","status":"available","title":"Full reset","grantedAt":600,"expiresAt":3600}]}}"#;
    let (ok, result) = collector_filter(&directory, "codex", body).await;
    assert!(ok);
    assert_eq!(result["reset_credits"]["credits"][0]["scope"], "all");
}
