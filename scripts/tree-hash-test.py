"""Checks tree-hash.py: run by scripts/check.sh."""

import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))


def run(d):
    return subprocess.run([sys.executable, "-I", os.path.join(HERE, "tree-hash.py"), d],
                          capture_output=True, text=True)


with tempfile.TemporaryDirectory() as t:
    root = os.path.join(t, "tree")
    os.makedirs(os.path.join(root, "lib"))
    with open(os.path.join(root, "lib", "a.so"), "w") as f:
        f.write("a")
    os.symlink("a.so", os.path.join(root, "lib", "a.dylib"))
    first = run(root)
    assert first.returncode == 0, first.stderr

    # same tree, same hash; any change to content, mode or a link changes it
    assert run(root).stdout == first.stdout
    os.chmod(os.path.join(root, "lib", "a.so"), 0o755)
    assert run(root).stdout != first.stdout

    # a link out of the tree is refused, even a relative one
    with open(os.path.join(t, "outside.js"), "w") as f:
        f.write("x")
    os.symlink("../../outside.js", os.path.join(root, "lib", "evil.js"))
    out = run(root)
    assert out.returncode != 0 and "links outside the tree" in out.stderr, out

    os.remove(os.path.join(root, "lib", "evil.js"))

    # a setuid file is refused
    os.chmod(os.path.join(root, "lib", "a.so"), 0o4755)
    out = run(root)
    assert out.returncode != 0 and "setuid" in out.stderr, out

print("tree-hash: OK")
