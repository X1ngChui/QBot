"""Read a .env file into the process environment, for scripts run outside compose.

The deployed bot gets its variables from compose, which reads .env itself; a
script run from a workstation venv has to do the same reading by hand. Values
already in the environment win, so a variable exported in the shell overrides
the file the way it does under compose.
"""

from __future__ import annotations

import os
import pathlib


def load_dotenv(path: pathlib.Path) -> None:
    """Apply KEY=VALUE lines from `path` with os.environ.setdefault.

    Blank lines and comments are skipped, a leading `export ` is dropped, and a
    value wrapped in one matching pair of quotes loses them - the three forms the
    same file takes when it is also sourced by a shell. Nothing is printed: the
    file holds credentials.
    """
    for line in path.read_text(encoding="utf-8-sig").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        if line.startswith("export "):
            line = line[len("export "):].lstrip()
        key, value = line.split("=", 1)
        key, value = key.strip(), value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
            value = value[1:-1]
        if key:
            os.environ.setdefault(key, value)
