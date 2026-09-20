"""Deterministic prompt-bundle validation with no model or database calls."""

from __future__ import annotations

from ..core.segments import FACE_NAMES, RPS_NAMES
from ..core.tools import send_def, tool_defs
from ..services.memory_extractor import tools as extraction_tools
from ..settings import Settings
from .cases import CASES
from .templates import PromptCatalog, PromptKey


def lint_catalog(catalog: PromptCatalog, cfg: Settings) -> list[str]:
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

    send = send_def(cfg)
    message_schema = send.parameters["properties"]["messages"]
    if (
        message_schema.get("maxItems")
        != cfg.tools.send_messages.max_messages_per_call
    ):
        errors.append("send message batch limit differs from global configuration")
    segment_schemas = (
        message_schema["items"]["properties"]["content"]["items"]["anyOf"]
    )
    segment_types = {
        schema["properties"]["type"]["enum"][0] for schema in segment_schemas
    }
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
            "send segment schema differs from the closed supported set: "
            f"{sorted(segment_types)}"
        )

    rendered_send = catalog.render(
        PromptKey.TOOL_SEND_MESSAGES,
        face_catalog="、".join(
            f"{face_id}={label}" for face_id, label in FACE_NAMES.items()
        ),
        message_limit=str(cfg.tools.send_messages.max_messages_per_call),
        rps_result_map="、".join(
            f"{result}={name}" for result, name in RPS_NAMES.items()
        ),
    )
    if any(
        slot in rendered_send
        for slot in ("{{face_catalog}}", "{{message_limit}}", "{{rps_result_map}}")
    ):
        errors.append("tool_send_messages left a dynamic slot unresolved")
    hidden_segments = ("music", "music_custom", "json")
    exposed_hidden = [name for name in hidden_segments if name in rendered_send]
    if exposed_hidden:
        errors.append(
            "tool_send_messages exposes hidden historical segment type(s): "
            + ", ".join(exposed_hidden)
        )
    standalone_segments = ("dice", "rps", "contact_member", "contact_group")
    if not all(name in rendered_send for name in standalone_segments):
        errors.append("tool_send_messages omits a parameter-only segment type")
    if not any(word in rendered_send for word in ("单独", "独占", "唯一消息段")):
        errors.append("tool_send_messages does not state the standalone segment rule")
    if not all(f"{face_id}={label}" in rendered_send for face_id, label in FACE_NAMES.items()):
        errors.append("tool_send_messages does not expose the complete fixed face catalog")
    if not all(
        f"{result}={name}" in rendered_send for result, name in RPS_NAMES.items()
    ):
        errors.append("tool_send_messages does not expose the complete RPS result map")

    reply_names = {tool.name for tool in tool_defs(cfg)}
    if reply_names != {
        "send_messages",
        "web_search",
        "search_history",
        "recall_events",
        "read_url",
        "open_images",
    }:
        errors.append(f"unexpected reply tool set: {sorted(reply_names)}")
    extract_names = {tool.name for tool in extraction_tools()}
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
