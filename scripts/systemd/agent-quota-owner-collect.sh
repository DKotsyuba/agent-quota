#!/bin/bash
# Run one configured credential-owner probe and submit its result as the service user.
# With the single argument `redeem`, first execute the account's pending reset-credit
# redemption as the credential owner, report it, then collect as usual.
set -euo pipefail
umask 077
export LC_ALL=C

# The unit supplies this fixed root-owned directory via StateDirectory=.
lock_directory=/var/lib/agent-quota-owner-collect
max_observation=2097152
# Redemption claims and outcomes are single short JSON lines.
max_message=4096
redeem_retry_delay=2
# Report retries: about two minutes of waiting in total.
redeem_report_delays='2 4 8 16 30 30 30'
# Seconds a failed redeem run waits before exiting.
redeem_outage_delay=30
# One deadline covers the whole redeem run: the unit's TimeoutStartSec in seconds.
redeem_unit_budget=1200
# Longest wait for the shared lock.
redeem_lock_wait=120
# Worst case after the lock, in seconds: three claim calls of 5 with two pauses (19),
# credential renewal (95), the redemption script's upper limit (300) with process
# cleanup (5), eight report calls of 5 plus their pauses (160), the pause after a
# failed run (30), and a margin (51).
redeem_reserve=660

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
# An optional second argument bounds the wait in seconds.
take_lock() {
    exec 9<"$1"
    if [[ -n ${2:-} ]]; then
        /usr/bin/flock -x -w "$2" 9
    else
        /usr/bin/flock -x 9
    fi
}

# Execute a command only after switching to its configured unprivileged OS account.
run_as() {
    local selected_user=$1
    shift
    /usr/sbin/runuser -u "$selected_user" -- /usr/bin/env -i "$@"
}

# Echo only an allowlisted fixed probe failure code.
probe_code() {
    case $1 in
        'quota probe: credential_unavailable' | 'quota probe: credential_expired' | \
        'quota probe: credential_refresh_failed' | 'quota probe: credential_refresh_limited' | \
        'quota probe: credential_refresh_unsupported_host' | \
        'quota probe: credential_refresh_unsupported_host (credential_refresh_policy_cache_unknown)' | \
        'quota probe: credential_refresh_cleanup_unconfirmed' | \
        'quota probe: external_probe_failed' | \
        'quota probe: external_config_invalid' | 'quota probe: external_source_unavailable' | \
        'quota probe: collector_output_invalid' | 'quota probe: collector_output_empty' | \
        'quota probe: collector_output_contains_credential')
            printf '%s' "${1#quota probe: }" ;;
    esac
}

# Retain the first line of at most 128 bytes. Drain the original pipe through EOF
# so a noisy diagnostic cannot interrupt the probe with SIGPIPE.
read_probe_code() {
    local line
    line=$(/usr/bin/head -c 128 | {
        IFS= read -r line || :
        /usr/bin/cat >/dev/null
        probe_code "$line"
    })
    /usr/bin/cat >/dev/null
    printf '%s' "$line"
}

# Capture a complete probe before submitting it; no credential or envelope is logged.
# The diagnostic uses only pipes, so interruption leaves no files.
collect() {
    local observation code
    if ! observation=$(
        {
            # FD 3 sends observation stdout past the inner diagnostic capture.
            exec 3>&1
            if code=$(
                run_as "$AQ_PROBE_USER" \
                    HOME="$AQ_PROBE_OS_HOME" USER="$AQ_PROBE_USER" LOGNAME="$AQ_PROBE_USER" \
                    PATH="$AQ_PROBE_PATH" TMPDIR="$AQ_PROBE_TMP" TMP="$AQ_PROBE_TMP" TEMP="$AQ_PROBE_TMP" \
                    "$AQ_BINARY" probe --home "$AQ_PROBE_PRIVATE_HOME" --config "$AQ_CONFIG" \
                    --account "$AQ_ACCOUNT" --provider "$AQ_PROVIDER" --source "$AQ_SOURCE" \
                    9<&- 2>&1 1>&3 3>&- | read_probe_code 3>&- 9<&-
            ); then
                exit 0
            fi
            printf '\nquota probe: %s\n' "$code"
            exit 1
        } 2>/dev/null | /usr/bin/head -c "$((max_observation + 1))" 9<&-
    ) || (( ${#observation} > max_observation )); then
        code=
        if (( ${#observation} < max_observation )); then
            code=$(probe_code "${observation##*$'\n'}")
        fi
        printf '%s\n' "quota owner collection: probe failed${code:+: $code}" >&2
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

# Run a Quota subcommand as the service user against its socket.
service_call() {
    run_as "$AQ_SERVICE_USER" \
        PATH=/usr/bin:/bin TMPDIR="$AQ_SERVICE_TMP" TMP="$AQ_SERVICE_TMP" TEMP="$AQ_SERVICE_TMP" \
        "$AQ_BINARY" "$@" --socket "$AQ_SOCKET" 9<&-
}

# Send one report until the service accepts it, with bounded backoff. Stdin holds
# the message and the arguments are passed to redeem-report. The payload never
# changes between attempts and the service treats a repeated report as a no-op.
report() {
    local payload delay
    payload=$(/usr/bin/cat)
    for delay in $redeem_report_delays ''; do
        printf '%s\n' "$payload" | service_call redeem-report "$@" >/dev/null 2>&1 && return 0
        [[ -z $delay ]] || /usr/bin/sleep "$delay"
    done
    return 1
}

# Claim the pending request once, redeem it as the credential owner and report the
# outcome under the claim's token. Never repeats a provider call: without a usable
# outcome the claim is reported as unknown. A claim whose reply is lost stays taken
# and the service ends it as unknown. Neither the claim nor the outcome is logged,
# and the trigger is left to the service, which removes it when it hands out, ends
# or refuses the request.
redeem() {
    local claim='' outcome='' status=1 attempt
    for attempt in 1 2 3; do
        status=0
        claim=$(service_call redeem-claim --account "$AQ_ACCOUNT" 2>/dev/null |
            /usr/bin/head -c "$((max_message + 1))") || status=$?
        (( status == 0 || status == 3 )) && break
        (( attempt == 3 )) || /usr/bin/sleep "$redeem_retry_delay"
    done
    if (( status == 3 )); then
        printf '%s\n' 'quota owner redeem: no pending request' >&2
        return 0
    fi
    if (( status != 0 )) || [[ -z $claim ]] || (( ${#claim} > max_message )); then
        printf '%s\n' 'quota owner redeem: claim failed' >&2
        return 1
    fi
    outcome=$(printf '%s\n' "$claim" | run_as "$AQ_PROBE_USER" \
        HOME="$AQ_PROBE_OS_HOME" USER="$AQ_PROBE_USER" LOGNAME="$AQ_PROBE_USER" \
        PATH="$AQ_PROBE_PATH" TMPDIR="$AQ_PROBE_TMP" TMP="$AQ_PROBE_TMP" TEMP="$AQ_PROBE_TMP" \
        "$AQ_BINARY" redeem-execute --home "$AQ_PROBE_PRIVATE_HOME" --config "$AQ_CONFIG" \
        --account "$AQ_ACCOUNT" --provider "$AQ_PROVIDER" --source "$AQ_SOURCE" \
        9<&- 2>/dev/null | /usr/bin/head -c "$((max_message + 1))") || outcome=
    if [[ -n $outcome ]] && (( ${#outcome} <= max_message )); then
        printf '%s\n' "$outcome" | report || status=1
    else
        # The service derives request and token from the claim itself.
        printf '%s\n' "$claim" | report --state unknown --reason executor_failed || :
        status=1
    fi
    if (( status != 0 )); then
        printf '%s\n' 'quota owner redeem: execution or report failed; outcome unknown' >&2
        return 1
    fi
    printf '%s\n' 'quota owner redeem: outcome reported' >&2
}

# Wait before a failed redeem run ends. Overridden by the synthetic tests.
pause() {
    /usr/bin/sleep "$1"
}

# Entry point. A redeem run that fails for any reason, such as an unreachable
# service, a failed claim or bad settings, waits before it exits: while the
# service-owned trigger exists the path unit starts this unit again at once, and
# this bounded pause is what keeps that from becoming a tight loop.
main() {
    local status=0
    run "$@" || status=$?
    if (( status != 0 )) && [[ ${1:-} == redeem ]]; then
        pause "$redeem_outage_delay"
    fi
    return "$status"
}

# The caller is a root-managed systemd unit, never a general-purpose sudo endpoint.
run() {
    (( EUID == 0 )) || { printf '%s\n' 'quota owner collection: root required' >&2; return 1; }
    local name mode=${1:-collect} result=0
    [[ $# -le 1 && ( $mode == collect || $mode == redeem ) ]] || {
        printf '%s\n' 'quota owner collection: invalid mode' >&2; return 1;
    }
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
    if [[ $mode == redeem ]]; then
        take_lock "$lock_directory" "$redeem_lock_wait" || {
            printf '%s\n' 'quota owner redeem: lock wait exhausted' >&2; return 1;
        }
        # Claim only when the whole worst case after it still fits the unit deadline.
        if (( SECONDS + redeem_reserve > redeem_unit_budget )); then
            printf '%s\n' 'quota owner redeem: lock wait exhausted' >&2
            return 1
        fi
        redeem || result=1
        # Refresh the inventory after any attempt; its failure does not change the outcome.
        collect || :
        return "$result"
    fi
    take_lock "$lock_directory" || return 1
    collect
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
    main "$@"
fi
