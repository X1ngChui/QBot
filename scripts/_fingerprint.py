"""Content hash of the deployed source, computed identically on the host and in the
container.

Comparing the two is how deploy.sh knows the running image is the code it just sent.
`docker compose restart` rebuilds nothing, so without this check a deployment looks
successful - container up, logs clean - while the old image keeps serving.

Takes any number of roots: source directories contribute their *.py files, the
mounted config directory contributes all files, and a file contributes itself. Every
path the Dockerfile COPYs code from, and the config mounted by compose, should be
listed - a stale image or a stale/mismatched prompt bundle must not pass as "ok".

Hashes file bytes, not mtimes, so it also catches an uncommitted local edit that never
made it into the tar. Entries are keyed by root-relative names (qqbot/..., bot.py), so
the digest agrees between the host tree and /app in the container.
"""

import hashlib
import pathlib
import sys

h = hashlib.sha256()
for arg in sys.argv[1:]:
    p = pathlib.Path(arg)
    if p.is_file():
        entries = [(p.name, p)]
    else:
        entries = sorted(
            (f"{p.name}/{f.relative_to(p).as_posix()}", f)
            for f in p.rglob("*" if p.name == "config" else "*.py")
            if f.is_file() and "__pycache__" not in f.parts
        )
    for name, f in entries:
        h.update(name.encode())
        h.update(f.read_bytes())
print(h.hexdigest()[:16])
