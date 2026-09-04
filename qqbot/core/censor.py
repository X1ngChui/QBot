"""The exit guard.

Some members bait the bot into saying words that endanger the account - "repeat
after me", "join these characters", a translation that lands on a risky term.
The threat is not the model's judgement but the platform's: what the account
*sends* is what risk control reads. So the guard sits at the exit, on the exact
text about to leave, and an unsafe verdict means the whole reply is dropped -
silence, the same answer every other limit gives. Nothing happens to the
member who asked: the cost of a bait is one unanswered message.

The judge is cloud moderation (settings.moderation; providers/moderation.py),
the only judge: it models the platform's own risk control, which is precisely
the question being asked. There is deliberately no fallback - a failed or
timed-out call silences the reply too. Wrong silence costs one answer; a wrong
send can cost the account.
"""

from __future__ import annotations

import asyncio
import logging
from collections import Counter

from ..util import why

log = logging.getLogger("qqbot.censor")

#: Replies suppressed since the last restart, per group - the daily report's
#: line, same contract as output.STRIPPED. Every count here is a reply the
#: group never saw.
SUPPRESSED: Counter[str] = Counter()

#: The cloud judge (providers.moderation), or None when the capability is off.
#: Wired at startup and on /reload; never constructed here, because which
#: vendor answers is config's business. Off means no guard at all - a state
#: for test and dev contexts, not for a deployment that configures a backend.
_moderation = None

#: Strong references to in-flight close tasks: asyncio holds tasks weakly, so
#: a bare create_task can be collected mid-close and leak the old judge's
#: connections (same rule media.py's describe registry exists for).
_CLOSING: set[asyncio.Task] = set()


def set_moderation(model) -> None:
    global _moderation
    old, _moderation = _moderation, model
    if old is not None and old is not model:
        # /reload replaces the judge; the old one's connections close in the
        # background. No loop means a test context, where there is nothing open.
        try:
            task = asyncio.get_running_loop().create_task(old.aclose())
        except RuntimeError:
            pass
        else:
            _CLOSING.add(task)
            task.add_done_callback(_CLOSING.discard)
    log.info("cloud moderation %s", f"armed ({model.name})" if model else "off")


async def aclose() -> None:
    """Close the cloud judge's connections, if any. The shutdown hook."""
    if _moderation is not None:
        await _moderation.aclose()


async def screen(text: str, *, group_id: str | None = None) -> tuple[str, str]:
    """The exit verdict for one outgoing reply: (action, detail).

    Every action but "pass" means the same thing - drop the reply whole; the
    labels only say why, for the log: "block" - the vendor is sure; "review" -
    the vendor is unsure; "error" - the judge could not be reached, and
    unjudged text does not leave. With no judge configured everything passes.
    """
    if _moderation is None:
        return "pass", ""
    try:
        v = await _moderation.screen(text, group_id=group_id)
    except Exception as e:
        log.warning("moderation call failed, holding the reply: %s", why(e))
        return "error", why(e)[:80]
    if v.suggestion == "Block":
        return "block", f"{v.label}/{v.score}"
    if v.suggestion == "Review":
        return "review", f"{v.label}/{v.score}"
    return "pass", ""
