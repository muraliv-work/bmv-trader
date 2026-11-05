#!/usr/bin/env bash
# Writes one agent's keys file (DESIGN §4c, S38). Run as you, right after `desk sign` shows that
# agent's new token:
#
#   scripts/clip-keys.sh <agent>
#
# - the desk token: pasted at a hidden prompt
# - the model key, from your Keychain:
#     Claude agents:    service bmv-desk-models, account anthropic-desk   (S35)
#     OmniRoute agents: service bmv-desk-models, account omniroute-agents (S38, the bmv-local key)
# - written as the agent's own user, readable by it only, at the path the agents file gives
# No key is ever put on a command line or printed.
set -euo pipefail

DESK_AGENT=/usr/local/libexec/bmv-desk/desk-agent
AGENTS=/usr/local/etc/bmv-clip/agents.json

[[ $EUID -ne 0 ]] || { echo "clip-keys: run as yourself, not root (the keys are in your Keychain)" >&2; exit 1; }
AGENT=${1:?usage: scripts/clip-keys.sh <agent>}

[[ -e $AGENTS ]] || { echo "clip-keys: $AGENTS isn't installed; run sudo scripts/clip-agents-setup.sh first" >&2; exit 1; }
LIST=$("$DESK_AGENT" --list "$AGENTS")
ROW=$(echo "$LIST" | /usr/bin/python3 -I -c '
import json, sys
for a in json.load(sys.stdin):
    if a["id"] == sys.argv[1]:
        print(a["runs_as"], a["provider"], a["keys"])
' "$AGENT")
[[ -n $ROW ]] || { echo "clip-keys: $AGENT isn't in $AGENTS" >&2; exit 1; }
read -r RUNS_AS PROVIDER KEYS <<< "$ROW"

case $PROVIDER in
    anthropic) ACCOUNT=anthropic-desk FIELD=anthropic_key ;;
    omniroute) ACCOUNT=omniroute-agents FIELD=omniroute_key ;;
    *) echo "clip-keys: unknown provider $PROVIDER" >&2; exit 1 ;;
esac
MODEL_KEY=$(security find-generic-password -s bmv-desk-models -a "$ACCOUNT" -w) ||
    { echo "clip-keys: no Keychain item bmv-desk-models / $ACCOUNT; add it with:" >&2
      echo "  security add-generic-password -s bmv-desk-models -a $ACCOUNT -w" >&2; exit 1; }

read -r -s -p "Desk token for $AGENT (hidden): " TOKEN
echo
[[ $TOKEN =~ ^[A-Za-z0-9_-]{20,200}$ ]] || { echo "clip-keys: that doesn't look like a desk token" >&2; exit 1; }

# the JSON is built from stdin and written by the agent's user, so neither key is in any argv;
# from /, because the agent's user can't see your working folder
printf '%s\n%s\n' "$TOKEN" "$MODEL_KEY" | /usr/bin/python3 -I -c '
import json, sys
token, key = sys.stdin.read().splitlines()[:2]
print(json.dumps({"desk_token": token, sys.argv[1]: key}))
' "$FIELD" | (cd / && sudo -u "$RUNS_AS" /bin/sh -c 'umask 077; set -C; rm -f "$1.new"; cat > "$1.new" && mv -f "$1.new" "$1"' clip-keys "$KEYS")
unset TOKEN MODEL_KEY
echo "wrote $KEYS (readable by $RUNS_AS only)"
