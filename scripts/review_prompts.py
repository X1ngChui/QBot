"""Ask the configured DeepSeek text model to review the complete prompt family.

This is a paid, on-demand check for prompt edits. It uses the disposable test database
for metering and never includes a real persona, roster, transcript, or credential.
"""

from __future__ import annotations

import asyncio
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

from qqbot.core.prompt import build_developer, build_policy
from qqbot.core.tools import tool_defs
from qqbot.db import close_pool, init_pool
from qqbot.providers import build_default, providers, set_providers
from qqbot.providers.contracts import (
    CallContext,
    CallPurpose,
    GenerationPolicy,
    Message,
    ModelRequest,
    ReasoningEffort,
    Role,
)
from qqbot.services.memory_extractor import MemoryExtractor
from qqbot.settings import Persona, config, ptext
from qqbot.workers.memory import transcript_legend


REVIEW_MODEL = "deepseek-flash"

REVIEW_REQUEST = """Review this Chinese prompt family as an Agent/prompt engineer.
Focus only on actionable issues:
1. contradictions between files or role layers;
2. duplicated rules that can be stated once without weakening behavior;
3. ambiguous terminology, authority, tool ordering, parameter origins, or failure recovery;
4. instructions likely to cause tool misuse, prompt leakage, fabricated references, or false claims;
5. extraction rules that conflict with reply rules or validator-enforced behavior.

Do not rewrite the persona or alter product policy. Do not ask for more context. Examples in
your suggestions must be fictional and must not name real people. Report findings by
severity with exact section/file names, then list any parts that are already coherent.
If no material issue exists, say so directly.
"""


def review_document() -> str:
    cfg = config().default
    sample_developer = build_developer(
        Persona(
            name="小X",
            system_prompt="你是这个虚构测试群的一员。",
            group_knowledge="这是一个用于提示词检查的虚构群。",
        ),
        [
            {
                "user_id": "100",
                "nickname": "成员甲",
                "former_names": ["甲同学"],
                "aliases": ["小甲"],
                "manual_note": "成员本人确认喜欢摄影。",
                "persona_card": "可能经常分享摄影作品。",
            }
        ],
    )
    extraction = MemoryExtractor(cfg, legend=transcript_legend()).prompt
    tools = "\n\n".join(
        f"### tool_{spec.name}.txt\n{spec.description}"
        for spec in tool_defs(cfg)
    )
    return (
        "## Rendered reply system role\n"
        + build_policy()
        + "\n\n## Rendered reply developer role (fictional sample)\n"
        + sample_developer
        + "\n\n## Reply user-tail contract\n"
        + ptext("reply_final")
        + "\n\n## Rendered extraction system role\n"
        + extraction
        + "\n\n## Tool descriptions\n"
        + tools
    )


async def main() -> int:
    await init_pool()
    await assert_disposable_database()
    bundle = config()
    cfg = bundle.default.capabilities.text
    if cfg.provider != "deepseek":
        raise RuntimeError(
            f"prompt review requires the DeepSeek provider, got {cfg.provider!r}"
        )
    set_providers(build_default())
    request = ModelRequest(
        prompt=(
            Message(Role.SYSTEM, REVIEW_REQUEST),
            Message(Role.USER, review_document()),
        ),
        policy=GenerationPolicy(
            model=REVIEW_MODEL,
            reasoning=ReasoningEffort.LOW,
            timeout_sec=max(cfg.timeout_sec, 120.0),
            retries=cfg.retries,
            max_output_tokens=4000,
        ),
        context=CallContext(CallPurpose.PREFLIGHT),
    )
    try:
        async with providers().text.open_session(request) as session:
            turn = await session.start()
        print(turn.text)
        return 0 if turn.text.strip() else 1
    finally:
        await providers().aclose()
        await close_pool()


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
