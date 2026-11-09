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
The owner chose automatic continuation even after an interrupted probe or lost
ingest acknowledgement. An older ingest may therefore finish after a later
probe, or a collector may outlive an abnormal termination. This can temporarily
produce stale or contradictory quota facts; the launcher does not claim an
idempotent receipt or a cross-process transaction. `KillMode=control-group`
helps systemd stop its children; the real-child timeout and cgroup cleanup were
observed on dobi (see below).

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
