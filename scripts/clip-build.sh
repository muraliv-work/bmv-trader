#!/usr/bin/env bash
# Builds Paperclip for the hidden `bmvclip` user into target/clip (DESIGN §4c, S36). Run as you,
# not root; then `sudo scripts/clip-setup.sh` installs what this made.
#
#   - Node from nodejs.org, checked against a pinned SHA-256. Homebrew's node loads libraries from
#     /opt/homebrew, which your user can change, so bmvclip doesn't run it.
#   - Paperclip 2026.1023.0 and every dependency exactly as deploy/paperclip/package-lock.json
#     pins them (npm checks each package's hash), with every install script switched off.
#   - The one install step Postgres needs, its library symlinks, done here from its own list.
set -euo pipefail
cd "$(dirname "$0")/.."

NODE_VERSION=24.16.0
NODE_SHA256=39189dab4eeb15706c424af0ac08a3044c9e48f7db12a7d77f6b7aafc7dd5df6
NODE_TAR=node-v$NODE_VERSION-darwin-arm64.tar.gz
OUT=target/clip

[[ $(uname -sm) == "Darwin arm64" ]] || { echo "clip-build: Apple silicon macOS only" >&2; exit 1; }
[[ $EUID -ne 0 ]] || { echo "clip-build: run as yourself, not root" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/app"

/usr/bin/curl -fsSL --proto '=https' "https://nodejs.org/dist/v$NODE_VERSION/$NODE_TAR" -o "$OUT/$NODE_TAR"
echo "$NODE_SHA256  $OUT/$NODE_TAR" | /usr/bin/shasum -a 256 -c -
/usr/bin/tar -xzf "$OUT/$NODE_TAR" -C "$OUT"
mv "$OUT/node-v$NODE_VERSION-darwin-arm64" "$OUT/node"
rm "$OUT/$NODE_TAR"

cp deploy/paperclip/package.json deploy/paperclip/package-lock.json "$OUT/app/"
# the downloaded node's own npm, with a clean environment and an empty npmrc, so no ~/.npmrc or
# registry override applies
: > "$OUT/npmrc"
(cd "$OUT/app" && env -i HOME="$PWD" PATH="$PWD/../node/bin:/usr/bin:/bin" \
    npm_config_userconfig="$PWD/../npmrc" npm_config_cache="$PWD/../npm-cache" npm_config_update_notifier=false \
    npm ci --ignore-scripts --no-audit --no-fund --loglevel=error)
rm -rf "$OUT/npmrc" "$OUT/npm-cache" "$OUT/app/.npm"

PG=$OUT/app/node_modules/@embedded-postgres/darwin-arm64
(cd "$PG" && /usr/bin/python3 -I -c '
import json, os
for s in json.load(open("native/pg-symlinks.json")):
    d, t = os.path.dirname(s["source"]), s["target"]
    # each link stays inside the package and points at a file next to it
    assert not os.path.isabs(t) and ".." not in t.split("/") and d.startswith("native/"), s
    assert os.path.dirname(t) == d and "/" not in os.path.relpath(s["source"], d), s
    if not os.path.lexists(t):
        os.symlink(os.path.relpath(s["source"], d), t)
')

V=$("$OUT/node/bin/node" -p 'require("./target/clip/app/node_modules/paperclipai/package.json").version')
[[ $V == 2026.1023.0 ]] || { echo "clip-build: got Paperclip $V, expected 2026.1023.0" >&2; exit 1; }

echo "built $OUT (node $NODE_VERSION, Paperclip $V)"
echo "tree hash (clip-setup shows the same one for what it installs):"
/usr/bin/python3 -I scripts/tree-hash.py "$OUT"
