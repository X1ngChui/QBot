"""Deterministic prompt-bundle validation with no model or database calls."""

from __future__ import annotations

from qqbot.providers.contracts import JsonObject
from qqbot.gateway.segments import FACE_NAMES
from qqbot.conversation.tools import send_def
from qqbot.conversation.tools import tool_defs
from qqbot.services.memory_extractor import tools as extraction_tools
from qqbot.configuration import PredicateTable, Settings
from qqbot.prompting.cases import CASES
from qqbot.prompting.templates import PromptCatalog
from qqbot.prompting.templates import PromptKey


def _object_at(schema: JsonObject, *path: str) -> JsonObject:
    for key in path:
        child = schema[key]
        if not isinstance(child, dict):
            raise ValueError(f"schema path {path!r} is not an object at {key!r}")
        schema = child
    return schema


def lint_catalog(catalog: PromptCatalog, cfg: Settings, predicates: PredicateTable) -> list[str]:
    errors: list[str] = []
    sources = catalog.sources()
    joined = "\n".join(sources.values())

    if "⟦你⟧" in joined:
        errors.append("legacy self marker ⟦你⟧ remains in the prompt bundle")
    if "⟦0⟧" not in catalog.source(PromptKey.SHARED_LEGEND):
        errors.append("shared_legend does not define the reserved bot number ⟦0⟧")
    if "mface" in joined or "商城表情" in joined:
        errors.append("the model-facing prompt bundle exposes mface")
    if "⟦依据:" in joined or "⟦依据：" in joined:
        errors.append("the prompt bundle trusts the retired permanent evidence marker")

    send = send_def(cfg, prompts=catalog)
    properties = _object_at(send.parameters, "properties")
    if "content" not in properties or "messages" in properties:
        errors.append("send schema must accept one message's content")
    segment_types = set(
        _object_at(properties, "content", "items", "discriminator", "mapping")
    )
    if "mface" in segment_types:
        errors.append("the model-facing send schema exposes mface")
    expected_segments = {
        "text",
        "at",
        "reply",
        "face",
        "dice",
        "rps",
        "contact_member",
        "contact_group",
    }
    if segment_types != expected_segments:
        errors.append(
            f"send segment schema differs from the closed supported set: {sorted(segment_types)}"
        )

    rendered_send = catalog.render(
        PromptKey.TOOL_SEND_MESSAGE,
        face_catalog="、".join(f"{face_id}={label}" for face_id, label in FACE_NAMES.items()),
        message_limit=str(cfg.conversation.max_messages_per_reply),
    )
    if "{{face_catalog}}" in rendered_send or "{{message_limit}}" in rendered_send:
        errors.append("tool_send_message left a dynamic slot unresolved")
    hidden_segments = ("music", "music_custom", "json")
    exposed_hidden = [name for name in hidden_segments if name in rendered_send]
    if exposed_hidden:
        errors.append(
            "tool_send_message exposes hidden historical segment type(s): "
            + ", ".join(exposed_hidden)
        )
    standalone_segments = ("dice", "rps", "contact_member", "contact_group")
    if not all(name in rendered_send for name in standalone_segments):
        errors.append("tool_send_message omits a parameter-only segment type")
    if not any(word in rendered_send for word in ("单独", "独占", "唯一消息段")):
        errors.append("tool_send_message does not state the standalone segment rule")
    if not all(f"{face_id}={label}" in rendered_send for face_id, label in FACE_NAMES.items()):
        errors.append("tool_send_message does not expose the complete fixed face catalog")

    reply_names = {tool.name for tool in tool_defs(cfg, prompts=catalog)}
    if reply_names != {
        "send_message",
        "finish_reply",
        "web_search",
        "search_history",
        "recall_events",
        "read_url",
        "open_images",
        "schedule_task",
        "list_scheduled_tasks",
        "cancel_scheduled_task",
    }:
        errors.append(f"unexpected reply tool set: {sorted(reply_names)}")
    extract_names = {tool.name for tool in extraction_tools(predicates)}
    if extract_names != {
        "record_alias",
        "record_fact",
        "record_group_term",
        "record_group_topic",
        "record_episode",
    }:
        errors.append(f"unexpected extraction tool set: {sorted(extract_names)}")

    ids = [case.case_id for case in CASES]
    if len(ids) != len(set(ids)):
        errors.append("fictional prompt case ids are not unique")
    if not any("⟦0⟧" in case.input for case in CASES):
        errors.append("fictional cases do not exercise the reserved bot number")
    return errors
