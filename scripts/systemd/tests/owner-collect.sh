#!/bin/bash
# Synthetic launcher checks; run with AQ_TEST_TMP pointing to a private directory.
set -euo pipefail
source "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/agent-quota-owner-collect.sh"
fixture=${AQ_TEST_TMP:?}
mkdir "$fixture/lock"
lock_directory="$fixture/lock"

if [[ $(/usr/bin/stat -c '%u' /usr/bin/env) == 0 ]]; then
    trusted_path /usr/bin/env file
fi
private_mode 700
if private_mode 755 || private_mode 710; then
    printf '%s\n' 'non-private lock mode was accepted' >&2
    exit 1
fi
if [[ -d /root && ! -L /root ]] && [[ $(/usr/bin/stat -c '%u:%a' /root) == 0:700 ]]; then
    trusted_private_directory /root
fi
if [[ -d /var/lib && ! -L /var/lib ]] && [[ $(/usr/bin/stat -c '%u:%a' /var/lib) == 0:755 ]]; then
    if trusted_private_directory /var/lib; then
        printf '%s\n' 'public root-owned lock directory was accepted' >&2
        exit 1
    fi
fi
if (( EUID != 0 )) && trusted_path "$fixture" directory; then
    printf '%s\n' 'worker-owned directory was trusted' >&2
    exit 1
fi
ln -s /usr/bin/env "$fixture/link"
if trusted_path "$fixture/link" file; then
    printf '%s\n' 'symlink was trusted' >&2
    exit 1
fi
if [[ -L /bin ]] && trusted_path /bin/bash file; then
    printf '%s\n' 'symlinked ancestor was trusted' >&2
    exit 1
fi
if unprivileged_user root; then
    printf '%s\n' 'root was accepted as an unprivileged account' >&2
    exit 1
fi
if (( EUID != 0 )); then
    unprivileged_user "$(/usr/bin/id -un)"
    if /bin/bash "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/agent-quota-owner-collect.sh" >/dev/null 2>&1; then
        printf '%s\n' 'non-root launcher was accepted' >&2
        exit 1
    fi
fi

AQ_PROBE_USER=worker
AQ_SERVICE_USER=service
AQ_PROBE_OS_HOME="$fixture/owner home"
AQ_PROBE_PRIVATE_HOME="$fixture/private home"
AQ_PROBE_TMP="$fixture/probe tmp"
AQ_PROBE_PATH=/usr/bin:/bin
AQ_SERVICE_TMP="$fixture/service tmp"
AQ_BINARY="$fixture/binary with spaces"
AQ_CONFIG="$fixture/config with spaces"
AQ_SOCKET="$fixture/socket with spaces"
AQ_ACCOUNT="\$(touch $fixture/injected)"
AQ_PROVIDER='Claude;literal'
AQ_SOURCE='source`literal`'

# Replace only the UID switch: no real user, credential or network is touched.
run_as() {
    local side=$1
    shift
    if [[ -e /proc/self/fd/9 ]]; then
        printf '%s\n' 'lock descriptor leaked' >&2
        return 1
    fi
    if [[ $side == worker ]]; then
        printf '%s\n' "$@" > "$fixture/probe-args"
        if [[ -e $fixture/oversize ]]; then
            /usr/bin/head -c "$((max_observation + 1))" /dev/zero | /usr/bin/tr '\0' x
        else
            printf '%s\n' '{"version":1,"observation":{"synthetic":true}}'
        fi
        if [[ -e $fixture/probe-fail ]]; then
            printf '%s\n' "$(<"$fixture/probe-fail")" >&2
            if [[ -e $fixture/noisy ]]; then
                /usr/bin/head -c 262144 /dev/zero | /usr/bin/tr '\0' x >&2 || return 2
                : > "$fixture/noise-drained"
            fi
            return 1
        fi
        if [[ -e $fixture/interrupt ]]; then
            printf '%s\n' 'SYNTHETIC_PRIVATE_STDERR' >&2
            printf '%s\n' ready > "$fixture/probe-ready"
            read -r _ < "$fixture/probe-release"
            return 1
        fi
    else
        printf '%s\n' invoked >> "$fixture/ingest-calls"
        cat > "$fixture/observation"
        [[ ! -e $fixture/ingest-fail ]]
    fi
}

# The root launcher owns fd 9; neither child is allowed to inherit it.
exec 9<"$fixture/lock"
collect
[[ ! -e $fixture/injected ]]
found=0
while IFS= read -r argument; do
    [[ $argument == "$AQ_ACCOUNT" ]] && found=1
done < "$fixture/probe-args"
(( found == 1 ))
[[ $(<"$fixture/observation") == '{"version":1,"observation":{"synthetic":true}}' ]]

[[ ! -e $fixture/lock/probe-diagnostic ]]
rm "$fixture/ingest-calls"
# Only allowlisted fixed codes reach the journal; other diagnostics stay generic.
for diagnostic in 'synthetic-private-diagnostic' 'quota probe: SYNTHETIC_SECRET_CANARY' \
    'quota probe: credential_expired' 'quota probe: credential_refresh_failed' \
    'quota probe: credential_refresh_limited' 'quota probe: credential_refresh_unsupported_host' \
    'quota probe: credential_refresh_unsupported_host (credential_refresh_policy_cache_unknown)' \
    'quota probe: credential_refresh_cleanup_unconfirmed'; do
    printf '%s\n' "$diagnostic" > "$fixture/probe-fail"
    if collect 2>"$fixture/error"; then
        printf '%s\n' 'late probe failure was accepted' >&2
        exit 1
    fi
    [[ ! -e $fixture/ingest-calls && ! -e $fixture/lock/probe-diagnostic ]]
    case $diagnostic in
        'quota probe: credential_'*)
            expected="quota owner collection: probe failed: ${diagnostic#quota probe: }" ;;
        *) expected='quota owner collection: probe failed' ;;
    esac
    [[ $(<"$fixture/error") == "$expected" ]]
done

: > "$fixture/noisy"
printf '%s\n' 'quota probe: credential_refresh_cleanup_unconfirmed' > "$fixture/probe-fail"
if collect 2>"$fixture/error"; then exit 1; fi
[[ -e $fixture/noise-drained ]]
[[ $(<"$fixture/error") == 'quota owner collection: probe failed: credential_refresh_cleanup_unconfirmed' ]]
# A code found only past byte 128 is discarded, even when a complete line follows it.
printf '%0130d\n%s\n' 0 'quota probe: credential_expired' > "$fixture/probe-fail"
if collect 2>"$fixture/error"; then exit 1; fi
[[ $(<"$fixture/error") == 'quota owner collection: probe failed' ]]
rm "$fixture/noisy"
rm "$fixture/probe-fail"

mkfifo "$fixture/probe-ready" "$fixture/probe-release"
: > "$fixture/interrupt"
(collect) 2>"$fixture/error" &
collection=$!
read -r ready < "$fixture/probe-ready"
[[ $ready == ready ]]
kill -TERM "$collection"
printf '%s\n' release > "$fixture/probe-release"
if wait "$collection"; then exit 1; fi
[[ -z $(/usr/bin/find "$lock_directory" -mindepth 1 -print -quit) ]]
rm "$fixture/interrupt"

: > "$fixture/oversize"
if collect 2>"$fixture/error"; then
    printf '%s\n' 'oversized probe output was accepted' >&2
    exit 1
fi
[[ ! -e $fixture/ingest-calls ]]
rm "$fixture/oversize"

# An unknown ingest is sent once; the owner's policy allows the next scheduled run.
: > "$fixture/ingest-fail"
if collect 2>"$fixture/error"; then
    printf '%s\n' 'failed ingest was accepted' >&2
    exit 1
fi
[[ $(<"$fixture/error") == 'quota owner collection: ingest failed or outcome unknown' ]]
rm "$fixture/ingest-fail"
collect
[[ $(/usr/bin/wc -l < "$fixture/ingest-calls") -eq 2 ]]
exec 9<&-

# A second invocation cannot take the host lock until the first releases it.
/usr/bin/mkfifo "$fixture/ready" "$fixture/release"
lock_pid=
cleanup_lock() {
    if [[ -n $lock_pid ]]; then
        kill "$lock_pid" 2>/dev/null || :
        wait "$lock_pid" 2>/dev/null || :
    fi
}
trap cleanup_lock EXIT
(
    take_lock "$fixture/lock"
    printf '%s\n' ready > "$fixture/ready"
    read -r _ < "$fixture/release"
) &
lock_pid=$!
read -r ready < "$fixture/ready"
[[ $ready == ready ]]
if /usr/bin/flock -n "$fixture/lock" /bin/true; then
    printf '%s\n' 'concurrent collector acquired the lock' >&2
    exit 1
fi
printf '%s\n' release > "$fixture/release"
wait "$lock_pid"
lock_pid=
/usr/bin/flock -n "$fixture/lock" /bin/true
printf '%s\n' 'owner collection checks passed'
