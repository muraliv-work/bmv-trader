#!/usr/bin/env bash
# Installs the Quant Desk as its own macOS user (DECISION S30, DESIGN §4c). The owner runs it:
#
#   CARGO_TARGET_DIR=target/desk cargo build --release -p desk   # as you, not root
#   scripts/build-desk-sign.sh target/desk/release               # as you
#   sudo scripts/desk-setup.sh target/desk/release
#   /usr/local/libexec/bmv-desk/desk init --db /usr/local/var/bmv-desk/desk.db --as bmvdesk
#
# What it does, and nothing else:
#   - a hidden user and group `bmvdesk` (no shell, no home, no login)
#   - binaries in /usr/local/libexec/bmv-desk, owned by root, so no agent can replace them
#   - the database directory /usr/local/var/bmv-desk, owned by bmvdesk, mode 700
#   - a LaunchDaemon com.bmv.desk running `desk serve` as bmvdesk on 127.0.0.1:8790
# It never builds anything as root and never touches Alpaca, model or Keychain secrets.
#
# To remove it:  sudo launchctl bootout system/com.bmv.desk; sudo rm /Library/LaunchDaemons/com.bmv.desk.plist
#                sudo dscl . -delete /Users/bmvdesk; sudo dscl . -delete /Groups/bmvdesk
#                (the database in /usr/local/var/bmv-desk is kept until you delete it yourself)
set -euo pipefail

USER_NAME=bmvdesk
BIN_DIR=/usr/local/libexec/bmv-desk
DATA_DIR=/usr/local/var/bmv-desk
PLIST=/Library/LaunchDaemons/com.bmv.desk.plist
LABEL=com.bmv.desk
PORT=8790

[[ $(uname) == Darwin ]] || { echo "desk-setup: macOS only" >&2; exit 1; }
[[ $EUID -eq 0 ]] || { echo "desk-setup: run with sudo" >&2; exit 1; }
SRC=${1:?usage: sudo scripts/desk-setup.sh <dir with desk, desk-mcp and desk-sign>}
for b in desk desk-mcp desk-sign; do
    [[ -x "$SRC/$b" ]] || { echo "desk-setup: $SRC/$b is missing; build it first (see the top of this script)" >&2; exit 1; }
done

# every existing directory above what gets installed must be root's alone, or a user could swap a
# root-owned directory out from under it (e.g. an Intel Homebrew /usr/local)
for d in /usr /usr/local /usr/local/libexec /usr/local/var /Library /Library/LaunchDaemons; do
    [[ -e $d ]] || continue
    if [[ -L $d || $(stat -f %u "$d") != 0 ]] || (( 0$(stat -f %Lp "$d") & 022 )); then
        echo "desk-setup: $d is not root-owned and closed to others; refusing to install under it" >&2
        exit 1
    fi
done

# the user and group: hidden, no shell, an id below 500 so it stays off the login window
id_used() { dscl . -list /Users UniqueID | awk '{print $2}' | grep -qx "$1" ||
            dscl . -list /Groups PrimaryGroupID | awk '{print $2}' | grep -qx "$1"; }
if dscl . -read "/Groups/$USER_NAME" PrimaryGroupID >/dev/null 2>&1; then
    GID=$(dscl . -read "/Groups/$USER_NAME" PrimaryGroupID | awk '{print $2}')
    echo "group $USER_NAME already exists (id $GID)"
else
    GID=""
    for n in $(seq 450 499); do if ! id_used "$n"; then GID=$n; break; fi; done
    [[ -n $GID ]] || { echo "desk-setup: no free id in 450-499" >&2; exit 1; }
    dscl . -create "/Groups/$USER_NAME"
    dscl . -create "/Groups/$USER_NAME" PrimaryGroupID "$GID"
    dscl . -create "/Groups/$USER_NAME" RealName "bmv Quant Desk"
    echo "made group $USER_NAME (id $GID)"
fi
if dscl . -read "/Users/$USER_NAME" UniqueID >/dev/null 2>&1; then
    echo "user $USER_NAME already exists"
else
    # the user id matches the group's unless another user already has it
    UID_NEW=$GID
    if dscl . -list /Users UniqueID | awk '{print $2}' | grep -qx "$UID_NEW"; then
        UID_NEW=""
        for n in $(seq 450 499); do if ! id_used "$n"; then UID_NEW=$n; break; fi; done
        [[ -n $UID_NEW ]] || { echo "desk-setup: no free id in 450-499" >&2; exit 1; }
    fi
    dscl . -create "/Users/$USER_NAME"
    dscl . -create "/Users/$USER_NAME" UniqueID "$UID_NEW"
    dscl . -create "/Users/$USER_NAME" PrimaryGroupID "$GID"
    dscl . -create "/Users/$USER_NAME" UserShell /usr/bin/false
    dscl . -create "/Users/$USER_NAME" NFSHomeDirectory /var/empty
    dscl . -create "/Users/$USER_NAME" RealName "bmv Quant Desk"
    dscl . -create "/Users/$USER_NAME" IsHidden 1
    dscl . -create "/Users/$USER_NAME" Password "*"
    echo "made user $USER_NAME (id $UID_NEW)"
fi

# binaries: root-owned, so the desk user and the agents can run them but never change them
# copied first into a root-only staging directory, so what is checked is what gets installed;
# $SRC is writable by you, and so by anything running as you
STAGE=$(mktemp -d /var/root/bmv-desk-stage.XXXXXX)
trap 'rm -rf "$STAGE"' EXIT
for b in desk desk-mcp desk-sign; do
    [[ -L $SRC/$b ]] && { echo "desk-setup: $SRC/$b is a symlink; refusing" >&2; exit 1; }
    cp "$SRC/$b" "$STAGE/$b"
done
echo "About to install these (compare with: shasum -a 256 $SRC/desk $SRC/desk-mcp $SRC/desk-sign, run right after your build):"
(cd "$STAGE" && shasum -a 256 desk desk-mcp desk-sign)
read -r -p "Install them? Type yes: " answer
[[ $answer == yes ]] || { echo "desk-setup: not installed" >&2; exit 1; }
install -d -o root -g wheel -m 755 "$BIN_DIR"
for b in desk desk-mcp desk-sign; do
    install -o root -g wheel -m 755 "$STAGE/$b" "$BIN_DIR/$b"
done
echo "installed $BIN_DIR/{desk,desk-mcp,desk-sign}"

# the database: only the desk user can read or write it
install -d -o "$USER_NAME" -g "$USER_NAME" -m 700 "$DATA_DIR"
# the log belongs to the desk user, so the server can keep writing it
[[ -e $DATA_DIR/desk.log ]] || install -o "$USER_NAME" -g "$USER_NAME" -m 600 /dev/null "$DATA_DIR/desk.log"

# the server, started at boot and restarted if it stops; umask 077 keeps new files private
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>UserName</key><string>$USER_NAME</string>
  <key>GroupName</key><string>$USER_NAME</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN_DIR/desk</string><string>serve</string>
    <string>--db</string><string>$DATA_DIR/desk.db</string>
    <string>--port</string><string>$PORT</string>
  </array>
  <key>WorkingDirectory</key><string>$DATA_DIR</string>
  <key>Umask</key><integer>63</integer>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>$DATA_DIR/desk.log</string>
</dict>
</plist>
EOF
chown root:wheel "$PLIST"
chmod 644 "$PLIST"
plutil -lint "$PLIST" >/dev/null

launchctl bootout "system/$LABEL" 2>/dev/null || true
launchctl bootstrap system "$PLIST"
echo "started $LABEL as $USER_NAME on 127.0.0.1:$PORT"
echo
echo "Next, as yourself (Touch ID):"
echo "  $BIN_DIR/desk init --db $DATA_DIR/desk.db --as $USER_NAME"
