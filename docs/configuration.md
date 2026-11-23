# Quota service configuration

The service reads an absolute TOML config path. The supplied
[example](../examples/config.toml) is illustrative and must be edited before
use. A missing or invalid config prevents initial startup; an invalid later
revision does not replace the last valid settings and the affected polling
round is skipped. Configuration is re-read at each collection round.

`interval_seconds` defaults to 300 and `sample_retention` to 1000. Each
`[[sources]]` entry binds a global `account`, a provider alias, an `auth_family`,
a credential reference, a harness, a model list and, for `limits_source = "exec"`,
an external collector. A source with `limits_source = "none"` has no collector;
its configured models remain visible with unknown quota. `execution = "service"`
is the default; `execution = "external"` uses the separate credential-owner
probe described in [the local API](api.md).

Provider aliases preserve case and follow agent-run's identifier grammar: 1–64
ASCII characters, beginning with a letter or digit and continuing with letters,
digits, `_`, `-`, or `.`. Account IDs retain their separate lowercase-led grammar.

The same global account and stable collector source are collected once per
round, with the union of configured native model lanes. Conflicting aliases are
rejected before credentials are read. Set `source` explicitly to preserve its
physical identity; if omitted, the service derives it from the executable and
literal arguments. The command must be absolute. Arguments are not expanded by a
shell unless an interpreter is explicitly configured. The timeout defaults to
30 seconds and may be 1–300 seconds. Only names listed in `env_from` are added to
the restricted child environment; private `TMPDIR`, `TMP` and `TEMP` are supplied
inside the service or worker home.

## Capacity ranking

The [capacity order](api.md#capacity-order) uses two optional weights migrated
from agent-run's provider configuration. Both must be positive finite numbers
and default to `1.0`; existing files need no change.

```toml
[[sources]]
account = "acct-personal"
provider = "codex-plus"
priority_multiplier = 2.0   # account weight for this provider binding
# ...

[providers.codex-plus]      # must name a configured source provider
priority_multiplier = 5.0   # provider weight
ordered_accounts = false    # true: account weights are a fallback order, ranked neutrally
```

Mapping from an agent-run schema-2 provider configuration:

| agent-run | agent-quota |
|---|---|
| `[providers.P] priority_multiplier` | `[providers.P] priority_multiplier` |
| `[providers.P] ordered_accounts` | `[providers.P] ordered_accounts` |
| `[[providers.P.models]] id/native_model` | `models = [{ id, native_model }]` of each source with `provider = "P"` |
| `[[providers.P.bindings]] account, priority_multiplier` | `account`, `priority_multiplier` of a source with `provider = "P"` |
| binding `models = [...]` subset | that source's own `models` list |

Every agent-run provider that should appear in the order needs at least one
source; one without a collector uses `limits_source = "none"` and stays
`unknown`. Changing a weight changes the configuration revision, so coverage is
`partial` until the next completed collection round under that revision. The
weights affect only the capacity order; collection and `get_quota` are unchanged.

## Credential references

Never put a token value in TOML, command arguments, diagnostics, or logs.
Supported references are:

- `native:codex`: the native Codex login directory; `native_directory` can name
  it explicitly. The collector queries app-server metadata without a model turn.
- `named:codex:<label>`: the named Codex directory under `app_home/accounts/codex`.
- `native:claude-code`: the current Claude Code login. On macOS the login
  Keychain may supply its OAuth document; an explicit `native_directory` selects
  the matching named Keychain service.
- `named:claude-code:<label>`: the named Claude login under `app_home`; a
  `harness.home` is also required to check the legacy location. Ambiguous
  distinct locations are rejected instead of selecting another login.
- `env:<NAME>`, `file:</absolute/path>`, or `keychain:<service>:<account>`:
  an explicit existing store reference. Credential files must be owned by the
  executing UID, private, regular, and at most 64 KiB.

The service does not create logins or change credential permissions. A service
running under one UID cannot read a private store owned by another UID; use the
external-probe path with a separately authorized launcher where needed. The
supplied Codex, Claude, GLM and OpenCode Go scripts require jq;
the HTTP scripts also require curl. Use `agent-quota export-assets --directory`
to create a new owner-private export from the running binary, then configure the
absolute script path under its `collectors/` directory. The command never
overwrites an existing destination; `verify-assets` compares the export to this
exact binary. For another binary version, make a fresh export rather than
silently replacing files used by a running service.
Verification checks the files at that moment; it does not prevent later changes.
An interrupted export or `assets_output_uncertain` requires inspecting the
destination before retrying. If the export reports
`assets_export_succeeded_output_failed`, verify the same directory instead of
repeating the export: the files were created, but the CLI response was lost.
Configured collectors execute as trusted code. When the context includes a
direct token, the output check rejects that exact token in JSON keys and strings.
Native-login directory references supply no token to this check; it also cannot
prove the absence of an encoded or partial leak.

## Claude OAuth renewal

A Claude Code OAuth access token expires a few hours after its last renewal, and
only the Claude CLI renews it, normally while a Claude agent runs. A Claude token
whose recorded `expiresAt` has passed is never sent to the provider: the
collection fails with `credential_expired`.

Set `refresh_oauth = true` in `[sources.harness]` (only for `native:claude-code` or
`named:claude-code:<label>` credentials) to let the configured harness `binary`
renew the login. Only when a collection is due and the token is within five
minutes of expiry (Claude Code's own renewal window), Quota runs, as the
executing user:

```text
<binary> -p --model haiku --tools "" --strict-mcp-config --mcp-config '{"mcpServers":{}}' \
  --no-session-persistence --disable-slash-commands --setting-sources "" \
  --settings '{"disableAllHooks":true,"disableClaudeAiConnectors":true}' \
  --system-prompt "Reply with OK." -- "Reply with OK."
```

The supported profile is the official Claude Code **2.1.322** executable and an
unmanaged personal Pro or Max OAuth login on native Linux. Quota probes the
exact version with private bounded stdout and without the login environment.
Managed settings take precedence over the hook flag, and a managed MCP file
conflicts with strict MCP configuration. Quota checks for `managed-settings.json`,
`managed-settings.d`, `managed-mcp.json` and `CLAUDE.md` under
`/etc/claude-code`. It also excludes alternate Anthropic profiles.
If present, a remote-settings cache in the selected login or default `.claude`
location must be a readable, regular, single-link file owned by the probe user,
with group and other write bits clear. It must contain a JSON object of at most
4 KiB. The only allowed key is `channelsEnabled`, with one boolean value when
present. The renewal turn enables no MCP channels. Refused caches report
`credential_refresh_unsupported_host (credential_refresh_policy_cache_unknown)`.
Claude Code continues to enforce its separate policy-limits cache and sidecars.
macOS and WSL return `credential_refresh_unsupported_host`,
as do managed policy, unreadable policy paths, ineligible logins and an unrecognized CLI.
Existing credential collection without renewal still supports the macOS Keychain.
See [Claude Code managed settings](https://code.claude.com/docs/en/managed-settings),
[server-managed settings](https://code.claude.com/docs/en/server-managed-settings),
and the [CLI reference](https://code.claude.com/docs/en/cli-reference).

Claude Code awaits its OAuth renewal before the single small model request and
writes its own credential store. The turn uses a small amount of the subscription.
The renewal CLI gets a fixed system `PATH`, the credential owner's `HOME`, an
explicit canonical `CLAUDE_CONFIG_DIR`, private `TMPDIR`, `LANG=C`, and fixed
settings to suppress nonessential traffic and Claude.ai MCP. Collector
`env_from` grants apply exclusively to the collector. The turn has null stdio,
a 90-second deadline and process-group cleanup. `claude auth status` can exit
before this CLI version finishes saving the rotated token. `--bare` skips the
OAuth credentials used by this profile.

Every directory component of the configured store path, from the filesystem root,
must be owned by root or the credential owner and deny group and other writes.
Quota refuses directory symlinks, then uses the canonical directory to identify
the store by device and inode. The private credential file must belong to that directory
without file-level symlinks or hard links. A private `.agent-quota-oauth-renewal.lock` and
`.agent-quota-oauth-renewals.json` in that directory coordinate aliases and
different Quota homes. The synced ledger caps the store at six attempts in a
trailing 24 hours. Recent entries in a version-1 Quota-home ledger also count
until they age out, regardless of the old path spelling. Inaccessible, malformed
or non-private ledger state prevents a launch.

Quota syncs a pending marker before the version probe, then charges the rolling
allowance immediately before launching the print turn. It clears the marker
after confirmed cleanup. If termination is uncertain, including after an interrupted collection,
Quota reports `credential_refresh_cleanup_unconfirmed` and leaves the store
fenced beyond the daily window. Recovery requires the credential owner to verify
the previous invocation and its descendants are stopped before clearing only the
ledger's `pending` flag, preserving its attempt times.

Quota rechecks the store and wall clock after the CLI exits, then checks the
wall clock again immediately before resolving collector credentials. If the token
remains valid after a confirmed run with no renewal, collection proceeds and the
next due collection can try again. If it has expired, the collection fails with
`credential_refresh_failed`, with `credential_refresh_limited` when the cap (or an
unusable ledger) prevented the attempt, or with `credential_expired` when the
store has no refresh token (the CLI is then not started). A store without
`expiresAt` (for example a long-lived token) is never renewed. With service
execution, failures also count toward collector backoff.

## OpenCode Go source

The `opencode-go.sh` collector reads the undocumented account-level usage endpoint
`GET https://opencode.ai/inference/go/v1/usage` with `Authorization: Bearer <key>`
from a `file:`/`env:` token credential. Declare `auth_family`, `harness` and the
provider id exactly as the Agent Run provider does (for a Go key used through
Claude Code: `auth_family = "anthropic"`, `harness.id = "claude-code"`); the
harness binary is only passed through to the collector, never run by it.
Its `usage.rolling`, `usage.weekly` and `usage.monthly` windows (5 hours, 7 days and
the full month; `percent` is percent **used**) become the `five_hour`, `seven_day`
and `monthly` windows of pool `primary`, with `remaining = 100 - percent`, and
apply to every model of the source. A status other than `ok` is never read as zero
use: `exhausted`/`limited` report 0 remaining, any other value reports unknown
remaining. Missing, extra or mistyped windows, a missing reset, or a percent
outside 0–100 fail the whole observation, which keeps earlier observations.
Provider limits are dollar-based per model, so the percentages are an
account-level approximation. See `examples/config.toml`.

## Interpretation

Remaining percentage, reset and observation times are provider facts. A speed
is inferred only when compatible historical observations exist; the first
sample is a warmup and does not imply zero consumption. The transferred
agent-run forecast marks remaining quota at or below 10% as high risk and at or
below 30% as medium; above that it also considers burn against the sustainable
rate when at least one hour of evidence exists. Advice is informational and
does not override an explicit owner choice. Automatic Bridge delivery is deferred.
