"""Shared interpretation of archived group-message payloads.

The archive has three readers with different outputs: window reconstruction, history
search and memory extraction. This module owns only the facts they must agree on; each
reader still owns its own projection.
"""

from __future__ import annotations

import re
from collections.abc import Mapping

from ..domain.archive import AuthorKind
from ..util import SYS_L, SYS_R, display_name
from .segments import at_mentions

_PROVENANCE_TAIL = re.compile(
    rf"\s*{re.escape(SYS_L)}依据[:：][^{re.escape(SYS_R)}]*"
    rf"{re.escape(SYS_R)}\s*$"
)


def payload_of(row: Mapping) -> dict:
    payload = row.get("payload") or {}
    return payload if isinstance(payload, dict) else {}


def archive_author(
    payload: Mapping,
    user_id: str,
    *,
    current_self_id: str = "",
) -> AuthorKind | None:
    """Explicit authorship, then bounded legacy bot hints, else unknown."""

    try:
        return AuthorKind(str(payload.get("author_kind") or ""))
    except ValueError:
        pass
    user_id = str(user_id or "")
    stored_self = str(payload.get("self_id") or "")
    try:
        structured_outbound = int(payload.get("outbound_schema") or 0) > 0
    except (TypeError, ValueError):
        structured_outbound = False
    if user_id and (
        structured_outbound
        or stored_self == user_id
        or (current_self_id and current_self_id == user_id)
    ):
        return AuthorKind.BOT
    return None


def archive_sender(payload: Mapping, fallback: str = "成员") -> str:
    sender = payload.get("sender") or {}
    if not isinstance(sender, Mapping):
        sender = {}
    return display_name(sender.get("card"), sender.get("nickname"), fallback)


def without_legacy_provenance(text: str) -> str:
    """Remove the retired permanent evidence marker from a stored reading."""

    return _PROVENANCE_TAIL.sub("", text or "").strip()


def archive_text(row: Mapping) -> str:
    """Stored reading, with a text-segment fallback for incomplete old rows."""

    if text := without_legacy_provenance(str(row.get("plain_text") or "")):
        return text
    payload = payload_of(row)
    segments = payload.get("segments") or []
    parts = [
        str((segment.get("data") or {}).get("text") or "").strip()
        for segment in segments
        if isinstance(segment, dict) and segment.get("type") == "text"
    ]
    return without_legacy_provenance(" ".join(part for part in parts if part))


def archive_mentions(
    payload: Mapping,
    *,
    self_id: str = "",
    self_name: str = "",
) -> list[tuple[str, str]]:
    return at_mentions(
        payload.get("segments") or [],
        self_id=self_id,
        self_name=self_name,
    )
