# Changelog

## Unreleased

### Changed

- Paid credit balances are shown rounded to two decimals without a currency sign;
  credits are not dollars.

## 0.4.0

### Added

- Expose the optional Codex paid credit balance per account in socket snapshots
  and CLI JSON, preserving its exact decimal representation and unknown state.

### Changed

- Report one changed-only quota outage line per previously updated hook session.
- Show account and model narrowing filters and candidate identifiers for oversized reads.
- Detect resident service version skew in MCP and doctor. MCP invalidates the tool
  catalog once and asks the host to reconnect.

- Rank provider capacity with usable banked resets and available Codex paid
  credits, while preserving the plan's status and window priority in the local API.
- Show all model windows, UTC resets, rounded percentages, stale evidence and
  risk reasons in quota text. Show the paid Codex balance in `get_quota`.
- Inject quota context on the first prompt and significant changes per host
  session. The `SessionStart` hook refreshes context after compact, clear or resume.

### Security

- Provide a resident Linux unit and tmpfiles rule with no privileged startup
  command beneath a service-writable directory. See the installed-host migration
  in the Linux owner collection guide.

## 0.3.0

Dated 2026-11-24.

### Added

- Owner-requested redemption of one banked reset credit for Claude, Codex and GLM. New socket methods `redeem_reset_credit` and `redeem_status` are idempotent per client `request_id` and are open only to UIDs named by the new `serve --allow-redeem-uid` flag (default none). `agent-quota redeem` is their CLI client. They are not MCP tools.
  The service stays credential-free: it records the request and writes a trigger file under its home. The new root-managed `agent-quota-owner-redeem@.path` and `.service` units run the launcher's new `redeem` mode, which executes the new `[sources.redeem]` script (`claude-redeem.sh`, `codex-redeem.sh`, `glm-redeem.sh`) as the credential owner and then collects again. An ambiguous provider outcome is stored as `unknown` and never retried. The service OS identity is trusted to spend credits of accounts that have `[sources.redeem]`; the credential owner executes at most one redemption per account in 120 seconds.
  Upgrading needs no change for existing deployments; redemption stays off until the flag, the source table and the units are added. See `docs/linux-owner-collection.md`.

## 0.2.4

Dated 2026-11-24.

### Fixed

- Claude OAuth renewal accepts Team subscription logins as well as Pro and Max; Enterprise logins remain refused. A Team login previously failed with `credential_refresh_unsupported_host` once its access token expired, leaving Claude quota unknown.
- Codex reset credits of kind `codexRateLimits` (full reset of the 5-hour and weekly windows) report scope `all` instead of `unknown`.

## 0.2.3

Dated 2026-11-24.

### Added

- Collect banked reset credits for Claude, Codex and GLM. Account results in CLI `limits`, IPC snapshots and MCP `get_quota` expose the latest observed inventory and distinguish unknown availability from zero.
  Claude grants are requested with the Claude Code CLI client headers, because the usage service reports them only to that surface; an ineligible surface reads as unknown.

## 0.2.2

Dated 2026-11-23.

### Fixed

- Claude OAuth renewal accepts the pinned CLI's benign remote-settings cache with a boolean `channelsEnabled`. Unsafe or unrecognized caches still block renewal and identify the cache check in diagnostics.
  The final cache check leaves the same trusted-writer race as the previous absence check. Quota accepts updates by the login owner or root in that window. Other unprivileged UIDs have no write path.

## 0.2.1

Released 2026-11-19. Qualification in `family.toml` is unchanged from 0.1.1: Linux x86-64 on dobi, evidence from earlier candidates of the same source line, not the exact release bytes. Claude OAuth renewal is off by default and has not been exercised against a live login.

### Added

- On native Linux, `refresh_oauth = true` in `[sources.harness]` enables one isolated Claude Code 2.1.322 Haiku turn to renew an unmanaged personal Claude login within five minutes of expiry. Renewal requires a trusted store path and allows at most six turns per store in a trailing 24-hour window. Quota never writes tokens. Live OAuth renewal has not been exercised.

### Changed

- A Claude OAuth token past its recorded `expiresAt` is no longer sent to the provider; collection fails with `credential_expired`, `credential_refresh_failed` when an attempted renewal left it expired, or `credential_refresh_limited` when the renewal cap prevented an attempt.
- The owner-collection launcher appends an allowlisted probe failure code to its journal message (for example `probe failed: credential_expired`); other diagnostics stay private.

## 0.2.0

Released 2026-11-19. Qualification in `family.toml` is unchanged from 0.1.1: Linux x86-64 on dobi, evidence from earlier candidates of the same source line, not the exact release bytes. The capacity ranking has not been exercised against live Agent Run integration on dobi.

### Added

- Capacity ranking moved from agent-run: optional source `priority_multiplier` (account weight per provider binding; an account's weight on a model is the maximum of its bindings) and `[providers.<alias>]` tables with `priority_multiplier` and `ordered_accounts`. Omitted values are neutral, so existing configurations stay valid.
- `get_capacity` MCP tool, `agent-quota capacity` CLI and protocol-v1 socket method `capacity`: account-free ranked providers and models with status `available`, `limited` (known capacity, high risk on every known account), `unknown`, `priority_overflow` or `exhausted`, best priority, remaining percentage, limiting window, reset, risk, evidence freshness and per-window details. It has no account or pool identities. The capacity result has its own `schema_version` 1.
- `agent-quota hook context`: read-only, fail-soft `UserPromptSubmit` block with the provider capacity order, in the line format of agent-run's former block.

### Changed

- `get_status` describes the product name, version and release qualification instead of the template's "scaffold qualification status".

## 0.1.1

Released 2026-11-15. Qualification in `family.toml` is unchanged from 0.1.0: Linux x86-64 on dobi, evidence from earlier candidates of the same source line, not the exact release bytes. The OpenCode Go collector has not been exercised against a real OpenCode Go login.

### Added

- OpenCode Go collector (`collectors/opencode-go.sh` and `.jq`, embedded in the binary): reads the account-level usage endpoint with a bearer key and reports `five_hour`, `seven_day` and `monthly` windows for every configured model; unknown statuses are unknown, schema drift fails closed.

### Changed

- Stale test scratch cleanup accepts only canonical decimal PIDs and is skipped when the PID namespace is unknown.
- Gate test scratch directories are named with the PID namespace and a random suffix, and stale cleanup only removes directories of exited processes from the same namespace.
- The integration-test check ignores commented-out module lines and requires `autotests = false`.
- CI installs cargo-nextest into a runner-owned directory added to `PATH` (macOS runners lack `~/.cargo/bin`).
- `cargo xtask check` follows the Rust gate standard: tests run under cargo-nextest (required; CI installs 0.9.149 from a checksum-verified archive) with every failure reported, doctests via `cargo test --doc`, and test temporaries in a fresh tmpfs directory (`XTASK_TEST_TMPFS=0` opts out). Integration tests build into one `it` binary: every `tests/*.rs` must be a module of `tests/it/main.rs`. Release acceptance runs `--test it <suite>::` filters and refuses a filter that selects no tests. The dev profile keeps line-table debug info only (`--profile debugging` for full).

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
