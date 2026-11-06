#!/usr/bin/env bash
# Trial 10's nightly run (DESIGN §7f, DECISION R13 step 0b), started by com.bmv-trader.desk:
# the pinned binary's `desk-run`, unchanged, then one ping so a failed night reaches the owner.
#
#   exit 0      pings https://hc-ping.com/<key>/desk-run
#   otherwise   pings .../desk-run/fail, and passes the exit code on to launchd
#
# The ping key is the Keychain item bmv-trader / HEALTHCHECKS_PING_KEY. It goes to curl on stdin,
# never on the command line. With no key, the failure is only written to the log (run.log).
# Pin once (owner): sudo cp target/release/bmv-trader /usr/local/libexec/bmv-trial10
set -uo pipefail
BIN=${BMV_TRIAL10_BIN:-/usr/local/libexec/bmv-trial10}

"$BIN" desk-run
code=$?
check=desk-run
((code == 0)) || { check=desk-run/fail; echo "desk-run: failed with exit code $code" >&2; }

if key=$(security find-generic-password -s bmv-trader -a HEALTHCHECKS_PING_KEY -w 2>/dev/null); then
    printf 'url = "https://hc-ping.com/%s/%s"\n' "$key" "$check" |
        curl -K - -fsS -o /dev/null --max-time 10 --retry 3 ||
        echo "desk-run: ping $check failed" >&2
else
    echo "desk-run: no HEALTHCHECKS_PING_KEY in the Keychain, ping skipped" >&2
fi
exit "$code"
