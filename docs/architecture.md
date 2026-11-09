# Architecture

The resident service owns a private SQLite database, collection schedule,
collector failure backoff, and quota calculations. The CLI and stdio MCP
frontend use a versioned, read-only Unix socket API; a read never starts a
collector or a model. The database holds account-bound physical windows,
per-window model membership, history, and durable exhaustion latches.

Configured external collectors receive a private version-1 JSON context on
stdin. Their bounded output is validated before one SQLite transaction updates
facts and revision. When the context contains a direct token, JSON keys and
strings are checked for that exact token. A native-login directory does not
supply a token to that check, which also cannot detect encoded or partial leaks;
configured collector code must therefore be trusted. A failed round
preserves earlier observations and their original times. The supplied Codex,
Claude and GLM collectors are external processes. Their source files under
`scripts/collectors/` are embedded in the binary and may be exported explicitly.
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

Forecasts and advisory recommendations retain the agent-run calculation rules.
Unknown, expired, failed, and exhausted observations remain distinguishable.
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
