#!/usr/bin/env bash
# Closes Paperclip's sign-up once the owner's login exists (DESIGN §4c, part 3). The owner runs:
#
#   sudo scripts/clip-close-signup.sh
#
# Paperclip refuses the first account while sign-up is off, so clip-setup.sh leaves it open; this
# sets auth.disableSignUp in bmvclip's config, as bmvclip, and restarts Paperclip. Reruns are fine.
set -euo pipefail

USER_NAME=bmvclip
BIN_DIR=/usr/local/libexec/bmv-clip
CONFIG=/usr/local/var/bmv-clip/home/instances/default/config.json
LABEL=com.bmv.clip

[[ $EUID -eq 0 ]] || { echo "clip-close-signup: run with sudo" >&2; exit 1; }

# written by bmvclip with its own node, so root never writes inside bmvclip's folder
(cd /usr/local/var/bmv-clip && sudo -u "$USER_NAME" /usr/bin/env -i PATH=/usr/bin:/bin \
    "$BIN_DIR/node/bin/node" -e '
const fs = require("fs");
const path = process.argv[1];
const cfg = JSON.parse(fs.readFileSync(path, "utf8"));
cfg.auth = { ...cfg.auth, disableSignUp: true };
fs.writeFileSync(path + ".new", JSON.stringify(cfg, null, 2), { mode: 0o600 });
fs.renameSync(path + ".new", path);
' "$CONFIG")

launchctl kickstart -k "system/$LABEL"
for i in $(seq 1 60); do
    if /usr/bin/python3 -I -c 'import urllib.request; urllib.request.urlopen("http://127.0.0.1:3100/api/health", timeout=2)' 2>/dev/null; then
        echo "sign-up is closed and Paperclip is back up; only existing logins work now"
        exit 0
    fi
    sleep 2
done
echo "clip-close-signup: the config is changed, but Paperclip isn't answering; see /usr/local/var/log/bmv-clip/clip.log" >&2
exit 1
