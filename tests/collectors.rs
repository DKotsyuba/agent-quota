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

/// Serves exactly one local fixture response and returns the captured HTTP request.
fn endpoint(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}/quota", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
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
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        String::from_utf8(bytes).unwrap()
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
    ] {
        let (url, server) = endpoint(body);
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
        assert!(
            server
                .join()
                .unwrap()
                .to_ascii_lowercase()
                .contains(authorization)
        );
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
