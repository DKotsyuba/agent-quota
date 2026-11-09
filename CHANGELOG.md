# Changelog

## 0.1.0

### Added

- Linux x86-64 (`x86_64-unknown-linux-gnu`) release qualified on dobi (Ubuntu 24.04.5 LTS x86-64):
  `family.toml` records `qualification = "verified"` and enables release publication. Evidence came from
  earlier candidates of the same source line, not the exact release bytes: a real Codex quota metadata
  read (candidate `3a704d5`), a real Claude quota metadata read (candidate binary SHA-256 `2d3c142b`) and
  a privileged two-UID PAM/systemd launcher qualification of that candidate (run `9e478e83da01`).
  No model turn was started; real GLM logins and macOS are untested and unclaimed.
- Reproducible `THIRD_PARTY_NOTICES` (`cargo xtask notices [--check]`) from the locked dependency set; the full gate fails on drift.
- Release publication uploads and checksums `LICENSE` and `THIRD_PARTY_NOTICES` beside the binary; the strict two-file bundle is unchanged.
- Linux CI package job also runs on manual dispatch and archives a verified candidate artifact.
- Single-binary embedded collector and Linux launcher assets, with explicit
  no-overwrite export and verification against the running executable.
- Resident quota service with local SQLite history, transferred collector,
  exhaustion-latch, forecast, risk, and advisory behavior from agent-run.
- Versioned local socket API, `limits` and `service-status` CLI commands, and
  the read-only `get_quota` MCP tool. Agent-run integration is in progress and
  has not been deployed.
- Credential-owner external probe and owner-only ingest for separately managed
  OS identities; automatic Bridge notifications remain deferred.
- Optional, uninstalled Linux owner-UID probe and ingest launcher with a shared
  lock and explicit continuation after uncertain outcomes.
- Initial agent-quota working set: the read-only `get_status` identity tool and
  the local development gate.

### Changed

- Update `rmcp` to 3.9.0 and `rusqlite` to 0.41.1 (RUSTSEC-2026-0457, -0459, -0461, -0462); EOF before a
  stateless client's first request is a clean stdio close.
- Prepare the installer, CI and release workflow for Linux x86-64 on Ubuntu 24.04.
- Audit dependencies of every workspace member before release.
- Run release checks and payload construction in parallel while keeping publication gated on both.
- Seed separate check/package dependency caches from main; document focused checks for local iteration.
- Reuse Cargo dependency builds and versioned cargo-deny tooling in CI and release jobs.
- Run Clippy/tests once for featureless workspaces; retain both default and all-features configurations when workspace features exist.

### Fixed

- Modern MCP 2026-07-28 tools/list includes a 60000 ms private cache lifetime; legacy catalog responses stay unchanged.
- Raw stdio regression tests cover modern/legacy tools/list and modern server/discover; supported protocol revisions are explicit.
