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
<binary> -p --model haiku --tools "" --strict-mcp-config --no-session-persistence \
  --disable-slash-commands --setting-sources "" --settings '{"disableAllHooks":true}' \
  --system-prompt "Reply with OK." -- "Reply with OK."
```

Claude Code awaits its OAuth renewal before that single small model request,
under the same cross-process refresh lock its agents use, and writes its own
store with its own permissions. The turn therefore uses a tiny amount of the
subscription. It has no tools, no MCP servers, no hooks, no user/project/local
settings (and so no plugins), no slash commands and no saved session; managed
policy settings still apply. stdin, stdout and stderr are null, so nothing the
CLI prints is read or logged. The working and temporary directories are the
private Quota home, the environment is the collector's restricted one plus
`CLAUDE_CONFIG_DIR` naming the selected login (for named or explicitly located
logins) and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. The run is bounded to
90 seconds and its process group is cleaned up. Quota never exchanges, copies or
writes a token, contacts no endpoint itself, and reads only the new expiry.
`claude auth status` is deliberately not used: in Claude Code 2.1.322 its renewal
runs unawaited, so the process can exit while a rotated token is still unsaved.

Renewal is single-flight: a non-blocking lock (`oauth-renewal.lock`) in the
private Quota home skips the CLI while another renewal runs. At most six CLI
runs per login are started in any trailing 24 hours; the private ledger
`oauth-renewals.json` in the same home records only attempt times under a digest
of the login path. A corrupt, foreign-owned or non-private ledger refuses
renewal rather than lifting the cap; remove it to reset.

The CLI's exit status does not report renewal; the store does. If the token is
still valid but was not renewed, collection proceeds and the next due collection
tries again. If it has expired, the collection fails with
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
