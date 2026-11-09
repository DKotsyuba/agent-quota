//! Synthetic checks for the optional Linux credential-owner launcher.

#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::expect_used, reason = "Synthetic fixture setup must succeed")]
/// Checks trust, handoff, failure handling and the shared lock without changing UID.
fn owner_launcher_contract() {
    let directory = tempfile::tempdir().expect("private launcher fixture");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/systemd/tests/owner-collect.sh");
    let output = std::process::Command::new("/bin/bash")
        .arg(script)
        .env("AQ_TEST_TMP", directory.path())
        .output()
        .expect("run synthetic launcher check");
    assert!(
        output.status.success(),
        "launcher check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
