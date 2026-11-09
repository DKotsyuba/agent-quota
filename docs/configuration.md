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
supplied Codex, Claude and GLM scripts are copied from agent-run and require jq;
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

## Interpretation

Remaining percentage, reset and observation times are provider facts. A speed
is inferred only when compatible historical observations exist; the first
sample is a warmup and does not imply zero consumption. The transferred
agent-run forecast marks remaining quota at or below 10% as high risk and at or
below 30% as medium; above that it also considers burn against the sustainable
rate when at least one hour of evidence exists. Advice is informational and
does not override an explicit owner choice. Automatic Bridge delivery is deferred.
