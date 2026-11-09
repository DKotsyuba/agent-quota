//! Synthetic checks of the release bootstrap without network or real credentials.

#[cfg(unix)]
#[test]
#[allow(clippy::expect_used, reason = "Synthetic fixture setup must succeed")]
/// Exercises platform selection, verified execution and safe failure paths.
fn installer_contract() {
    let directory = tempfile::tempdir().expect("private installer fixture");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/install.sh");
    let output = std::process::Command::new("/bin/bash")
        .arg(script)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", directory.path())
        .env("TMPDIR", directory.path())
        .env("TMP", directory.path())
        .env("TEMP", directory.path())
        .env("AQ_TEST_TMP", directory.path())
        .output()
        .expect("run synthetic installer check");
    assert!(
        output.status.success(),
        "installer check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
