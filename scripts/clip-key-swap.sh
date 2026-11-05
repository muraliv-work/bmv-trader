#!/usr/bin/env bash
# Replaces one model key in an agent's keys file, keeping its desk token (DESIGN §4c, S41). Used by
# scripts/rotate-keys.sh after a new key is in the Keychain; run as you:
#
#   scripts/clip-key-swap.sh <agent> anthropic   # from bmv-desk-models / anthropic-desk
#   scripts/clip-key-swap.sh <agent> omniroute   # from bmv-desk-models / omniroute-agents
#
# The agent's own user reads its file, swaps the one field and writes it back, mode 600. The key
# travels on stdin, never in argv, and is never printed.
set -euo pipefail

DESK_AGENT=/usr/local/libexec/bmv-desk/desk-agent
AGENTS=/usr/local/etc/bmv-clip/agents.json

[[ $EUID -ne 0 ]] || { echo "clip-key-swap: run as yourself, not root (the keys are in your Keychain)" >&2; exit 1; }
AGENT=${1:?usage: scripts/clip-key-swap.sh <agent> anthropic|omniroute}
case ${2:-} in
    anthropic) ACCOUNT=anthropic-desk FIELD=anthropic_key ;;
    omniroute) ACCOUNT=omniroute-agents FIELD=omniroute_key ;;
    *) echo "usage: scripts/clip-key-swap.sh <agent> anthropic|omniroute" >&2; exit 1 ;;
esac
[[ -e $AGENTS ]] || { echo "clip-key-swap: $AGENTS isn't installed" >&2; exit 1; }

LIST=$("$DESK_AGENT" --list "$AGENTS")
ROW=$(echo "$LIST" | /usr/bin/python3 -I -c '
import json, sys
for a in json.load(sys.stdin):
    if a["id"] == sys.argv[1]:
        print(a["runs_as"], a["keys"])
' "$AGENT")
[[ -n $ROW ]] || { echo "clip-key-swap: $AGENT isn't in $AGENTS" >&2; exit 1; }
read -r RUNS_AS KEYS <<< "$ROW"

NEW=$(security find-generic-password -s bmv-desk-models -a "$ACCOUNT" -w) ||
    { echo "clip-key-swap: no Keychain item bmv-desk-models / $ACCOUNT" >&2; exit 1; }

# as the agent's user: read its file, swap the field, write a new file beside it, then rename
printf '%s\n' "$NEW" | (cd / && sudo -u "$RUNS_AS" /usr/bin/env -i PATH=/usr/bin:/bin /usr/bin/python3 -I -c '
import json, os, sys
path, field = sys.argv[1], sys.argv[2]
new = sys.stdin.readline().rstrip("\n")
if not new:
    sys.exit("clip-key-swap: empty key")
with open(path) as f:
    keys = json.load(f)
if "desk_token" not in keys:
    sys.exit("clip-key-swap: " + path + " has no desk token; write it with scripts/clip-keys.sh")
keys[field] = new
tmp = path + ".new"
fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(keys, f)
os.replace(tmp, path)
' "$KEYS" "$FIELD")
unset NEW
echo "swapped $FIELD in $KEYS (desk token kept, readable by $RUNS_AS only)"
