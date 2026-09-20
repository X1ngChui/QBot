"""The conversation engine: retrieve -> assemble -> call -> strip -> send.

There is no provider fallback and no downgrade, so any failure means the bot says
nothing.

A reply is one terminal call to the send tool (tools.SEND). The call carries an ordered
batch of independent QQ messages; each message holds the text, whom to @ and which line to
reply to as closed segments. The call is the only way out: a round that ends in bare text
sends nothing, and the reply ends there.

Money normally bounds the tool loop; a high round-count tripwire exists only for a
backend that bills zero. Free tools themselves cost nothing, but the model rounds carrying
them are paid and booked into the scope around this reply. Once that scope is spent,
searching stops and one wrap-up round, offered only the send tool, answers from what was
already fetched: a mid-reply limit ends the spending, not the speech. Only the daily cap,
checked before anything is spent, means silence.
"""

from __future__ import annotations

import logging
import re
from dataclasses import dataclass
from datetime import timedelta
from urllib.parse import urlsplit, urlunsplit

from ..db import repo
from ..domain.evidence import EvidenceItem, EvidenceMemo, EvidenceOutcome, EvidenceSource
from ..providers import providers
from ..settings import Persona, Settings
from ..util import SYS_L, SYS_R, defang, now_local, why
from . import agent, prompt, retrieval, tools
from .botapi import BotApi
from .member_numbers import MemberNumbers
from .members import MEMBERS
from .outbound import (
    AtSegment,
    OutboundSegment,
    TextSegment,
    display_text,
    reply_target,
    to_onebot,
    without_replies,
)
from .output import clean_reply
from .state import ChatMsg, GroupState

log = logging.getLogger("qqbot.engine")

TOOL_SOURCE = {
    "web_search": EvidenceSource.WEB_SEARCH,
    "search_history": EvidenceSource.HISTORY,
    "recall_events": EvidenceSource.EVENTS,
    "read_url": EvidenceSource.PAGE,
    "open_images": EvidenceSource.IMAGE,
}

#: A member number as search results render it behind a name. Removed before evidence
#: persistence because the number belongs only to the prompt snapshot that assigned it.
_MEMBER_NO = re.compile(rf"{re.escape(SYS_L)}\d{{1,9}}{re.escape(SYS_R)}")


Reply = agent.ReplyDraft


def _safe_url_summary(value: object) -> str:
    """A page reference without credentials, query parameters or fragments."""

    try:
        parsed = urlsplit(str(value or ""))
        host = parsed.hostname or ""
        if parsed.port is not None:
            host += f":{parsed.port}"
        return urlunsplit((parsed.scheme, host, parsed.path, "", ""))
    except ValueError:
        return ""


def _evidence_memo(
    executed: tuple[agent.ToolExecution, ...], cfg: Settings
) -> EvidenceMemo | None:
    """Build the bounded durable evidence for one reply's retrieval work."""

    items: list[EvidenceItem] = []
    used = 0
    for execution in executed:
        source = TOOL_SOURCE.get(execution.name)
        if source is None:
            continue
        args = execution.arguments
        if source is EvidenceSource.PAGE:
            request = _safe_url_summary(args.get("url"))
        elif source is EvidenceSource.IMAGE:
            request = ""
        else:
            request = str(args.get("query") or args.get("question") or "")
        request = defang(" ".join(request.split()))[
            : cfg.prompt.evidence_request_chars
        ]
        digest = defang(" ".join(_MEMBER_NO.sub("", execution.output or "").split()))
        digest = digest[: cfg.prompt.evidence_result_chars]
        size = len(request) + len(digest)
        if used + size > cfg.prompt.evidence_total_chars:
            break
        items.append(
            EvidenceItem(
                source=source,
                request=request,
                outcome=(
                    EvidenceOutcome.VERIFIED
                    if execution.verified
                    else EvidenceOutcome.UNCONFIRMED
                ),
                digest=digest,
            )
        )
        used += size
    if not items:
        return None
    created_at = now_local()
    return EvidenceMemo(
        items=tuple(items),
        created_at=created_at,
        expires_at=created_at + timedelta(days=cfg.prompt.evidence_ttl_days),
    )


async def generate(
    *,
    bot: BotApi,
    st: GroupState,
    cfg: Settings,
    persona: Persona,
    msg: ChatMsg,
    window: list[ChatMsg] | None = None,
) -> Reply | None:
    """The reply to `msg`, as the model asked for it to be sent; None for silence."""
    # Names in history were captured when each message arrived. Re-read them so a rename
    # does not leave the same person appearing under two names across the prompt.
    await MEMBERS.relabel(bot, st.group_id, list(st.recent))

    profiles = await retrieval.gather(group_id=st.group_id, bot=bot)
    # Episodic memory is not pushed here: what is injected uninvited sits right next
    # to the incoming message, and an elliptical question resolves against it instead
    # of against the conversation. The model pulls with recall_events instead.
    #
    # Window and numbering are computed once and handed to both the tool context and
    # assemble: every number the model writes back - a picture to open, a member to
    # @, a line to reply to - resolves against the maps of the very render it read.
    # The caller normally passes the window in, cut when the message arrived, so
    # later arrivals cannot shift what this reply is looking at. Evidence is fetched by
    # reply id and rendered only for replies still inside this frozen window.
    if window is None:
        window = prompt.history_window(st, msg, cfg)
    shown = window + [msg]
    nums, marks = prompt.numbered(shown)
    pics, by_pic = prompt.numbered_images(shown)
    lines = {n: m for m in shown if (n := nums.get(m.msg_id))}

    people = MemberNumbers(self_id=str(bot.self_id))
    prompt.teach_roster(people, profiles)
    await people.learn(
        [m.user_id for m in shown]
        + [a for m in window if m.is_bot for a, _ in m.at]
        + [a for m in shown if not m.is_bot for a, _ in m.mentions]
    )
    prompt.number_people(people, profiles, window, msg)

    ctx = tools.ToolCtx(bot=bot, by_pic=by_pic, people=people)
    evidence = await repo.evidence_for(
        int(st.group_id), [m.msg_id for m in window if m.is_bot]
    )
    messages = prompt.assemble(
        persona=persona,
        cfg=cfg,
        st=st,
        msg=msg,
        profiles=profiles,
        group_facts=await retrieval.group_knowledge(st.group_id),
        evidence=evidence,
        window=window,
        nums=nums,
        marks=marks,
        pics=pics,
        people=people,
    )
    names = {m.user_id: m.nickname for m in shown if not m.is_bot}
    names.update({a: n for m in window if m.is_bot for a, n in m.at if n})

    run = agent.AgentRun(
        model=providers().text,
        request=agent.request_for_reply(messages, cfg, group_id=st.group_id),
        cfg=cfg,
        state=st,
        tool_context=ctx,
        people=people,
        lines=lines,
    )
    outcome = await run.run()
    reply = outcome.reply
    if reply is None:
        return None
    reply.evidence = _evidence_memo(outcome.executed, cfg)
    reply.names = {account: names[account] for account in reply.at if account in names}
    return reply


def _strip_addresses(text: str, names: list[str]) -> str:
    """The body without the @s the send adds itself.

    The group reads "@name text": the at segments carry the addresses, so a model
    that also opened its text with them would send each one twice. Only whole
    names of the accounts being @-ed, at the very start: an address of a longer
    name that merely starts with one must survive, as must anyone else's.
    """
    forms = sorted({n for n in names if n}, key=len, reverse=True)
    while forms:
        for form in forms:
            m = re.match(rf"@{re.escape(form)}(?=$|\s|[:：,，@])[\s:：,，]*", text)
            if m:
                text = text[m.end() :]
                break
        else:
            break
    return text


def _clean_outbound(
    segments: tuple[OutboundSegment, ...],
    *,
    names: dict[str, str],
    max_text_chars: int,
) -> tuple[OutboundSegment, ...]:
    """Clean and bound text parts without changing control-segment order."""

    out: list[OutboundSegment] = []
    pending_addresses: list[str] = []
    remaining = max_text_chars
    for segment in segments:
        if isinstance(segment, AtSegment):
            out.append(segment)
            pending_addresses.append(names.get(segment.account) or "")
            continue
        if isinstance(segment, TextSegment):
            text = clean_reply(segment.text)
            if pending_addresses:
                text = _strip_addresses(text, pending_addresses)
                if text and text[0] not in "，。！？、；：,.!?;:\n":
                    text = " " + text
            pending_addresses = []
            if text and remaining > 0:
                text = text[:remaining]
                remaining -= len(text)
                out.append(TextSegment(text))
            continue
        pending_addresses = []
        out.append(segment)
    return tuple(out)


@dataclass(frozen=True, slots=True)
class _DeliveredMessage:
    msg_id: str
    segments: tuple[OutboundSegment, ...]
    reply_to: str


async def _deliver_one(
    bot: BotApi,
    *,
    group_id: str,
    segments: tuple[OutboundSegment, ...],
) -> _DeliveredMessage | None:
    """Deliver one message, retrying a rejected reply segment once."""

    reply_to = reply_target(segments) or ""
    sent_segments = segments
    try:
        sent = await bot.send_group_msg(
            group_id=int(group_id),
            message=[to_onebot(segment) for segment in segments],
        )
    except Exception as exc:
        # A quoted message can be recalled between generation and delivery. Retry only
        # a known protocol refusal and preserve every non-reply segment in order.
        if not reply_to or type(exc).__name__ != "ActionFailed":
            log.warning("group %s: send failed: %s", group_id, why(exc))
            return None
        log.warning(
            "group %s: send with a reply segment failed (%s), retrying without it",
            group_id,
            why(exc),
        )
        reply_to = ""
        sent_segments = without_replies(segments)
        try:
            sent = await bot.send_group_msg(
                group_id=int(group_id),
                message=[to_onebot(segment) for segment in sent_segments],
            )
        except Exception as retry_exc:
            log.warning("group %s: send failed: %s", group_id, why(retry_exc))
            return None

    return _DeliveredMessage(
        msg_id=str((sent or {}).get("message_id") or ""),
        segments=sent_segments,
        reply_to=reply_to,
    )


async def respond(
    *,
    bot: BotApi,
    st: GroupState,
    cfg: Settings,
    persona: Persona,
    msg: ChatMsg,
    window: list[ChatMsg] | None = None,
) -> bool:
    """Generate and deliver one terminal reply batch."""
    try:
        reply = await generate(bot=bot, st=st, cfg=cfg, persona=persona, msg=msg, window=window)
    except Exception as e:
        # A provider or transport failure is a one-line warning; anything else is
        # a fault in this code, and the traceback is the only way to find it.
        log.warning(
            "group %s: generation failed, staying silent: %s",
            st.group_id,
            why(e),
            exc_info=not _expected(e),
        )
        return False

    if reply is None:
        return False  # generate logged why
    accounts = reply.at
    live = await MEMBERS.names_of(bot, st.group_id, accounts) if accounts else {}
    names = {
        account: live.get(account) or reply.names.get(account) or "成员"
        for account in accounts
    }
    messages = tuple(
        _clean_outbound(
            message.segments,
            names=names,
            max_text_chars=cfg.tools.send_messages.max_text_chars_per_message,
        )
        for message in reply.messages
    )
    if any(not display_text(segments, names=names) for segments in messages):
        log.warning(
            "group %s: reply batch contained an empty message after cleaning",
            st.group_id,
        )
        return False

    delivered_count = 0
    evidence_msg_id = ""
    async with st.delivery_lock:
        for index, segments in enumerate(messages):
            delivered = await _deliver_one(
                bot,
                group_id=st.group_id,
                segments=segments,
            )
            if delivered is None:
                log.warning(
                    "group %s: reply batch stopped after %d/%d messages",
                    st.group_id,
                    delivered_count,
                    len(messages),
                )
                break

            # Delivery acknowledgement and self-observation are independent. The
            # reported event will add the canonical platform message to the window
            # and archive; the send path retains only the ID needed by reply evidence.
            if delivered_count == 0:
                evidence_msg_id = delivered.msg_id
                if not evidence_msg_id:
                    log.warning(
                        "group %s: first delivered message returned no message id; "
                        "reply evidence cannot be attached",
                        st.group_id,
                    )
            delivered_count += 1
            at_count = sum(
                isinstance(segment, AtSegment) for segment in delivered.segments
            )
            log.info(
                "group %s: delivered %d/%d (%d text chars, %d segments, %d @, %s)",
                st.group_id,
                index + 1,
                len(messages),
                sum(
                    len(segment.text)
                    for segment in delivered.segments
                    if isinstance(segment, TextSegment)
                ),
                len(delivered.segments),
                at_count,
                "replying" if delivered.reply_to else "not replying",
            )

    if delivered_count and reply.evidence is not None and evidence_msg_id:
        try:
            await repo.evidence_add(int(st.group_id), evidence_msg_id, reply.evidence)
        except Exception:
            log.exception("failed to persist reply evidence in group %s", st.group_id)

    return delivered_count > 0


def _expected(e: BaseException) -> bool:
    """Whether a generation failure is the kind a log line explains on its own:
    the provider or the network said no. Matched by name so this module stays
    importable without the SDK's error classes at hand."""
    names = {c.__name__ for c in type(e).__mro__}
    return bool(names & {"APIError", "HTTPError", "TimeoutError", "QuotaExhausted", "OSError"})
