"""Prints one SHA-256 for a directory tree: every file's path, mode bits and content, and every
symlink's target. Refuses a symlink that leads outside the tree, so an installed copy can't run
code from a place someone else can change, and a setuid or setgid file, which would run as root
once root owns it. Used by clip-build.sh and clip-setup.sh.

    python3 -I scripts/tree-hash.py DIR
"""

import hashlib
import os
import stat
import sys


def tree_hash(root):
    root = os.path.realpath(root)
    h = hashlib.sha256()
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(dirnames + filenames):
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            st = os.lstat(path)
            if stat.S_ISLNK(st.st_mode):
                target = os.readlink(path)
                end = os.path.realpath(path)
                if os.path.commonpath([root, end]) != root:
                    sys.exit(f"tree-hash: {rel} links outside the tree, to {target}")
                h.update(f"L {rel} {target}\n".encode())
            elif stat.S_ISREG(st.st_mode):
                if st.st_mode & (stat.S_ISUID | stat.S_ISGID):
                    sys.exit(f"tree-hash: {rel} is setuid or setgid")
                f = hashlib.sha256()
                with open(path, "rb") as fh:
                    for chunk in iter(lambda: fh.read(1 << 20), b""):
                        f.update(chunk)
                h.update(f"F {rel} {stat.S_IMODE(st.st_mode) & 0o111:o} {f.hexdigest()}\n".encode())
            elif stat.S_ISDIR(st.st_mode):
                h.update(f"D {rel}\n".encode())
            else:
                sys.exit(f"tree-hash: {rel} is not a file, directory or symlink")
    return h.hexdigest()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: tree-hash.py DIR")
    print(tree_hash(sys.argv[1]))
