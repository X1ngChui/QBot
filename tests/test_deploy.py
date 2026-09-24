"""Run the local deployment smoke test through its Bash entry point."""

import os
import pathlib
import shutil
import subprocess

script = pathlib.Path(__file__).with_suffix(".sh")
bash = "bash"
if os.name == "nt":
    git = pathlib.Path(shutil.which("git") or "")
    bash = next(
        str(parent / "usr" / "bin" / "bash.exe")
        for parent in git.parents
        if (parent / "usr" / "bin" / "bash.exe").is_file()
    )
raise SystemExit(subprocess.call([bash, str(script)]))
