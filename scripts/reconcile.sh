#!/usr/bin/env bash
# Reconciliation every 15 minutes, 9:00 to 15:45 ET (DESIGN §12, R13 step 3b), started by
# com.bmv-trader.reconcile: `bmv-trader desk-reconcile`, then one ping, so a dead Mac, a dead job
# or a mismatch reaches the owner.
#
#   exit 0      pings https://hc-ping.com/<key>/reconcile
#   otherwise   pings .../reconcile/fail, and passes the exit code on to launchd
#
# The run's output is the ping's body, so the alert says what was wrong (a mismatch or a failed
# read). The ping runs after the check, so a failed ping never blocks or delays it. The ping key is
# the Keychain item bmv-trader / HEALTHCHECKS_PING_KEY, sent to curl on stdin, never on the command
# line. With no key, only the log (data/reconcile.log) has it.
set -uo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out=$(mktemp) || out=/dev/null
trap 'rm -f "$out"' EXIT

"$root/target/release/bmv-trader" desk-reconcile 2>&1 | tee "$out"
code=${PIPESTATUS[0]}
check=reconcile
((code == 0)) || check=reconcile/fail

if key=$(security find-generic-password -s bmv-trader -a HEALTHCHECKS_PING_KEY -w 2>/dev/null); then
    printf 'url = "https://hc-ping.com/%s/%s"\ndata-binary = "@%s"\n' "$key" "$check" "$out" |
        curl -K - -fsS -o /dev/null --max-time 10 --retry 3 ||
        echo "reconcile: ping $check failed" >&2
else
    echo "reconcile: no HEALTHCHECKS_PING_KEY in the Keychain, ping skipped" >&2
fi
exit "$code"
