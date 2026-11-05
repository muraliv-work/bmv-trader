# Sourced by desk-setup.sh and clip-setup.sh (run as root).
# hidden_user NAME "Real name": makes a hidden user and group NAME with no shell, no home and no
# login, with an id below 500 so it stays off the login window. Leaves an existing one as it is.

id_used() { dscl . -list /Users UniqueID | awk '{print $2}' | grep -qx "$1" ||
            dscl . -list /Groups PrimaryGroupID | awk '{print $2}' | grep -qx "$1"; }

free_id() {
    local n
    for n in $(seq 450 499); do if ! id_used "$n"; then echo "$n"; return; fi; done
    echo "no free id in 450-499" >&2
    return 1
}

hidden_user() {
    local name=$1 real=$2 gid uid
    if dscl . -read "/Groups/$name" PrimaryGroupID >/dev/null 2>&1; then
        gid=$(dscl . -read "/Groups/$name" PrimaryGroupID | awk '{print $2}')
        echo "group $name already exists (id $gid)"
    else
        gid=$(free_id)
        dscl . -create "/Groups/$name"
        dscl . -create "/Groups/$name" PrimaryGroupID "$gid"
        dscl . -create "/Groups/$name" RealName "$real"
        echo "made group $name (id $gid)"
    fi
    if dscl . -read "/Users/$name" UniqueID >/dev/null 2>&1; then
        # one made by someone else must still be unable to log in
        if [[ $(dscl . -read "/Users/$name" UserShell | awk '{print $2}') != /usr/bin/false ]]; then
            echo "user $name exists with a login shell; refusing" >&2
            return 1
        fi
        echo "user $name already exists"
    else
        # the user id matches the group's unless another user already has it
        uid=$gid
        if dscl . -list /Users UniqueID | awk '{print $2}' | grep -qx "$uid"; then
            uid=$(free_id)
        fi
        dscl . -create "/Users/$name"
        dscl . -create "/Users/$name" UniqueID "$uid"
        dscl . -create "/Users/$name" PrimaryGroupID "$gid"
        dscl . -create "/Users/$name" UserShell /usr/bin/false
        dscl . -create "/Users/$name" NFSHomeDirectory /var/empty
        dscl . -create "/Users/$name" RealName "$real"
        dscl . -create "/Users/$name" IsHidden 1
        dscl . -create "/Users/$name" Password "*"
        echo "made user $name (id $uid)"
    fi
}

# every existing directory above what gets installed must be root's alone, or a user could swap a
# root-owned directory out from under it (e.g. an Intel Homebrew /usr/local)
root_only_parents() {
    local d
    for d in "$@"; do
        [[ -e $d ]] || continue
        if [[ -L $d || $(stat -f %u "$d") != 0 ]] || (( 0$(stat -f %Lp "$d") & 022 )); then
            echo "$d is not root-owned and closed to others; refusing to install under it" >&2
            return 1
        fi
    done
}
