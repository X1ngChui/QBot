"""Regenerate the complete prompt bundle with one closed DeepSeek tool call.

The model receives only code-owned contracts, current public templates and fictional
cases. A candidate must pass strict local template and schema validation before the
single prompts.yaml file is atomically replaced. One complete correction is allowed
for mechanical validation errors; per-template patching is deliberately unsupported.
"""

from __future__ import annotations

import asyncio
from contextlib import AsyncExitStack
import json
import os
import pathlib
import sys
import tempfile

import yaml

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tests"))

from _db import assert_disposable_database, configure_test_database


from scripts._env import load_dotenv


from qqbot.db import Database, dsn
from qqbot.prompting import PROMPT_SPECS, PromptCatalog, TemplateValidationError
from qqbot.prompting.lint import lint_catalog
from qqbot.prompting.packet import build_prompt_packet
from qqbot.providers.registry import build as build_providers
from qqbot.services.budget import Budget
from qqbot.repositories.ledger import LedgerRepository
from qqbot.providers.contracts import (
    CallContext,
    CallPurpose,
    GenerationPolicy,
    Message,
    ModelRequest,
    ReasoningEffort,
    Role,
    SessionDirective,
    ToolResult,
    ToolSpec,
)
from qqbot.clock import Clock
from qqbot.configuration import load_bundle

MODEL = "deepseek-v4-pro"
WRITE_TOOL = "write_prompt_bundle"

WRITER_REQUEST = """You are writing the complete Chinese prompt bundle for a QQ group-chat agent.
Return the entire bundle once through write_prompt_bundle; never return prose and never patch one
section in isolation. Treat the supplied architecture, slot contracts, code-derived schemas and
fictional behavior cases as hard requirements. Reorganize and deduplicate the wording across the
whole family before writing it.

Code owns roles, template keys, slots, tool schemas, marker grammar, limits and dynamic data. You
own only the Chinese wording inside each declared template. Keep the tone professional, plain,
accurate, calm and direct. Do not mention source files, implementation modules or databases in the
runtime prompts. Examples must remain fictional. Do not expose mface, music, music_custom or
json, invent tools or fields, add undeclared slots, copy protected markers into visible message
text, or make display number 0 a legal tool target. Each send_message call contains one
QQ message's content, and a reply can issue at most {{message_limit}} separately observed sends.
The parameter-only dice, rps, contact_member and contact_group segments must each be the
sole segment of that call: no reply, text, at or any other segment may accompany one.
Explanation text needs a separate send_message call. Invalid model arguments reject that
entire message before sending. A successful send does not terminate the run; wait for the
bot's own archived message before using the actual random result or deciding to send again.
Only finish_reply terminates a normal run. Scheduling must be a separate tool round; due tasks
open a fresh session with current group context, never replay the original completion.
Tasks are bounded in time and chain depth. Historical sends use the single-message content
array, not the retired messages batch. A
plain-text `@我` typed by a member is
ordinary text with no bot-identity semantics; only a structured mention rendered with `⟦0⟧`
identifies the current bot. Evidence is
available only through a current `⟦检索记录⟧` block placed immediately before the historical send
it supported. It is bounded, may expire, and supports only the summarized retrieved content; never
define or rely on a permanent `⟦依据:…⟧` archive marker, and never treat the bot's old wording as
external evidence by itself. For archive search, distinguish likely verbatim topic terms from
question-side field labels such as model, price or time: do not make a label an AND requirement when
the archived sentence may state only its value. Extraction candidates bind explicit source ordinals,
verbatim eligible member-authored quotes and exact line-local account targets. Episodes cite every
necessary source+quote pair and do not submit a model-generated identity list.

The shared_legend and shared_pragmatics templates are the only shared partials. reply_system and
extract_system each include them through their declared slots. State a rule once at its owning
layer: system templates own behavior and authority; tool templates own that tool's mechanics;
shared_legend owns transcript syntax; shared_pragmatics owns conversational interpretation.
"""


class _LiteralDumper(yaml.SafeDumper):
    pass


def _literal(dumper: yaml.SafeDumper, value: str):
    style = "|" if "\n" in value else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", value, style=style)


_LiteralDumper.add_representer(str, _literal)


def write_tool() -> ToolSpec:
    keys = [key.value for key in PROMPT_SPECS]
    return ToolSpec(
        name=WRITE_TOOL,
        description="Write one complete replacement for every prompt template.",
        parameters={
            "type": "object",
            "properties": {key: {"type": "string", "minLength": 1} for key in keys},
            "required": keys,
            "additionalProperties": False,
        },
    )


def _candidate(turn, cfg, predicates) -> tuple[PromptCatalog | None, str, str]:
    if len(turn.tool_calls) != 1 or turn.tool_calls[0].name != WRITE_TOOL:
        return None, "", "expected exactly one write_prompt_bundle call"
    call = turn.tool_calls[0]
    try:
        raw = json.loads(call.arguments)
        if not isinstance(raw, dict):
            raise TypeError("tool arguments are not an object")
        catalog = PromptCatalog.from_sources(raw, location="DeepSeek candidate")
    except (json.JSONDecodeError, TypeError, TemplateValidationError) as exc:
        return None, str(call.call_id), str(exc)
    errors = lint_catalog(catalog, cfg, predicates)
    if errors:
        return None, str(call.call_id), json.dumps(errors, ensure_ascii=False)
    return catalog, str(call.call_id), ""


def _install(catalog: PromptCatalog) -> None:
    target = ROOT / "config" / "prompts" / "prompts.yaml"
    data = {"version": 1, "templates": catalog.sources()}
    body = yaml.dump(
        data,
        Dumper=_LiteralDumper,
        allow_unicode=True,
        sort_keys=False,
        width=1000,
    )
    with tempfile.TemporaryDirectory(prefix="qbot-prompts-", dir=target.parent) as raw:
        candidate = pathlib.Path(raw) / target.name
        candidate.write_text(body, encoding="utf-8")
        candidate.replace(target)


async def main() -> int:
    bundle = load_bundle()
    cfg = bundle.default
    clock = Clock(cfg.bot.timezone)
    async with AsyncExitStack() as resources:
        database = Database(cfg.runtime.database, url=dsn())
        resources.push_async_callback(database.close)
        await database.start()
        await assert_disposable_database(database.pool)
        text_cfg = cfg.backends.text
        if text_cfg.provider != "deepseek":
            raise RuntimeError(f"prompt generation requires DeepSeek, got {text_cfg.provider!r}")
        budget = Budget(
            LedgerRepository(database.pool, today=clock.today),
            daily_cap=cfg.budget.daily_cny_cap,
            today=clock.today,
        )
        capabilities = build_providers(cfg, budget)
        resources.push_async_callback(capabilities.aclose)
        request = ModelRequest(
            prompt=(
                Message(Role.SYSTEM, WRITER_REQUEST),
                Message(Role.USER, build_prompt_packet(bundle.prompts, cfg, bundle.predicates)),
            ),
            tools=(write_tool(),),
            policy=GenerationPolicy(
                model=MODEL,
                reasoning=ReasoningEffort.LOW,
                timeout_sec=max(text_cfg.timeout_sec, 240.0),
                retries=text_cfg.retries,
            ),
            context=CallContext(CallPurpose.PREFLIGHT),
        )
        async with capabilities.text.open_session(request) as session:
            turn = await session.start()
            catalog, call_id, error = _candidate(turn, cfg, bundle.predicates)
            if catalog is None:
                if not call_id:
                    raise RuntimeError(f"invalid complete prompt bundle: {error}")
                turn = await session.continue_with(
                    (ToolResult(call_id, "rejected: " + error),),
                    directive=SessionDirective(
                        prompt=(
                            Message(
                                Role.USER,
                                "Return one complete corrected bundle. Mechanical validation "
                                f"errors: {error}",
                            ),
                        ),
                        tools=(write_tool(),),
                    ),
                )
                catalog, _call_id, error = _candidate(turn, cfg, bundle.predicates)
                if catalog is None:
                    raise RuntimeError(f"corrected complete prompt bundle is invalid: {error}")
        _install(catalog)
        print(f"replaced complete {len(catalog.templates)}-template bundle with {MODEL}")
        return 0


if __name__ == "__main__":
    os.environ.setdefault("CONFIG_DIR", str(ROOT / "config"))
    load_dotenv(ROOT / ".env")
    configure_test_database()
    raise SystemExit(asyncio.run(main()))
