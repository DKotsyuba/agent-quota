# Architecture

The resident service owns a private SQLite database, collection schedule,
collector failure backoff, and quota calculations. The CLI and stdio MCP
frontend use the read methods of a versioned Unix socket API; a read never starts a
collector or a model. The database holds account-bound physical windows,
per-window model membership, history, durable exhaustion latches, and the
latest banked reset and paid credit balance observations per account.

Configured external collectors receive a private version-1 JSON context on
stdin. Their bounded output is validated before one SQLite transaction updates
facts and revision. When the context contains a direct token, JSON keys and
strings are checked for that exact token. A native-login directory does not
supply a token to that check, which also cannot detect encoded or partial leaks;
configured collector code must therefore be trusted. A failed round
preserves earlier observations and their original times. The supplied Codex,
Claude, GLM and OpenCode Go collectors are external processes. Their source files under
`scripts/collectors/` are embedded in the binary and may be exported explicitly.
A Claude source may opt in to OAuth renewal: before a due collection of a token
near expiry on native Linux, Quota checks the trusted store path, pinned CLI and unmanaged OAuth profile
before starting one isolated print-mode turn. The remote-settings cache may contain
an optional boolean `channelsEnabled` and must pass the policy file safety checks.
Claude Code renews its own store.
A directory-anchored lock and synced ledger cap attempts at six per store in a
trailing day and fence retries after unconfirmed cleanup. Quota checks the new
expiry against the current clock before collecting and never writes tokens.
Collectors normally execute as the service's OS user. An external probe and
owner-only ingest path can be launched separately under the credential owner.
An optional root-managed Linux launcher normally serializes those invocations;
the owner chose automatic continuation after uncertain outcomes. Synthetic tests
exercise the actual launcher with distinct UIDs in a network-isolated container.
On dobi, the packaged service also ran under the user systemd manager with an
isolated home and obtained a real Codex quota observation through its API. The
root-managed launcher's host PAM and systemd lifecycle passed a privileged two-UID
qualification on dobi (Ubuntu 24.04.5 LTS x86-64) with an earlier candidate of the
same source line, not the exact release bytes. Other hosts remain unqualified.

An owner may redeem one banked reset credit through a separately authorized
socket method (`--allow-redeem-uid`, default none). The service only records
the idempotent request in its database and writes a trigger file in its private
home. A root-managed launcher takes the request once, lets the credential owner
run the configured redemption script, reports the outcome and collects again.
The service still reads no credential and makes no provider call; an ambiguous
outcome is stored as `unknown` and never retried. The service identity is
trusted to spend credits; the credential owner only limits the rate. Redemption is not an MCP tool.
See the [redemption API](api.md#reset-credit-redemption).

Forecasts and advisory recommendations retain the agent-run calculation rules.
Unknown, expired, failed, and exhausted observations remain distinguishable.
The account-free capacity order uses Agent Run's window score and tie rules,
with a credit adjustment for usable reset and paid capacity. A scored model
row publishes its native lane and window details from the account and lane
that supplied its best plan priority. See the [capacity API](api.md#capacity-order).
A service result is a timed observation, not a reservation or an atomic promise
about a later agent-run launch. The owner chose unknown fallback when this
service is unavailable; native quota refusals are not sent back to this service.

The socket authenticates clients by OS UID; an additional UID needs an explicit
allowlist and a separately prepared group-accessible socket directory. Clients
verify the configured service UID. The private state home must belong to the
service user and have no group/other access. No service registration, provider
login, release, or host qualification is performed by the application startup.

MCP definitions remain Rust-authoritative in `src/tools/mod.rs`, with the
reviewed export in `schemas/tools.json`. MCP text uses bounded typed views and
embedded MiniJinja templates; machine clients use the JSON socket or CLI.
Agent-run's client, admission, account switching and public reads have synthetic
coverage. The source-level transition is not deployed; the Linux launcher was qualified
on dobi only (see the dobi verification section of the integration design).
Automatic Agent Bridge warning delivery is deferred. Existing
quota history is intentionally not imported. See
[integration design](quota-integration.md) for the remaining scope.
