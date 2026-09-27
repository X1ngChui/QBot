"""Deployment ordering against a disposable local fake host, never SSH or Docker."""

import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parent.parent


def _bash():
    if os.name != "nt":
        bash = shutil.which("bash")
        if bash:
            return bash
        pytest.skip("Bash is needed to exercise the shell deployment entry point")
    git = Path(shutil.which("git") or "")
    for parent in git.parents:
        executable = parent / "usr" / "bin" / "bash.exe"
        if executable.is_file():
            return str(executable)
    pytest.skip("Git Bash is needed to exercise the shell deployment entry point")


def _shell_path(path):
    path = Path(path).resolve().as_posix()
    return f"/{path[0].lower()}{path[2:]}" if path[1:2] == ":" else path


@pytest.fixture
def fake_host(tmp_path):
    project = tmp_path / "project"
    remote = tmp_path / "remote"
    bin_dir = tmp_path / "bin"
    for directory in (
        project / "scripts",
        project / "qqbot",
        project / "config" / "prompts",
        project / "sql",
        remote / "config",
        bin_dir,
    ):
        directory.mkdir(parents=True, exist_ok=True)
    shutil.copy2(ROOT / "scripts" / "deploy.sh", project / "scripts" / "deploy.sh")
    shutil.copy2(ROOT / "scripts" / "_fingerprint.py", project / "scripts" / "_fingerprint.py")
    (project / "qqbot" / "__init__.py").write_text("", encoding="utf-8")
    (project / "config" / "prompts" / "prompts.yaml").write_text(
        "example: public\n", encoding="utf-8"
    )
    for filename in (
        "bot.py",
        "requirements.txt",
        "Dockerfile",
        "docker-compose.yml",
        ".dockerignore",
    ):
        (project / filename).write_text("public fixture\n", encoding="utf-8")
    (remote / "config" / "marker").write_text("old\n", encoding="utf-8")
    trace = tmp_path / "trace"
    ssh = bin_dir / "ssh"
    ssh.write_text('#!/usr/bin/env bash\ncommand=${!#}\nbash -c "$command"\n', encoding="utf-8")
    docker = bin_dir / "docker"
    docker.write_text(
        """#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$TRACE"
case "$*" in
    'compose ps -q bot') printf 'fake-bot\\n' ;;
    'inspect --format '* ) printf 'fake-image\\n' ;;
    'tag '* | 'compose logs '* ) ;;
    'compose build bot') [ "$FAIL_AT" != build ] ;;
    'compose run '* )
        [[ "$*" == *'CONFIG_DIR=/app/config.next'* ]]
        [[ "$*" == *'.deploy.new/config:/app/config.next:ro'* ]]
        [ "$(< "$QBOT_REMOTE/config/marker")" = old ]
        [ -f "$QBOT_REMOTE/.deploy.new/config/prompts/prompts.yaml" ]
        [ "$FAIL_AT" != schema ]
        ;;
    'compose stop bot') ;;
    'compose up -d --no-build bot')
        [ ! -e "$QBOT_REMOTE/config/marker" ]
        [ -f "$QBOT_REMOTE/config/prompts/prompts.yaml" ]
        [ "$FAIL_AT" != startup ]
        ;;
    'compose exec -T bot python /app/scripts/_fingerprint.py '* )
        if [ "$FAIL_AT" = fingerprint ]; then
            printf 'changed after switch\\n' >> "$QBOT_REMOTE/config/prompts/prompts.yaml"
        fi
        python "$QBOT_REMOTE/scripts/_fingerprint.py" "$QBOT_REMOTE/qqbot" \\
            "$QBOT_REMOTE/bot.py" "$QBOT_REMOTE/scripts" "$QBOT_REMOTE/config"
        ;;
    *) printf 'unexpected docker call: %s\\n' "$*" >&2; exit 1 ;;
esac
""",
        encoding="utf-8",
    )
    ssh.chmod(0o755)
    docker.chmod(0o755)
    return project, remote, bin_dir, trace


@pytest.mark.parametrize("phase", ["build", "schema", "startup", "success", "fingerprint"])
def test_deployment_gates_and_rolls_back(fake_host, phase):
    project, remote, bin_dir, trace = fake_host
    env = os.environ.copy()
    env.update(
        {
            "PATH": (
                f"{_shell_path(bin_dir)}:{_shell_path(Path(_bash()).parent)}:"
                f"{_shell_path(Path(sys.executable).parent)}:/usr/bin:/mingw64/bin"
            ),
            "QBOT_HOST": "fake",
            "QBOT_REMOTE": _shell_path(remote),
            "TRACE": _shell_path(trace),
            "FAIL_AT": phase if phase != "success" else "none",
            "PYTHON": "python",
        }
    )
    for program in ("ssh", "docker"):
        resolved = subprocess.run(
            [_bash(), "-c", f"command -v {program}"],
            cwd=project,
            env=env,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        assert resolved == f"{_shell_path(bin_dir)}/{program}"
    result = subprocess.run(
        [_bash(), "scripts/deploy.sh"],
        cwd=project,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=40,
        check=False,
    )
    calls = trace.read_text(encoding="utf-8") if trace.exists() else ""
    if phase in ("success", "fingerprint"):
        assert (result.returncode == 0) is (phase == "success"), result.stdout + result.stderr
        if phase == "fingerprint":
            assert "DEPLOY FAILED: the running container" in result.stderr
        assert not (remote / "config" / "marker").exists()
        assert (remote / "config" / "prompts" / "prompts.yaml").is_file()
        assert not (remote / ".deploy.new").exists()
        assert "compose exec -T bot python /app/scripts/_fingerprint.py " in calls
    else:
        assert result.returncode != 0, result.stdout + result.stderr
        assert (remote / "config" / "marker").read_text(encoding="utf-8") == "old\n"
        if phase == "startup":
            assert calls.splitlines().count("compose stop bot") == 2
        else:
            assert "compose stop bot" not in calls.splitlines()
    if phase != "build":
        assert "CONFIG_DIR=/app/config.next" in calls, result.stdout + result.stderr
        assert ".deploy.new/config:/app/config.next:ro" in calls
