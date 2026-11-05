#!/usr/bin/env bash
# Installs the agents and models files and what they need (DECISION S38, S39, DESIGN §4c). The
# owner runs it after clip-setup.sh, and again after any change to deploy/models.json (a provider,
# model or budget) or deploy/agents.json (a prompt, a user, a schedule):
#
#   sudo scripts/clip-agents-setup.sh
#
# What it does, and nothing else:
#   - checks both files together with the installed desk-agent, shows what changes, and installs
#     them in /usr/local/etc/bmv-clip (root's, so no agent can change its own model)
#   - a hidden user for each agent that doesn't run as bmvclip, with its own folder
#     /usr/local/var/<user> for its keys file
#   - /etc/sudoers.d/bmv-clip: bmvclip (Paperclip) may run exactly one command as each of them,
#     `desk-agent <agents file> <that agent> <its keys>`, and nothing else
# It never touches keys: scripts/clip-keys.sh writes those.
set -euo pipefail

CLIP_USER=bmvclip
DESK_AGENT=/usr/local/libexec/bmv-desk/desk-agent
AGENTS=/usr/local/etc/bmv-clip/agents.json
MODELS=/usr/local/etc/bmv-clip/models.json
# /etc is a link to /private/etc on macOS; the checks below refuse links, so use the real path
SUDOERS=/private/etc/sudoers.d/bmv-clip
RUN_VARS="PAPERCLIP_API_URL PAPERCLIP_API_KEY PAPERCLIP_RUN_ID PAPERCLIP_AGENT_ID PAPERCLIP_COMPANY_ID"

[[ $(uname) == Darwin ]] || { echo "clip-agents-setup: macOS only" >&2; exit 1; }
[[ $EUID -eq 0 ]] || { echo "clip-agents-setup: run with sudo" >&2; exit 1; }
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=$HERE/../deploy/agents.json
MSRC=$HERE/../deploy/models.json
[[ -x $DESK_AGENT && $(stat -f %u "$DESK_AGENT") == 0 ]] ||
    { echo "clip-agents-setup: $DESK_AGENT is missing; rerun desk-setup.sh first" >&2; exit 1; }
[[ -d /usr/local/etc/bmv-clip ]] || { echo "clip-agents-setup: run clip-setup.sh first" >&2; exit 1; }
dscl . -read "/Users/$CLIP_USER" UniqueID >/dev/null 2>&1 ||
    { echo "clip-agents-setup: no $CLIP_USER user; run clip-setup.sh first" >&2; exit 1; }

# shellcheck source=hidden-user.sh
source "$HERE/hidden-user.sh"
root_only_parents /usr /usr/local /usr/local/var /usr/local/etc /usr/local/etc/bmv-clip /private /private/etc /private/etc/sudoers.d ||
    { echo "clip-agents-setup: refusing" >&2; exit 1; }

# copied first, so what is checked is what gets installed
STAGE=$(mktemp -d /var/root/bmv-agents-stage.XXXXXX)
trap 'rm -rf "$STAGE"' EXIT
for f in "$SRC" "$MSRC"; do
    [[ ! -L $f ]] || { echo "clip-agents-setup: $f is a symlink; refusing" >&2; exit 1; }
done
cp "$SRC" "$STAGE/agents.json"
cp "$MSRC" "$STAGE/models.json"
LIST=$("$DESK_AGENT" --list "$STAGE/agents.json")
echo "The agents and models files say:"
echo "$LIST" | /usr/bin/python3 -I -c '
import json, sys
for a in json.load(sys.stdin):
    print("  {id:9} {role:13} {provider:9} {model:18} as {runs_as:9} ${budget_usd}/month, {when}{s}".format(
        when=a["schedule"] or "on demand", s=", signs" if a["signs"] else "", **a))'
for pair in "$AGENTS:agents.json" "$MODELS:models.json"; do
    installed=${pair%%:*}
    if [[ -e $installed ]]; then
        echo "Changes from the installed ${pair##*:}:"
        diff -u "$installed" "$STAGE/${pair##*:}" || true
    fi
done
read -r -p "Install it? Type yes: " answer
[[ $answer == yes ]] || { echo "clip-agents-setup: not installed" >&2; exit 1; }
# models first: desk-agent reads the pair, and a new agent with no models line is refused
install -o root -g wheel -m 644 "$STAGE/models.json" "$MODELS"
install -o root -g wheel -m 644 "$STAGE/agents.json" "$AGENTS"
echo "installed $AGENTS and $MODELS"

# one line per field, for the shell: id runs_as keys signs
ROWS=$(echo "$LIST" | /usr/bin/python3 -I -c '
import json, sys
for a in json.load(sys.stdin):
    print(a["id"], a["runs_as"], a["keys"], "yes" if a["signs"] else "no")')

RULES="Defaults:$CLIP_USER env_keep += \"$RUN_VARS\""$'\n'
while read -r id runs_as keys signs; do
    [[ $runs_as == "$CLIP_USER" ]] && continue
    # an existing user must be one this script made for this agent, never someone else's account
    if dscl . -read "/Users/$runs_as" UniqueID >/dev/null 2>&1 &&
       [[ $(dscl . -read "/Users/$runs_as" RealName 2>/dev/null | tail -n 1 | sed 's/^RealName: *//; s/^ *//') != "bmv desk agent $id" ]]; then
        echo "clip-agents-setup: user $runs_as exists but wasn't made for $id; refusing" >&2
        exit 1
    fi
    hidden_user "$runs_as" "bmv desk agent $id"
    # its folder sits in root's /usr/local/var, so the user can't swap it for a link
    [[ ! -L /usr/local/var/$runs_as ]] || { echo "clip-agents-setup: /usr/local/var/$runs_as is a symlink" >&2; exit 1; }
    install -d -o "$runs_as" -g "$runs_as" -m 700 "/usr/local/var/$runs_as"
    RULES+="$CLIP_USER ALL=($runs_as) NOPASSWD: $DESK_AGENT $AGENTS $id $keys"$'\n'
done <<< "$ROWS"

# checked by visudo before it replaces the old rule; a broken file would lock out all of sudo
printf '%s' "$RULES" > "$STAGE/sudoers"
visudo -cqf "$STAGE/sudoers" ||
    { echo "clip-agents-setup: the agents file and users are installed, but the new sudo rule didn't check, so the old one stays" >&2; exit 1; }
install -o root -g wheel -m 440 "$STAGE/sudoers" "$SUDOERS"
echo "installed $SUDOERS:"
sed 's/^/  /' "$SUDOERS"
echo
echo "Next: register the agents on the desk (scripts/desk-wave1.sh), then their keys (scripts/clip-keys.sh)."
