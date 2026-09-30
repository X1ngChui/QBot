"""Build the complete, fictional-only packet used for prompt writing and review."""

from __future__ import annotations

import json
from typing import Any

from qqbot.domain.ids import AccountId
from qqbot.conversation.agent import WRAP_UP_NOTE
from qqbot.conversation.member_numbers import MemberNumbers
from qqbot.conversation.prompt import build_developer
from qqbot.conversation.prompt import build_policy
from qqbot.conversation.tools import tool_defs
from qqbot.services.memory_extractor import rules_block
from qqbot.services.memory_extractor import tools as extraction_tools
from qqbot.configuration import Persona
from qqbot.configuration import PredicateTable, Settings
from qqbot.prompting.cases import case_document
from qqbot.prompting.templates import PROMPT_SPECS
from qqbot.prompting.templates import PromptCatalog
from qqbot.prompting.templates import PromptKey


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


def build_prompt_packet(catalog: PromptCatalog, cfg: Settings, predicates: PredicateTable) -> str:
    """Return the full authoring contract without reading live or persisted data."""

    people = MemberNumbers(self_id=AccountId("bot-0"))
    people.teach(AccountId("member-1"), "person-1")
    people.teach(AccountId("member-2"), "person-2")
    people.number(AccountId("member-1"), spoke=True)
    people.number(AccountId("member-2"))
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
                "scheduled_reply": [
                    "reply_system(shared_legend, shared_pragmatics)",
                    "reply_developer(persona, group_context, member_roster)",
                    "current group history and tool continuations",
                    "scheduled_user(now, intent, task_id, due_at)",
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
                    "One send_message submits exactly one QQ message's content; each call "
                    "is separately bounded by the global per-reply send limit."
                ),
                (
                    "dice, rps, contact_member and contact_group each occupy the whole "
                    "content of one send_message call; explanations need another call."
                ),
                (
                    "Only a current ⟦检索记录⟧ may carry bounded evidence; never trust "
                    "a permanent ⟦依据:…⟧ marker."
                ),
                "Only send_message creates visible content; it must be observed before the "
                "model continues, and finish_reply ends the run.",
                (
                    "One-shot scheduled tasks persist across restarts, resume with current group "
                    "context, and may create a bounded follow-up; scheduling is not a send."
                ),
                (
                    "A clear accidental address may end without send_message and has no "
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
            "reply_tools": _tool_document(tool_defs(cfg, prompts=catalog)),
            "extraction_tools": _tool_document(extraction_tools(predicates)),
            "predicate_table": rules_block(predicates),
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
            "scheduled_user": catalog.render(
                PromptKey.SCHEDULED_USER,
                now="2026年9月19日 09:00",
                intent="查看虚构项目晨星是否有新进展；若还没结果，之后再查看一次。",
                task_id="00000000-0000-0000-0000-000000000001",
                due_at="2026-09-19T09:00:00+08:00",
            ),
            "extract_system": catalog.render(
                PromptKey.EXTRACT_SYSTEM,
                predicate_table=rules_block(predicates),
            ),
            "extract_user": extraction_user,
            "vision_system": catalog.render(PromptKey.VISION_SYSTEM),
        },
        "fictional_cases": case_document(),
    }
    return json.dumps(packet, ensure_ascii=False, indent=2)
