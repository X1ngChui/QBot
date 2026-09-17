"""Deterministic prompt-bundle validation with no model or database calls."""

from __future__ import annotations

from ..core.segments import FACE_NAMES
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

    send = send_def()
    segment_schemas = send.parameters["properties"]["content"]["items"]["anyOf"]
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
        "music",
        "music_custom",
        "json",
    }
    if segment_types != expected_segments:
        errors.append(
            "send segment schema differs from the closed supported set: "
            f"{sorted(segment_types)}"
        )

    rendered_send = catalog.render(
        PromptKey.TOOL_SEND_MESSAGE,
        face_catalog="、".join(
            f"{face_id}={label}" for face_id, label in FACE_NAMES.items()
        ),
    )
    if "{{face_catalog}}" in rendered_send:
        errors.append("tool_send_message left face_catalog unresolved")
    if not all(f"{face_id}={label}" in rendered_send for face_id, label in FACE_NAMES.items()):
        errors.append("tool_send_message does not expose the complete fixed face catalog")

    reply_names = {tool.name for tool in tool_defs(cfg)}
    if reply_names != {
        "send_message",
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
