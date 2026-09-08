"""Triggering: whether this message gets an answer, and who asked for it.

The bot speaks when it is spoken to, and not otherwise. Being addressed means an @ or one
of its nicknames as a whole word - both are things somebody typed deliberately, so there
is nothing here to tune and nothing to pay for.

One message, one verdict, at most one reply attempt: every message that addresses the
bot earns its own task, so no ask is ever collapsed away behind somebody else's. Because
being spoken to is the only way a reply happens, every reply has exactly one cause - the
addressed message itself - and this is the moment it is known: the same look that answers
"reply at all?" names the initiator, and the Decision carries them downstream (the reply
quotes their message, the spend is attributed to them) instead of anyone re-deriving it
later.
"""

from __future__ import annotations

from dataclasses import dataclass

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


def decide(msg: ChatMsg, at_bot: bool, *, st: GroupState,
           cfg: Settings) -> Decision:
    """Answer this message, or do not. No model call, no randomness, no state to
    calibrate.

    `at_bot` is whether the message @-ed the bot (the adapter strips the segment,
    so the flag travels beside the text).
    """
    if st.muted:
        return Decision(False, reason="muted")

    if msg.is_bot:
        return Decision(False, reason="not_addressed")
    if at_bot:
        reason = "at"
    elif (hit := nickname.word_hit(msg.text, cfg.trigger.nicknames)) is not None:
        reason = f"nick:{hit}"
    elif msg.reply_to and any(m.msg_id == msg.reply_to and m.is_bot
                              for m in st.recent):
        # Quoting the bot's own line is speech aimed at the bot, even with the
        # auto-@ the reply button adds stripped off by hand. Window-scoped on
        # purpose: the check must stay synchronous (the caller cuts the context
        # slice right after), and a quote of a line already evicted is old
        # enough that answering it uninvited would surprise more than silence.
        reason = "reply_to_bot"
    else:
        return Decision(False, reason="not_addressed")

    return Decision(True, reason=reason,
                    initiator=msg.user_id, initiator_msg_id=msg.msg_id)
