//! Reset-credit redemption: socket contract, launcher handoff and provider scripts
//! against loopback fixtures. No provider, real login or other OS user is touched.
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
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

const TOKEN: &str = "synthetic-redeem-secret-0123456789";
const CLAIM_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const ONE: &str = "11111111-1111-4111-8111-111111111111";
const TWO: &str = "22222222-2222-4222-8222-222222222222";
const THREE: &str = "33333333-3333-4333-8333-333333333333";
const FOUR: &str = "44444444-4444-4444-8444-444444444444";

/// Resolves an explicitly supplied shipped binary or the Cargo-built product.
fn binary() -> PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_agent-quota")))
}

/// Returns a shipped redemption script from the source tree.
fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/collectors")
        .join(name)
}

/// Creates an owner-private fixture root.
fn private_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}

/// Writes an executable synthetic script.
fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// Writes an owner-private synthetic credential file.
fn write_secret(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Sends one raw protocol line and returns the decoded reply.
async fn raw(socket: &Path, request: Value) -> Value {
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Waits until the service answers a status read.
async fn ready(socket: &Path) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if socket.exists()
                && let Ok(mut stream) = tokio::net::UnixStream::connect(socket).await
                && stream
                    .write_all(b"{\"version\":1,\"method\":\"status\"}\n")
                    .await
                    .is_ok()
            {
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).await.is_ok()
                    && line.contains("running")
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("service start deadline");
}

/// Starts one uninstalled service with a private synthetic home.
fn start(home: &Path, config: &Path, socket: &Path, extra: &[String]) -> Child {
    Command::new(binary())
        .arg("serve")
        .arg("--home")
        .arg(home)
        .arg("--config")
        .arg(config)
        .arg("--socket")
        .arg(socket)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Stops the service gracefully and returns everything it wrote to its journal stream.
async fn stop(child: Child) -> String {
    let pid = i32::try_from(child.id().expect("live test child")).unwrap();
    // SAFETY: pid is the still-owned, unreaped child created by this test.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Runs launcher functions from the shipped script with the UID switch stubbed.
async fn launcher(root: &Path, config: &Path, socket: &Path, body: &str) -> std::process::Output {
    let wrapper =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/systemd/agent-quota-owner-collect.sh");
    let program = format!(
        r#"source "$1"; lock_directory="$2"; redeem_retry_delay=0; redeem_report_delays=""; run_as() {{ shift; /usr/bin/env -i "$@"; }}; {body}"#
    );
    let mut command = Command::new("/bin/bash");
    command
        .arg("-c")
        .arg(program)
        .arg("fixture")
        .arg(wrapper)
        .arg(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("AQ_BINARY", binary())
        .env("AQ_CONFIG", config)
        .env("AQ_SOCKET", socket)
        .env("AQ_SERVICE_USER", "fixture")
        .env("AQ_SERVICE_TMP", root)
        .env("AQ_PROBE_USER", "fixture")
        .env("AQ_PROBE_OS_HOME", root)
        .env("AQ_PROBE_PRIVATE_HOME", root.join("worker-home"))
        .env("AQ_PROBE_TMP", root)
        .env("AQ_PROBE_PATH", "/usr/bin:/bin")
        .env("AQ_ACCOUNT", "fixture")
        .env("AQ_PROVIDER", "glm")
        .env("AQ_SOURCE", "fixture-source")
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("launcher deadline")
        .unwrap()
}

/// Builds the redemption request line.
fn redeem(account: &str, credit: &str, id: &str) -> Value {
    json!({"version":1,"method":"redeem_reset_credit","account":account,"credit_id":credit,"request_id":id})
}

/// Covers authorization, idempotency, refusals, the trigger, the launcher handoff,
/// state transitions and the CLI client against a real service process.
#[tokio::test]
async fn service_contract_and_launcher_handoff() {
    let root = private_root();
    let home = root.path().join("home");
    let config = root.path().join("config.toml");
    let socket = root.path().join("quota.sock");
    let collector = root.path().join("collector.sh");
    let redeemer = root.path().join("redeem.sh");
    let token = root.path().join("credential");
    let seen = root.path().join("seen");
    let verdict = root.path().join("verdict");
    write_secret(&token, TOKEN);
    write_script(
        &collector,
        r#"#!/bin/sh
observed=$(jq -r .now)
printf '{"version":1,"windows":[{"pool":"primary","window":"five_hour","models":["model-a"],"remaining_percent":50,"observed_at":%s,"valid_until":4000000000,"reset_at":4000000000}],"reset_credits":{"known":true,"available_count":1,"credits":[{"id":"credit-a","provider_kind":"FIVE_HOUR","label":"Fixture","scope":"five_hour","resets_left":null,"usable_now":true,"granted_at":null,"expires_at":null},{"id":"credit-b","provider_kind":"WEEK","label":"Spent","scope":"seven_day","resets_left":null,"usable_now":false,"granted_at":null,"expires_at":null}]}}' "$observed"
"#,
    );
    // Records the nonsecret request and returns the verdict chosen by the test.
    write_script(
        &redeemer,
        &format!(
            "#!/bin/sh\njq -c '{{redeem, has_token:(.auth.token | length > 0)}}' >> '{}'\ncat '{}'\n",
            seen.display(),
            verdict.display()
        ),
    );
    std::fs::write(
        &verdict,
        r#"{"version":1,"state":"redeemed","reason":null}"#,
    )
    .unwrap();
    std::fs::write(
        &config,
        format!(
            r#"
interval_seconds = 300
[[sources]]
account = "fixture"
provider = "glm"
auth_family = "zai"
credential = "file:{}"
execution = "external"
models = [{{ id = "model-a" }}]
[sources.harness]
id = "claude-code"
binary = "/bin/true"
[sources.collector]
command = "/bin/sh"
args = ["{}"]
source = "fixture-source"
[sources.redeem]
command = "/bin/sh"
args = ["{}"]
instance = "fixture-instance"
"#,
            token.display(),
            collector.display(),
            redeemer.display()
        ),
    )
    .unwrap();
    // SAFETY: geteuid has no pointer arguments or preconditions.
    let uid = unsafe { libc::geteuid() };

    // Without --allow-redeem-uid even the service's own UID is refused.
    let closed = start(&home, &config, &socket, &[]);
    ready(&socket).await;
    for request in [
        redeem("fixture", "credit-a", ONE),
        json!({"version":1,"method":"redeem_status","request_id":ONE}),
    ] {
        assert_eq!(raw(&socket, request).await["error"], "unauthorized");
    }
    assert!(!home.join("redeem").exists());
    stop(closed).await;

    // A redemption UID that could not connect at all is a startup error.
    let unreachable = start(
        &home,
        &config,
        &socket,
        &["--allow-redeem-uid".into(), (uid + 1).to_string()],
    );
    let refused = unreachable.wait_with_output().await.unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("redeem_uid_not_allowed"));

    let service = start(
        &home,
        &config,
        &socket,
        &["--allow-redeem-uid".into(), uid.to_string()],
    );
    ready(&socket).await;

    // Before any inventory the credit cannot be confirmed usable.
    let early = raw(&socket, redeem("fixture", "credit-a", ONE)).await;
    assert_eq!(early["result"]["state"], "rejected");
    assert_eq!(early["result"]["reason"], "inventory_unknown");
    assert!(
        launcher(root.path(), &config, &socket, "collect")
            .await
            .status
            .success()
    );

    for (request, error) in [
        (redeem("missing", "credit-a", TWO), "unknown_account"),
        (
            redeem("fixture", "credit-a", "not-a-uuid"),
            "invalid_request",
        ),
        (redeem("fixture", "credit-b", ONE), "request_conflict"),
        (
            json!({"version":1,"method":"redeem_status","request_id":TWO}),
            "unknown_request",
        ),
        (
            json!({"version":1,"method":"redeem_status","request_id":TWO,"account":"fixture"}),
            "invalid_request",
        ),
        (
            json!({"version":1,"method":"status","request_id":TWO}),
            "invalid_request",
        ),
    ] {
        assert_eq!(raw(&socket, request).await["error"], error);
    }
    let spent = raw(&socket, redeem("fixture", "credit-b", TWO)).await;
    assert_eq!(spent["result"]["reason"], "credit_not_usable");
    let trigger = home.join("redeem/fixture-instance.pending");
    assert!(!trigger.exists());

    // `op` is an accepted alias of `method`; the reply has exactly the documented keys.
    let pending = raw(
        &socket,
        json!({"version":1,"op":"redeem_reset_credit","account":"fixture","credit_id":"credit-a","request_id":THREE}),
    )
    .await;
    let record = pending["result"].as_object().unwrap();
    assert_eq!(
        record.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "account",
            "created_at",
            "credit_id",
            "reason",
            "request_id",
            "state",
            "updated_at"
        ]
    );
    assert_eq!(record["state"], "pending");
    assert!(record["reason"].is_null());
    assert!(record["created_at"].is_u64() && record["updated_at"].is_u64());
    assert_eq!(
        std::fs::read_to_string(&trigger).unwrap(),
        format!("{THREE}\n")
    );
    assert_eq!(
        raw(&socket, redeem("fixture", "credit-a", THREE)).await,
        pending
    );
    let busy = raw(&socket, redeem("fixture", "credit-a", FOUR)).await;
    assert_eq!(busy["result"]["state"], "rejected");
    assert_eq!(busy["result"]["reason"], "redeem_in_progress");
    assert_eq!(
        raw(
            &socket,
            json!({"version":1,"method":"redeem_status","request_id":THREE})
        )
        .await,
        pending
    );

    // A launcher that cannot reach the service executes nothing and leaves the trigger
    // to the service, which alone removes it.
    let lost = launcher(
        root.path(),
        &config,
        &root.path().join("absent.sock"),
        "redeem",
    )
    .await;
    assert!(!lost.status.success());
    assert!(String::from_utf8_lossy(&lost.stderr).contains("claim failed"));
    assert!(trigger.exists() && !seen.exists());

    // The launcher claims once, executes as the "owner", reports and refreshes.
    let run = launcher(root.path(), &config, &socket, "redeem && collect").await;
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(!trigger.exists());
    let done = raw(
        &socket,
        json!({"version":1,"method":"redeem_status","request_id":THREE}),
    )
    .await;
    assert_eq!(done["result"]["state"], "redeemed");
    assert!(done["result"]["reason"].is_null());
    let handed: Value =
        serde_json::from_str(std::fs::read_to_string(&seen).unwrap().trim()).unwrap();
    assert_eq!(
        handed,
        json!({"redeem":{"request_id":THREE,"credit_id":"credit-a","scope":"five_hour","provider_kind":"FIVE_HOUR"},"has_token":true})
    );
    // With nothing pending the launcher is a no-op and does not call the script again.
    let idle = launcher(root.path(), &config, &socket, "redeem").await;
    assert!(idle.status.success());
    assert!(String::from_utf8_lossy(&idle.stderr).contains("no pending request"));
    assert_eq!(std::fs::read_to_string(&seen).unwrap().lines().count(), 1);

    // CLI client: a refusal is a final state with exit status 2. Here the credential
    // owner's own spacing refuses a second execution right after the first, whatever
    // the service hands out, and the script is not started.
    std::fs::write(
        &verdict,
        r#"{"version":1,"state":"rejected","reason":"already_used"}"#,
    )
    .unwrap();
    let client = Command::new(binary())
        .args([
            "redeem",
            "--account",
            "fixture",
            "--credit",
            "credit-a",
            "--wait",
            "20",
        ])
        .arg("--socket")
        .arg(&socket)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !trigger.exists() {
        assert!(Instant::now() < deadline, "client did not create a request");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let run = launcher(root.path(), &config, &socket, "redeem").await;
    assert!(run.status.success());
    let client = tokio::time::timeout(Duration::from_secs(25), client.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(client.status.code(), Some(2));
    let printed: Value = serde_json::from_slice(&client.stdout).unwrap();
    assert_eq!(printed["state"], "rejected");
    assert_eq!(printed["reason"], "owner_rate_limited");
    assert_eq!(std::fs::read_to_string(&seen).unwrap().lines().count(), 1);
    let guard = root.path().join("worker-home/redeem-fixture.last");
    assert_eq!(
        std::fs::metadata(&guard).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&guard).unwrap();

    // A script without a usable verdict never becomes a success or a retry.
    assert!(
        launcher(root.path(), &config, &socket, "collect")
            .await
            .status
            .success()
    );
    std::fs::write(
        &verdict,
        format!(r#"{{"version":1,"state":"redeemed","reason":null,"echo":"{TOKEN}"}}"#),
    )
    .unwrap();
    assert_eq!(
        raw(&socket, redeem("fixture", "credit-a", FOUR)).await["result"]["state"],
        // FOUR was already answered while another request was pending.
        "rejected"
    );
    let fifth = "55555555-5555-4555-8555-555555555555";
    assert_eq!(
        raw(&socket, redeem("fixture", "credit-a", fifth)).await["result"]["state"],
        "pending"
    );
    let run = launcher(root.path(), &config, &socket, "redeem").await;
    assert!(run.status.success());
    let leaked = raw(
        &socket,
        json!({"version":1,"method":"redeem_status","request_id":fifth}),
    )
    .await;
    assert_eq!(leaked["result"]["state"], "unknown");
    assert_eq!(
        leaked["result"]["reason"],
        "redeem_output_contains_credential"
    );
    assert_eq!(std::fs::read_to_string(&seen).unwrap().lines().count(), 2);

    // The inventory predates that unknown spend until the next collection.
    let sixth = "66666666-6666-4666-8666-666666666666";
    let stale = raw(&socket, redeem("fixture", "credit-a", sixth)).await;
    assert_eq!(stale["result"]["reason"], "inventory_stale");

    // A report applies only with the token of its claim, and the first verdict wins.
    assert!(
        launcher(root.path(), &config, &socket, "collect")
            .await
            .status
            .success()
    );
    let seventh = "77777777-7777-4777-8777-777777777777";
    assert_eq!(
        raw(&socket, redeem("fixture", "credit-a", seventh)).await["result"]["state"],
        "pending"
    );
    let claimed = raw(
        &socket,
        json!({"version":1,"method":"redeem_claim","account":"fixture"}),
    )
    .await["result"]["request"]
        .clone();
    assert_eq!(claimed["request_id"], seventh);
    let token = claimed["claim_token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 32);
    assert!(!trigger.exists());
    let report = |token: Option<&str>, state: &str| {
        let reason = match state {
            "failed" => json!("auth_rejected"),
            "leak" => json!("0123456789abcdef0123456789abcdef"),
            "reserved" => json!("outcome_unreported"),
            _ => Value::Null,
        };
        let state = if matches!(state, "leak" | "reserved") {
            "unknown"
        } else {
            state
        };
        let mut outcome = json!({"version":1,"request_id":seventh,"account":"fixture","state":state,"reason":reason});
        if let Some(token) = token {
            outcome["claim_token"] = json!(token);
        }
        raw(
            &socket,
            json!({"version":1,"method":"redeem_report","outcome":outcome}),
        )
    };
    assert_eq!(report(None, "redeemed").await["error"], "invalid_request");
    // Even with the right token, a reason outside the fixed list is refused.
    for reason in ["leak", "reserved"] {
        assert_eq!(
            report(Some(&token), reason).await["error"],
            "invalid_request"
        );
    }
    assert_eq!(
        report(Some("ffffffffffffffffffffffffffffffff"), "redeemed").await["error"],
        "request_conflict"
    );
    let status = json!({"version":1,"method":"redeem_status","request_id":seventh});
    let untouched = raw(&socket, status.clone()).await;
    assert_eq!(untouched["result"]["state"], "pending");
    // Clients never see the token.
    assert!(!untouched.to_string().contains(&token));
    assert_eq!(
        report(Some(&token), "failed").await["result"]["state"],
        "failed"
    );
    assert_eq!(
        report(Some(&token), "failed").await["result"]["state"],
        "failed"
    );
    assert_eq!(
        report(Some(&token), "redeemed").await["error"],
        "request_conflict"
    );
    assert_eq!(raw(&socket, status).await["result"]["state"], "failed");

    // Launcher-only methods are not exposed through MCP and the tool list is unchanged.
    let catalog = Command::new(binary())
        .args(["contract", "export"])
        .output()
        .await
        .unwrap();
    let tools: Value = serde_json::from_slice(&catalog.stdout).unwrap();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["get_status", "get_quota", "get_capacity"]);
    assert!(!String::from_utf8_lossy(&catalog.stdout).contains("redeem"));

    let journal = stop(service).await;
    assert!(journal.contains(&format!(
        "quota redeem: request {THREE} account fixture state redeemed"
    )));
    for output in [&journal, &String::from_utf8_lossy(&run.stderr).into_owned()] {
        assert!(!output.contains(TOKEN));
    }
}

/// One scripted HTTP exchange of the loopback provider fixture.
enum Step {
    /// Answer with this status line and JSON body.
    Reply(&'static str, &'static str),
    /// Read the request, then close without any response.
    Drop,
}

/// Serves the scripted exchanges in order and returns each full request (head and body).
fn provider(steps: Vec<Step>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for step in steps {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                if Instant::now() >= deadline {
                    // The script decided not to make this call.
                    return requests;
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let complete = |bytes: &[u8]| {
                let text = String::from_utf8_lossy(bytes);
                text.split_once("\r\n\r\n").is_some_and(|(head, body)| {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    body.len() >= length
                })
            };
            while !complete(&bytes) {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0 && bytes.len() < 65_536);
                bytes.extend_from_slice(&chunk[..count]);
            }
            if let Step::Reply(status, body) = step {
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests.push(String::from_utf8(bytes).unwrap());
        }
        requests
    });
    (origin, handle)
}

/// Gives every executor run its own private probe home, so the owner-side spacing of
/// executions does not couple unrelated cases.
static WORKER_HOME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Runs the real `redeem-execute` for one configured source and returns its outcome
/// plus everything the process printed.
async fn execute(root: &Path, config: &str, provider: &str, claim: Value) -> (Value, String) {
    use sha2::{Digest, Sha256};
    let path = root.join("config.toml");
    std::fs::write(&path, config).unwrap();
    // A real claim is pinned to the exact configuration bytes it was admitted under.
    let mut claim = claim;
    if claim["config_revision"].is_null() {
        let revision: String = Sha256::digest(config.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        claim["config_revision"] = json!(revision);
    }
    let mut child = Command::new(binary())
        .arg("redeem-execute")
        .arg("--home")
        .arg(root.join(format!(
            "worker-home-{}",
            WORKER_HOME.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )))
        .arg("--config")
        .arg(&path)
        .args([
            "--account",
            "fixture",
            "--provider",
            provider,
            "--source",
            "fixture-source",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("TMPDIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(format!("{claim}\n").as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("executor deadline")
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(!text.contains(TOKEN), "credential reached executor output");
    (serde_json::from_slice(&output.stdout).unwrap(), text)
}

/// Builds the launcher claim for one credit.
fn claim(credit: &str, scope: &str, kind: &str) -> Value {
    json!({"version":1,"request_id":ONE,"account":"fixture","credit_id":credit,"scope":scope,"provider_kind":kind,"claim_token":CLAIM_TOKEN})
}

/// Returns the state and reason of an executor outcome after checking its identity.
fn verdict(outcome: &Value) -> (String, Option<String>) {
    assert_eq!(outcome["version"], 1);
    assert_eq!(outcome["request_id"], ONE);
    assert_eq!(outcome["account"], "fixture");
    assert_eq!(outcome["claim_token"], CLAIM_TOKEN);
    (
        outcome["state"].as_str().unwrap().to_owned(),
        outcome["reason"].as_str().map(str::to_owned),
    )
}

/// Returns the JSON body of a captured HTTP request.
fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

/// Token-file source whose redemption script is pointed at a loopback origin.
fn token_config(root: &Path, provider: &str, script_name: &str, argument: &str) -> String {
    let token = root.join("credential");
    write_secret(&token, TOKEN);
    format!(
        r#"
[[sources]]
account = "fixture"
provider = "{provider}"
auth_family = "anthropic"
credential = "file:{}"
execution = "external"
models = [{{ id = "model-a" }}]
[sources.harness]
id = "claude-code"
binary = "/bin/true"
[sources.collector]
command = "/bin/true"
source = "fixture-source"
[sources.redeem]
command = "/bin/bash"
args = ["{}", "{argument}"]
"#,
        token.display(),
        script(script_name).display()
    )
}

const PROFILE: &str = r#"{"organization":{"uuid":"0f0f0f0f-1111-4222-8333-444444444444"}}"#;

/// The Claude script reads the profile, posts one claim and maps every answer class.
#[tokio::test]
async fn claude_redeem_maps_provider_answers() {
    let root = private_root();
    let run = |steps: Vec<Step>| {
        let root = root.path().to_owned();
        async move {
            let (origin, server) = provider(steps);
            let config = token_config(&root, "claude", "claude-redeem.sh", &origin);
            let (outcome, _) = execute(
                &root,
                &config,
                "claude",
                claim("grant:g-1", "five_hour", "GRANT"),
            )
            .await;
            (verdict(&outcome), server.join().unwrap())
        }
    };
    let ok = |body| Step::Reply("200 OK", body);

    let (result, requests) = run(vec![
        ok(PROFILE),
        ok(r#"{"result":"reset","resets_left":0}"#),
    ])
    .await;
    assert_eq!(result, ("redeemed".into(), None));
    let profile = requests[0].to_ascii_lowercase();
    assert!(profile.starts_with("get /api/oauth/profile http/1.1"));
    let claim_request = requests[1].to_ascii_lowercase();
    assert!(claim_request.starts_with(
        "post /api/organizations/0f0f0f0f-1111-4222-8333-444444444444/reset_rate_limits http/1.1"
    ));
    for request in [&profile, &claim_request] {
        assert!(request.contains(&format!("authorization: bearer {TOKEN}")));
        assert!(request.contains("anthropic-beta: oauth-2025-04-20"));
        assert!(request.contains("user-agent: claude-cli/2.1.326 (external, cli)"));
        assert!(request.contains("x-app: cli"));
    }
    assert!(claim_request.contains("content-type: application/json"));
    assert_eq!(
        body(&requests[1]),
        json!({"program":"cedar_ember","grant_id":"g-1","request_id":ONE})
    );

    for (steps, state, reason, calls) in [
        (
            vec![ok(PROFILE), ok(r#"{"result":"already_used"}"#)],
            "rejected",
            "already_used",
            2,
        ),
        (
            vec![ok(PROFILE), ok(r#"{"result":"not_limited"}"#)],
            "rejected",
            "not_limited",
            2,
        ),
        (
            vec![ok(PROFILE), ok(r#"{"result":"ineligible"}"#)],
            "rejected",
            "ineligible",
            2,
        ),
        (
            vec![ok(PROFILE), ok(r#"{"result":"unavailable"}"#)],
            "rejected",
            "unavailable",
            2,
        ),
        // Only listed answers are verdicts: any other 4xx proves nothing.
        (
            vec![ok(PROFILE), Step::Reply("400 Bad Request", "{}")],
            "unknown",
            "provider_response_unrecognized",
            2,
        ),
        (
            vec![ok(PROFILE), Step::Reply("404 Not Found", "{}")],
            "unknown",
            "provider_response_unrecognized",
            2,
        ),
        (
            vec![ok(PROFILE), Step::Reply("499 Client Closed", "{}")],
            "unknown",
            "provider_response_unrecognized",
            2,
        ),
        (
            vec![ok(PROFILE), Step::Reply("409 Conflict", "{}")],
            "unknown",
            "provider_conflict",
            2,
        ),
        (
            vec![ok(PROFILE), Step::Reply("408 Request Timeout", "{}")],
            "unknown",
            "provider_conflict",
            2,
        ),
        (
            vec![Step::Reply("401 Unauthorized", "{}")],
            "failed",
            "auth_rejected",
            1,
        ),
        (
            vec![ok(PROFILE), Step::Reply("403 Forbidden", "{}")],
            "failed",
            "auth_rejected",
            2,
        ),
        (
            vec![Step::Reply("500 Error", "{}")],
            "failed",
            "profile_unavailable",
            1,
        ),
        (
            vec![ok(r#"{"organization":{"uuid":"../../escape"}}"#)],
            "failed",
            "organization_unknown",
            1,
        ),
        (
            vec![ok(PROFILE), Step::Reply("429 Too Many", "{}")],
            "unknown",
            "rate_limited",
            2,
        ),
        (
            vec![ok(PROFILE), Step::Reply("503 Unavailable", "{}")],
            "unknown",
            "provider_error",
            2,
        ),
        (
            vec![ok(PROFILE), ok(r#"{"result":"brand_new"}"#)],
            "unknown",
            "provider_response_unrecognized",
            2,
        ),
        (
            vec![ok(PROFILE), ok("not json")],
            "unknown",
            "provider_response_unrecognized",
            2,
        ),
        (vec![ok(PROFILE), Step::Drop], "unknown", "network_error", 2),
    ] {
        let (result, requests) = run(steps).await;
        assert_eq!(result, (state.into(), Some(reason.into())));
        // Exactly the scripted calls: nothing is retried.
        assert_eq!(requests.len(), calls, "{reason}");
    }
}

/// The GLM script posts one card claim to the quota URL's origin and reads the envelope.
#[tokio::test]
async fn glm_redeem_maps_provider_answers() {
    let root = private_root();
    let run = |steps: Vec<Step>, claim: Value| {
        let root = root.path().to_owned();
        async move {
            let (origin, server) = provider(steps);
            let quota_url = format!("{origin}/api/monitor/usage/quota/limit");
            let config = token_config(&root, "glm", "glm-redeem.sh", &quota_url);
            let (outcome, _) = execute(&root, &config, "glm", claim).await;
            (verdict(&outcome), server.join().unwrap())
        }
    };
    let ok = |body| Step::Reply("200 OK", body);
    let card = || claim("123", "five_hour", "PACKAGE");

    let (result, requests) =
        run(vec![ok(r#"{"success":true,"code":200,"data":{}}"#)], card()).await;
    assert_eq!(result, ("redeemed".into(), None));
    let request = requests[0].to_ascii_lowercase();
    assert!(request.starts_with("post /api/biz/customer-package-reset/use http/1.1"));
    assert!(request.contains(&format!("authorization: bearer {TOKEN}")));
    assert!(request.contains("content-type: application/json"));
    assert_eq!(
        body(&requests[0]),
        json!({"targetType":"PERSONAL","resetType":"FIVE_HOUR","recordId":123,"requestId":ONE,"grantType":"PACKAGE"})
    );
    let (result, requests) = run(
        vec![ok(r#"{"success":true,"code":0}"#)],
        claim("card-x", "seven_day", "unknown"),
    )
    .await;
    assert_eq!(result, ("redeemed".into(), None));
    assert_eq!(
        body(&requests[0]),
        json!({"targetType":"PERSONAL","resetType":"WEEK","recordId":"card-x","requestId":ONE})
    );

    for (steps, state, reason, calls) in [
        // No refusal code is documented, so an application failure is never "rejected".
        (
            vec![ok(r#"{"success":false,"code":1234,"msg":"used"}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![Step::Reply("404 Not Found", "{}")],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        // Contradictory envelopes and success outside HTTP 200 are not verdicts.
        (
            vec![ok(r#"{"success":false,"code":200}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![ok(r#"{"success":false,"code":0}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![Step::Reply("201 Created", r#"{"success":true,"code":200}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![ok(r#"{"success":true,"code":1001}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![ok(r#"{"success":false,"code":401}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![Step::Reply("409 Conflict", "{}")],
            "unknown",
            "provider_conflict",
            1,
        ),
        (
            vec![Step::Reply("408 Request Timeout", "{}")],
            "unknown",
            "provider_conflict",
            1,
        ),
        (
            vec![Step::Reply("401 Unauthorized", "{}")],
            "failed",
            "auth_rejected",
            1,
        ),
        (
            vec![ok(r#"{"success":false,"code":1001}"#)],
            "failed",
            "auth_rejected",
            1,
        ),
        (
            vec![ok(r#"{"success":false,"code":429}"#)],
            "unknown",
            "rate_limited",
            1,
        ),
        // An ambiguous HTTP status wins over whatever the body says.
        (
            vec![Step::Reply(
                "409 Conflict",
                r#"{"success":false,"code":1001}"#,
            )],
            "unknown",
            "provider_conflict",
            1,
        ),
        (
            vec![Step::Reply(
                "503 Unavailable",
                r#"{"success":true,"code":200}"#,
            )],
            "unknown",
            "provider_error",
            1,
        ),
        (
            vec![Step::Reply(
                "429 Too Many",
                r#"{"success":false,"code":401}"#,
            )],
            "unknown",
            "rate_limited",
            1,
        ),
        (
            vec![ok(r#"{"success":false,"code":409}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![ok(r#"{"success":false,"code":500}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![Step::Reply("502 Bad Gateway", "oops")],
            "unknown",
            "provider_error",
            1,
        ),
        (
            vec![ok(r#"{"success":true,"code":7}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![ok(r#"{"success":true}"#)],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (
            vec![ok("<html>")],
            "unknown",
            "provider_response_unrecognized",
            1,
        ),
        (vec![Step::Drop], "unknown", "network_error", 1),
    ] {
        let (result, requests) = run(steps, card()).await;
        assert_eq!(result, (state.into(), Some(reason.into())));
        assert_eq!(requests.len(), calls, "{reason}");
    }
    // A card can only reset one of the two windows; other scopes never reach the provider.
    let unreachable = token_config(
        root.path(),
        "glm",
        "glm-redeem.sh",
        "http://127.0.0.1:9/quota",
    );
    let (outcome, _) = execute(
        root.path(),
        &unreachable,
        "glm",
        claim("123", "all", "PACKAGE"),
    )
    .await;
    assert_eq!(
        verdict(&outcome),
        ("failed".into(), Some("unsupported_scope".into()))
    );
    // A refused connection is a definite non-spend.
    let (outcome, _) = execute(root.path(), &unreachable, "glm", card()).await;
    assert_eq!(
        verdict(&outcome),
        ("failed".into(), Some("provider_unreachable".into()))
    );
}

/// The Codex script drives a synthetic app-server through the documented consume method.
#[tokio::test]
async fn codex_redeem_uses_app_server_consume() {
    let root = private_root();
    let login = root.path().join("login");
    std::fs::create_dir(&login).unwrap();
    write_secret(
        &login.join("auth.json"),
        &format!(r#"{{"tokens":{{"access_token":"{TOKEN}"}}}}"#),
    );
    let fake = root.path().join("codex");
    let answer = root.path().join("answer");
    let seen = root.path().join("consume");
    // A line-oriented stand-in for `codex app-server`: no network, no model.
    write_script(
        &fake,
        &format!(
            r#"#!/bin/bash
[[ $1 == app-server && -r $CODEX_HOME/auth.json ]] || exit 1
while IFS= read -r line; do
  case $line in
    *'"initialize"'*) printf '%s\n' '{{"id":1,"result":{{}}}}' ;;
    *'rateLimitResetCredit/consume'*)
      printf '%s\n' "$line" >> '{seen}'
      printf '%s\n' '{{"method":"account/rateLimits/updated","params":{{}}}}'
      cat '{answer}' ;;
  esac
done
"#,
            answer = answer.display(),
            seen = seen.display()
        ),
    );
    let config = format!(
        r#"
[[sources]]
account = "fixture"
provider = "codex"
auth_family = "openai"
credential = "native:codex"
native_directory = "{}"
execution = "external"
models = [{{ id = "model-a" }}]
[sources.harness]
id = "codex"
binary = "{}"
[sources.collector]
command = "/bin/true"
source = "fixture-source"
[sources.redeem]
command = "/bin/bash"
args = ["{}"]
timeout_seconds = 45
"#,
        login.display(),
        fake.display(),
        script("codex-redeem.sh").display()
    );
    for (reply, state, reason) in [
        (r#"{"id":2,"result":{"outcome":"reset"}}"#, "redeemed", None),
        (
            r#"{"id":2,"result":{"outcome":"alreadyRedeemed"}}"#,
            "redeemed",
            Some("already_redeemed"),
        ),
        (
            r#"{"id":2,"result":{"outcome":"nothingToReset"}}"#,
            "rejected",
            Some("nothing_to_reset"),
        ),
        (
            r#"{"id":2,"result":{"outcome":"noCredit"}}"#,
            "rejected",
            Some("no_credit"),
        ),
        (
            r#"{"id":2,"error":{"code":-32601,"message":"Method not found"}}"#,
            "failed",
            Some("codex_method_unsupported"),
        ),
        (
            r#"{"id":2,"error":{"code":-32602,"message":"Invalid params"}}"#,
            "failed",
            Some("codex_method_unsupported"),
        ),
        (
            r#"{"id":2,"error":{"code":-32600,"message":"Invalid request"}}"#,
            "unknown",
            Some("provider_error"),
        ),
        (
            r#"{"id":2,"error":{"code":-32000,"message":"backend unavailable"}}"#,
            "unknown",
            Some("provider_error"),
        ),
        (
            r#"{"id":2,"result":{"outcome":"somethingNew"}}"#,
            "unknown",
            Some("provider_response_unrecognized"),
        ),
    ] {
        std::fs::write(&answer, format!("{reply}\n")).unwrap();
        let (outcome, _) = execute(
            root.path(),
            &config,
            "codex",
            claim("credit-9", "all", "codexRateLimits"),
        )
        .await;
        assert_eq!(
            verdict(&outcome),
            (state.into(), reason.map(str::to_owned)),
            "{reply}"
        );
    }
    let lines = std::fs::read_to_string(&seen).unwrap();
    assert_eq!(lines.lines().count(), 9);
    assert_eq!(
        serde_json::from_str::<Value>(lines.lines().next().unwrap()).unwrap(),
        json!({"id":2,"method":"account/rateLimitResetCredit/consume",
            "params":{"idempotencyKey":ONE,"creditId":"credit-9"}})
    );
    assert!(!lines.contains(TOKEN));
}

/// Failures before the provider call are definite; a broken or missing script is not a spend.
#[tokio::test]
async fn executor_distinguishes_definite_failures_from_unknown_outcomes() {
    let root = private_root();
    let config = token_config(
        root.path(),
        "claude",
        "claude-redeem.sh",
        "http://127.0.0.1:9",
    );
    let run = |config: String, provider: &'static str| {
        let root = root.path().to_owned();
        async move {
            verdict(
                &execute(&root, &config, provider, claim("g", "five_hour", "GRANT"))
                    .await
                    .0,
            )
        }
    };
    // Nothing listens on the discard port: the profile read fails before any claim.
    assert_eq!(
        run(config.clone(), "claude").await,
        ("failed".into(), Some("profile_unavailable".into()))
    );
    assert_eq!(
        run(config.clone(), "other").await,
        ("failed".into(), Some("redeem_not_configured".into()))
    );
    // A claim admitted under other configuration bytes resolves and runs nothing.
    let marker = root.path().join("ran");
    let touching = root.path().join("touch.sh");
    write_script(
        &touching,
        &format!("#!/bin/sh\n: > '{}'\n", marker.display()),
    );
    let mut pinned = claim("g", "five_hour", "GRANT");
    pinned["config_revision"] = json!("0".repeat(64));
    let changed = config.replace(
        &script("claude-redeem.sh").display().to_string(),
        &touching.display().to_string(),
    );
    assert_eq!(
        verdict(&execute(root.path(), &changed, "claude", pinned).await.0),
        ("failed".into(), Some("owner_binding_changed".into()))
    );
    assert!(!marker.exists());
    assert_eq!(
        run(
            config.replace("/bin/bash", "/nonexistent/interpreter"),
            "claude"
        )
        .await,
        ("failed".into(), Some("redeem_script_unavailable".into()))
    );
    let exits = root.path().join("exits.sh");
    write_script(&exits, "#!/bin/sh\ncat >/dev/null\nexit 7\n");
    let broken = config.replace(
        &script("claude-redeem.sh").display().to_string(),
        &exits.display().to_string(),
    );
    assert_eq!(
        run(broken.clone(), "claude").await,
        ("unknown".into(), Some("redeem_script_failed".into()))
    );
    write_script(
        &exits,
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"version\":1,\"state\":\"pending\",\"reason\":null}'\n",
    );
    assert_eq!(
        run(broken.clone(), "claude").await,
        ("unknown".into(), Some("redeem_output_invalid".into()))
    );
    std::fs::remove_file(root.path().join("credential")).unwrap();
    assert_eq!(
        run(broken, "claude").await,
        ("failed".into(), Some("credential_unavailable".into()))
    );

    // A claim for another account is refused before anything else happens.
    write_secret(&root.path().join("credential"), TOKEN);
    let path = root.path().join("config.toml");
    let mut child = Command::new(binary())
        .arg("redeem-execute")
        .arg("--home")
        .arg(root.path().join("worker-home"))
        .arg("--config")
        .arg(&path)
        .args([
            "--account",
            "other",
            "--provider",
            "claude",
            "--source",
            "fixture-source",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            format!("{}\n", {
                let mut other = claim("g", "five_hour", "GRANT");
                other["config_revision"] = json!("0".repeat(64));
                other
            })
            .as_bytes(),
        )
        .await
        .unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "quota redeem-execute: redeem_claim_invalid"
    );
}
