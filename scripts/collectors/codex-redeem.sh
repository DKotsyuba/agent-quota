#!/bin/bash
# Redeem one Codex reset credit through the official app-server method
# `account/rateLimitResetCredit/consume` (params idempotencyKey and creditId).
# A CLI without that method answers "method not found" or "invalid params": nothing
# was spent. Every other RPC error is unknown. Reads the version-1 private context with a
# `redeem` object on stdin and prints {"version":1,"state":...,"reason":...}.
# The request id is the provider idempotency key. Nothing is retried: an ambiguous
# answer is reported as "unknown". No model turn is started.
set -euo pipefail

# Print the verdict and stop; an empty reason becomes null.
emit() {
  jq -cn --arg state "$1" --arg reason "${2:-}" \
    '{version:1, state:$state, reason:(if $reason == "" then null else $reason end)}'
  exit 0
}

context=$(cat)
binary=$(printf '%s' "$context" | jq -er 'select(.version == 1) | .harness.command | select(type == "string" and startswith("/") and ((explode | index(0)) == null))') ||
  emit failed codex_app_server_unavailable
login=$(printf '%s' "$context" | jq -er '.auth | select(.kind == "native_login") | .directory | select(type == "string" and startswith("/") and ((explode | index(0)) == null))') ||
  emit failed credential_unavailable
consume=$(printf '%s' "$context" | jq -ec '.redeem
  | select((.credit_id | type) == "string" and (.request_id | type) == "string")
  | {id:2, method:"account/rateLimitResetCredit/consume", params:{idempotencyKey:.request_id, creditId:.credit_id}}') ||
  emit failed redeem_request_invalid
[[ -x $binary && -r $login/auth.json ]] || emit failed credential_unavailable
umask 077
temporary=$(mktemp -d "${TMPDIR:-/tmp}/agent-quota-redeem.XXXXXXXX")
# Remove the private home; the Rust runner owns process termination.
cleanup() {
  rm -rf -- "$temporary"
}
trap cleanup EXIT
trap 'exit 1' TERM INT
ln -s "$login/auth.json" "$temporary/auth.json"
printf 'cli_auth_credentials_store = "file"\n' > "$temporary/config.toml"
mkfifo "$temporary/input" "$temporary/output"
exec 3<>"$temporary/input"
exec 4<>"$temporary/output"
CODEX_HOME="$temporary" "$binary" app-server 3>&- 4>&- < "$temporary/input" > "$temporary/output" &
# Print the raw response line for one request id, ignoring notifications.
response() {
  local expected=$1 limit=$2 line
  while IFS= read -r -t "$limit" line <&4; do
    if printf '%s' "$line" | jq -e --argjson id "$expected" '.id == $id' > /dev/null 2>&1; then
      printf '%s' "$line"
      return
    fi
  done
  return 1
}
printf '%s\n' '{"id":1,"method":"initialize","params":{"clientInfo":{"name":"agent-quota-redeem","version":"1"},"capabilities":{"experimentalApi":true}}}' >&3
ready=$(response 1 15) || emit failed codex_app_server_unavailable
printf '%s' "$ready" | jq -e 'has("result")' > /dev/null || emit failed codex_app_server_unavailable
printf '%s\n' '{"method":"initialized"}' "$consume" >&3
# From here the credit may be spent: every unclear answer is unknown.
answer=$(response 2 35) || emit unknown provider_timeout
verdict=$(printf '%s' "$answer" | jq -r '
  if has("error") then
    ((if (.error | type) == "object" then .error.code else null end) as $code
     | if $code == -32601 or $code == -32602
       then "failed codex_method_unsupported" else "unknown provider_error" end)
  else (.result.outcome // "") as $outcome
    | if $outcome == "reset" then "redeemed"
      elif $outcome == "alreadyRedeemed" then "redeemed already_redeemed"
      elif $outcome == "nothingToReset" then "rejected nothing_to_reset"
      elif $outcome == "noCredit" then "rejected no_credit"
      else "unknown provider_response_unrecognized" end
  end' 2>/dev/null) || verdict='unknown provider_response_unrecognized'
read -r state reason <<< "$verdict"
emit "$state" "${reason:-}"
