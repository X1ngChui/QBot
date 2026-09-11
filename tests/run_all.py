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


def lint() -> bool:
    """ruff over the whole tree, if it is installed. True when it is clean.

    Run here rather than only by hand, because a linter nobody runs is a linter
    nobody obeys. Skipped rather than failed when ruff is absent: the suites are
    meant to run in a bare venv with the runtime dependencies alone.
    """
    print(f"\n{'=' * 70}\nruff\n{'=' * 70}")
    try:
        import ruff  # noqa: F401  (imported to find out whether it is installed)
    except ImportError:
        print("ruff is not installed - skipping (pip install ruff)")
        return True
    return subprocess.call([sys.executable, "-m", "ruff", "check",
                            str(HERE.parent)]) == 0


def complete() -> bool:
    """Whether the ordered list above names every test_*.py here, and nothing else.

    The order matters (the suites that share the test database run in a fixed
    sequence), so the list stays hand-written; what must not happen is a new suite
    that nobody added to it running only when somebody remembers to.
    """
    on_disk = sorted(p.name for p in HERE.glob("test_*.py"))
    missing = sorted(set(on_disk) - set(SCRIPTS))
    stale = sorted(set(SCRIPTS) - set(on_disk))
    if missing:
        print(f"suites on disk but not in run_all.SCRIPTS: {', '.join(missing)}")
    if stale:
        print(f"suites listed in run_all.SCRIPTS but not on disk: {', '.join(stale)}")
    return not missing and not stale


def main() -> int:
    failed = []
    if not complete():
        failed.append("run_all.SCRIPTS")
    for name in SCRIPTS:
        print(f"\n{'=' * 70}\n{name}\n{'=' * 70}")
        rc = subprocess.call([sys.executable, str(HERE / name)])
        if rc != 0:
            failed.append(name)
    if not lint():
        failed.append("ruff")

    print(f"\n{'=' * 70}")
    if failed:
        print(f"FAILED: {', '.join(failed)}")
        return 1
    print(f"all {len(SCRIPTS)} suites passed, lint clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
