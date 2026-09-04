"""Run every test script in order and summarise. Exits non-zero if any of them fail."""

import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
SCRIPTS = [
    "test_domain.py",
    "test_repositories.py",
    "test_services.py",
    "test_commands.py",
    "test_memory.py",
    "test_logic.py",
    "test_nickname.py",
    "test_keys.py",
    "test_backends.py",
    "test_repo.py",
    "test_media.py",
    "test_pipeline.py",
]


def main() -> int:
    failed = []
    for name in SCRIPTS:
        print(f"\n{'=' * 70}\n{name}\n{'=' * 70}")
        rc = subprocess.call([sys.executable, str(HERE / name)])
        if rc != 0:
            failed.append(name)

    print(f"\n{'=' * 70}")
    if failed:
        print(f"FAILED: {', '.join(failed)}")
        return 1
    print(f"all {len(SCRIPTS)} suites passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
