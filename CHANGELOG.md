# Changelog

No published release is implied by the Cargo package version.

## Unreleased

### Added

- Reproducible `THIRD_PARTY_NOTICES` (`cargo xtask notices [--check]`) from the locked dependency set; the full gate fails on drift.
- Release publication uploads and checksums `LICENSE` and `THIRD_PARTY_NOTICES` beside the binary; the strict two-file bundle is unchanged.
- Linux CI package job also runs on manual dispatch and archives a verified candidate artifact.

## 0.1.0

### Changed

- Prepare the installer, CI and release workflow for Linux x86-64 on Ubuntu 24.04.
- Audit dependencies of every workspace member before release.
- Run release checks and payload construction in parallel while keeping publication gated on both.
- Seed separate check/package dependency caches from main; document focused checks for local iteration.
- Reuse Cargo dependency builds and versioned cargo-deny tooling in CI and release jobs.
- Run Clippy/tests once for featureless workspaces; retain both default and all-features configurations when workspace features exist.

### Fixed

- Modern MCP 2026-07-28 tools/list includes a 60000 ms private cache lifetime; legacy catalog responses stay unchanged.
- Raw stdio regression tests cover modern/legacy tools/list and modern server/discover; supported protocol revisions are explicit.

### Added

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
