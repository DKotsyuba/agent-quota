#!/bin/bash
# Read version-1 private OAuth context on stdin and emit normalized usage windows.
# Tokens are passed to curl through a pipe, never command arguments or temp files.
set -euo pipefail
directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
context=$(cat)
token=$(printf '%s' "$context" | jq -er 'select(.version == 1) | .auth.token | select(type == "string" and length > 0 and (test("[\u0000-\u001f\u007f]") | not))')
case "$token" in *$'\n'*|*$'\r'*) exit 1 ;; esac
metadata=$(printf '%s' "$context" | jq -ec '{models,now}')
# Reset grants are only reported to the Claude Code CLI surface; mirror its client headers.
binary=$(printf '%s' "$context" | jq -r '.harness.command // empty')
version=
if [[ $binary == /* && -x $binary ]]; then
  version=$(timeout 5 "$binary" --version 2>/dev/null | awk 'NR == 1 {print $1}') || version=
fi
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || version=2.1.326
url=${1:-https://api.anthropic.com/api/oauth/usage}
case "$url" in *\?*) url="$url&cedar_ember=1" ;; *) url="$url?cedar_ember=1" ;; esac
printf 'Authorization: Bearer %s\nanthropic-beta: oauth-2025-04-20\nUser-Agent: claude-cli/%s (external, cli)\nx-app: cli\n' "$token" "$version" |
  curl -q --fail --silent --show-error --connect-timeout 10 --max-time 25 --max-filesize 2097152 --header @- \
    "$url" |
  jq -e -L "$directory" --argjson ctx "$metadata" -f "$directory/claude.jq"
