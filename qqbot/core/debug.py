"""Short-lived, owner-armed capture of provider-neutral model rounds.

Provider replay state and reasoning never cross the session boundary, so this module can
persist a useful diagnostic projection without filtering vendor-specific wire items.
"""

from __future__ import annotations

import json
import logging
import os
from dataclasses import asdict
from datetime import datetime
from pathlib import Path

from ..providers.contracts import ModelTurn, PromptItem

log = logging.getLogger("qqbot.debug")

_left = 0
MAX_ROUNDS = 50


def arm(n: int) -> int:
    global _left
    _left = max(0, min(int(n), MAX_ROUNDS))
    return _left


def armed() -> int:
    return _left


def capture(
    group_id: str,
    round_no: int,
    prompt: tuple[PromptItem, ...],
    turn: ModelTurn,
) -> None:
    """Write one neutral round if armed; diagnostics must never break a reply."""
    global _left
    if _left <= 0:
        return
    _left -= 1
    try:
        out = Path(os.getenv("LOG_DIR", "/app/logs")) / "debug"
        out.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S-%f")
        payload = {
            "group": group_id,
            "round": round_no,
            "prompt": [asdict(item) for item in prompt],
            "turn": asdict(turn),
        }
        (out / f"reply-{group_id}-{stamp}-r{round_no}.json").write_text(
            json.dumps(payload, ensure_ascii=False, indent=1, default=str),
            encoding="utf-8",
        )
    except Exception:
        log.exception("debug capture failed")
