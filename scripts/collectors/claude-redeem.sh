#!/bin/bash
# Redeem one banked Claude reset grant. Reads the version-1 private context with a
# `redeem` object on stdin and prints {"version":1,"state":...,"reason":...}.
# The token reaches curl through a pipe, never arguments, environment or files.
# Nothing is retried: an ambiguous provider answer is reported as "unknown".
# An optional first argument overrides the API origin for an operator or fixture.
set -euo pipefail

# Print the verdict and stop; an empty reason becomes null.
emit() {
  jq -cn --arg state "$1" --arg reason "${2:-}" \
    '{version:1, state:$state, reason:(if $reason == "" then null else $reason end)}'
  exit 0
}

# Run curl with headers on stdin; set `status` and `body`, return curl's exit code.
call() {
  local limit=$1 output code=0
  shift
  output=$(curl -q --silent --connect-timeout 10 --max-time "$limit" --max-filesize 1048576 \
    --header @- --write-out '\n%{http_code}' "$@") || code=$?
  status=${output##*$'\n'}
  body=${output%$'\n'*}
  return "$code"
}

context=$(cat)
token=$(printf '%s' "$context" | jq -er 'select(.version == 1) | .auth.token | select(type == "string" and length > 0 and (test("[\u0000-\u001f\u007f]") | not))') ||
  emit failed credential_unavailable
case "$token" in *$'\n'*|*$'\r'*) emit failed credential_unavailable ;; esac
payload=$(printf '%s' "$context" | jq -ec '.redeem
  | select((.credit_id | type) == "string" and (.request_id | type) == "string")
  | {program:"cedar_ember", grant_id:(.credit_id | sub("^grant:"; "")), request_id}') ||
  emit failed redeem_request_invalid
# Reset grants belong to the Claude Code CLI surface; mirror its client headers.
binary=$(printf '%s' "$context" | jq -r '.harness.command // empty')
version=
if [[ $binary == /* && -x $binary ]]; then
  version=$(timeout 5 "$binary" --version 2>/dev/null | awk 'NR == 1 {print $1}') || version=
fi
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || version=2.1.326
base=${1:-https://api.anthropic.com}
base=${base%/}
headers() {
  printf 'Authorization: Bearer %s\nanthropic-beta: oauth-2025-04-20\nUser-Agent: claude-cli/%s (external, cli)\nx-app: cli\n' "$token" "$version"
}

# The read-only profile names the organization; its failure spends nothing.
code=0
call 10 "$base/api/oauth/profile" < <(headers) || code=$?
(( code == 0 )) || emit failed profile_unavailable
case $status in
  200) ;;
  401|403) emit failed auth_rejected ;;
  *) emit failed profile_unavailable ;;
esac
organization=$(printf '%s' "$body" | jq -er '.organization.uuid | select(type == "string" and test("^[0-9A-Fa-f-]{8,64}$"))' 2>/dev/null) ||
  emit failed organization_unknown

code=0
call 30 --request POST --header 'Content-Type: application/json' --data-raw "$payload" \
  "$base/api/organizations/$organization/reset_rate_limits" < <(headers) || code=$?
case $code in
  0) ;;
  6|7) emit failed provider_unreachable ;;
  28) emit unknown provider_timeout ;;
  *) emit unknown network_error ;;
esac
# Only listed answers are verdicts. Everything else, including any other 4xx, is unknown.
case $status in
  2??)
    result=$(printf '%s' "$body" | jq -r 'if type == "object" and (.result | type) == "string" then .result else "" end' 2>/dev/null) || result=
    case $result in
      reset) emit redeemed ;;
      already_used|not_limited|cooldown|ineligible|unavailable) emit rejected "$result" ;;
      *) emit unknown provider_response_unrecognized ;;
    esac ;;
  401|403) emit failed auth_rejected ;;
  429) emit unknown rate_limited ;;
  408|409) emit unknown provider_conflict ;;
  5??) emit unknown provider_error ;;
  *) emit unknown provider_response_unrecognized ;;
esac
