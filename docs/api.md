# Local API and external probes

The resident service exposes protocol version 1 as one newline-delimited JSON
request and response per Unix socket connection. Reads use persisted observations;
they do not run collectors or start model turns. The service and client authenticate
the local peer by UID. The default socket is owner-only; a separate explicitly
allowlisted client UID requires an operator-prepared setgid socket directory.
Clients must configure the expected service UID when it differs from their own.

## Read requests

```json
{"version":1,"method":"status","accounts":[]}
{"version":1,"method":"snapshot","accounts":["account-example"],"model":"model-example"}
```

For `snapshot`, omitted or empty `accounts` means all configured accounts;
`model` is optional and matches a configured or native model name. The result is
`{"version":1,"result":{...}}`; a failure is
`{"version":1,"error":"safe_code"}`. `status` reports the latest collection
round and external-ingest status without credentials or raw collector output.
The client must reject unsupported protocol or snapshot schema versions and
unknown state values rather than treating them as available capacity.

The response envelope has exactly `version` plus either `result` or `error`;
unknown envelope fields and simultaneous result/error are invalid. The current
snapshot schema is `2`; the transport envelope remains version `1`. Within a
version-2 snapshot, additional informational fields may be added without a
schema-version change. Consumers may ignore those fields, but must validate the
required fields, types, bounds, identities, and enum values they use. Existing
field meanings, units, required fields, and recognized state values remain
stable. A change that affects how an older consumer interprets a result, or
requires a new field for a safe decision, requires a new snapshot schema version;
a change to the envelope requires a new protocol version.

The checked-in synthetic contract examples are
[request](../tests/fixtures/snapshot-request-v1.json),
[known snapshot](../tests/fixtures/snapshot-v2.json),
[unknown snapshot](../tests/fixtures/unknown-snapshot-v2.json),
[exhausted snapshot](../tests/fixtures/exhausted-snapshot-v2.json),
[numeric overflow snapshot](../tests/fixtures/overflow-snapshot-v2.json), and
[unknown-account error](../tests/fixtures/unknown-account-v1.json). Regression
tests build these snapshots from real store and projection code at fixed
times, replacing only the random source epoch and configuration hash. The old
snapshot-v1 fixtures are historical compatibility examples and are not served.

The snapshot includes the persistent database `source_epoch`, committed quota
`revision`, exact-byte `config_revision`, `evaluated_at`, account results, and
coverage. Each account includes configured model states (`known`, `unknown`,
or `exhausted`), physical pools, limiting window, remaining percentage, burn
rate in percentage points per hour, reset time, risk, evidence and quota score.
For known usable data, `quota_score` is `1 + limiting_window.slack`, matching
agent-run before its own provider multipliers and local policy are applied.
Window details carry exact source, runtime, lane, target, membership, observation
and validity times, forecast, and window explanation. `last_remaining_percent`
and `last_reset_at` are nullable raw facts from the latest sample and remain
available when that sample is stale; they do not establish freshness.
Timestamps are Unix seconds; nullable values remain unknown, not zero. Active latches are independent of
sample freshness. Advice and its noise-tolerant key are returned per account.
The normal collector and external-ingest paths require `observed_at` and supply
`valid_until` when omitted. An unsupported manually written or legacy row that
lacks either bound is not reported as a fresh known forecast; its raw last value
does not become confirmed quota evidence. Existing latches remain independent.
If a derived number exceeds the finite range, it is returned as null. The latest
sample can still be fresh for diagnostics; a window whose original forecast is
unsuitable for ranking has no `evaluation` and defers its model as unknown
unless an active governing latch takes precedence. Model `evidence: "stale"`
can therefore indicate unusable ranking evidence even when an individual sample
is fresh. This does not hide healthy models or active latches. An unrepresentable estimate
that is not used by the ranker, such as the sustainable rate, does not by itself
invalidate an otherwise usable score.

Each model has a required `state_validity` tagged object. It describes when
the model's **state class** can change as time passes, assuming no newer Quota
observation or configuration change:

- `{"kind":"until","at":...}` supplies an exclusive Unix-seconds boundary.
  For a known state or an unlatched fresh zero, it is the earliest validity or
  reset boundary of all governing windows. An active finite-reset exhaustion
  latch is bounded by the earliest active latch reset, even if its sample is
  stale. A boundary may equal `evaluated_at`; such a result is already unusable
  for a later decision.
- `{"kind":"latched_no_reset"}` means an active governing exhaustion latch
  has no clock reset in this particular snapshot. It is not proof of indefinite
  freshness or immunity from later observations.
- `{"kind":"unknown"}` accompanies an unknown state and grants no decision.

Consumers must request a fresh snapshot for each new decision and after a
local selection retry. Immediately before local admission, they must reject a
clock earlier than `evaluated_at` and require `now < at` for `until`; an expired
result needs another request or an unknown fallback. No maximum response age
has been selected. `state_validity` does not freeze `quota_score`, risk, or
limiting-window order, which are estimates at `evaluated_at` and can change
before the boundary. A later service update can also change the state between
the response and admission; neither the deadline nor a version check provides a
cross-service transaction. A clock boundary can pass after a local check and
before commit, so the check must not be described as global time atomicity.

The revision is not a guarantee that a time-dependent estimate stays unchanged:
a window can expire or reset without a new observation. `coverage.state` is
`unknown` before a completed round, `partial` for failures or unobserved sources,
and `complete` after the current scheduled round succeeds and each external
source has supplied an observation under that configuration. A successful
collector can still report no windows. Coverage does not convert stale facts
into fresh ones; each model's `state`, `evidence`, and times must be inspected.
If a client splits an oversized request, equal source epoch and revision do not
make independently evaluated replies one atomic time snapshot. The client must
not silently combine them into a complete simultaneous ranking.

Requests are capped at 64 KiB for reads, 256 account IDs, and 256 bytes for a
model name. Responses are capped at 4 MiB; the service refuses oversized views
and retained histories over 50,000 rows per account instead of silently cutting
them. Narrow the account/model scope when `response_too_large` is returned;
`history_too_large` requires operator attention. One absolute five-second I/O
deadline applies to each socket exchange. The CLI `limits` prints the same
snapshot JSON; MCP `get_quota` renders at most 20 model rows and asks for a
narrower scope rather than omitting rows.

## Credential-owner probe

An optional source configured with `execution = "external"` is not polled by the
resident service. An operator can run the same binary as the credential-owning
OS user to produce a bounded, token-free fenced observation:

```bash
agent-quota probe --home /absolute/worker-private-home \
  --config /absolute/shared-nonsecret-config.toml \
  --account account-example --provider codex --source codex-appserver
```

The probe resolves credentials only under its own UID, executes the configured
collector, rejects a credential echoed in JSON, and writes a compact envelope to
stdout. The service owner can pipe that envelope to:

```bash
agent-quota ingest --socket /absolute/private-home/quota.sock
```

Only the service UID may ingest, even when other UIDs can read the socket. The
service re-reads and compares the exact configuration revision, binds account,
provider, source and models to its own configuration, validates the observation,
and commits through its single writer. Invalid, empty, oversized, or changed
configuration results do not replace the last good data. Configuration can
still change after the last file check and before commit; the fence is not a
filesystem/database transaction.

Ingestion does not automatically retry. If its reply is lost after a possible
write, the CLI returns `outcome_unknown`; do not resend as a new operation.
The current storage logic, inherited from agent-run, does not provide an
idempotent external receipt or a general out-of-order observation fence. An
operator scheduling external probes must prevent overlapping normal invocations
for the same physical account and its sources through the ingest response. The
optional launcher serializes invocations, but the owner chose automatic
continuation after an interrupted probe or an unknown ingest outcome: an earlier
operation may still finish later. Different sources for one physical window also
need ordered scheduling: an old zero arriving after a newer positive observation
can re-establish a latch.

The two commands do not switch UID, install a scheduler, change permissions, or
read another user's login. An optional [Linux launcher example](linux-owner-collection.md)
is provided for a root-managed scheduler; it is not installed or enabled by
Quota. The operator must arrange its trusted configuration, private worker home,
and separate execution identities. The synthetic tests in this repository do not qualify the real cross-UID path;
it was qualified separately on dobi (see the Linux launcher document). macOS is unqualified.
