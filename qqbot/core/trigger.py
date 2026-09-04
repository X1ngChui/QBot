"""Triggering: whether this batch gets an answer, and who asked for it.

The bot speaks when it is spoken to, and not otherwise. Being addressed means an @ or one
of its nicknames as a whole word - both are things somebody typed deliberately, so there
is nothing here to tune and nothing to pay for.

Because being spoken to is the only way a reply happens, every reply has exactly one
cause, and this is the moment it is known: the same look that answers "reply at all?"
names the initiator, and the Decision carries them downstream (the reply quotes their
message, the spend is attributed to them) instead of anyone re-deriving it later.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Sequence

from ..settings import Settings
from . import nickname
from .state import ChatMsg, GroupState



@dataclass(frozen=True, slots=True)
class Decision:
    reply: bool
    reason: str = ""
    #: The account whose @ or name-call caused this reply; set exactly when reply is.
    initiator: str = ""
    #: The platform id of the message that did the calling, for the reply to quote.
    initiator_msg_id: str = ""


def decide(batch: Sequence[tuple[ChatMsg, bool]], *, st: GroupState,
           cfg: Settings) -> Decision:
    """Answer, or do not. No model call, no randomness, no state to calibrate.

    `batch` pairs each message with whether it @-ed the bot (the adapter strips the
    segment, so the flag travels beside the text). Scanned newest-first: when two
    people call the bot in one batch, the reply - which reply_final points at the
    tail - and its bill go to the later ask.

    The per-minute cap still outranks everything, including being addressed: it is the
    ban-avoidance floor, and a bot that can be made to talk without limit by @-ing it
    repeatedly is a bot that gets the account restricted.
    """
    if st.muted:
        return Decision(False, reason="muted")

    cause, reason = None, ""
    for msg, at_bot in reversed(list(batch)):
        if msg.is_bot:
            continue
        if at_bot:
            cause, reason = msg, "at"
            break
        if (hit := nickname.word_hit(msg.text, cfg.trigger.nicknames)) is not None:
            cause, reason = msg, f"nick:{hit}"
            break
    if cause is None:
        return Decision(False, reason="not_addressed")

    if not st.reply_window.allow(cfg.trigger.max_replies_per_min):
        return Decision(False, reason="rate_limited")

    return Decision(True, reason=reason,
                    initiator=cause.user_id, initiator_msg_id=cause.msg_id)
