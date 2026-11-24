#!/bin/bash
# Synthetic checks of the launcher's redeem mode; run with AQ_TEST_TMP pointing to a
# private directory. Only the UID switch is replaced: no user, credential or network.
set -euo pipefail
source "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/agent-quota-owner-collect.sh"
fixture=${AQ_TEST_TMP:?}
mkdir "$fixture/lock" "$fixture/reports"
lock_directory="$fixture/lock"
redeem_retry_delay=0
redeem_report_delays='0 0'

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
# Stands for the service-owned trigger; the launcher must never touch it.
trigger="$fixture/instance.pending"
: > "$trigger"
fixture_claim='{"version":1,"request_id":"11111111-1111-4111-8111-111111111111","account":"a","claim_token":"0123456789abcdef0123456789abcdef"}'
fixture_outcome='{"version":1,"state":"redeemed","claim_token":"0123456789abcdef0123456789abcdef"}'

# Stand in for both OS users and record every step in order.
run_as() {
    local side=$1 step= number
    shift
    if [[ -e /proc/self/fd/9 ]]; then
        printf '%s\n' 'lock descriptor leaked' >&2
        return 1
    fi
    for argument in "$@"; do
        case $argument in redeem-claim|redeem-execute|redeem-report|probe|ingest) step=$argument ;; esac
    done
    [[ -n $step ]] || { printf '%s\n' 'unexpected command' >> "$fixture/steps"; return 1; }
    printf '%s %s\n' "$side" "$step" >> "$fixture/steps"
    case $step in
        redeem-claim)
            if [[ -e $fixture/claim-flaky ]]; then
                rm "$fixture/claim-flaky"
                return 1
            fi
            [[ ! -e $fixture/claim-fail ]] || return 1
            [[ ! -e $fixture/claim-none ]] || return 3
            if [[ -e $fixture/claim-oversize ]]; then
                /usr/bin/head -c "$((max_message + 1))" /dev/zero | /usr/bin/tr '\0' x
            else
                printf '%s\n' "$fixture_claim"
            fi ;;
        redeem-execute)
            cat > "$fixture/handed"
            printf '%s\n' "$@" > "$fixture/execute-args"
            printf '%s\n' 'SYNTHETIC_PRIVATE_STDERR' >&2
            [[ ! -e $fixture/execute-fail ]] || return 1
            printf '%s\n' "$fixture_outcome" ;;
        redeem-report)
            # Keep every attempt's arguments and payload for comparison.
            number=$(/usr/bin/find "$fixture/reports" -name 'payload.*' | /usr/bin/wc -l)
            printf '%s\n' "$@" > "$fixture/reports/args.$number"
            cat > "$fixture/reports/payload.$number"
            if [[ -e $fixture/report-flaky ]]; then
                rm "$fixture/report-flaky"
                return 1
            fi
            [[ ! -e $fixture/report-fail ]] ;;
        probe) printf '%s\n' '{"version":1,"observation":{"synthetic":true}}' ;;
        ingest) cat > /dev/null ;;
    esac
}
steps() { /usr/bin/tr '\n' ',' < "$fixture/steps"; rm "$fixture/steps"; }

# Require exactly this many report attempts, all with the expected payload and with
# identical arguments, then forget them.
reports() {
    local expected=$1 payload=$2 flags=$3 index
    [[ $(/usr/bin/find "$fixture/reports" -name 'payload.*' | /usr/bin/wc -l) -eq $expected ]]
    for (( index = 0; index < expected; index++ )); do
        [[ $(<"$fixture/reports/payload.$index") == "$payload" ]]
        /usr/bin/cmp -s "$fixture/reports/args.0" "$fixture/reports/args.$index"
        [[ $(/usr/bin/tr '\n' ' ' < "$fixture/reports/args.$index") == *"$flags"* ]]
    done
    /usr/bin/find "$fixture/reports" -type f -delete
}

exec 9<"$fixture/lock"
# Claim as the service, execute as the owner, report as the service: once each.
redeem 2>"$fixture/error"
[[ $(steps) == 'service redeem-claim,worker redeem-execute,service redeem-report,' ]]
[[ $(<"$fixture/handed") == "$fixture_claim" ]]
reports 1 "$fixture_outcome" 'redeem-report --socket '
[[ $(<"$fixture/error") == 'quota owner redeem: outcome reported' ]]
[[ ! -e $fixture/injected ]]
found=0
while IFS= read -r argument; do
    [[ $argument == "$AQ_ACCOUNT" ]] && found=1
done < "$fixture/execute-args"
(( found == 1 ))

# Nothing pending is success without an execution or a report.
: > "$fixture/claim-none"
redeem 2>"$fixture/error"
[[ $(steps) == 'service redeem-claim,' ]]
[[ $(<"$fixture/error") == 'quota owner redeem: no pending request' ]]

# A claim whose reply was lost is not second-guessed: when the retry finds nothing,
# the launcher reports nothing and the service's own time limit decides.
: > "$fixture/claim-flaky"
redeem 2>"$fixture/error"
[[ $(steps) == 'service redeem-claim,service redeem-claim,' ]]
reports 0 '' ''
rm "$fixture/claim-none"

# An unreachable service is retried for the claim only. Nothing is executed or
# reported, and the trigger stays for the service to remove.
: > "$fixture/claim-fail"
if redeem 2>"$fixture/error"; then exit 1; fi
[[ $(steps) == 'service redeem-claim,service redeem-claim,service redeem-claim,' ]]
[[ $(<"$fixture/error") == 'quota owner redeem: claim failed' ]]
rm "$fixture/claim-fail"
: > "$fixture/claim-oversize"
if redeem 2>/dev/null; then exit 1; fi
[[ $(steps) == 'service redeem-claim,' ]]
reports 0 '' ''
rm "$fixture/claim-oversize"
[[ -e $trigger ]]

# A failed execution is never repeated. The launcher reports unknown for exactly the
# claim it holds: the claim itself is the payload, so its token travels with it.
: > "$fixture/execute-fail"
if redeem 2>"$fixture/error"; then exit 1; fi
[[ $(steps) == 'service redeem-claim,worker redeem-execute,service redeem-report,' ]]
reports 1 "$fixture_claim" '--state unknown --reason executor_failed '
[[ $(<"$fixture/error") == 'quota owner redeem: execution or report failed; outcome unknown' ]]
rm "$fixture/execute-fail"

# A report that fails once is sent again with the same payload; the provider call is not.
: > "$fixture/report-flaky"
redeem 2>"$fixture/error"
[[ $(steps) == 'service redeem-claim,worker redeem-execute,service redeem-report,service redeem-report,' ]]
reports 2 "$fixture_outcome" 'redeem-report --socket '
[[ $(<"$fixture/error") == 'quota owner redeem: outcome reported' ]]

# The retries are bounded and identical; exhausting them fails without another execution.
: > "$fixture/report-fail"
if redeem 2>"$fixture/error"; then exit 1; fi
[[ $(steps) == 'service redeem-claim,worker redeem-execute,service redeem-report,service redeem-report,service redeem-report,' ]]
reports 3 "$fixture_outcome" 'redeem-report --socket '
: > "$fixture/execute-fail"
if redeem 2>"$fixture/error"; then exit 1; fi
[[ $(steps) == 'service redeem-claim,worker redeem-execute,service redeem-report,service redeem-report,service redeem-report,' ]]
reports 3 "$fixture_claim" '--state unknown --reason executor_failed '
rm "$fixture/report-fail" "$fixture/execute-fail"
if /usr/bin/grep -q SYNTHETIC_PRIVATE_STDERR "$fixture/error"; then exit 1; fi
[[ -e $trigger ]]
exec 9<&-

# A bounded lock wait gives up instead of blocking behind a long collection.
/usr/bin/mkfifo "$fixture/ready" "$fixture/release"
(
    take_lock "$fixture/lock"
    printf '%s\n' ready > "$fixture/ready"
    read -r _ < "$fixture/release"
) &
holder=$!
read -r ready < "$fixture/ready"
if (take_lock "$fixture/lock" 1); then
    printf '%s\n' 'bounded lock wait acquired a held lock' >&2
    exit 1
fi
printf '%s\n' release > "$fixture/release"
wait "$holder"
(take_lock "$fixture/lock" 1)

# The reserve kept before a claim covers the documented worst case within the unit deadline.
(( redeem_lock_wait + redeem_reserve <= redeem_unit_budget ))
(( redeem_reserve >= 19 + 95 + 300 + 5 + 160 + redeem_outage_delay ))

# The unit entry point validates the mode before any step. A redeem run that fails,
# for whatever reason, pauses once before it exits so the path unit cannot spin; a
# failed collection run does not.
pause() { printf '%s\n' "$1" >> "$fixture/pauses"; }
if (( EUID != 0 )); then
    if main redeem 2>/dev/null || main redeem extra 2>/dev/null; then exit 1; fi
    [[ $(/usr/bin/tr '\n' ',' < "$fixture/pauses") == '30,30,' ]]
    if main other 2>/dev/null || main collect 2>/dev/null || main 2>/dev/null; then exit 1; fi
    [[ $(/usr/bin/wc -l < "$fixture/pauses") -eq 2 ]]
    [[ ! -e $fixture/steps ]]
fi

# Outage, then recovery: the failed run leaves the trigger alone, and the next run
# started by the path unit takes the request without any operator action.
exec 9<"$fixture/lock"
: > "$fixture/claim-fail"
if redeem 2>/dev/null; then exit 1; fi
[[ $(steps) == 'service redeem-claim,service redeem-claim,service redeem-claim,' && -e $trigger ]]
rm "$fixture/claim-fail"
redeem 2>"$fixture/error"
[[ $(steps) == 'service redeem-claim,worker redeem-execute,service redeem-report,' ]]
reports 1 "$fixture_outcome" 'redeem-report --socket '
exec 9<&-
