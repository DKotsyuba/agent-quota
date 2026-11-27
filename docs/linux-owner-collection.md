# Optional Linux credential-owner collection

The files in [scripts/systemd](../scripts/systemd) are embedded in the binary
and can be exported with `agent-quota export-assets`; they remain an **uninstalled**
example for one configured external source per timer instance. They run the
same Quota binary as the credential-owning OS user for `probe`, then as the
Quota service user for `ingest`. The service and the launcher do not read the
other user's credential store. Each source must use `execution = "external"`
in the shared Quota configuration; the normal service poller does not run it.

An administrator must review and arrange the deployment separately:

- Install the reviewed launcher, binary, unit and timer in root-controlled
  paths. The root launcher and all of its path components must not be writable
  by either unprivileged user. The binary and the shared nonsecret configuration
  must be absolute, root-owned, non-symlink paths whose ancestors are also
  root-owned and not group- or world-writable; the script checks these paths.
- Prepare one root-owned, non-writable `/etc/agent-quota/probes/<instance>.env`
  from [the example](../scripts/systemd/owner-collect.env.example). It binds a
  specific account, provider and source to the intended credential owner and
  the service user. Neither OS user may be root. The configuration file must be
  readable by both users and contain credential references, not token values.
- Configure trusted collector scripts and harness executable paths; they run as
  the credential owner. Prepare that user's actual login home, private Quota
  probe home and temporary directory. Prepare the service user's temporary
  directory and its Quota socket. The launcher never opens a credential file as
  root. It is not a general-purpose sudo interface.
- Keep the provided systemd `StateDirectory` at
  `/var/lib/agent-quota-owner-collect` root-owned, non-symlink and private
  (`0700`). All instances use its directory lock, so probes normally run one at
  a time across accounts and sources until their ingest reply. The lock is not
  passed to the worker or service process. The example timer fires every five
  minutes; the unit's 15-minute deadline includes time waiting for the lock.
  A timed-out run can be tried again by the timer. No unit, timer, account or
  path is installed or enabled by Quota.

The launcher waits for a successful bounded probe before a single ingest. It
does not print the observation or child diagnostics. Normal collection errors
and an unknown ingest result fail that invocation; the timer can try again.
The probe's diagnostic line is written to a root-private file in the lock
directory and removed after the run. Only an allowlisted fixed code is appended
to the journal message, for example
`quota owner collection: probe failed: credential_expired`,
`...: credential_refresh_failed`, `...: credential_refresh_limited`,
`...: credential_refresh_unsupported_host`,
`...: credential_refresh_unsupported_host (credential_refresh_policy_cache_unknown)` or
`...: credential_refresh_cleanup_unconfirmed`. Any other
diagnostic yields the generic `probe failed`.

For a Claude login, `refresh_oauth = true` in the source's `[sources.harness]`
lets the probe renew a token near expiry through one isolated print-mode turn of
the configured Claude CLI (see
[Claude OAuth renewal](configuration.md#claude-oauth-renewal)). The CLI then runs
as the credential owner, so `AQ_PROBE_PATH` and `AQ_PROBE_OS_HOME` must suit that
CLI; the probe's private home holds its working and temporary directories, the
single-flight lock and the attempt ledger. The probe env file needs no other
change. The CLI writes the login store itself; Quota does not change its owner
or mode. A renewal adds at most 90 seconds to a probe, well inside the unit's
15-minute deadline.
The owner chose automatic continuation even after an interrupted probe or lost
ingest acknowledgement. An older ingest may therefore finish after a later
probe, or a collector may outlive an abnormal termination. This can temporarily
produce stale or contradictory quota facts; the launcher does not claim an
idempotent receipt or a cross-process transaction. `KillMode=control-group`
helps systemd stop its children; the real-child timeout and cgroup cleanup were
observed on dobi (see below).

## Reset-credit redemption

The same launcher has a `redeem` mode for the owner-requested
[redemption API](api.md#reset-credit-redemption). Its units,
`agent-quota-owner-redeem@.path` and `agent-quota-owner-redeem@.service`, are also
exported by `export-assets` and remain uninstalled examples. The path unit
watches `/var/lib/agent-quota/redeem/<instance>.pending`, which the service
writes inside its private home when it accepts a request. The service unit then
runs, under the shared lock:

1. `agent-quota redeem-claim` as the service user. The service repeats its
   admission check, hands out the pending request once under a random claim
   token, and removes the trigger. A request whose credit is no longer usable,
   or whose inventory another redemption has overtaken, is rejected instead of
   handed out. With nothing to hand out the run ends successfully.
2. `agent-quota redeem-execute` as the credential owner, with the claim on
   stdin. It refuses a claim admitted under other configuration bytes than the
   file it reads (`owner_binding_changed`). It resolves the credential like `probe`, runs the source's
   `[sources.redeem]` command once and prints a credential-free outcome that
   repeats the claim token.
3. `agent-quota redeem-report` as the service user. The same report is sent
   again with backoff (eight attempts, about two minutes of waiting) until the
   service accepts it. The service applies a report only to the claim whose
   token it carries; the first verdict is final and a repetition has no effect.
4. A normal `probe` and `ingest`, so the inventory is current again.

The provider call in step 2 is never repeated. If the executor produces no
usable outcome, the launcher reports `unknown` (`executor_failed`) for the
claim it holds. If no report arrives, including when the launcher lost the
reply to its claim and therefore executed nothing, the service marks the
request `unknown` (`outcome_unreported`) 15 minutes after the claim; the
launcher never declares a claim unspent. A request nobody took ends as
`failed` (`executor_unavailable`) after 10 minutes. Repeating the same request
while it is still waiting writes the trigger again. The journal receives only
fixed messages; the service journal records request id, account, state and
reason.

Only the service removes the trigger: when it hands out, rejects, expires or
aborts the request, and whenever no request of the account is waiting (checked
on every redemption call and once per collection interval). The launcher never
touches it. While a request waits and the launcher cannot take it, the path
unit starts the unit again after each run. The launcher throttles itself
instead of relying on systemd start limits, which would leave the units failed
until an operator intervened:

- Every failed redeem run (service socket unreachable, failed claim, missing or
  invalid settings, lost report) waits 30 seconds before it exits. The service
  unit sets `StartLimitIntervalSec=0`, so it is never stopped for restarting,
  and reads its env file as optional so that even a missing file reaches this
  pause. When the cause disappears, the next run takes the request with no
  operator action; no `reset-failed` is needed.
- A run that cannot get the shared lock within two minutes fails with
  `quota owner redeem: lock wait exhausted`, pauses and is started again.
- A request that nobody could take ends as `failed` after its 10 minutes. The
  service then removes the trigger and the restarts stop.

One deadline covers the whole run: `TimeoutStartSec=20min`. After the lock the
launcher claims only if 660 seconds still fit: three claim calls with pauses
(19 s), a Claude credential renewal (95 s), the redemption script's upper limit
(300 s) with process cleanup (5 s), the report attempts (160 s), the pause after
a failed run (30 s) and a margin.
The remaining time serves the following collection, which the deadline may
cut short without affecting the reported outcome.

Who can spend a credit: the launcher executes whatever request the service
hands out. The `--allow-redeem-uid` check is made by the service process, so
it protects against other local users, not against the service identity
itself. Any process running as the service user (or root) can create or claim
requests and is therefore trusted to spend the configured accounts' credits. It
still cannot read a credential or choose the provider call. Independently of
the service, the credential owner's `redeem-execute` records each execution in
its private probe home (`redeem-<account>.last`, mode 0600) and refuses a
second one for the same account within 120 seconds (`owner_rate_limited`).
The time is taken after any credential renewal, immediately before the provider
call, and the check and the record are made under one exclusive file lock
(`redeem-guard.lock`). An unreadable or unsafe record refuses the execution
(`owner_guard_unavailable`).
This bounds the rate, not the total: over time the service identity can spend
every usable credit of an account that has `[sources.redeem]`.

To enable redemption for one existing `agent-quota-owner-collect@<instance>`:

- Install this version's binary and replace
  `/usr/local/libexec/agent-quota-owner-collect` with the exported
  `systemd/agent-quota-owner-collect.sh` of the same binary. The redeem unit
  calls it with the argument `redeem`.
- Export the assets of this binary to the directory the configuration names and
  add `[sources.redeem]` to the source, running the provider's `*-redeem.sh`
  with `instance = "<instance>"`
  (see [configuration](configuration.md#reset-credit-redemption)).
- Install both redeem units in `/etc/systemd/system`. They reuse
  `/etc/agent-quota/probes/<instance>.env` unchanged. If the service home is
  not `/var/lib/agent-quota`, change the trigger path in the path unit.
- Add `--allow-redeem-uid <uid>` for each UID that may request redemptions to
  the service's `serve` command line and restart the service. The UID must also
  be named by `--allow-uid` (or be the service UID).
- `systemctl daemon-reload`, then
  `systemctl enable --now agent-quota-owner-redeem@<instance>.path`. Enable the
  path unit for one instance per account only.

The redeem mode shares the collection lock, so a redemption waits, for at most
two minutes, for a running collection of any account.

Synthetic launcher tests cover handoff arguments, lock, failure and size
boundaries. A separate same-UID test invokes the real `probe` and `ingest`
commands with the UID switch stubbed. An isolated Linux container test also ran
the real launcher with distinct service and credential-owner UIDs, verified that
the service could not read the owner's synthetic secret, and exercised allowed
reads, rejected writes and a rejected unlisted UID. That container had no
network or host mounts and used a synthetic PAM configuration.

On dobi (Ubuntu 24.04.5 LTS x86-64), a privileged qualification (run
`9e478e83da01`, all observations passed) ran the launcher through the host's
PAM `runuser` and systemd with four distinct UIDs: credential and socket DAC
denials, peer-UID denial, a hung child ended by `TimeoutStartSec` with its
cgroup removed, a later run succeeding, and a clean serve stop. It used a
synthetic fixture provider and an earlier candidate binary (SHA-256 `2d3c142b`)
of the same source line, not the exact release bytes. Separately, the real
Claude quota-metadata endpoint was read once by the executing user's own native
login (no model turn), and an isolated same-UID user-systemd service collected
real Codex quota metadata. Real GLM logins, other hosts and macOS Keychain
behavior remain unverified.
Do not remove an existing Run launcher or change its live paths until the
operator has reviewed and verified the replacement for that deployment.

## Resident service startup migration

The exported [resident unit](../scripts/systemd/agent-quota.service) and
[tmpfiles rule](../scripts/systemd/agent-quota-tmpfiles.conf) are examples for
the administrator. The unit gives systemd ownership of the private state
directory at `/var/lib/agent-quota`. The only preparation within that
directory runs as `quota-service`. Tmpfiles prepares the shared socket
directory directly under root-owned `/run`; its setgid bit preserves the
socket group. The service still authorizes every peer by UID.

For an installed system unit, arrange an administrator maintenance window.

1. Review the effective unit and drop-ins using `systemctl cat agent-quota.service`.
   Record the binary, configuration, state and socket paths, socket group, and
   the existing `--allow-uid` and `--allow-redeem-uid` arguments. Stop the
   resident unit and any timer or path units that could trigger it during migration.
2. Check that the existing state directory is a real, service-owned directory
   at mode `0700` under root-controlled ancestors. Preserve the database and
   configuration. Inspect any unexpected symlinks before reuse. Use
   `StateDirectory=agent-quota` and `StateDirectoryMode=0700` for the example
   state path. Remove privileged directory creation from the unit and its
   drop-ins, including any `ExecStartPre=+` or equivalent privileged override
   for service-writable paths. Prepare the private temporary subdirectory as
   the service user.
3. Update `/etc/tmpfiles.d/agent-quota.conf` to contain only the required
   socket-directory rule for this service. The example uses
   `d /run/agent-quota 2710 quota-service quota-service -`. Substitute the
   installed service user and client group. Keep the socket directory under
   a root-owned, non-writable parent. Remove old privileged rules below the
   state or socket directory. Preserve the socket path and allowlists where
   possible. If a path changes, update client, hook and owner launcher settings.
4. Apply the reviewed tmpfiles rule with
   `systemd-tmpfiles --create /etc/tmpfiles.d/agent-quota.conf`, then run
   `systemctl daemon-reload` and start the service. Check the effective unit,
   directory owners and modes, then run `agent-quota doctor --json` with
   the installed socket and expected service UID. Verify a read from an
   allowlisted client before resuming the associated timers and paths.
