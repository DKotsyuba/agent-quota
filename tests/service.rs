//! End-to-end service checks with isolated local state and synthetic collectors.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Explicit fixture assertions"
)]

use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

/// Resolves an explicitly supplied shipped binary or the Cargo-built product.
fn binary() -> PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_agent-quota")))
}

/// Reads one versioned API response from the test-owned socket.
async fn request(socket: &Path, method: &str) -> Value {
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(format!("{}\n", json!({"version":1,"method":method,"accounts":[]})).as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Waits for a successful service response matching the supplied observation.
async fn wait_for(socket: &Path, method: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if socket.exists()
                && let Ok(Ok(value)) = tokio::time::timeout(Duration::from_millis(300), async {
                    let mut stream = tokio::net::UnixStream::connect(socket).await?;
                    stream
                        .write_all(
                            format!("{}\n", json!({"version":1,"method":method,"accounts":[]}))
                                .as_bytes(),
                        )
                        .await?;
                    let mut line = String::new();
                    BufReader::new(stream).read_line(&mut line).await?;
                    serde_json::from_str(&line).map_err(std::io::Error::other)
                })
                .await
                && predicate(&value)
            {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("service observation deadline")
}

/// Starts one uninstalled service with a private synthetic home and config.
fn start(home: &Path, config: &Path, socket: &Path) -> Child {
    Command::new(binary())
        .arg("serve")
        .arg("--home")
        .arg(home)
        .arg("--config")
        .arg(config)
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Requests graceful shutdown of this owned child and waits for its completion.
async fn stop(child: &mut Child) {
    let pid = i32::try_from(child.id().expect("live test child")).unwrap();
    // SAFETY: pid is the still-owned, unreaped child created by this test.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

/// Creates an executable synthetic collector; no provider or real login is touched.
fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Two observations produce a persisted forecast; a failed next round keeps it.
#[tokio::test]
async fn service_collects_forecasts_and_retains_last_good_data() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.path().join("home");
    let config = root.path().join("config.toml");
    let socket = root.path().join("quota.sock");
    let script = root.path().join("collector.sh");
    let counter = root.path().join("counter");
    write_script(
        &script,
        r#"#!/bin/sh
set -eu
n=0
if [ -f "$1" ]; then n=$(cat "$1"); fi
n=$((n + 1))
printf '%s' "$n" > "$1"
cat >/dev/null
remaining=$((100 - n * 10))
if [ "$remaining" -lt 0 ]; then remaining=0; fi
observed=$(date +%s)
printf '{"version":1,"windows":[{"pool":"primary","window":"five_hour","models":["model-a"],"remaining_percent":%s,"observed_at":%s,"valid_until":4000000000,"reset_at":4000000000}]}' "$remaining" "$observed"
"#,
    );
    std::fs::write(
        &config,
        format!(
            r#"
interval_seconds = 1
[[sources]]
account = "fixture"
provider = "codex"
auth_family = "openai"
credential = "native:codex"
native_directory = "/synthetic/login"
models = [{{ id = "model-a" }}]
[sources.harness]
id = "codex"
binary = "/bin/true"
[sources.collector]
command = "/bin/sh"
args = ["{}", "{}"]
source = "fixture-source"
"#,
            script.display(),
            counter.display()
        ),
    )
    .unwrap();
    let before_start = Command::new(binary())
        .arg("doctor")
        .arg("--json")
        .arg("--socket")
        .arg(&socket)
        .output()
        .await
        .unwrap();
    assert!(!before_start.status.success());
    let doctor: Value = serde_json::from_slice(&before_start.stdout).unwrap();
    assert_eq!(doctor["local_ready"], false);
    assert_eq!(doctor["service"], "unavailable");
    let mut child = start(&home, &config, &socket);
    let first = wait_for(&socket, "snapshot", |value| {
        value["result"]["accounts"][0]["models"][0]["state"] == "known"
    })
    .await;
    let ready = Command::new(binary())
        .arg("doctor")
        .arg("--json")
        .arg("--socket")
        .arg(&socket)
        .output()
        .await
        .unwrap();
    assert!(ready.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&ready.stdout).unwrap()["service"],
        "running"
    );
    let epoch = first["result"]["source_epoch"].as_str().unwrap().to_owned();
    let second = wait_for(&socket, "snapshot", |value| {
        value["result"]["accounts"][0]["windows"][0]["forecast"]["burn_percent_per_hour"]
            .as_f64()
            .is_some_and(|rate| rate > 0.0)
    })
    .await;
    assert_eq!(
        second["result"]["accounts"][0]["models"][0]["model"],
        "model-a"
    );
    assert_eq!(second["result"]["coverage"]["state"], "complete");
    let output = Command::new(binary())
        .arg("limits")
        .arg("--socket")
        .arg(&socket)
        .arg("--account")
        .arg("fixture")
        .arg("--model")
        .arg("model-a")
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cli["accounts"][0]["models"][0]["state"], "known");
    let invalid = Command::new(binary())
        .arg("limits")
        .arg("--socket")
        .arg(&socket)
        .arg("--account")
        .arg("fixture")
        .arg("--model")
        .arg("absent-model")
        .output()
        .await
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unknown_model"));

    let mut mcp = Command::new(binary());
    mcp.arg("mcp")
        .arg("--socket")
        .arg(&socket)
        .env_clear()
        .env("HOME", root.path())
        .kill_on_drop(true)
        .stderr(Stdio::null());
    let client = ().serve(TokioChildProcess::new(mcp).unwrap()).await.unwrap();
    let reply = client
        .call_tool(CallToolRequestParams::new("get_quota"))
        .await
        .unwrap();
    let wire = serde_json::to_value(reply).unwrap();
    assert_eq!(wire["isError"], false, "{wire}");
    assert!(
        wire["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("model-a")
    );
    client.cancel().await.unwrap();
    write_script(&script, "#!/bin/sh\ncat >/dev/null\nexit 7\n");
    let failed = wait_for(&socket, "status", |value| {
        value["result"]["last_collection"]["sources"][0]["status"] == "failed"
    })
    .await;
    assert_eq!(
        failed["result"]["last_collection"]["sources"][0]["issues"][0],
        "collector_exit_status:7"
    );
    let retained = request(&socket, "snapshot").await;
    assert_eq!(
        retained["result"]["accounts"][0]["models"][0]["state"],
        "known"
    );
    assert_eq!(retained["result"]["coverage"]["state"], "partial");
    stop(&mut child).await;
    assert!(!socket.exists());
    let mut restarted = start(&home, &config, &socket);
    let after = wait_for(&socket, "snapshot", |value| {
        value["result"]["source_epoch"].is_string()
    })
    .await;
    assert_eq!(after["result"]["source_epoch"], epoch);
    assert_eq!(
        after["result"]["accounts"][0]["models"][0]["state"],
        "known"
    );
    stop(&mut restarted).await;
}

/// A collector that echoes private stdin is rejected without exposing the token.
#[tokio::test]
async fn echoed_credential_never_reaches_status_or_snapshot() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.path().join("home");
    let config = root.path().join("config.toml");
    let socket = root.path().join("quota.sock");
    let token = root.path().join("credential");
    std::fs::write(&token, "synthetic-token-canary").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(
        &config,
        format!(
            r#"
interval_seconds = 1
[[sources]]
account = "fixture"
provider = "codex"
auth_family = "openai"
credential = "file:{}"
models = [{{ id = "model-a" }}]
[sources.harness]
id = "codex"
binary = "/bin/true"
[sources.collector]
command = "/bin/cat"
source = "fixture-source"
"#,
            token.display()
        ),
    )
    .unwrap();
    let mut child = start(&home, &config, &socket);
    let status = wait_for(&socket, "status", |value| {
        value["result"]["last_collection"]["sources"][0]["status"] == "failed"
    })
    .await;
    assert_eq!(
        status["result"]["last_collection"]["sources"][0]["issues"][0],
        "collector_output_contains_credential"
    );
    assert!(!status.to_string().contains("synthetic-token-canary"));
    let snapshot = request(&socket, "snapshot").await;
    assert!(!snapshot.to_string().contains("synthetic-token-canary"));
    assert_eq!(
        snapshot["result"]["accounts"][0]["models"][0]["state"],
        "unknown"
    );
    stop(&mut child).await;
}

/// A disabled collector keeps its model visible as unknown and never reads its credential.
#[tokio::test]
async fn disabled_collection_keeps_unknown_scope() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.path().join("home");
    let config = root.path().join("config.toml");
    let socket = root.path().join("quota.sock");
    std::fs::write(
        &config,
        r#"
interval_seconds = 1
[[sources]]
account = "fixture"
provider = "codex"
auth_family = "openai"
credential = "file:/synthetic/must-not-be-read"
limits_source = "none"
models = [{ id = "model-a" }]
[sources.harness]
id = "codex"
binary = "/bin/true"
"#,
    )
    .unwrap();
    let mut child = start(&home, &config, &socket);
    let response = wait_for(&socket, "snapshot", |value| {
        value["result"]["coverage"]["state"] == "partial"
    })
    .await;
    assert_eq!(
        response["result"]["accounts"][0]["models"][0]["state"],
        "unknown"
    );
    assert_eq!(
        response["result"]["coverage"]["unobserved"][0]["reason"],
        "limits_source_none"
    );
    stop(&mut child).await;
}

/// Tests external ingestion and fencing, including the same-UID shell handoff.
/// This does not qualify systemd, deployment checks, or switching between UIDs.
#[tokio::test]
async fn external_probe_and_owner_ingest_preserve_scope() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.path().join("home");
    let worker_home = root.path().join("worker-home");
    let config = root.path().join("config.toml");
    let socket = root.path().join("quota.sock");
    let script = root.path().join("probe.sh");
    let token = root.path().join("credential");
    write_script(
        &script,
        r#"#!/bin/sh
cat >/dev/null
observed=$(date +%s)
printf '{"version":1,"windows":[{"pool":"primary","window":"hour","models":["model-a"],"remaining_percent":73,"observed_at":%s,"valid_until":4000000000,"reset_at":4000000000}]}' "$observed"
"#,
    );
    let config_text = format!(
        r#"
interval_seconds = 300
[[sources]]
account = "fixture"
provider = "codex"
auth_family = "openai"
credential = "file:{}"
execution = "external"
models = [{{ id = "model-a" }}]
[sources.harness]
id = "codex"
binary = "/bin/true"
[sources.collector]
command = "/bin/sh"
args = ["{}"]
source = "fixture-source"
"#,
        token.display(),
        script.display()
    );
    std::fs::write(&config, &config_text).unwrap();
    let mut child = start(&home, &config, &socket);
    let status = wait_for(&socket, "status", |value| {
        value["result"]["last_collection"]["sources"][0]["status"] == "external"
    })
    .await;
    assert!(
        status["result"]["last_collection"]["sources"][0]["issues"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let before = request(&socket, "snapshot").await;
    assert_eq!(
        before["result"]["accounts"][0]["models"][0]["state"],
        "unknown"
    );
    assert_eq!(before["result"]["coverage"]["state"], "partial");
    std::fs::write(&token, "synthetic-worker-secret").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let wrapper =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/systemd/agent-quota-owner-collect.sh");
    let mut wrapper_command = Command::new("/bin/bash");
    wrapper_command
        .arg("-c")
        .arg(r#"source "$1"; run_as() { shift; /usr/bin/env -i "$@"; }; collect"#)
        .arg("fixture")
        .arg(wrapper)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path())
        .env("TMPDIR", root.path())
        .env("TMP", root.path())
        .env("TEMP", root.path())
        .env("AQ_BINARY", binary())
        .env("AQ_CONFIG", &config)
        .env("AQ_SOCKET", &socket)
        .env("AQ_SERVICE_USER", "fixture")
        .env("AQ_SERVICE_TMP", root.path())
        .env("AQ_PROBE_USER", "fixture")
        .env("AQ_PROBE_OS_HOME", root.path())
        .env("AQ_PROBE_PRIVATE_HOME", &worker_home)
        .env("AQ_PROBE_TMP", root.path())
        .env("AQ_PROBE_PATH", "/usr/bin:/bin")
        .env("AQ_ACCOUNT", "fixture")
        .env("AQ_PROVIDER", "codex")
        .env("AQ_SOURCE", "fixture-source")
        .kill_on_drop(true);
    let wrapper_output = tokio::time::timeout(Duration::from_secs(15), wrapper_command.output())
        .await
        .expect("wrapper deadline")
        .unwrap();
    assert!(wrapper_output.status.success(), "wrapper collection failed");
    assert!(!String::from_utf8_lossy(&wrapper_output.stdout).contains("synthetic-worker-secret"));
    assert!(!String::from_utf8_lossy(&wrapper_output.stderr).contains("synthetic-worker-secret"));
    let wrapped = request(&socket, "snapshot").await;
    assert_eq!(
        wrapped["result"]["accounts"][0]["models"][0]["state"],
        "known"
    );
    assert_eq!(
        wrapped["result"]["accounts"][0]["models"][0]["remaining_percent"],
        73.0
    );
    assert_eq!(wrapped["result"]["coverage"]["state"], "complete");

    let probe = Command::new(binary())
        .arg("probe")
        .arg("--home")
        .arg(&worker_home)
        .arg("--config")
        .arg(&config)
        .arg("--account")
        .arg("fixture")
        .arg("--provider")
        .arg("codex")
        .arg("--source")
        .arg("fixture-source")
        .output()
        .await
        .unwrap();
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(!String::from_utf8_lossy(&probe.stdout).contains("synthetic-worker-secret"));
    let mut ingest = Command::new(binary())
        .arg("ingest")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    ingest
        .stdin
        .take()
        .unwrap()
        .write_all(&probe.stdout)
        .await
        .unwrap();
    let accepted = ingest.wait_with_output().await.unwrap();
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&accepted.stdout).unwrap()["stored"],
        true
    );
    let after = request(&socket, "snapshot").await;
    assert_eq!(
        after["result"]["accounts"][0]["models"][0]["state"],
        "known"
    );
    assert_eq!(after["result"]["coverage"]["state"], "complete");

    std::fs::write(
        &config,
        format!("{config_text}\n# changed after the probe\n"),
    )
    .unwrap();
    let mut stale = Command::new(binary())
        .arg("ingest")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    stale
        .stdin
        .take()
        .unwrap()
        .write_all(&probe.stdout)
        .await
        .unwrap();
    let stale = stale.wait_with_output().await.unwrap();
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("config_changed"));
    let retained = request(&socket, "snapshot").await;
    assert_eq!(retained["result"]["revision"], after["result"]["revision"]);
    stop(&mut child).await;
}
