//! Black-box checks for the files embedded in the single executable.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "Explicit fixture assertions"
)]

use std::{
    fs,
    os::{
        fd::OwnedFd,
        unix::{
            ffi::OsStringExt,
            fs::{PermissionsExt, symlink},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

/// Resolves an explicitly supplied shipped binary or the Cargo-built product.
fn binary() -> PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_agent-quota")))
}

/// Runs the selected executable using only the test's temporary environment.
fn assets_command(operation: &str, directory: &Path, temporary: &Path) -> Output {
    Command::new(binary())
        .arg(operation)
        .arg("--directory")
        .arg(directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", temporary)
        .env("TMP", temporary)
        .env("TEMP", temporary)
        .output()
        .expect("run asset command")
}

/// Exports the embedded files, detects tampering and never overwrites an existing target.
#[test]
fn export_and_verify_embedded_assets() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let directory = root.path().join("assets");
    let exported = assets_command("export-assets", &directory, root.path());
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert_eq!(report["files"], 11);
    assert_eq!(report["verified"], true);
    let codex = directory.join("collectors/codex.sh");
    assert_eq!(
        fs::read(&codex).unwrap(),
        include_bytes!("../scripts/collectors/codex.sh")
    );
    assert_eq!(
        fs::metadata(&codex).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::read(directory.join("systemd/agent-quota-owner-collect@.service")).unwrap(),
        include_bytes!("../scripts/systemd/agent-quota-owner-collect@.service")
    );
    assert!(
        assets_command("verify-assets", &directory, root.path())
            .status
            .success()
    );

    let extra = directory.join("collectors/extra");
    fs::write(&extra, "unexpected").unwrap();
    assert!(
        !assets_command("verify-assets", &directory, root.path())
            .status
            .success()
    );
    fs::remove_file(extra).unwrap();
    assert!(
        assets_command("verify-assets", &directory, root.path())
            .status
            .success()
    );

    let repeated = assets_command("export-assets", &directory, root.path());
    assert!(!repeated.status.success());
    assert!(
        assets_command("verify-assets", &directory, root.path())
            .status
            .success()
    );

    let sentinel = root.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let link = root.path().join("link");
    symlink(&sentinel, &link).unwrap();
    assert!(
        !assets_command("export-assets", &link, root.path())
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "keep");

    fs::write(&codex, "changed").unwrap();
    assert!(
        !assets_command("verify-assets", &directory, root.path())
            .status
            .success()
    );
    assert_eq!(fs::read_to_string(&codex).unwrap(), "changed");
    fs::remove_file(&codex).unwrap();
    fs::write(&sentinel, include_bytes!("../scripts/collectors/codex.sh")).unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o700)).unwrap();
    fs::hard_link(&sentinel, &codex).unwrap();
    assert!(
        !assets_command("verify-assets", &directory, root.path())
            .status
            .success()
    );
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        include_bytes!("../scripts/collectors/codex.sh")
    );
}

/// A non-private parent or a relative path cannot receive an asset export.
#[test]
fn export_requires_a_private_parent_and_absolute_path() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let shared = root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        !assets_command("export-assets", &shared.join("assets"), root.path())
            .status
            .success()
    );
    assert!(!shared.join("assets").exists());
    assert!(
        !assets_command("export-assets", Path::new("relative-assets"), root.path())
            .status
            .success()
    );

    let invalid = root.path().join(std::ffi::OsString::from_vec(vec![0xff]));
    assert!(
        !assets_command("export-assets", &invalid, root.path())
            .status
            .success()
    );
    assert!(!invalid.exists());
}

/// A lost CLI response identifies the completed export so it can be verified.
#[test]
fn export_reports_success_even_when_its_output_is_closed() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let directory = root.path().join("assets");
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    let output_fd: OwnedFd = writer.into();
    let output = Command::new(binary())
        .arg("export-assets")
        .arg("--directory")
        .arg(&directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", root.path())
        .env("TMP", root.path())
        .env("TEMP", root.path())
        .stdout(Stdio::from(output_fd))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("assets_export_succeeded_output_failed")
    );
    assert!(
        assets_command("verify-assets", &directory, root.path())
            .status
            .success()
    );
}
