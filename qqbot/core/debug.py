"""Full prompt/response capture, armed by the owner for the next N model rounds.

The trajectory table keeps digests - enough to know *that* the model searched,
useless for seeing what the model was actually shown when it misbehaves. This is
the replay tap: /debug N writes the exact request messages and the raw result of
each of the next N model rounds to LOG_DIR/debug/, one JSON file per round, then
disarms itself. Diagnosis becomes reading a file instead of reconstructing a
prompt from memory (the assistant-line marker leak took hours of inference that
one dump would have settled in minutes).

The counter is in-memory: a restart disarms the tap, which is the safe direction
for a debugging aid - it can never be left running by accident across weeks.
"""

from __future__ import annotations

import json
import logging
import os
from datetime import datetime
from pathlib import Path

log = logging.getLogger("qqbot.debug")

#: Rounds still to capture. Module-level because there is exactly one bot.
_left = 0

#: The most anyone can arm at once - a tap, not a firehose.
MAX_ROUNDS = 50


def arm(n: int) -> int:
    """Set the tap to capture the next n rounds (0 disarms). Returns what took."""
    global _left
    _left = max(0, min(int(n), MAX_ROUNDS))
    return _left


def armed() -> int:
    return _left


def capture(group_id: str, round_no: int, messages: list, result: object) -> None:
    """Write one model round if the tap is armed. Never raises - a debugging aid
    must not take the reply down with it."""
    global _left
    if _left <= 0:
        return
    _left -= 1
    try:
        out = Path(os.getenv("LOG_DIR", "/app/logs")) / "debug"
        out.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S-%f")
        (out / f"reply-{group_id}-{stamp}-r{round_no}.json").write_text(
            json.dumps({
                "group": group_id,
                "round": round_no,
                "messages": messages,
                "text": getattr(result, "text", None),
                "tool_calls": getattr(result, "tool_calls", None),
                "model": getattr(result, "model", None),
            }, ensure_ascii=False, indent=1, default=str),
            encoding="utf-8",
        )
    except Exception:
        log.exception("debug capture failed")
