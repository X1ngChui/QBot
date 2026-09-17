"""Build the complete, fictional-only packet used for prompt writing and review."""

from __future__ import annotations

import json
from typing import Any

from ..core.member_numbers import MemberNumbers
from ..core.prompt import build_developer, build_policy
from ..core.tools import tool_defs
from ..services.memory_extractor import rules_block, tools as extraction_tools
from ..settings import Persona, Settings
from .cases import case_document
from .templates import PROMPT_SPECS, PromptCatalog, PromptKey


def _tool_document(specs) -> list[dict[str, Any]]:
    return [
        {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.parameters,
            "strict": spec.strict,
        }
        for spec in specs
    ]


def build_prompt_packet(catalog: PromptCatalog, cfg: Settings) -> str:
    """Return the full authoring contract without reading live or persisted data."""

    people = MemberNumbers(self_id="bot-0")
    people.teach("member-1", "person-1")
    people.teach("member-2", "person-2")
    people.number("member-1", spoke=True)
    people.number("member-2")
    persona = Persona(
        name="小X",
        system_prompt="你是虚构测试群中的成员小X。",
        group_knowledge="这是只用于验证提示词的虚构群。",
    )
    profiles = [
        {
            "user_id": "member-1",
            "entity_id": "person-1",
            "accounts": ["member-1"],
            "nickname": "成员甲",
            "former_names": ["甲同学"],
            "aliases": ["小甲"],
            "manual_note": "成员本人确认喜欢摄影。",
            "persona_card": "可能经常分享摄影作品。",
        },
        {
            "user_id": "member-2",
            "entity_id": "person-2",
            "accounts": ["member-2"],
            "nickname": "成员乙",
            "former_names": [],
            "aliases": [],
            "manual_note": "",
            "persona_card": "",
        },
    ]
    extraction_user = catalog.render(
        PromptKey.EXTRACT_USER,
        bot_names="小X、机器人X",
        account_roster="成员甲⟦1⟧（也叫：小甲）\n成员乙⟦2⟧",
        known_memory="【已经记过的】\n- 成员甲居住在南京",
        transcript=(
            "⟦09-17 14:03⟧ 成员甲⟦1⟧: @小X⟦0⟧ 我搬到杭州了\n"
            "⟦09-17 14:04⟧ 小X⟦0⟧: 收到\n"
            "⟦09-17 14:05⟧ 成员乙⟦2⟧: 小甲周六一起看展吗"
        ),
    )
    packet = {
        "contract": {
            "purpose": "Rewrite the complete Chinese prompt family as one coherent bundle.",
            "authority": (
                "Code owns roles, composition, slots, schemas, marker grammar and dynamic "
                "data. The model owns only Chinese wording in declared templates."
            ),
            "template_syntax": "Only declared {{ascii_slot}} markers are interpreted, once.",
            "composition": {
                "reply": [
                    "reply_system(shared_legend, shared_pragmatics)",
                    "reply_developer(persona, group_context, member_roster)",
                    "structured history and tool continuations",
                    "reply_user(now, current_message)",
                ],
                "extract": [
                    "extract_system(shared_legend, shared_pragmatics, predicate_table)",
                    "extract_user(bot_names, account_roster, known_memory, transcript)",
                ],
                "vision": ["vision_system"],
                "tools": ["one description per code-owned schema"],
            },
            "markers": {
                "bot_identity": "display-only 名字⟦0⟧; zero is never a tool target",
                "member_identity": "名字⟦N⟧ with N > 0",
                "reply_line": "#N, independent and positive",
                "media": "⟦图片N:…⟧ / ⟦表情N:…⟧, independent and positive",
                "owner": "orthogonal ⟦拥有者⟧ tag on a positive member only",
                "unforgeable": "member-controlled ⟦ ⟧ are defanged to [ ]",
            },
            "hard_constraints": [
                "No real persona, roster, transcript, URL, credential or production data.",
                "No invented tools, fields, enum values, markers or schema limits.",
                "mface remains absent from model-facing prompts and schemas.",
                "A valid send_message is the only visible reply and terminates the run.",
                "Extraction emits structured tool calls only and quotes exact eligible lines.",
                "Keep wording professional, plain, accurate, calm and direct.",
            ],
        },
        "template_specs": {
            key.value: {
                "role": spec.role.value,
                "reload_scope": spec.reload_scope.value,
                "slots": [
                    {"name": slot.name, "allow_empty": slot.allow_empty}
                    for slot in spec.slots
                ],
            }
            for key, spec in PROMPT_SPECS.items()
        },
        "current_templates": {
            key.value: catalog.source(key) for key in PROMPT_SPECS
        },
        "code_derived": {
            "reply_tools": _tool_document(tool_defs(cfg)),
            "extraction_tools": _tool_document(extraction_tools()),
            "predicate_table": rules_block(),
        },
        "fictional_rendered_examples": {
            "reply_system": build_policy(),
            "reply_developer": build_developer(
                persona,
                profiles,
                ["本群把“蓝盒”定义为虚构测试设备。"],
                people,
            ),
            "reply_user": catalog.render(
                PromptKey.REPLY_USER,
                now="2026年9月18日 14:06",
                current_message=(
                    "#3 ⟦09-17 14:06⟧ 成员甲⟦1⟧: "
                    "@小X⟦0⟧ 帮我问成员乙⟦2⟧周六几点出发"
                ),
            ),
            "extract_system": catalog.render(
                PromptKey.EXTRACT_SYSTEM,
                shared_legend=catalog.source(PromptKey.SHARED_LEGEND),
                shared_pragmatics=catalog.source(PromptKey.SHARED_PRAGMATICS),
                predicate_table=rules_block(),
            ),
            "extract_user": extraction_user,
            "vision_system": catalog.render(PromptKey.VISION_SYSTEM),
        },
        "fictional_cases": case_document(),
    }
    return json.dumps(packet, ensure_ascii=False, indent=2)
