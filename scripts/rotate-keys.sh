#!/usr/bin/env bash
# Monthly key rotation, step by step (DECISION S41, DESIGN §4c). Run as you, on or after the 1st:
#
#   scripts/rotate-keys.sh
#
# Anthropic keys can't be created by API (the Admin API only lists, renames and deactivates them),
# so you make each new key in the Claude Console and this script does the rest:
#   - takes it at a hidden prompt into a temporary Keychain item
#   - proves it works with a free call (the models list)
#   - only then replaces the real item, so a mistyped key never breaks anything
#   - puts it everywhere it is used, and tells you which old key to turn off
# Every step can be skipped, and a step that stopped halfway can be resumed. No key is ever printed
# or put in argv.
#
# Steps:
#   1. renew this month's desk budgets                (Touch ID per agent)
#   2. the engine's Claude key       bmv-trader / ANTHROPIC_API_KEY       (trial 10, the notes)
#   3. the desk agents' Claude key   bmv-desk-models / anthropic-desk     (every agent's keys file)
#   4. the desk agents' OmniRoute key   bmv-desk-models / omniroute-agents  (free models; optional)
# The engine's own OmniRoute key already rotates itself every day (llm-rotate, 05:30).
set -euo pipefail
cd "$(dirname "$0")/.."

DESK=/usr/local/libexec/bmv-desk/desk
DESK_AGENT=/usr/local/libexec/bmv-desk/desk-agent
AGENTS=/usr/local/etc/bmv-clip/agents.json
CLIP_USER=bmvclip
CONSOLE=https://console.anthropic.com/settings/keys
MONTH=$(TZ=America/New_York date +%Y-%m)
DATES=data/keys_rotated.json

[[ $EUID -ne 0 ]] || { echo "rotate-keys: run as yourself, not root (the keys are in your Keychain)" >&2; exit 1; }

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }
ask() { local a; read -r -p "$1 [y/N] " a; [[ $a == y || $a == Y ]]; }
pause() { read -r -p "$1 Press Enter when done. " _; }

# a free call that proves a key works; the key goes on stdin, never in argv, and the request uses
# no proxy and follows no redirect, since it carries the key
works() {  # works SERVICE ACCOUNT
    security find-generic-password -s "$1" -a "$2" -w | /usr/bin/python3 -I -c '
import json, sys, urllib.request, urllib.error
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        raise urllib.error.URLError("refusing to follow a redirect with the key")
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect)
key = sys.stdin.readline().strip()
req = urllib.request.Request("https://api.anthropic.com/v1/models?limit=1", headers={
    "x-api-key": key, "anthropic-version": "2026-10-21", "user-agent": "bmv-trader rotate-keys"})
try:
    with opener.open(req, timeout=20) as r:
        json.load(r)
    print("  works: Anthropic accepted the new key")
except urllib.error.HTTPError as e:
    sys.exit(f"  FAILED: Anthropic answered {e.code}; the key was not accepted")
except Exception as e:
    sys.exit(f"  FAILED: {type(e).__name__}; check the network and try again")
'
}

# the new key goes into SERVICE / ACCOUNT.new first; only after it works does it replace the real
# item, through `security -i` on stdin so it never appears in a process list
store_tested() {  # store_tested SERVICE ACCOUNT [anthropic]
    echo "  Paste the new key at the hidden prompt (security asks twice):"
    security add-generic-password -U -s "$1" -a "$2.new" -w
    if [[ ${3:-} == anthropic ]] && ! works "$1" "$2.new"; then
        security delete-generic-password -s "$1" -a "$2.new" >/dev/null
        echo "  The old key is untouched. Check you copied the whole key, from the right workspace." >&2
        return 1
    fi
    security find-generic-password -s "$1" -a "$2.new" -w | /usr/bin/python3 -I -c '
import re, sys
service, account = sys.argv[1], sys.argv[2]
key = sys.stdin.readline().strip()
if not re.fullmatch(r"[A-Za-z0-9_-]{20,300}", key):
    sys.exit("rotate-keys: that key has unexpected characters; nothing changed")
print(f"add-generic-password -U -s {service} -a {account} -w \"{key}\"")
' "$1" "$2" | security -i >/dev/null
    security delete-generic-password -s "$1" -a "$2.new" >/dev/null
    echo "  stored as $1 / $2"
}

# the installed agents as "id runs_as signs" lines; a failure here stops the script, so a broken
# list can never look like "no agents to update"
agent_rows() {
    [[ -e $AGENTS ]] || { echo "rotate-keys: $AGENTS isn't installed; run sudo scripts/clip-agents-setup.sh" >&2; return 1; }
    local list
    list=$("$DESK_AGENT" --list "$AGENTS")
    echo "$list" | /usr/bin/python3 -I -c '
import json, sys
rows = json.load(sys.stdin)
if not rows:
    sys.exit("rotate-keys: the agents file lists no agents")
for a in rows:
    print(a["id"], a["runs_as"], "yes" if a["signs"] else "no")'
}

# swaps one key into every agent that should hold it; true only if every swap succeeded
push() {  # push anthropic|omniroute
    local rows id runs_as signs failed=0 ok=0
    rows=$(agent_rows) || return 1
    while read -r id runs_as signs; do
        # no Claude key in Paperclip's own user's files (S39); no free-route key for a signer (A5)
        if [[ $1 == anthropic && $runs_as == "$CLIP_USER" ]] || [[ $1 == omniroute && $signs == yes ]]; then
            echo "  skipping $id (it shouldn't hold this key)"
            continue
        fi
        if scripts/clip-key-swap.sh "$id" "$1"; then ok=$((ok + 1)); else failed=$((failed + 1)); fi
    done <<< "$rows"
    if (( failed > 0 )); then
        echo "  $failed agent(s) still have the old key. Fix the error above, then rerun this step and" >&2
        echo "  answer yes to 'already stored'. Don't disable the old key yet." >&2
        return 1
    fi
    echo "  updated $ok agent(s)"
}

# records one key's rotation date, keeping the other's; written beside, then renamed
record() {  # record engine|desk
    mkdir -p data
    /usr/bin/python3 -I -c '
import datetime, json, os, sys
path, key = sys.argv[1], sys.argv[2]
try:
    with open(path) as f:
        dates = json.load(f)
    if not isinstance(dates, dict):
        dates = {}
except (FileNotFoundError, ValueError):
    dates = {}
dates[key] = datetime.datetime.now(datetime.timezone.utc).isoformat()
with open(path + ".new", "w") as f:
    json.dump(dates, f)
os.replace(path + ".new", path)
' "$DATES" "$1"
}

et_hour=$(TZ=America/New_York date +%H)
if (( 10#$et_hour >= 16 && 10#$et_hour < 19 )); then
    echo "Note: it's $(TZ=America/New_York date +%H:%M) in New York. Trial 10 runs at 16:30 and the notes at 18:15;"
    echo "rotating the engine key during them could fail one run. Before 16:00 or after 19:00 is best."
    ask "Carry on anyway?" || exit 0
fi

say "Monthly rotation for $MONTH"
echo "You'll need the Claude Console open at $CONSOLE, signed in as an admin, and Touch ID."

say "Step 1 of 4: renew this month's desk budgets"
echo "Desk budgets are per calendar month; a signer with none can't make a paid call."
if ask "Renew them now?"; then
    scripts/desk-wave1.sh budgets
    echo "  Approve each with: $DESK sign <id>   (list them: $DESK pending; they expire in 10 minutes)"
    pause "  Sign every budget request."
fi

say "Step 2 of 4: the engine's Claude key (trial 10's analyst and the daily notes)"
if ask "Rotate it now?"; then
    echo "  1. In the Console, pick the engine's workspace (the one trial 10 uses)."
    echo "  2. Click 'Create Key' and name it bmv-engine-$MONTH."
    echo "  3. Copy it. The Console shows it only once."
    pause "  Create and copy the key."
    store_tested bmv-trader ANTHROPIC_API_KEY anthropic
    record engine
    echo "  Now turn off the old one: in the same workspace, find the previous bmv-engine-* key"
    echo "  (the older 'Created' date), open its menu and choose Disable. Delete it a month later."
    pause "  Disable the old engine key."
fi

say "Step 3 of 4: the desk agents' Claude key (every agent's keys file)"
if ask "Rotate it now?"; then
    if ask "  Is the new key already stored from an earlier try that stopped halfway (just push it)?"; then
        works bmv-desk-models anthropic-desk
    else
        echo "  1. In the Console, pick the desk workspace (S34, the one with the hard monthly limit)."
        echo "  2. Click 'Create Key' and name it bmv-desk-$MONTH."
        echo "  3. Copy it."
        pause "  Create and copy the key."
        store_tested bmv-desk-models anthropic-desk anthropic
    fi
    echo "  Putting it in each agent's keys file (your Mac password may be asked for, for sudo):"
    push anthropic
    record desk
    echo "  Every agent has the new key. Now disable the old bmv-desk-* key in the desk workspace, as in step 2."
    pause "  Disable the old desk key."
fi

say "Step 4 of 4: the desk agents' OmniRoute key (only used by agents switched to free models)"
if ask "Rotate it now?"; then
    if ! ask "  Is the new key already stored from an earlier try (just push it)?"; then
        echo "  1. Open the OmniRoute dashboard (http://127.0.0.1:20128), API keys."
        echo "  2. Regenerate the desk agents' key; it must allow only the bmv-free route, at cost 0."
        echo "  3. Copy it."
        pause "  Regenerate and copy the key."
        store_tested bmv-desk-models omniroute-agents
    fi
    push omniroute
fi

say "Done."
echo "The daily health check warns 35 days after the older of the two Claude keys' dates ($DATES)."
