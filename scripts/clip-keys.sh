#!/usr/bin/env bash
# Writes one agent's keys file (DESIGN §4c, S38, S41). Run as you, right after `desk sign` shows
# that agent's new token:
#
#   scripts/clip-keys.sh <agent>
#
# - the desk token: pasted at a hidden prompt
# - both model keys, from your Keychain, so flipping the agent's `claude` switch in
#   deploy/models.json never needs new keys:
#     Claude:    service bmv-desk-models, account anthropic-desk   (S35; every agent with its own
#                user; never one running as bmvclip, S39)
#     OmniRoute: service bmv-desk-models, account omniroute-agents (the free route; optional, and
#                never for a signer, which can't be switched to it)
# - written as the agent's own user, readable by it only, at the path the agents file gives
# No key is ever put on a command line or printed. To replace only a model key later, keeping the
# desk token, use scripts/clip-key-swap.sh.
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
        print(a["runs_as"], a["provider"], a["keys"], "yes" if a["signs"] else "no")
' "$AGENT")
[[ -n $ROW ]] || { echo "clip-keys: $AGENT isn't in $AGENTS" >&2; exit 1; }
read -r RUNS_AS PROVIDER KEYS SIGNS <<< "$ROW"

# no Claude key in Paperclip's own user's files (S39), and no free-route key for a signer (A5)
CLAUDE_KEY= FREE_KEY=
if [[ $RUNS_AS != bmvclip ]]; then
    CLAUDE_KEY=$(security find-generic-password -s bmv-desk-models -a anthropic-desk -w) ||
        { echo "clip-keys: no Keychain item bmv-desk-models / anthropic-desk; add it with:" >&2
          echo "  security add-generic-password -s bmv-desk-models -a anthropic-desk -w" >&2; exit 1; }
fi
if [[ $SIGNS != yes ]]; then
    FREE_KEY=$(security find-generic-password -s bmv-desk-models -a omniroute-agents -w 2>/dev/null) || FREE_KEY=
fi
if [[ -z $FREE_KEY && $SIGNS != yes ]]; then
    [[ $PROVIDER == omniroute ]] && { echo "clip-keys: $AGENT is switched to its free model, but there's no Keychain item bmv-desk-models / omniroute-agents" >&2; exit 1; }
    echo "note: no OmniRoute key (bmv-desk-models / omniroute-agents), so $AGENT can't be switched to its free model until you rerun this"
fi

read -r -s -p "Desk token for $AGENT (hidden): " TOKEN
echo
[[ $TOKEN =~ ^[A-Za-z0-9_-]{20,200}$ ]] || { echo "clip-keys: that doesn't look like a desk token" >&2; exit 1; }

# the JSON is built from stdin and written by the agent's user, so no key is in any argv;
# from /, because the agent's user can't see your working folder
printf '%s\n%s\n%s\n' "$TOKEN" "$CLAUDE_KEY" "$FREE_KEY" | /usr/bin/python3 -I -c '
import json, sys
token, claude, free = (sys.stdin.read().split("\n") + ["", "", ""])[:3]
keys = {"desk_token": token}
if claude:
    keys["anthropic_key"] = claude
if free:
    keys["omniroute_key"] = free
print(json.dumps(keys))
' | (cd / && sudo -u "$RUNS_AS" /bin/sh -c 'umask 077; set -C; rm -f "$1.new"; cat > "$1.new" && mv -f "$1.new" "$1"' clip-keys "$KEYS")
unset TOKEN CLAUDE_KEY FREE_KEY
echo "wrote $KEYS (readable by $RUNS_AS only)"
