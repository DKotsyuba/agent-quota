//! Synthetic checks for the optional Linux credential-owner launcher.

#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::expect_used, reason = "Synthetic fixture setup must succeed")]
/// Checks trust, handoff, failure handling and the shared lock without changing UID.
fn owner_launcher_contract() {
    launcher_check("owner-collect.sh");
}

#[cfg(target_os = "linux")]
#[test]
/// Checks the redeem mode: one claim, one execution, identical bounded report retries.
fn owner_launcher_redeem_contract() {
    launcher_check("owner-redeem.sh");
}

#[cfg(target_os = "linux")]
#[allow(clippy::expect_used, reason = "Synthetic fixture setup must succeed")]
/// Runs one synthetic launcher script in a private directory.
fn launcher_check(name: &str) {
    let directory = tempfile::tempdir().expect("private launcher fixture");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts/systemd/tests")
        .join(name);
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
