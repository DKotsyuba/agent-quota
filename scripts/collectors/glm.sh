#!/bin/bash
# Read version-1 private context on stdin; emit quota JSON, never credentials.
# An optional first argument overrides the quota URL for an operator or fixture.
set -euo pipefail
directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
context=$(cat)
token=$(printf '%s' "$context" | jq -er 'select(.version == 1) | .auth.token | select(type == "string" and length > 0 and (test("[\u0000-\u001f\u007f]") | not))')
case "$token" in *$'\n'*|*$'\r'*) exit 1 ;; esac
metadata=$(printf '%s' "$context" | jq -ec '{models,now}')
url=${1:-https://api.z.ai/api/monitor/usage/quota/limit}
quota=$(printf 'Authorization: %s\n' "$token" |
  curl -q --fail --silent --show-error --connect-timeout 10 --max-time 25 --max-filesize 2097152 --header @- \
    "$url" |
  jq -ec -L "$directory" --argjson ctx "$metadata" -f "$directory/glm.jq")
# Use the quota URL's origin, including an operator's regional host override.
cards=null
if [[ $url =~ ^(https?://[^/?#]+) ]]; then
  cards_url="${BASH_REMATCH[1]}/api/biz/customer-package-reset/list?targetType=PERSONAL"
  case "$token" in [Bb][Ee][Aa][Rr][Ee][Rr]\ *) ;; *) token="Bearer $token" ;; esac
  cards=$(printf 'Authorization: %s\n' "$token" |
    curl -q --fail --silent --connect-timeout 2 --max-time 3 --max-filesize 2097152 --header @- "$cards_url" |
    jq -ces 'if length == 1 then .[0] else null end' 2>/dev/null) || cards=null
fi
printf '%s\n%s\n' "$quota" "$cards" |
  jq -es -L "$directory" --argjson ctx "$metadata" -f "$directory/glm-credits.jq"
