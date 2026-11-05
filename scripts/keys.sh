#!/usr/bin/env bash
# Every key the system holds, in one place (DECISION S43). Run as you:
#
#   scripts/keys.sh             the table: each key's purpose, renewal date, due date and state
#   scripts/keys.sh renew       walk through the due ones: what it is, where to get a new one, a
#                               hidden paste, a test, then save it and update everything using it
#   scripts/keys.sh record ID   mark a key as renewed today (one you just made by hand)
#
# A notification at 09:00 (com.bmv-trader.keys) says when any key is 10 days or less from due.
# All the logic is in Rust (`bmv-trader keys`); this only picks the arguments.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=target/release/bmv-trader
[[ -x $BIN ]] || { echo "keys: build first: cargo build --release" >&2; exit 1; }
case ${1:-} in
    "") exec "$BIN" keys ;;
    renew) exec "$BIN" keys --renew ;;
    record) exec "$BIN" keys --record "${2:?usage: scripts/keys.sh record ID}" ;;
    *) echo "usage: scripts/keys.sh [renew | record ID]" >&2; exit 1 ;;
esac
