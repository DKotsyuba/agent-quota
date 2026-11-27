//! Black-box SDK lifecycle and raw JSON-RPC compatibility tests against a spawned binary.
//! This is SDK-pair compatibility, not certification of all external agent hosts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Explicit test failures"
)]
use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
/// Resolve an explicitly supplied shipped binary or the Cargo-built product.
fn binary() -> PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_agent-quota")))
}
/// Exercise discovery, valid and invalid calls through the SDK within 20 seconds.
#[tokio::test]
async fn protocol() {
    let root = tempfile::tempdir().unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut command = tokio::process::Command::new(binary());
        command
            .arg("mcp")
            .env_clear()
            .env("HOME", root.path())
            .current_dir(root.path())
            .kill_on_drop(true)
            .stderr(Stdio::null());
        let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
        let listed = client.list_tools(Default::default()).await.unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/tools.json")).unwrap();
        assert_eq!(serde_json::to_value(&listed.tools).unwrap(), expected);
        let result = client
            .call_tool(CallToolRequestParams::new("get_status"))
            .await
            .unwrap();
        let wire = serde_json::to_value(result).unwrap();
        assert_eq!(wire["isError"], false);
        assert_eq!(wire["content"].as_array().unwrap().len(), 1);
        let args = serde_json::json!({"unexpected":true})
            .as_object()
            .unwrap()
            .clone();
        let invalid = client
            .call_tool(CallToolRequestParams::new("get_status").with_arguments(args))
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(invalid).unwrap()["isError"], true);
        assert!(
            client
                .call_tool(CallToolRequestParams::new("missing_tool"))
                .await
                .is_err()
        );
        client.cancel().await.unwrap();
    })
    .await
    .expect("protocol deadline");
}

/// Exchange raw JSON-RPC in an isolated child. Notifications receive no reply.
/// A 20-second deadline covers reads and EOF shutdown; cancellation kills the child.
async fn raw_exchange(requests: &[serde_json::Value]) -> Vec<serde_json::Value> {
    raw_exchange_with_socket(requests, None).await.0
}

/// Exchanges requests while collecting unsolicited tool-list invalidations.
async fn raw_exchange_with_socket(
    requests: &[serde_json::Value],
    socket: Option<&std::path::Path>,
) -> (Vec<serde_json::Value>, usize) {
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(binary())
        .arg("mcp")
        .env_clear()
        .env("HOME", root.path())
        .env(
            "AGENT_QUOTA_SOCKET",
            socket.unwrap_or(&root.path().join("absent.sock")),
        )
        .current_dir(root.path())
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut replies = Vec::new();
        let mut invalidations = 0;
        for request in requests {
            input
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            input.flush().await.unwrap();
            if request.get("id").is_some() {
                let reply = loop {
                    let line = output
                        .next_line()
                        .await
                        .unwrap()
                        .expect("JSON-RPC response before EOF");
                    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if reply["method"] == "notifications/tools/list_changed" {
                        invalidations += 1;
                    } else {
                        break reply;
                    }
                };
                assert_eq!(reply["id"], request["id"]);
                assert!(reply.get("error").is_none(), "{reply}");
                replies.push(reply);
            }
        }
        drop(input);
        assert!(child.wait().await.unwrap().success());
        assert!(output.next_line().await.unwrap().is_none());
        (replies, invalidations)
    })
    .await
    .expect("raw protocol deadline")
}

/// Build a modern request with its version and empty client capabilities.
fn modern_request(method: &str) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    }}})
}

/// Require a nonempty live catalog identical to the reviewed snapshot.
fn assert_catalog(result: &serde_json::Value) {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../schemas/tools.json")).unwrap();
    assert!(!result["tools"].as_array().unwrap().is_empty());
    assert_eq!(result["tools"], expected);
}

/// Modern tools/list has no handshake and supplies exact private cache hints.
#[tokio::test]
async fn modern_tools_list() {
    let replies = raw_exchange(&[modern_request("tools/list")]).await;
    let result = &replies[0]["result"];
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"].as_u64(), Some(60_000));
    assert_eq!(result["cacheScope"], "private");
    assert_catalog(result);
}

/// Legacy initialize/initialized sessions omit all modern-only result fields.
#[tokio::test]
async fn legacy_tools_list() {
    let replies = raw_exchange(&[
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"raw-test","version":"1"}
        }}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ])
    .await;
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    let result = &replies[1]["result"];
    for field in ["ttlMs", "cacheScope", "resultType"] {
        assert!(
            result.get(field).is_none(),
            "legacy field {field}: {result}"
        );
    }
    assert_catalog(result);
}

/// Guard modern server/discover defaults and the advertised modern revision.
#[tokio::test]
async fn modern_server_discover() {
    let replies = raw_exchange(&[modern_request("server/discover")]).await;
    let result = &replies[0]["result"];
    assert!(
        result["supportedVersions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|version| version == "2026-07-28")
    );
    assert_eq!(result["ttlMs"].as_u64(), Some(0));
    assert_eq!(result["cacheScope"], "private");
}

/// Require termination within five seconds after stdin closes.
#[test]
fn eof_releases_stdio_process() {
    let mut child = std::process::Command::new(binary())
        .arg("mcp")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("MCP process survived stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A confirmed service mismatch invalidates the list once and blocks both legacy and stateless calls.
#[tokio::test]
async fn service_version_skew_invalidates_catalog() {
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("quota.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let fixture = tokio::spawn(async move {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["method"],
                "status"
            );
            stream
                .write_all(
                    b"{\"version\":1,\"result\":{\"status\":\"running\",\"version\":\"9.8.7\"}}\n",
                )
                .await
                .unwrap();
        }
    });
    let call =
        |id| json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"get_status"}});
    let (replies, invalidations) = raw_exchange_with_socket(&[
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"raw-test","version":"1"}
        }}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        call(2),
        call(3),
    ], Some(&socket)).await;
    let diagnostic = format!(
        "agent-quota service is 9.8.7, this MCP frontend is {}: reconnect the agent-quota MCP server (Claude Code: /mcp → reconnect; Codex: restart the session)",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        replies[0]["result"]["capabilities"]["tools"]["listChanged"],
        true
    );
    assert_eq!(replies[0]["result"]["instructions"], diagnostic);
    assert_eq!(invalidations, 1);
    for reply in &replies[1..] {
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][0]["text"], diagnostic);
    }
    let mut modern = modern_request("tools/call");
    modern["params"]["name"] = "get_status".into();
    let (replies, invalidations) = raw_exchange_with_socket(&[modern], Some(&socket)).await;
    assert_eq!(replies[0]["result"]["isError"], true);
    assert_eq!(replies[0]["result"]["content"][0]["text"], diagnostic);
    assert_eq!(invalidations, 1);

    let mut doctor = tokio::process::Command::new(binary());
    doctor
        .args(["doctor", "--json", "--socket"])
        .arg(socket)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), doctor.output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["service_version"], "9.8.7");
    assert_eq!(report["version_skew"], diagnostic);
    fixture.await.unwrap();
}

/// Oversized reads identify usable filters and keep account names out of capacity errors.
#[tokio::test]
async fn oversized_read_names_narrowing_candidates() {
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("quota.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let fixture = tokio::spawn(async move {
        for (method, response) in [
            (
                "status",
                json!({"version":1, "result":{"status":"running", "version":env!("CARGO_PKG_VERSION")}}),
            ),
            (
                "snapshot",
                json!({"version":1,"error":"response_too_large"}),
            ),
            (
                "scope_options",
                json!({"version":1,"result":{"accounts":["fixture-account"],"models":["fixture-model"],"more_models":true}}),
            ),
            (
                "capacity",
                json!({"version":1,"error":"response_too_large"}),
            ),
            (
                "scope_options",
                json!({"version":1,"result":{"accounts":["fixture-account"],"models":["fixture-model"]}}),
            ),
        ] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["method"],
                method
            );
            stream
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let call = |id, name| {
        let mut request = modern_request("tools/call");
        request["id"] = json!(id);
        request["params"]["name"] = json!(name);
        request
    };
    let (replies, invalidations) = raw_exchange_with_socket(
        &[call(1, "get_quota"), call(2, "get_capacity")],
        Some(&socket),
    )
    .await;
    assert_eq!(invalidations, 0);
    for reply in &replies {
        assert_eq!(reply["result"]["isError"], true);
    }
    assert_eq!(
        replies[0]["result"]["content"][0]["text"],
        "ERROR response_too_large: narrow with accounts (candidates: \"fixture-account\") or model (candidates: \"fixture-model\", …).\n"
    );
    assert_eq!(
        replies[1]["result"]["content"][0]["text"],
        "ERROR response_too_large: narrow with model (candidates: \"fixture-model\").\n"
    );
    tokio::time::timeout(Duration::from_secs(1), fixture)
        .await
        .unwrap()
        .unwrap();
}
