# Local API and external probes

The resident service exposes protocol version 1 as one newline-delimited JSON
request and response per Unix socket connection. Reads use persisted observations;
they do not run collectors or start model turns. The only client-initiated
change is the separately authorized [reset-credit redemption](#reset-credit-redemption). The service and client authenticate
the local peer by UID. The default socket is owner-only; a separate explicitly
allowlisted client UID requires an operator-prepared setgid socket directory.
Clients must configure the expected service UID when it differs from their own.

## Read requests

```json
{"version":1,"method":"status","accounts":[]}
{"version":1,"method":"snapshot","accounts":["account-example"],"model":"model-example"}
{"version":1,"method":"capacity","accounts":[],"model":"model-example"}
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
For known usable data, `quota_score` is `1 + limiting_window.slack`, before
multipliers; the [capacity order](#capacity-order) applies the configured
account and provider multipliers, and agent-run applies its local admission policy.
Window details carry exact source, runtime, lane, target, membership, observation
and validity times, forecast, and window explanation. `last_remaining_percent`
and `last_reset_at` are nullable raw facts from the latest sample and remain
available when that sample is stale; they do not establish freshness.
Timestamps are Unix seconds; nullable values remain unknown, not zero. Active latches are independent of
sample freshness. Advice and its noise-tolerant key are returned per account.

Each account also includes [`reset_credits`](../schemas/reset-credits.json).
`known: false` means the inventory is unavailable, with a null count and no
rows. A known zero is an explicit `available_count: 0`. A provider can report a
count without listing individual credits, or return rows without a usable
count. `observed_at` gives the Unix time of the last credit observation and is
null before any observation. The service stores one latest observation per
account. Counts and per-credit availability describe that observation.
Account-level credit information is included with model-filtered reads.

Version-1 collector output accepts an optional
[`reset_credits` object](../schemas/reset-credits.json#/$defs/collector).
Older collectors can omit the field. An omission supplies no credit observation,
so availability stays unknown until a collector reports an inventory.
A credit contains its provider ID and kind, label, scope,
nullable resets remaining and usability, and nullable grant and expiry times
in Unix seconds. The contract permits up to 64 rows. The service bounds text
fields in UTF-8 bytes (128 for ID, label and scope, and 64 for provider kind).

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
snapshot JSON. MCP `get_quota` renders up to 20 model rows, 20 account credit
summaries, and 20 credit rows within its 8 KiB text budget. An oversized result
requires a narrower account or model scope. The CLI `capacity` prints the capacity
JSON; MCP `get_capacity` renders at most 60 model rows on the same terms.

## Capacity order

`capacity` is an account-free read added within protocol version 1; a service
older than this method answers `unsupported_method`. `accounts` must be empty;
`model` optionally narrows the order to one configured or native model name
(`unknown_model` otherwise). It reads every configured account once, at one
evaluation clock, under the same configuration as the snapshot it ranks.

```json
{"schema_version":1,"source_epoch":"...","revision":12,"config_revision":"...",
 "evaluated_at":1763490000.0,"coverage":{"state":"complete","incomplete":false},
 "providers":[{"provider":"codex","priority_multiplier":1.0,"score":2.62,"status":"available",
   "models":[{"model":"gpt","native_model":"gpt","status":"available","best_priority":2.62,
     "evidence":"fresh","newest_observed_at":1763489900.0,"exhausted_until":null,
     "limiting_window":"five_hour","remaining_percent":64.0,"reset_at":1763500000.0,
     "risk":"low","valid_until":1763490800.0,
     "windows":[{"window":"five_hour","source":"codex-appserver","known":true,
       "remaining_percent":64.0,"reset_at":1763500000.0,
       "observed_at":1763489900.0,"valid_until":1763490800.0}]}]}]}
```

The ranking is agent-run's former provider capacity order, now owned by Quota:

- A provider offers the models of its `[[sources]]` in declaration order. A
  source's account is eligible for its own models with the source's
  `priority_multiplier`; an account with several eligible sources uses the
  maximum, never a sum. `ordered_accounts = true` ranks with neutral weights.
- Per account, a `known` model whose `state_validity` is still valid has
  priority `quota_score * account multiplier`; a valid `exhausted` model is
  exhausted. A valid known model whose weighted priority overflows has
  `priority_overflow`. An expired bound or other unavailable evidence is unknown.
- Model `status`: `available` if any account has known capacity at low or
  medium risk; `limited` if all accounts with known capacity are at `high`
  risk (the existing forecast rule: at most 10% remaining, or burn above the
  sustainable rate). With no known priority, the status is `unknown` if any
  eligible lane is unknown, `priority_overflow` if any overflows, and
  `exhausted` otherwise. `best_priority` is the highest known priority.
- `evidence` is `fresh` for a known or exhausted decision, `stale` when
  samples exist but yield no known priority or exhaustion decision, and
  `missing` otherwise.
  `exhausted_until` is the earliest known reset of the exhausted accounts.
- Provider `score` is the best model priority times the provider
  `priority_multiplier` (saturated at the largest finite number), or null
  without known capacity. Providers sort by descending score, then best
  model status in Agent Run's order (known, `unknown`, `priority_overflow`,
  `exhausted`), then name. Both `available` and `limited` count as known.
- For a scored row, `native_model`, `limiting_window`, `remaining_percent`,
  `reset_at`, `risk`, `valid_until` and `windows` (at most 8) come from the
  account and native lane that supplied `best_priority`. The status still
  summarizes all eligible lanes and `newest_observed_at` uses their newest
  observation. Exhausted rows use the soonest-reset exhausted lane for those
  details. Unknown and overflow rows have no representative details.
- `coverage.incomplete` is true when collection coverage is not `complete` or
  any eligible lane is unknown or its weighted priority overflows.

No account, pool, credential or source-command identity is returned. The
order is quota-only: account enablement, cooldowns, pins and active-attempt
limits are agent-run admission state and are not applied. Like the snapshot,
the result is an estimate at `evaluated_at`, not a reservation; a client must
not use it as an admission fence.

## Reset-credit redemption

Two methods let an explicitly authorized local UID spend one banked reset credit
from an account's [`reset_credits`](../schemas/reset-credits.json) inventory. They
use the same transport as the reads: one newline-terminated JSON request and one
reply per Unix socket connection, protocol `version` 1, the five-second exchange
deadline and the 64 KiB request cap. The checked-in contract is
[schemas/redeem.json](../schemas/redeem.json).

```json
{"version":1,"method":"redeem_reset_credit","account":"account-example","credit_id":"credit-example","request_id":"0b6f1f0e-8d0a-4c55-9d0e-2f3b6a7c8d9e"}
{"version":1,"method":"redeem_status","request_id":"0b6f1f0e-8d0a-4c55-9d0e-2f3b6a7c8d9e"}
```

The key `op` is accepted in place of `method`. `version` is required. No other
field may be present. `request_id` is a canonical lowercase version-4 UUID chosen
by the client; `credit_id` is the `id` of an inventory row (1–128 bytes).
Both methods answer with the same record:

```json
{"version":1,"result":{"request_id":"0b6f1f0e-8d0a-4c55-9d0e-2f3b6a7c8d9e","account":"account-example","credit_id":"credit-example","state":"pending","reason":null,"created_at":1763990000,"updated_at":1763990000}}
```

`created_at` and `updated_at` are integer Unix seconds; `updated_at` is the time
of the latest state change. `reason` is null or a fixed code of `a-z0-9_`, at
most 64 bytes; it never contains provider text. `state` is one of:

| state | meaning | final |
|---|---|---|
| `pending` | recorded; the credential owner's launcher has not reported yet | no |
| `redeemed` | the provider confirmed the reset | yes |
| `rejected` | the service or the provider refused; the credit was not spent | yes |
| `failed` | the provider was not asked to redeem (no executor, credentials, authentication) | yes |
| `unknown` | the call may have reached the provider and its outcome is not known | see below |

Authorization and errors:

- Only a UID named by `serve --allow-redeem-uid` may call either method; the
  default is none, and the service's own UID is not implied. Every other caller
  gets `{"version":1,"error":"unauthorized"}`. A redemption UID must also be the
  service UID or be named by `--allow-uid`, otherwise the service refuses to start
  with `redeem_uid_not_allowed`.
- `invalid_request`: malformed fields, an extra field, or a `request_id` that is
  not a canonical version-4 UUID. `unknown_account`: the account is not
  configured. `unknown_request`: `redeem_status` for an id the service does not
  hold. `request_conflict`: the `request_id` was already used with another
  account or credit. `storage_error`: the request was not recorded.
- A refusal by rule is not an error. It is a stored record with
  `state: "rejected"` and one of these reasons: `redeem_not_configured` (the
  account has no `[sources.redeem]` binding), `redeem_in_progress` (another
  request for the account is pending), `inventory_unknown` (no known inventory),
  `inventory_stale` (an earlier redemption is newer than the stored inventory),
  `credit_not_usable` (the credit is absent from the latest inventory or not
  `usable_now: true`).

Idempotency: a repeated `redeem_reset_credit` with the same `request_id`,
account and credit returns the existing record and has no further effect, in any
state. A client whose reply was lost must repeat the same request or call
`redeem_status`; it must not send a new `request_id` for the same intent.
Records are never deleted: a `request_id` stays bound to its account, credit and
final state permanently and can never start a second redemption.

The service never calls a provider and holds no credential. For a `pending`
request it writes the trigger file `<service home>/redeem/<instance>.pending`
(`instance` from the binding, default the account id). A root-managed
[launcher](linux-owner-collection.md#reset-credit-redemption) then takes the
request once, performs the provider call as the credential owner, reports the
outcome and runs a normal collection, so `reset_credits` reflects the result.
Poll `redeem_status` until the state is no longer `pending`; a healthy launcher
finishes within about a minute.

No outcome is retried automatically. The reasons form a closed list; the
service accepts no other code from an executor, so a reason never carries
provider text or any other free value:

- `failed` (the provider was not asked to redeem): `executor_unavailable` (no
  launcher took the request within 10 minutes), `trigger_unavailable`,
  `binding_changed` (the configuration changed between admission and hand-out),
  `owner_binding_changed` (the credential owner read a different configuration
  than the request was admitted under), `auth_rejected` (provider answered 401
  or 403), `profile_unavailable`, `organization_unknown`,
  `provider_unreachable` (no connection was made), `unsupported_scope`,
  `codex_method_unsupported`, `codex_app_server_unavailable`,
  `redeem_script_unavailable`, `redeem_request_invalid`,
  `redeem_not_configured`, `config_unavailable`, `clock_invalid`,
  `owner_guard_unavailable`, `credential_unavailable`, `credential_expired`,
  `credential_refresh_failed`, `credential_refresh_limited`,
  `credential_refresh_unsupported_host`,
  `credential_refresh_cleanup_unconfirmed`.
- `rejected` by an explicit provider verdict: `already_used`, `not_limited`,
  `cooldown`, `ineligible`, `unavailable` (Claude); `nothing_to_reset`,
  `no_credit` (Codex). GLM documents no refusal codes, so a GLM redemption is
  never `rejected` by the provider. `owner_rate_limited` comes from the
  credential owner's side: it executes at most one redemption per account in
  120 seconds.
- `unknown`: `rate_limited` (429), `provider_conflict` (408 or 409),
  `provider_error` (5xx), `provider_timeout`, `network_error`,
  `provider_response_unrecognized` (any answer that is not a listed verdict,
  including every other 4xx and contradictory or unlisted envelopes),
  `redeem_timeout`, `redeem_script_failed`, `redeem_output_invalid`,
  `redeem_output_contains_credential`, `executor_failed`, and
  `outcome_unreported` (a taken request without a report after 15 minutes).
- `redeemed` normally has a null reason. `already_redeemed` means Codex reported
  that this idempotency key had already completed.

The first verdict reported for a request is final. The single exception is a
request that timed out as `unknown` (`outcome_unreported`): the executor's own
verdict for that claim may still arrive and replace it. Nothing else changes
an `unknown` state. In particular the service does not infer an outcome from
later inventories.

**Warning: `unknown` means the credit may already be spent.** Submitting a new
`request_id` after an `unknown` result is a new redemption and can spend another
credit. Check the provider's quota and the refreshed `reset_credits` first, and
send a new request only if the reset demonstrably did not happen.

The admission rules are applied twice: when the request is recorded and again
when the launcher takes it. A request can therefore move from `pending` to
`rejected` (`inventory_stale`, `credit_not_usable` or `inventory_unknown`)
without any provider call. Each request is also pinned to the exact
configuration revision and launcher instance it was admitted under; after any
configuration change it ends as `failed` (`binding_changed`) instead of being
handed out.

Two further methods, `redeem_claim` and `redeem_report`, serve the launcher. Like
`ingest`, only the service's own UID may call them. A claim carries a random
token that the matching report must repeat; the token is never part of a record. They are not part of the
client contract. None of the four methods is exposed as an MCP tool.

The CLI client is:

```bash
agent-quota redeem --socket /absolute/quota.sock --server-uid 990 \
  --account account-example --credit credit-example [--request-id UUID] [--wait SECONDS]
```

It generates a `request_id` when none is given, prints the record as JSON and,
with `--wait`, polls once per second while the state is `pending`. It exits 0
for `redeemed` or a still pending request, 2 for `rejected`, `failed` or
`unknown`, and 1 when the service returned no record; in that case the
`request_id` is printed to stderr for a later `redeem_status`.

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
