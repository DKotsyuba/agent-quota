# agent-quota

`agent-quota` collects provider quota observations into its own local database,
computes the existing `agent-run` forecasts and advice, and serves them through
a local API, CLI, and stdio MCP. On dobi, an isolated user-systemd service
collected real Codex quota metadata and returned it through the local API, and the
separate root-managed two-UID Linux launcher passed a host PAM/systemd qualification.
Source-level integration with `agent-run` has been exercised with synthetic data;
it has not been deployed. Automatic Agent Bridge notifications are deferred.
Existing quota history is intentionally not imported.

The [integration design](docs/quota-integration.md) records the migration
boundary and remaining work; the implemented wire contract is in [local API](docs/api.md).

Read AGENTS.md and docs/MCP_RESPONSE_STANDARD.md before adding tools.

## Tools

| Tool | Effect | Purpose |
|---|---|---|
| `get_status` | read | product identity and release qualification status |
| `get_quota` | read | stored quota metrics and advice; optional account/model filters |

`get_quota` never contacts a provider. Narrow the scope when a result exceeds
the text response budget. Use CLI JSON or the [local API](docs/api.md) for the
full machine-readable response.

## Run locally

Copy [examples/config.toml](examples/config.toml) to an operator-controlled
location and replace its placeholder paths and model IDs. Configure only
credential references; do not put token values in the file. The service home
must be an absolute owner-private directory; if absent, its parent must exist.
See [configuration](docs/configuration.md) for account and credential options.

```bash
agent-quota serve --home /absolute/private-home --config /absolute/config.toml
agent-quota service-status --socket /absolute/private-home/quota.sock
agent-quota limits --socket /absolute/private-home/quota.sock --account account-example
agent-quota mcp --socket /absolute/private-home/quota.sock
```

When paths are omitted, the home defaults to `AGENT_QUOTA_HOME` or
`$HOME/.agent-quota`, and the socket to `AGENT_QUOTA_SOCKET` or `quota.sock`
in that home. A service running as another UID must be selected explicitly with
`--server-uid`; cross-UID readers also require a prepared shared socket directory
and the service's `--allow-uid`. The API validates both socket owner and peer.
The supplied collector scripts need Bash and jq; the HTTP collectors also need
curl. Their exact bytes and the optional Linux launcher are embedded in the
binary; export them to a new directory under an existing owner-private parent:

```bash
agent-quota export-assets --directory /absolute/private-parent/quota-assets
agent-quota verify-assets --directory /absolute/private-parent/quota-assets
```

Configure collector paths under the exported `collectors/` directory. Exports
never overwrite an existing directory; an interrupted export may leave a partial
directory to inspect. A separate credential owner can use the external-probe
path or review the [optional Linux launcher](docs/linux-owner-collection.md).
`doctor --json` reports the frontend and service separately and exits nonzero
when the service cannot be reached; it does not query providers.

## Development

```bash
cargo fetch --locked
cargo xtask check
cargo xtask add-tool NAME --effect read --response page
cargo xtask test protocol
cargo xtask test presentation
cargo xtask test delivery
cargo run --locked -- doctor --json
cargo run --locked -- mcp
```

Product source is in src/, the authoritative tool registry and embedded tool
templates are in src/tools/mod.rs, and the exported discovery snapshot is in
schemas/tools.json. `cargo xtask contract update` is an explicit reviewed
snapshot change. Generated tool stubs do not perform effects or return fake
success; finish their contract and tests.

Cargo.lock must be reviewed and committed.

For a short edit loop, fetch the locked dependencies once, run formatting
directly, then check the changed crate or run its relevant test target:

```bash
cargo fmt --all --check
cargo check --frozen -p agent-quota --all-targets --all-features
cargo nextest run --frozen -p agent-quota --all-features -E 'test(/^protocol::/)'
```

Select the changed workspace crate and test filter as appropriate. These focused
commands give early feedback; finish a coherent change with `cargo xtask check` (requires cargo-nextest)
and its applicable protocol/presentation/delivery checks before a PR. Changes to
embedded templates or other compile-time assets require the same checks as code.
Optimized packaging belongs to delivery, not every edit iteration.

## Local delivery

Commit your source first. No remote repository is required to build a local package.

```bash
cargo xtask package
cargo xtask package verify dist/agent-quota-0.1.0-x86_64-unknown-linux-gnu
```

The bundle executable accepts `self-install --bundle PATH --home ABS_PATH --bin-dir ABS_PATH`.
Create the bin directory explicitly. Installation preserves immutable versions, never restarts
services and does not edit MCP host configuration. `releases use VERSION` verifies and selects
a retained compatible version. Quota data are stored outside `standalone/`; external effects are
not undone by a binary rollback. No automatic pruning or removal of user data.
The single-binary bundle contains the collector and launcher bytes. Exporting or
installing those files remains an explicit operator action, separate from binary
installation and service registration.

CI restores Cargo dependency builds and a versioned cargo-deny tool cache. Successful
main check jobs warm the check cache; an independent package job (main pushes and manual
runs only) builds, verifies and archives a 7-day Linux candidate artifact without
publishing a release. This adds an optimized build to main CI, which runs alongside
checks and costs extra compute on cold inputs. PRs use the check cache without the
package producer. CI is Linux x86-64 only; no macOS job is required.
Release jobs restore both workloads without writing competing cache snapshots.
Cache misses run the normal commands. The gate still
runs on cache hits: workspaces without features need one all-features Clippy/test
pass; workspaces with features retain default and all-features passes. Platform,
compiler, dependency and build-setting changes invalidate the relevant cache.
Release packages and their integrity/acceptance evidence remain separate artifacts.

The release workflow targets Linux x86-64 on Ubuntu 24.04; macOS remains an
unqualified candidate and no macOS support is claimed. The release source check and exact payload build run in
parallel. Publication waits for both to succeed, then preserves environment approval and tests the
downloaded payload's exact bytes. Packaging alone does not enable release publication
or establish native host qualification; both are recorded in family.toml.

Third-party licenses: `LICENSE` is the MIT license of this source. `THIRD_PARTY_NOTICES` is
generated from the locked dependency set and the crate sources Cargo unpacked
(`cargo xtask notices`; `cargo xtask notices --check` and the full gate fail on drift, on a
dependency without a license text, or on a non-registry source). Both files are published as
separate release assets beside the binary, manifest and installer and are listed in
`SHA256SUMS`; they are not part of the strict two-file bundle, which still rejects extras.

The release workflow is guarded by release.enabled and the qualification settings in family.toml
(`verified`, `x86_64-unknown-linux-gnu`, and the dobi host). Evidence for `x86_64-unknown-linux-gnu` on dobi (Ubuntu 24.04.5 LTS x86-64) comes from earlier candidates of the same source line, not the exact release bytes: a real Codex quota metadata read through an isolated same-UID user-systemd service (candidate `3a704d5`), a real Claude quota metadata read by the executing user's own native login (candidate binary SHA-256 `2d3c142b`, 2026-11-09), and a privileged two-UID PAM/systemd launcher qualification of that same candidate (run `9e478e83da01`, all observations passed). No model turn was started. Real GLM logins and macOS were not tested.
Version preparation is `cargo xtask release prepare VERSION --apply`; inspect/commit the changes,
then create and push an annotated tag yourself. Publishing tests the exact shipped executable.

## Template updates

`cargo xtask template diff --from /trusted/template/checkout` compares original managed bytes,
local changes and the next template. `template upgrade --dry-run` writes nothing.
Apply the reviewed plan through Git; conflicting local changes and Cargo dependencies need review.
The build has no live dependency on the private template repository.
