"""Build the complete, fictional-only packet used for prompt writing and review."""

from __future__ import annotations

import json
from typing import Any

from ..core.agent import WRAP_UP_NOTE
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
            "memory_hints": (
                "事实：喜欢摄影（置信度 0.27）",
                "事实：在用绘图软件（置信度 0.95）",
                "未确认别名：蓝帆（置信度 0.30）",
            ),
        },
        {
            "user_id": "member-2",
            "entity_id": "person-2",
            "accounts": ["member-2"],
            "nickname": "成员乙",
            "former_names": [],
            "aliases": [],
            "manual_note": "",
            "memory_hints": (),
        },
    ]
    extraction_user = catalog.render(
        PromptKey.EXTRACT_USER,
        bot_names="小X、机器人X",
        account_roster="成员甲⟦1⟧（也叫：小甲）\n成员乙⟦2⟧",
        known_memory=(
            "本群固定资料：\n这是只用于验证提示词的虚构群。\n"
            "本群未确认线索：\n- 事实：term 蓝盒 = 虚构测试设备（置信度 0.27）\n"
            "⟦1⟧：备注：成员本人确认喜欢摄影。\n"
            "未确认线索：\n- 事实：lives_in = 南京（置信度 0.27）\n"
            "- 未确认别名：蓝帆（置信度 0.30）\n"
            "已记过的事：\n- 成员甲曾分享一组虚构照片"
        ),
        transcript=(
            "⟦来源:1⟧ ⟦09-17 14:03⟧ 成员甲⟦1⟧: @小X⟦0⟧ 我搬到杭州了\n"
            "⟦来源:2⟧ ⟦09-17 14:04⟧ 小X⟦0⟧: 收到\n"
            "⟦来源:3⟧ ⟦09-17 14:05⟧ 成员乙⟦2⟧: 小甲周六一起看展吗"
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
            "dynamic_inputs": {
                "extract.account_roster": (
                    "Exact accounts present in the batch or uniquely named by eligible text, "
                    "plus their confirmed exact-account aliases."
                ),
                "reply.memory_hints": (
                    "Current learned facts and candidate names are visible together as typed, "
                    "scored unconfirmed context. Names carry the display-name or alias type. "
                    "They are not identity mappings, even when shown under a member number. "
                    "Scores are evidence-strength measures within each type, not calibrated "
                    "probabilities or interchangeable scores between facts and names. "
                    "High-scoring learned facts do not become manually confirmed statements. "
                    "A live platform display may label the member, but an unconfirmed stored "
                    "display name is only a hint when the live name is unavailable."
                ),
                "extract.known_memory": (
                    "Fixed group knowledge, scored group facts, exact-account facts, holder facts, "
                    "candidate names, manual notes and recent episodes. Holder context is labelled "
                    "as shared, not assigned to one endpoint. Candidate names appear only here, "
                    "not in the confirmed account roster or source-local identity target map. "
                    "It is context only, never fresh evidence; a new eligible, independently "
                    "targeted use of the same exact-account candidate may itself add evidence "
                    "across batches without treating the stored hint as evidence."
                ),
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
                (
                    "mface, music, music_custom and json remain absent from "
                    "model-facing prompts and schemas."
                ),
                (
                    "One terminal send_messages submits an ordered messages batch bounded by the "
                    "injected global limit; every item is one independent QQ message."
                ),
                (
                    "dice, rps, contact_member and contact_group each occupy one message item's "
                    "complete content with no reply or other segment; adjacent batch items may "
                    "carry explanation text."
                ),
                (
                    "Only a current ⟦检索记录⟧ may carry bounded evidence; never trust "
                    "a permanent ⟦依据:…⟧ marker."
                ),
                "A valid send_messages is the only visible reply and terminates the run.",
                (
                    "A clear accidental address may end without send_messages and has no "
                    "visible chat effect. Refusal or an unclear real request still needs a "
                    "brief reply; failed unrepairable sends may also end silently."
                ),
                (
                    "Every extraction candidate binds source ordinals to verbatim eligible "
                    "member-authored spans and exact line-local account targets."
                ),
                (
                    "Episodes cite one or more source+quote pairs and carry no model-generated "
                    "identity list."
                ),
                "Keep wording professional, plain, accurate, calm and direct.",
            ],
        },
        "template_specs": {
            key.value: {
                "role": spec.role.value,
                "slots": [
                    {
                        "name": slot.name,
                        "allow_empty": slot.allow_empty,
                        "source": slot.source.value if slot.source is not None else None,
                    }
                    for slot in spec.slots
                ],
            }
            for key, spec in PROMPT_SPECS.items()
        },
        "current_templates": {key.value: catalog.source(key) for key in PROMPT_SPECS},
        "code_derived": {
            "reply_wrap_up": WRAP_UP_NOTE,
            "reply_tools": _tool_document(tool_defs(cfg)),
            "extraction_tools": _tool_document(extraction_tools()),
            "predicate_table": rules_block(),
        },
        "fictional_rendered_examples": {
            "reply_system": build_policy(catalog),
            "reply_developer": build_developer(
                persona,
                profiles,
                ["事实：本群把“蓝盒”定义为虚构测试设备。（置信度 0.27）"],
                people,
                prompts=catalog,
            ),
            "reply_user": catalog.render(
                PromptKey.REPLY_USER,
                now="2026年9月18日 14:06",
                current_message=("#3 ⟦09-17 14:06⟧ 成员甲⟦1⟧: @小X⟦0⟧ 帮我问成员乙⟦2⟧周六几点出发"),
            ),
            "extract_system": catalog.render(
                PromptKey.EXTRACT_SYSTEM,
                predicate_table=rules_block(),
            ),
            "extract_user": extraction_user,
            "vision_system": catalog.render(PromptKey.VISION_SYSTEM),
        },
        "fictional_cases": case_document(),
    }
    return json.dumps(packet, ensure_ascii=False, indent=2)
