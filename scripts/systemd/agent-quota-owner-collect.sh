#!/bin/bash
# Run one configured credential-owner probe and submit its result as the service user.
set -euo pipefail
umask 077
export LC_ALL=C

# The unit supplies this fixed root-owned directory via StateDirectory=.
lock_directory=/var/lib/agent-quota-owner-collect
max_observation=2097152

# Require every path component to be owned by root and not writable by other users.
trusted_path() {
    local candidate=$1 expected=$2 current=/ part owner mode index=0
    local -a components
    [[ $candidate == /* && $candidate != / && $candidate != *[[:cntrl:]]* ]] || return 1
    read -r owner mode < <(/usr/bin/stat -c '%u %a' -- /) || return 1
    [[ $owner == 0 && $mode =~ ^[0-7]{3,4}$ ]] && (( (8#$mode & 0022) == 0 )) || return 1
    IFS=/ read -r -a components <<< "${candidate#/}"
    for part in "${components[@]}"; do
        [[ -n $part && $part != . && $part != .. ]] || return 1
        current="${current%/}/$part"
        [[ ! -L $current ]] || return 1
        if (( index + 1 < ${#components[@]} )); then
            [[ -d $current ]] || return 1
        elif [[ $expected == directory ]]; then
            [[ -d $current ]] || return 1
        else
            [[ -f $current ]] || return 1
        fi
        read -r owner mode < <(/usr/bin/stat -c '%u %a' -- "$current") || return 1
        [[ $owner == 0 && $mode =~ ^[0-7]{3,4}$ ]] || return 1
        (( (8#$mode & 0022) == 0 )) || return 1
        (( index += 1 ))
    done
}

# Check Unix mode text without consulting or changing filesystem permissions.
private_mode() {
    local mode=$1
    [[ $mode =~ ^[0-7]{3,4}$ ]] && (( (8#$mode & 0077) == 0 ))
}

# The lock directory must not be accessible to either unprivileged side.
trusted_private_directory() {
    local mode
    trusted_path "$1" directory || return 1
    mode=$(/usr/bin/stat -c '%a' -- "$1") || return 1
    private_mode "$mode"
}

# Only an unprivileged named OS account may own either side of the handoff.
unprivileged_user() {
    local uid
    uid=$(/usr/bin/id -u -- "$1") || return 1
    [[ $uid =~ ^[0-9]+$ ]] && (( uid != 0 ))
}

# A single host lock serializes sources and accounts until the ingest response.
take_lock() {
    exec 9<"$1"
    /usr/bin/flock -x 9
}

# Execute a command only after switching to its configured unprivileged OS account.
run_as() {
    local selected_user=$1
    shift
    /usr/sbin/runuser -u "$selected_user" -- /usr/bin/env -i "$@"
}

# Capture a complete probe before submitting it; no credential or envelope is logged.
collect() {
    local observation
    if ! observation=$(
        {
            run_as "$AQ_PROBE_USER" \
                HOME="$AQ_PROBE_OS_HOME" USER="$AQ_PROBE_USER" LOGNAME="$AQ_PROBE_USER" \
                PATH="$AQ_PROBE_PATH" TMPDIR="$AQ_PROBE_TMP" TMP="$AQ_PROBE_TMP" TEMP="$AQ_PROBE_TMP" \
                "$AQ_BINARY" probe --home "$AQ_PROBE_PRIVATE_HOME" --config "$AQ_CONFIG" \
                --account "$AQ_ACCOUNT" --provider "$AQ_PROVIDER" --source "$AQ_SOURCE" 9<&- |
                /usr/bin/head -c "$((max_observation + 1))" 9<&-
        } 2>/dev/null
    ) || (( ${#observation} > max_observation )); then
        printf '%s\n' 'quota owner collection: probe failed' >&2
        return 1
    fi
    if ! {
        printf '%s\n' "$observation" | run_as "$AQ_SERVICE_USER" \
            PATH=/usr/bin:/bin TMPDIR="$AQ_SERVICE_TMP" TMP="$AQ_SERVICE_TMP" TEMP="$AQ_SERVICE_TMP" \
            "$AQ_BINARY" ingest --socket "$AQ_SOCKET" 9<&- >/dev/null
    } 2>/dev/null; then
        printf '%s\n' 'quota owner collection: ingest failed or outcome unknown' >&2
        return 1
    fi
}

# The caller is a root-managed systemd unit, never a general-purpose sudo endpoint.
main() {
    (( EUID == 0 )) || { printf '%s\n' 'quota owner collection: root required' >&2; return 1; }
    local name
    for name in AQ_BINARY AQ_CONFIG AQ_SOCKET AQ_SERVICE_USER AQ_SERVICE_TMP \
        AQ_PROBE_USER AQ_PROBE_OS_HOME AQ_PROBE_PRIVATE_HOME AQ_PROBE_TMP AQ_PROBE_PATH \
        AQ_ACCOUNT AQ_PROVIDER AQ_SOURCE; do
        [[ -n ${!name:-} ]] || { printf '%s\n' 'quota owner collection: missing setting' >&2; return 1; }
    done
    for name in AQ_BINARY AQ_CONFIG AQ_SOCKET AQ_SERVICE_TMP AQ_PROBE_OS_HOME AQ_PROBE_PRIVATE_HOME AQ_PROBE_TMP; do
        [[ ${!name} == /* && ${!name} != *[[:cntrl:]]* ]] || {
            printf '%s\n' 'quota owner collection: invalid path' >&2; return 1;
        }
    done
    trusted_path "$AQ_BINARY" file && [[ -x $AQ_BINARY ]] &&
        trusted_path "$AQ_CONFIG" file && trusted_private_directory "$lock_directory" &&
        unprivileged_user "$AQ_SERVICE_USER" && unprivileged_user "$AQ_PROBE_USER" || {
            printf '%s\n' 'quota owner collection: unsafe deployment settings' >&2; return 1;
        }
    take_lock "$lock_directory"
    collect
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
