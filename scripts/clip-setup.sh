#!/usr/bin/env bash
# Installs Paperclip as its own hidden macOS user, `bmvclip` (DECISION S36, DESIGN §4c). The owner
# runs it, after desk-setup.sh has installed desk-agent:
#
#   scripts/clip-build.sh                          # as you, not root
#   sudo scripts/clip-setup.sh                     # installs target/clip
#   sudo -u bmvclip /usr/local/libexec/bmv-clip/clip auth bootstrap-ceo   # your one-time login link
#
# What it does, and nothing else:
#   - a hidden user and group `bmvclip` (no shell, no home, no login)
#   - node and Paperclip 2026.1023.0 in /usr/local/libexec/bmv-clip, owned by root, so bmvclip and
#     its agents can run them but never change them
#   - its own Postgres (the one Paperclip ships) with a random password only bmvclip can read,
#     on 127.0.0.1:54330 only. Paperclip's built-in database has a fixed password any local
#     program could use to rewrite an agent's command.
#   - Paperclip in `authenticated` mode (login required) on 127.0.0.1:3100 only, with telemetry,
#     announcements and update checks off
#   - /usr/local/etc/bmv-clip for the agents' charters (root's; bmvclip can read, not change) and
#     /usr/local/var/bmv-clip/keys for their keys files (bmvclip's alone). Both start empty.
#   - two LaunchDaemons, com.bmv.clip-db and com.bmv.clip, then a check that nothing bmvclip runs
#     has a connection off this Mac
# It never builds anything as root and never touches Alpaca, model or Keychain secrets.
#
# To remove it:  sudo launchctl bootout system/com.bmv.clip; sudo launchctl bootout system/com.bmv.clip-db
#                sudo rm /Library/LaunchDaemons/com.bmv.clip.plist /Library/LaunchDaemons/com.bmv.clip-db.plist
#                sudo rm -rf /usr/local/libexec/bmv-clip /usr/local/var/log/bmv-clip
#                sudo dscl . -delete /Users/bmvclip; sudo dscl . -delete /Groups/bmvclip
#                (the data in /usr/local/var/bmv-clip and the charters are kept until you delete them)
set -euo pipefail

USER_NAME=bmvclip
BIN_DIR=/usr/local/libexec/bmv-clip
DATA_DIR=/usr/local/var/bmv-clip
CHARTERS=/usr/local/etc/bmv-clip
DESK_AGENT=/usr/local/libexec/bmv-desk/desk-agent
DB_LABEL=com.bmv.clip-db
APP_LABEL=com.bmv.clip
DB_PORT=54330
PORT=3100
PG_BIN=$BIN_DIR/app/node_modules/@embedded-postgres/darwin-arm64/native/bin
INSTANCE=$DATA_DIR/home/instances/default
LOG_DIR=/usr/local/var/log/bmv-clip

[[ $(uname) == Darwin ]] || { echo "clip-setup: macOS only" >&2; exit 1; }
[[ $EUID -eq 0 ]] || { echo "clip-setup: run with sudo" >&2; exit 1; }
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=${1:-$HERE/../target/clip}
[[ -x $SRC/node/bin/node && -d $SRC/app/node_modules/paperclipai ]] ||
    { echo "clip-setup: $SRC has no build; run scripts/clip-build.sh first, as yourself" >&2; exit 1; }
[[ -x $DESK_AGENT && $(stat -f %u "$DESK_AGENT") == 0 ]] ||
    { echo "clip-setup: $DESK_AGENT is missing; rerun desk-setup.sh first (it installs desk-agent now)" >&2; exit 1; }

# shellcheck source=hidden-user.sh
source "$HERE/hidden-user.sh"
root_only_parents /usr /usr/local /usr/local/libexec /usr/local/var /usr/local/var/log /usr/local/etc /Library /Library/LaunchDaemons ||
    { echo "clip-setup: refusing" >&2; exit 1; }
hidden_user "$USER_NAME" "bmv Paperclip"

# the build is copied first into a root-only staging directory, so what is checked is what gets
# installed; $SRC is writable by you, and so by anything running as you
STAGE=$(mktemp -d /var/root/bmv-clip-stage.XXXXXX)
trap 'rm -rf "$STAGE"' EXIT
cp -RP "$SRC/node" "$SRC/app" "$STAGE/"
echo "About to install node and Paperclip with this tree hash (clip-build.sh printed the one it built):"
/usr/bin/python3 -I "$HERE/tree-hash.py" "$STAGE"
read -r -p "Install them? Type yes: " answer
[[ $answer == yes ]] || { echo "clip-setup: not installed" >&2; exit 1; }

# stop both before replacing what they run or touching their files, and wait until they're gone
stop() {
    local i
    launchctl bootout "system/$1" 2>/dev/null || true
    for i in $(seq 1 30); do launchctl print "system/$1" >/dev/null 2>&1 || return 0; sleep 1; done
    echo "clip-setup: $1 didn't stop" >&2
    exit 1
}
stop "$APP_LABEL"
stop "$DB_LABEL"

# made root's and closed in the staging directory, then swapped in, so an interrupted run leaves
# either the old install or the new one
chown -R -h root:wheel "$STAGE"
chmod -R go-w,u-s,g-s "$STAGE"
install -d -o root -g wheel -m 755 "$BIN_DIR"
for d in node app; do
    rm -rf "$BIN_DIR/$d.old"
    [[ -e $BIN_DIR/$d ]] && mv "$BIN_DIR/$d" "$BIN_DIR/$d.old"
    mv "$STAGE/$d" "$BIN_DIR/$d"
    rm -rf "$BIN_DIR/$d.old"
done

# the one way to run Paperclip, by launchd or by you: a fixed environment, whatever the caller's
cat > "$BIN_DIR/clip" <<EOF
#!/bin/bash
set -euo pipefail
[[ \$(id -un) == $USER_NAME ]] || { echo "clip: run it as $USER_NAME: sudo -u $USER_NAME \$0 ..." >&2; exit 1; }
exec /usr/bin/env -i \\
    HOME=$DATA_DIR/home PAPERCLIP_HOME=$DATA_DIR/home PAPERCLIP_CONFIG=$INSTANCE/config.json \\
    PATH=$BIN_DIR/node/bin:/usr/bin:/bin LANG=en_US.UTF-8 TZ=America/New_York \\
    PAPERCLIP_TELEMETRY_DISABLED=1 DO_NOT_TRACK=1 PAPERCLIP_ANNOUNCEMENTS_ENABLED=false PAPERCLIP_UPDATE_CHECK=0 \\
    PAPERCLIP_DEPLOYMENT_MODE=authenticated PAPERCLIP_DEPLOYMENT_EXPOSURE=private PAPERCLIP_BIND=loopback PORT=$PORT \\
    PAPERCLIP_SECRETS_STRICT_MODE=true PAPERCLIP_MIGRATION_AUTO_APPLY=true \\
    /bin/sh -c 'umask 077; cd $DATA_DIR && exec $BIN_DIR/node/bin/node $BIN_DIR/app/node_modules/paperclipai/dist/index.js "\$@"' clip "\$@"
EOF
chown root:wheel "$BIN_DIR/clip"
chmod 755 "$BIN_DIR/clip"
echo "installed $BIN_DIR/{node,app,clip}"

# data: bmvclip's alone. Root makes only the top directory (its parent is root's, so it can't be
# swapped for a link); everything inside is made as bmvclip, so a link bmvclip planted there can
# never steer a root write
[[ ! -L $DATA_DIR ]] || { echo "clip-setup: $DATA_DIR is a symlink; refusing" >&2; exit 1; }
install -d -o "$USER_NAME" -g "$USER_NAME" -m 700 "$DATA_DIR"
as_clip() { (cd "$DATA_DIR" && sudo -u "$USER_NAME" /usr/bin/env -i PATH=/usr/bin:/bin LANG=en_US.UTF-8 "$@"); }
as_clip /bin/mkdir -p -m 700 "$DATA_DIR/home" "$DATA_DIR/keys"
# logs: bmvclip's files in a root-owned directory, so bmvclip can write them but not swap them for
# links to somewhere launchd would then write
install -d -o root -g wheel -m 755 "$LOG_DIR"
for f in pg.log clip.log; do
    [[ -e $LOG_DIR/$f ]] || install -o "$USER_NAME" -g "$USER_NAME" -m 600 /dev/null "$LOG_DIR/$f"
done
# charters: root's, readable by bmvclip (desk-agent refuses a charter its own user could change)
install -d -o root -g wheel -m 755 "$CHARTERS"

# the database password: made once, never printed, kept across reruns
if [[ ! -s $DATA_DIR/db.pass ]]; then
    as_clip /bin/sh -c "umask 077; /usr/bin/openssl rand -hex 32 > $DATA_DIR/db.pass"
    echo "made the database password ($DATA_DIR/db.pass, bmvclip's alone)"
fi
# a marker written last, so a run interrupted halfway starts the database over
if ! as_clip /bin/test -e "$DATA_DIR/pg/.bmv-ready"; then
    as_clip /bin/rm -rf "$DATA_DIR/pg"
    as_clip "$PG_BIN/initdb" -D "$DATA_DIR/pg" -U "$USER_NAME" --pwfile="$DATA_DIR/db.pass" \
        --auth=scram-sha-256 -E UTF8 --locale=C >/dev/null
    echo 'CREATE DATABASE paperclip;' | as_clip "$PG_BIN/postgres" --single -D "$DATA_DIR/pg" postgres >/dev/null
    as_clip /usr/bin/touch "$DATA_DIR/pg/.bmv-ready"
    echo "made the database in $DATA_DIR/pg"
fi

# Paperclip's config: written once, by bmvclip, into a file only it can read (it holds the
# password). Root only formats it; the password comes in on stdin from bmvclip's own file.
if ! as_clip /bin/test -e "$INSTANCE/config.json"; then
    as_clip /bin/mkdir -p -m 700 "$DATA_DIR/home/instances" "$INSTANCE"
    as_clip /bin/cat "$DATA_DIR/db.pass" | /usr/bin/python3 -I -c '
import json, sys, datetime
inst, dbport, port = sys.argv[1:]
pw = sys.stdin.read().strip()
assert len(pw) == 64 and all(c in "0123456789abcdef" for c in pw), "db.pass is not a 64-digit hex password"
cfg = {
    "$meta": {"version": 1, "source": "onboard",
              "updatedAt": datetime.datetime.now(datetime.timezone.utc).isoformat()},
    "database": {"mode": "postgres",
                 "connectionString": f"postgres://bmvclip:{pw}@127.0.0.1:{dbport}/paperclip",
                 "backup": {"enabled": True, "intervalMinutes": 60, "retentionDays": 7,
                            "dir": f"{inst}/data/backups"}},
    "logging": {"mode": "file", "logDir": f"{inst}/logs"},
    "server": {"deploymentMode": "authenticated", "exposure": "private", "bind": "loopback",
               "host": "127.0.0.1", "port": int(port), "allowedHostnames": [], "serveUi": True},
    "auth": {"baseUrlMode": "auto", "disableSignUp": False},
    "storage": {"provider": "local_disk", "localDisk": {"baseDir": f"{inst}/data/storage"}},
    "secrets": {"provider": "local_encrypted", "strictMode": True,
                "localEncrypted": {"keyFilePath": f"{inst}/secrets/master.key"}},
    "telemetry": {"enabled": False},
    "updates": {"checkEnabled": False},
}
json.dump(cfg, sys.stdout, indent=2)
' "$INSTANCE" "$DB_PORT" "$PORT" | as_clip /bin/sh -c "umask 077; set -C; cat > $INSTANCE/config.json"
    echo "wrote $INSTANCE/config.json"
fi

plist() {  # plist LABEL LOG ARG...
    local label=$1 log=$2 a args=""
    shift 2
    for a in "$@"; do args+="    <string>$a</string>"$'\n'; done
    cat > "/Library/LaunchDaemons/$label.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>UserName</key><string>$USER_NAME</string>
  <key>GroupName</key><string>$USER_NAME</string>
  <key>ProgramArguments</key>
  <array>
$args  </array>
  <key>WorkingDirectory</key><string>$DATA_DIR</string>
  <key>Umask</key><integer>63</integer>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>StandardOutPath</key><string>$LOG_DIR/$log</string>
  <key>StandardErrorPath</key><string>$LOG_DIR/$log</string>
</dict>
</plist>
EOF
    chown root:wheel "/Library/LaunchDaemons/$label.plist"
    chmod 644 "/Library/LaunchDaemons/$label.plist"
    plutil -lint "/Library/LaunchDaemons/$label.plist" >/dev/null
}
# no Unix socket: the database is reachable only over 127.0.0.1, with the password
plist "$DB_LABEL" pg.log "$PG_BIN/postgres" -D "$DATA_DIR/pg" -p "$DB_PORT" \
    -c listen_addresses=127.0.0.1 -c unix_socket_directories=
plist "$APP_LABEL" clip.log "$BIN_DIR/clip" run

listening() { lsof -nP -a -u "$USER_NAME" -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1; }
wait_for() {  # wait_for PORT WHAT LOG
    local i
    for i in $(seq 1 60); do listening "$1" && return; sleep 2; done
    echo "clip-setup: $2 didn't start on 127.0.0.1:$1; see $LOG_DIR/$3" >&2
    exit 1
}
launchctl bootstrap system "/Library/LaunchDaemons/$DB_LABEL.plist"
wait_for "$DB_PORT" "the database" pg.log
launchctl bootstrap system "/Library/LaunchDaemons/$APP_LABEL.plist"
wait_for "$PORT" "Paperclip" clip.log
echo "started $DB_LABEL and $APP_LABEL as $USER_NAME"

# the network check: every socket bmvclip has must be on loopback
# shortcut: a snapshot 30 s after start, so a later connection isn't caught; a per-user pf rule or
# an outbound firewall would make it permanent
sleep 30
for p in "$DB_PORT" "$PORT"; do
    listening "$p" || { echo "clip-setup: nothing is listening on 127.0.0.1:$p after 30 s; see $LOG_DIR" >&2; stop "$APP_LABEL"; exit 1; }
done
bad=$(lsof -nP -a -u "$USER_NAME" -i -F n 2>/dev/null | sed -n 's/^n//p' |
      grep -v -E '^(127\.0\.0\.1|\[::1\]):[0-9]+(->(127\.0\.0\.1|\[::1\]):[0-9]+)?$' || true)
if [[ -n $bad ]]; then
    stop "$APP_LABEL"
    stop "$DB_LABEL"
    echo "clip-setup: something bmvclip runs had a socket off this Mac, so both were stopped:" >&2
    echo "$bad" >&2
    exit 1
fi
echo "network check passed: every connection bmvclip has is on 127.0.0.1"
echo
echo "Next, as yourself:"
echo "  sudo -u $USER_NAME $BIN_DIR/clip auth bootstrap-ceo"
echo "  then open the link it prints and make your login straight away: sign-up stays open until"
echo "  your login exists, and part 4 closes it"
