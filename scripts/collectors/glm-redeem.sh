#!/bin/bash
# Redeem one GLM reset card. Reads the version-1 private context with a `redeem`
# object on stdin and prints {"version":1,"state":...,"reason":...}.
# The key reaches curl through a pipe, never arguments, environment or files.
# Nothing is retried: an ambiguous provider answer is reported as "unknown".
# An optional first argument is the quota URL whose origin is used, as in glm.sh.
set -euo pipefail

# Print the verdict and stop; an empty reason becomes null.
emit() {
  jq -cn --arg state "$1" --arg reason "${2:-}" \
    '{version:1, state:$state, reason:(if $reason == "" then null else $reason end)}'
  exit 0
}

context=$(cat)
token=$(printf '%s' "$context" | jq -er 'select(.version == 1) | .auth.token | select(type == "string" and length > 0 and (test("[\u0000-\u001f\u007f]") | not))') ||
  emit failed credential_unavailable
case "$token" in *$'\n'*|*$'\r'*) emit failed credential_unavailable ;; esac
case "$token" in [Bb][Ee][Aa][Rr][Ee][Rr]\ *) ;; *) token="Bearer $token" ;; esac
url=${1:-https://api.z.ai/api/monitor/usage/quota/limit}
[[ $url =~ ^(https?://[^/?#]+) ]] || emit failed redeem_request_invalid
use_url="${BASH_REMATCH[1]}/api/biz/customer-package-reset/use"
# A card resets exactly one window; any other scope is refused before the call.
payload=$(printf '%s' "$context" | jq -ec '.redeem
  | select((.credit_id | type) == "string" and (.request_id | type) == "string")
  | {targetType:"PERSONAL",
     resetType:(if .scope == "five_hour" then "FIVE_HOUR" elif .scope == "seven_day" then "WEEK" else error("scope") end),
     recordId:(.credit_id | if test("^[0-9]{1,15}$") then tonumber else . end),
     requestId:.request_id}
    + (if (.provider_kind | type) == "string" and .provider_kind != "unknown" then {grantType:.provider_kind} else {} end)' 2>/dev/null) ||
  emit failed unsupported_scope

code=0
output=$(printf 'Authorization: %s\nAccept: application/json\nContent-Type: application/json\n' "$token" |
  curl -q --silent --connect-timeout 10 --max-time 30 --max-filesize 1048576 --header @- \
    --request POST --data-raw "$payload" --write-out '\n%{http_code}' "$use_url") || code=$?
case $code in
  0) ;;
  6|7) emit failed provider_unreachable ;;
  28) emit unknown provider_timeout ;;
  *) emit unknown network_error ;;
esac
status=${output##*$'\n'}
body=${output%$'\n'*}
# HTTP success alone is insufficient: read the application envelope.
envelope=$(printf '%s' "$body" | jq -r 'if type == "object" then
    [(if .success == true then "true" elif .success == false then "false" else "none" end),
     (if (.code | type) == "number" and .code == (.code | floor) and .code >= 0 and .code < 100000 then (.code | tostring) else "none" end)]
    | join(" ") else "none none" end' 2>/dev/null) || envelope='none none'
success=${envelope% *}
app_code=${envelope#* }
# Only listed answers are verdicts; everything else is unknown. The reference
# documents one success envelope and one authentication code (1001). It documents
# no "card used/expired/unavailable" codes, so this script never reports rejected.
case $status in
  401|403) emit failed auth_rejected ;;
  200)
    if [[ $success == true && ( $app_code == 0 || $app_code == 200 ) ]]; then emit redeemed; fi
    if [[ $success == false && $app_code == 1001 ]]; then emit failed auth_rejected; fi
    if [[ $success == false && $app_code == 429 ]]; then emit unknown rate_limited; fi
    emit unknown provider_response_unrecognized ;;
  429) emit unknown rate_limited ;;
  408|409) emit unknown provider_conflict ;;
  5??) emit unknown provider_error ;;
  *) emit unknown provider_response_unrecognized ;;
esac
