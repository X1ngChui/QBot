"""Review the complete rendered prompt bundle once with DeepSeek.

The review packet is the same fictional-only, code-derived packet used by the writer, so
review cannot miss a role, dynamic input, schema, slot contract or example that generation saw.
"""

from __future__ import annotations

import asyncio
import json
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tests"))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "config"))

from _db import assert_disposable_database, configure_test_database

configure_test_database()

from _env import load_dotenv

load_dotenv(ROOT / ".env")

from qqbot.db import close_pool, init_pool
from qqbot.prompting.packet import build_prompt_packet
from qqbot.providers import build_default, providers, set_providers
from qqbot.providers.contracts import (
    CallContext,
    CallPurpose,
    GenerationPolicy,
    Message,
    ModelRequest,
    ReasoningEffort,
    Role,
    ToolSpec,
)
from qqbot.settings import config

REVIEW_MODEL = "deepseek-flash"
REVIEW_TOOL = "report_prompt_review"

REVIEW_REQUEST = """Review this complete Chinese prompt-template bundle as an Agent and prompt
engineer. The packet contains the entire family, strict slot contracts, actual code-derived tool
schemas, composition, complete fictional rendered inputs and behavior cases. Treat those contracts
as ground truth.

Call report_prompt_review exactly once and return no prose. Report only material,
actionable contradictions that would change runtime behavior or violate a schema/case. Do not report
intentional repetition caused by the same shared partial appearing in two independent model calls.
Do not confuse reliable prior context with fresh extraction evidence, display identity 0 with a
legal tool target, or reply and extraction's distinct dynamic projections. Do not propose persona or
product-policy changes. Each finding must name the logical template key, quote the conflicting
clauses, identify a concrete failing fictional case and state the runtime impact. Use an empty
findings array when nothing material remains. List the coherent boundaries actually verified.
"""


def review_tool() -> ToolSpec:
    """The bounded report shape keeps one review from becoming another rewrite."""

    return ToolSpec(
        name=REVIEW_TOOL,
        description="Report the bounded result of reviewing the complete prompt bundle.",
        parameters={
            "type": "object",
            "properties": {
                "findings": {
                    "type": "array",
                    "maxItems": 5,
                    "items": {
                        "type": "object",
                        "properties": {
                            "template_key": {"type": "string", "maxLength": 80},
                            "conflicting_clauses": {"type": "string", "maxLength": 1000},
                            "failing_case": {"type": "string", "maxLength": 500},
                            "runtime_impact": {"type": "string", "maxLength": 500},
                        },
                        "required": [
                            "template_key",
                            "conflicting_clauses",
                            "failing_case",
                            "runtime_impact",
                        ],
                        "additionalProperties": False,
                    },
                },
                "verified_boundaries": {
                    "type": "array",
                    "maxItems": 12,
                    "items": {"type": "string", "maxLength": 300},
                },
            },
            "required": ["findings", "verified_boundaries"],
            "additionalProperties": False,
        },
    )


def _validate_report(value) -> dict:
    """Validate the report locally; provider tool schemas are advisory."""

    if not isinstance(value, dict) or set(value) != {"findings", "verified_boundaries"}:
        raise RuntimeError("review report must contain exactly findings and verified_boundaries")
    findings = value["findings"]
    boundaries = value["verified_boundaries"]
    if not isinstance(findings, list) or len(findings) > 5:
        raise RuntimeError("review findings must be an array of at most 5 items")
    if not isinstance(boundaries, list) or len(boundaries) > 12:
        raise RuntimeError("review verified_boundaries must be an array of at most 12 items")
    finding_fields = {
        "template_key": 80,
        "conflicting_clauses": 1000,
        "failing_case": 500,
        "runtime_impact": 500,
    }
    for index, finding in enumerate(findings):
        if not isinstance(finding, dict) or set(finding) != set(finding_fields):
            raise RuntimeError(f"review finding {index} has an invalid shape")
        for field, limit in finding_fields.items():
            item = finding[field]
            if not isinstance(item, str) or not item.strip() or len(item) > limit:
                raise RuntimeError(f"review finding {index}.{field} is invalid")
    if any(
        not isinstance(item, str) or not item.strip() or len(item) > 300
        for item in boundaries
    ):
        raise RuntimeError("review verified_boundaries contains an invalid item")
    return value


async def main() -> int:
    await init_pool()
    await assert_disposable_database()
    bundle = config()
    cfg = bundle.default
    text_cfg = cfg.capabilities.text
    if text_cfg.provider != "deepseek":
        raise RuntimeError(
            f"prompt review requires DeepSeek, got {text_cfg.provider!r}"
        )
    set_providers(build_default())
    request = ModelRequest(
        prompt=(
            Message(Role.SYSTEM, REVIEW_REQUEST),
            Message(Role.USER, build_prompt_packet(bundle.prompts, cfg)),
        ),
        tools=(review_tool(),),
        policy=GenerationPolicy(
            model=REVIEW_MODEL,
            reasoning=ReasoningEffort.OFF,
            timeout_sec=max(text_cfg.timeout_sec, 180.0),
            retries=text_cfg.retries,
            max_output_tokens=32000,
        ),
        context=CallContext(CallPurpose.PREFLIGHT),
    )
    try:
        async with providers().text.open_session(request) as session:
            turn = await session.start()
        if len(turn.tool_calls) != 1 or turn.tool_calls[0].name != REVIEW_TOOL:
            raise RuntimeError("review expected exactly one report_prompt_review call")
        try:
            report = json.loads(turn.tool_calls[0].arguments)
        except json.JSONDecodeError as exc:
            raise RuntimeError("review returned invalid JSON arguments") from exc
        report = _validate_report(report)
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0
    finally:
        await providers().aclose()
        await close_pool()


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
