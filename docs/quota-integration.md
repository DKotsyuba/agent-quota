# agent-quota architecture and agent-run integration

Status: the owner decisions and product boundary below were reviewed with the
agent-run collaborator. A resident service, local store, quota calculations,
CLI/MCP, and a versioned local API are implemented and exercised with synthetic
collectors. Source-level agent-run integration has synthetic coverage; deployment
and Linux launcher qualification remain open. Automatic Bridge notifications are
deferred, and the owner chose not to import old quota history.

## Product boundary

agent-quota is an independent application with its own continuously running
service, configuration, and durable database. It collects quota observations
from configured providers, keeps their history, computes all quota-related
metrics and warnings, and provides results to clients. It does not start model
turns merely to discover quota.

agent-run requests ready-to-use quota data when it needs them. It remains the
owner of agent launches, provider/model choices, account selection, local
priority policy, active-attempt accounting, process limits, cleanup, and
retries. It does not maintain a second quota forecasting implementation or
become the authoritative quota-history store. A small reference to the quota
result used for a launch is an integration and audit detail, not another
source of quota truth.

## Service and data flow

1. The service schedules collection from configured provider/account sources.
   Collection is bounded and runs with the credentials of the authorized owner.
   Failures, timeouts, and backoff are reported without exposing credentials or
   replacing valid observations with invented values.
2. Its database stores observations, physical pool and per-window model
   identities, collection status, and durable exhaustion facts. Source identity
   remains stable across aliases and restarts. Newly collected history and
   retention preserve the evidence needed for forecasts and latches; old
   agent-run quota history is not imported.
3. Quota calculations use those observations and the evaluation time. Responses
   expose remaining quota, reset, freshness, consumption speed, limiting windows,
   forecasts, and warnings where supported by evidence. Inferring speed from
   history requires enough compatible observations; absent data and estimates
   remain unknown, not zero. A provider-reported rate must be identified as such.
4. CLI, MCP, and the machine API for agent-run access the same service and quota
   logic. Ordinary reads use stored observations and do not call a provider.
   The initial API uses an authenticated local Unix socket; an explicit refresh
   operation is not implemented.
5. Existing risk levels and advisory recommendations are part of the quota
   result. Automatic warning delivery is deferred by the owner; when designed,
   warnings go only to the relevant coordinator/orchestrator through Agent Bridge.

The project declares a resident process and local-state profile. The service
must be explicitly started; it is not automatically installed or registered.

## Observation and API contract

The [local API](api.md) returns a versioned, bounded result for an explicit
scope. A response identifies its source/epoch/revision, scope and coverage,
evaluation and observation times, validity, and collection problems. It needs
global account and physical pool identity, source/window identity and exact
per-window model membership so shared pools are neither duplicated nor summed.

For each applicable window, return known or unknown remaining percentage, reset,
the speed and time span used for its calculation, and the relevant forecast,
risk, and warnings. Report the active exhaustion state independently of
freshness. Preserve missing values and report insufficient history explicitly.
The result must provide the existing run-side choice with the equivalent of
burn rate, observation span, sustainable rate, projected balance, slack, risk,
and the warmup/thin-evidence/no-reset distinctions; the wire representation
may differ only when parity is demonstrated. Current reset-cycle calculation,
limiting-window selection, and unknown fallback are the compatibility baseline.
The current local wire has passed synthetic interoperation with an agent-run
client. A separate synthetic Quota process supplied known, unknown and
exhausted accounts to Run's public reads and admission, which selected the known
account. Admission and account switching also have synthetic coverage, and the
old Run CLI collection commands have been removed in its working tree.
The Run collaborator reports that `cargo xtask check` passed after removal of
the remaining legacy quota path, including both feature configurations. A
local Run package and synthetic start, replay, status and export checks also
passed. Run's working tree no longer contains its old in-process collector,
forecast, quota ranker, advice, Linux quota scripts, ingest helper or quota
storage implementation;
external Rust callers of removed internal APIs must migrate. Historical tables,
migrations and retired operation formats remain for compatibility and receipt
replay, while new requests for retired quota writes are rejected. Existing
installations were not changed and may still have the old Linux launcher. The
new launcher passed an isolated two-UID synthetic test but is not yet qualified
for a live deployment. A separate same-UID user-systemd service on dobi
collected a real Codex quota observation and returned it through the local API;
that check did not start a model or qualify the cross-UID launcher.
Automatic warning thresholds remain undecided.

Include the calculation time, its validity bound, and calculation identity.
The same source revision does not make a forecast timeless: projections and
warnings depend on the clock, and a window can expire or reset without a new
observation. A validity bound does not claim that a time-sensitive estimate
remains numerically unchanged until that bound.

Clients must be able to distinguish a complete result from partial coverage,
unavailable sources, stale data, a changed source epoch, and an oversized or
inconsistent paged result. A read must not silently present incomplete data as
complete. The current API has bounded responses and refuses oversized results
instead of paginating; account and model filters narrow the scope. The normal
read response must not contain credentials, credential references, raw collector
output, or process diagnostics.

## Admission and unavailable service

Quota data are observed facts, not reservations or a guarantee of a provider's
remaining capacity at the moment an agent starts. The quota result can change
between agent-run reading it and committing its own launch. The two databases
do not form one transaction, and a source revision or a second read alone does
not close that race. Record the identity and time of the result used for a
decision; agent-run still rechecks its own account, local limits, cooldowns,
and current load in its transaction.

The owner chose to continue launching when agent-quota is unavailable, with
quota marked unknown. A launch may therefore encounter the provider's limit;
unknown must not be represented as either exhausted or unlimited. A stopped
quota service cannot produce new warnings until it runs again. The snapshot
schema supplies a state-validity bound for each model; Run must fetch a fresh
snapshot for each decision or local selection retry and reject expired state. The owner chose
to show available limits with an explicit incomplete marker when coverage is
partial, keeping other quotas unknown. This does not provide an atomic fence
against later Quota updates.

A single quota service may provide the same observations to several independent
agent-run brokers. That alone does not combine their local active-attempt counts
or provide cross-broker admission or reservations. Whether shared accounts need
that stronger coordination is not yet decided.

## Exhaustion and migration of existing behavior

The quota domain owns exhaustion facts and the rules for their release.
A confirmed restriction must survive missing or unknown observations. Positive
evidence can release an active restriction only under the existing freshness
and model-coverage rules. A newer covering zero may replace an older latch,
including across sources, without releasing the restriction.

agent-run can receive a provider refusal for exhausted quota during a run. The
owner chose not to send that fact to agent-quota: it learns of exhaustion at
the next successful provider poll. Until then, other launches can see an older
observation and encounter the same provider limit. This deliberately changes
the previous immediate native-exhaustion latch propagation. agent-run still
handles the result of its own attempt and its existing failover lifecycle.
Ordered provider cooldowns, including billing errors, remain run-specific
state rather than fabricated measured quota.

Legacy rows without valid membership and rows with an explicitly empty model
set are different. The existing selection fallback uses pool-equals-model;
legacy latch settlement uses its historical any-model rule. Equal observation
times are ordered using the stored row identity. Existing agent-run history is
not migrated; these distinctions remain relevant for compatible future reads.

## Bridge warnings (deferred)

Automatic delivery is not part of the current transfer. Agent Bridge is the
future delivery mechanism requested by the owner. agent-quota would be a
Bridge producer, not an agent impersonating another session. It must use an
authorized producer identity and the relevant coordinator's exact, verified
recipient route; a path, process name, display label, or model-provided address
is not sufficient proof. The design must establish how a coordinator subscribes
or how a trusted owner provides its route and quota scope. Bridge routes are
producer-scoped: a route registered by agent-run cannot simply be assumed usable
by agent-quota. Direct addressing requires the corresponding producer grant and
an attested native recipient.

Two candidate registration paths require validation: a coordinator makes a
supported direct MCP call to agent-quota through the appropriate same-UID
frontend so Bridge can verify the original native context and issue a route for
this producer; or an operator explicitly configures an authorized canonical
destination for a producer with direct-address rights.
The latter is a trusted configuration, not proof of the calling coordinator.
Either path needs an authorized quota scope and a defined subscription lifetime.
agent-run's stored session identifier alone is not a verified full address of
the coordinator. Native host support and route recovery need their own tests.

Persist warning intention and stable event identity before publication. Reconcile
a lost publication response using the same identity. Once Bridge accepts
custody, Bridge owns delivery; acceptance alone does not prove that a native
session received, observed, or acted on the message. Unknown outcomes, held
recipients, retries, expiry, and duplicate suppression must follow the Bridge
producer contract. Warning thresholds, transitions, coordinator route binding,
and suppression rules remain open; no new policy is silently assumed.

## Security and operational boundary

Provider credentials remain in the owner's approved credential store. Native
Linux deployments can have separate broker and worker UIDs; macOS can use native
login and Keychain identities. A shared local group does not authorize another
user's credential store. The transport and worker arrangement must enforce the
actual peer and credential-owner boundaries and be checked for both authorized
and denied callers.

Service supervision, startup, recovery, backup, and credential access must be
defined for the supported deployment. No service has been installed, started,
or connected to real provider accounts during this design work.

## Migration and verification

1. Specify the API, quota calculation compatibility, provider configuration,
   credential-owner boundaries, and warning delivery contract using synthetic
   examples; preserve current agent-run public response behavior.
2. Build agent-quota's collection, independent storage, calculation, and
   read-only API. Bridge publication is a separate future feature. Do not enable
   a second authoritative live collector as part of design or tests.
3. Integrate agent-run as a client of ready-made quota results and remove
   duplicated quota calculations only after parity, recovery, and rollback are
   demonstrated. Start quota history fresh rather than importing old data.

Verification must cover shared physical pools, model memberships, history and
same-time ordering, reset cycles and insufficient evidence, unknown versus zero,
staleness and active latches, resets without a new revision, failed collectors,
restart and unavailable service, out-of-order collector observations, a native
refusal before the next poll, lost Bridge acknowledgements, warning delivery
evidence, authorized and denied callers, and an observation racing a launch.
Test legacy missing validity bounds and both existing limits schemas, including
the account-redaction behavior of MCP text. Exact provider or host qualification
is not implied by a local test.

## dobi verification

On 2026-11-07 UTC, the local Linux x86-64 package built from commit
`8a13e8af63dcb2c90d07f362f1aa32b7a50d94e2` had executable SHA-256
`82001e8c4544d20a6a560808fbe6553439242cb6fcee65842087a401bd438b06`.
That exact executable passed protocol, asset, collector and service tests, a
synthetic two-UID container handoff, and a temporary same-UID user-systemd
service test on dobi. The latter collected one real Codex quota observation
and returned it through the local API without a model turn. A separate
synthetic user-systemd run checked restart and shutdown. Only isolated homes,
configuration and state were used; temporary credential copies were removed.

The current working tree also passed `cargo xtask check`. Its Linux installer
was exercised against that package with a local mock download and a private
installation directory. An isolated, offline `cargo-deny 0.20.2` check covered
all 145 workspace packages, using RustSec database commit
`309392b18f6b17022f82c701f300433126901068` and a freshly fetched locked
registry-index snapshot. Advisories, yanked versions and sources passed for
those snapshots; two duplicate-version and three unused-license-allowance
warnings remain within the existing policy.
These results do not establish GitHub CI provenance or qualify a later package.

The root-managed two-UID systemd launcher and host PAM have not been tested on
the actual host, nor have real Claude or GLM logins. The Linux CI workflow has
been reviewed locally but not run in GitHub. No live deployment, release,
credential store or existing agent-run state was changed by these checks.

## Open decisions

- Whether several independent brokers or machines must coordinate usage of one
  subscription, beyond reading the same observations.
- Scope authorization and deployment across OS users, provider configuration,
  credential ownership, and how coordinators acquire verified Bridge routes.
- Warning triggers, thresholds, coordinator route binding, suppression,
  expiry, and the details of delivery recovery.
- Collector-ingestion replay and ordering, any future pagination, and cutover
  rollback without importing historical quota data.

Existing agent-run behavior was inspected at commit
14004a9846dbd26cf8629a3a725c187f866f9120, especially its quota collector,
provider contract, forecast, provider-ranking, quota-store, and admission code.
Bridge producer semantics were inspected in agent-bridge commit
77adfb3a23362926a69ec52f91406f12fc0d75a3, docs/producer-integration.md.
