#!/usr/bin/env bash
# Adds or replaces a free cloud provider's key in OmniRoute, for a provider with no key API (Groq,
# Google AI Studio, ...), and logs it for its expiry (DECISION S42). Run as you:
#
#   scripts/free-key-add.sh <provider>
#
# The provider and its OmniRoute connection come from free_keys in deploy/models.json, with
# "auto": false. The key is typed at a hidden prompt and goes to OmniRoute on stdin (a new
# connection) or in one child's environment (a replacement), never in argv; it's stored only in
# OmniRoute's encrypted store. OpenRouter is rotated automatically by `bmv-trader free-keys`.
set -euo pipefail
cd "$(dirname "$0")/.."

[[ $EUID -ne 0 ]] || { echo "free-key-add: run as yourself, not root" >&2; exit 1; }
PROVIDER=${1:?usage: scripts/free-key-add.sh <provider>}
BIN=target/release/bmv-trader
[[ -x $BIN ]] || { echo "free-key-add: build first: cargo build --release" >&2; exit 1; }

# the connection and whether OmniRoute has it yet, from the Rust side (rule: Rust first)
read -r CONNECTION STATE < <("$BIN" free-keys --connection "$PROVIDER") || true
[[ -n ${CONNECTION:-} && ( $STATE == new || $STATE == existing ) ]] ||
    { echo "free-key-add: couldn't look up $PROVIDER (see the error above)" >&2; exit 1; }

# OmniRoute from ~/.omniroute with a cleared environment. The key, when set, is exported with a
# shell builtin, so it is in the child's environment and never in any process's arguments.
omniroute() {
    (
        local home=$HOME key=${NEW_KEY:-} v
        cd "$home/.omniroute"
        for v in $(compgen -e); do unset "$v" 2>/dev/null || true; done
        export HOME=$home PATH=/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin NO_COLOR=1
        [[ -n $key ]] && export BMV_NEW_KEY=$key
        exec /opt/homebrew/bin/omniroute "$@"
    )
}

echo "Get a key from $PROVIDER's console (a free-tier key), then paste it here."
read -r -s -p "$PROVIDER key (hidden): " NEW_KEY
echo
[[ $NEW_KEY =~ ^[A-Za-z0-9_.+/=-]{20,300}$ ]] || { unset NEW_KEY; echo "free-key-add: that doesn't look like an API key" >&2; exit 1; }

if [[ $STATE == existing ]]; then
    # an existing connection: replace its key; OmniRoute tests it before it reports success
    omniroute providers rotate "$CONNECTION" --from-env BMV_NEW_KEY --yes
else
    printf '%s\n' "$NEW_KEY" | NEW_KEY= omniroute providers add "$PROVIDER" --name "$CONNECTION" --credential-stdin
    NEW_KEY= omniroute providers test "$CONNECTION"
fi
unset NEW_KEY
"$BIN" free-keys --record "$PROVIDER"
echo "Done. Add $CONNECTION to the bmv-free combo in OmniRoute's dashboard if it isn't there yet."
