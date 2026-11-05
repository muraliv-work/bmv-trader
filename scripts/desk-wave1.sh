#!/usr/bin/env bash
# Files the desk requests for the agents in the installed agents file (DESIGN §4c, S38). Run as
# you; each request waits for your Touch ID with `desk sign <id>`. Two rounds, because a token can
# only be issued once its agent exists:
#
#   scripts/desk-wave1.sh agents    # add each agent; then desk sign each request
#   scripts/desk-wave1.sh tokens    # its token and monthly budget; desk sign each, and after each
#                                   # token, scripts/clip-keys.sh <agent> while it is on screen
#   scripts/desk-wave1.sh budgets   # each new month: desk budgets are per calendar month, and a
#                                   # signer with none can't make a paid call
set -euo pipefail

DESK=/usr/local/libexec/bmv-desk/desk
DESK_AGENT=/usr/local/libexec/bmv-desk/desk-agent
AGENTS=/usr/local/etc/bmv-clip/agents.json

[[ $EUID -ne 0 ]] || { echo "desk-wave1: run as yourself, not root" >&2; exit 1; }
ROUND=${1:-}
[[ $ROUND == agents || $ROUND == tokens || $ROUND == budgets ]] ||
    { echo "usage: scripts/desk-wave1.sh agents | tokens | budgets" >&2; exit 1; }

# the requests, built from the installed file so they match what the agents will run with
REQUESTS=$("$DESK_AGENT" --list "$AGENTS" | /usr/bin/python3 -I -c '
import json, sys
from decimal import Decimal
rnd = sys.argv[1]
for a in json.load(sys.stdin):
    if rnd == "agents":
        print(json.dumps({"kind": "add_agent", "id": a["id"], "role": a["role"]}))
    if rnd == "tokens":
        print(json.dumps({"kind": "issue_token", "agent": a["id"]}))
    if rnd in ("tokens", "budgets") and Decimal(a["budget_usd"]) > 0:
        print(json.dumps({"kind": "set_budget", "agent": a["id"], "limit_usd": a["budget_usd"]}))
' "$ROUND")

while read -r req; do
    [[ -n $req ]] || continue
    echo "$req"
    "$DESK" request "$req"
done <<< "$REQUESTS"
echo
echo "Now approve each with: $DESK sign <id>   (list them with: $DESK pending)"
[[ $ROUND == tokens ]] && echo "After each token appears, run: scripts/clip-keys.sh <agent>, and paste it there." || true
