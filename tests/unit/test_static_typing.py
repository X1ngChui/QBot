"""The configured type checker enforces nominal identifiers at model boundaries."""

import json
from pathlib import Path
import subprocess
import sys
from textwrap import dedent

import pytest

ROOT = Path(__file__).resolve().parents[2]


@pytest.mark.parametrize("argument, accepted", [("group_id", True), ("str(group_id)", False)])
def test_call_context_rejects_erased_group_identity(tmp_path, argument, accepted):
    source = tmp_path / "extraction_context.py"
    source.write_text(
        dedent(f"""\
            from qqbot.domain.ids import GroupId
            from qqbot.providers.contracts import CallContext, CallPurpose

            def extraction_context(group_id: GroupId) -> CallContext:
                return CallContext(CallPurpose.EXTRACT, {argument})
            """),
        encoding="utf-8",
    )
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "pyright",
            "--project",
            str(ROOT / "pyrightconfig.json"),
            "--outputjson",
            str(source),
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=60,
    )
    report = json.loads(result.stdout)
    errors = [item for item in report["generalDiagnostics"] if item["severity"] == "error"]
    if accepted:
        assert result.returncode == 0, errors
        assert errors == []
    else:
        assert result.returncode == 1, result.stderr
        assert len(errors) == 1, errors
        assert errors[0]["rule"] == "reportArgumentType"
        assert "GroupId" in errors[0]["message"] and '"str"' in errors[0]["message"]
