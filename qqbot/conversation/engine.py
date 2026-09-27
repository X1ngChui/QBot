"""The conversation engine: retrieve, assemble, send, observe, then continue.

Each model send is one QQ message. GroupDelivery only locks the platform send; an
independent own-event observation completes before another model round can start.
"""

from __future__ import annotations

import asyncpg
from collections.abc import Callable

import asyncio
import logging
import re
from datetime import timedelta
from zoneinfo import ZoneInfo
from urllib.parse import urlsplit, urlunsplit

from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.evidence import EvidenceRepository
from qqbot.repositories.archive import ArchiveRepository
from qqbot.domain.evidence import EvidenceItem
from qqbot.domain.evidence import EvidenceMemo
from qqbot.domain.evidence import EvidenceOutcome
from qqbot.domain.evidence import EvidenceSource
from qqbot.delivery.observation import ECHO_TIMEOUT_SEC
from qqbot.conversation.snapshot import PromptSnapshot
from qqbot.conversation.limits import EVIDENCE_LIMITS
from qqbot.domain.reply import ReplyEnd
from qqbot.domain.reply import ReplyOutcome
from qqbot.domain.reply import ReplyProgress
from qqbot.providers.base import Providers
from qqbot.repositories.scheduled_task import ScheduledTask
from qqbot.services import Directory
from qqbot.configuration import Persona
from qqbot.services.budget import Budget, BudgetExceeded, BudgetUnavailable
from qqbot.clock import Clock
from qqbot.prompting import PromptCatalog
from qqbot.configuration import Settings
from qqbot.util import SYS_L
from qqbot.util import SYS_R
from qqbot.util import defang
from qqbot.util import sysmark
from qqbot.util import why
from qqbot.conversation import agent
from qqbot.conversation import prompt
from qqbot.services import retrieval
from qqbot.conversation import tools
from qqbot.gateway.botapi import BotApi
from qqbot.delivery.service import MessageDelivery
from qqbot.media.service import MediaProcessor
from qqbot.conversation.member_numbers import MemberNumbers
from qqbot.services.members import MemberDirectory
from qqbot.delivery.segments import AtSegment
from qqbot.delivery.segments import DiceSegment
from qqbot.delivery.segments import RpsSegment
from qqbot.delivery.segments import SendSegment
from qqbot.delivery.segments import TextSegment
from qqbot.delivery.segments import display_text
from qqbot.delivery.output import clean_reply
from qqbot.gateway.segments import parse_segments
from qqbot.conversation.state import ChatMsg
from qqbot.conversation.state import GroupState

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
    executed: tuple[agent.ToolExecution, ...], cfg: Settings, clock: Clock
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
        request = defang(" ".join(request.split()))[: EVIDENCE_LIMITS.evidence_request_chars]
        digest = defang(" ".join(_MEMBER_NO.sub("", execution.output or "").split()))
        digest = digest[: EVIDENCE_LIMITS.evidence_result_chars]
        size = len(request) + len(digest)
        if used + size > EVIDENCE_LIMITS.evidence_total_chars:
            break
        items.append(
            EvidenceItem(
                source=source,
                request=request,
                outcome=(
                    EvidenceOutcome.VERIFIED if execution.verified else EvidenceOutcome.UNCONFIRMED
                ),
                digest=digest,
            )
        )
        used += size
    if not items:
        return None
    created_at = clock.now()
    return EvidenceMemo(
        items=tuple(items),
        created_at=created_at,
        expires_at=created_at + timedelta(days=cfg.conversation.evidence_ttl_days),
    )


async def generate(
    *,
    bot: BotApi,
    st: GroupState,
    cfg: Settings,
    clock: Clock,
    prompts: PromptCatalog,
    database: Callable[[], asyncpg.Pool],
    identities: IdentityRepository,
    evidence_store: EvidenceRepository,
    archive: ArchiveRepository,
    budget: Budget,
    members: MemberDirectory,
    persona: Persona,
    msg: ChatMsg | None,
    providers: Providers,
    media: MediaProcessor,
    directory: Directory,
    delivery: MessageDelivery,
    progress: ReplyProgress,
    window: list[ChatMsg] | None = None,
    scheduled: ScheduledTask | None = None,
) -> agent.AgentOutcome:
    """Run one private model session while recording observable side effects."""
    # Names in history were captured when each message arrived. Re-read them so a rename
    # does not leave the same person appearing under two names across the prompt.
    await members.relabel(bot, st.group_id, list(st.recent))

    profiles = await retrieval.gather(
        members=members,
        group_id=st.group_id,
        directory=directory,
        bot=bot,
    )
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
    if scheduled is None:
        if msg is None:
            raise ValueError("a reply needs a received message or a scheduled task")
        initiator = msg.user_id
    else:
        initiator = scheduled.creator_id
    if window is None:
        window = prompt.history_window(st, msg)
    shown = window + ([msg] if msg is not None else [])
    initial_cursor = (
        msg.arrival_seq
        if msg is not None and msg.arrival_seq > 0
        else max((m.arrival_seq for m in shown), default=st.arrival_seq)
    )
    seen_messages = {
        m.raw_event_id or m.msg_id for m in st.recent if m.arrival_seq <= initial_cursor
    }
    seen_messages.update(m.raw_event_id or m.msg_id for m in shown)
    snapshot = PromptSnapshot.capture(window, msg, cursor=initial_cursor)
    frozen_window, frozen_msg = snapshot.history, snapshot.current
    shown = snapshot.shown
    nums, marks = snapshot.numbers, snapshot.quotes
    pics, by_pic = snapshot.image_numbers, dict(snapshot.pictures)
    lines = {n: m for m in shown if (n := nums.get(m.msg_id))}

    people = MemberNumbers(self_id=str(bot.self_id), lookup=identities.holder_ids_for_accounts)
    prompt.teach_roster(people, profiles)
    await people.learn(
        [m.user_id for m in shown]
        + [a for m in frozen_window if m.is_bot for a, _ in m.at]
        + [a for m in shown if not m.is_bot for a, _ in m.mentions]
        + ([scheduled.creator_id] if scheduled is not None else [])
    )
    prompt.number_people(people, profiles, frozen_window, frozen_msg)

    registry = tools.tool_registry(cfg, prompts=prompts)
    ctx = tools.ToolCtx(
        registry=registry,
        clock=clock,
        database=database,
        identities=identities,
        providers=providers,
        media=media,
        bot=bot,
        initiator=initiator,
        parent_task=scheduled,
        by_pic=by_pic,
        people=people,
    )
    evidence = await evidence_store.evidence_for(
        st.group_id, [m.msg_id for m in frozen_window if m.is_bot]
    )
    messages = prompt.assemble(
        clock=clock,
        prompts=prompts,
        persona=persona,
        cfg=cfg,
        st=st,
        msg=frozen_msg,
        profiles=profiles,
        task_intent=scheduled.intent if scheduled is not None else None,
        task_initiator=(
            "原发起人" + sysmark(str(people.number(scheduled.creator_id)))
            if scheduled is not None
            else None
        ),
        group_facts=await retrieval.group_knowledge(st.group_id, database=database, clock=clock),
        evidence=evidence,
        window=frozen_window,
        nums=nums,
        marks=marks,
        pics=pics,
        people=people,
    )
    names = {str(m.user_id): m.nickname for m in shown if not m.is_bot}
    names.update({str(a): n for m in frozen_window if m.is_bot for a, n in m.at if n})
    first_evidence = True

    async def send_one(
        draft: agent.MessageDraft, executed: tuple[agent.ToolExecution, ...]
    ) -> agent.SendResult:
        nonlocal first_evidence
        accounts = [str(account) for account in draft.at]
        live = await members.names_of(bot, st.group_id, accounts) if accounts else {}
        named = {account: live.get(account) or names.get(account) or "成员" for account in accounts}
        segments = _clean_outbound(
            draft.segments,
            names=named,
            max_text_chars=cfg.conversation.max_text_chars_per_message,
        )
        if not display_text(segments, names=named):
            return agent.SendResult("（清理后消息为空，没有发送。）")
        progress.begin_send()
        delivered = await delivery.deliver_one(bot, group_id=st.group_id, segments=segments)
        if delivered is None:
            progress.sending = False
            progress.uncertain = True
            return agent.SendResult("（发送接口失败，是否已经发出不确定；本轮停止，不要重发。）")
        progress.confirm()
        if delivered.message_id is None:
            return agent.SendResult(
                "（发送响应缺少消息编号，是否已发出不确定；本轮停止，不要重发。）",
                confirmed=True,
            )
        if first_evidence:
            first_evidence = False
            if evidence_memo := _evidence_memo(executed, cfg, clock):
                try:
                    await evidence_store.evidence_add(
                        st.group_id, str(delivered.message_id), evidence_memo
                    )
                except Exception:
                    log.exception("failed to persist reply evidence in group %s", st.group_id)
        echoed = await delivery.echo.wait(
            str(bot.self_id),
            st.group_id,
            delivered.message_id,
            timeout=ECHO_TIMEOUT_SEC,
        )
        if echoed is None:
            return agent.SendResult(
                "（平台确认发送但本机器人未收到并归档自己的消息；本轮停止，不要重发。）",
                confirmed=True,
            )
        progress.observe()
        shown_text = echoed.text
        if (
            segments
            and isinstance(segments[0], DiceSegment | RpsSegment)
            and shown_text in {sysmark("骰子"), sysmark("猜拳")}
        ):
            try:
                fetched = await asyncio.wait_for(
                    bot.call_api("get_msg", message_id=int(delivered.message_id)), timeout=2.0
                )
                if (
                    isinstance(fetched, dict)
                    and str(fetched.get("message_id")) == str(delivered.message_id)
                    and str(fetched.get("group_id")) == str(st.group_id)
                    and str(fetched.get("user_id")) == str(bot.self_id)
                ):
                    kind = "dice" if isinstance(segments[0], DiceSegment) else "rps"
                    for item in fetched.get("message", ()):
                        if isinstance(item, dict) and item.get("type") == kind:
                            result = (item.get("data") or {}).get("result")
                            if (
                                result is not None
                                and str(result).isdigit()
                                and 1 <= int(result) <= (6 if kind == "dice" else 3)
                            ):
                                actual = parse_segments(
                                    [item],
                                    str(bot.self_id),
                                    display_zone=ZoneInfo(cfg.bot.timezone),
                                    self_name=persona.name,
                                ).render()
                                echoed.text = actual
                                try:
                                    await archive.backfill_plain_text(
                                        str(delivered.message_id), actual
                                    )
                                except Exception:
                                    log.exception(
                                        "failed to persist observed random result in group %s",
                                        st.group_id,
                                    )
                                shown_text = f"{actual}（平台按消息编号查询到结果：{result}）"
                                break
            except Exception as exc:
                log.info("group %s: result lookup unavailable: %s", st.group_id, why(exc))
        if (
            segments
            and isinstance(segments[0], DiceSegment | RpsSegment)
            and shown_text in {sysmark("骰子"), sysmark("猜拳")}
        ):
            return agent.SendResult(
                "（随机消息已经确认并归档，但结果暂不可核实；本轮停止，不要猜点数或重发。）",
                confirmed=True,
            )
        return agent.SendResult(f"已确认发送并收到自身回显；平台显示：{shown_text}", echoed, True)

    run = agent.AgentRun(
        registry=registry,
        budget=budget,
        model=providers.text,
        request=agent.request_for_reply(messages, cfg, group_id=st.group_id, registry=registry),
        cfg=cfg,
        state=st,
        tool_context=ctx,
        people=people,
        lines=lines,
        on_send=send_one,
        seen_messages=seen_messages,
        initial_cursor=snapshot.cursor,
    )
    return await run.run()


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
    segments: tuple[SendSegment, ...],
    *,
    names: dict[str, str],
    max_text_chars: int,
) -> tuple[SendSegment, ...]:
    """Clean and bound text parts without changing control-segment order."""

    out: list[SendSegment] = []
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


async def respond(
    *,
    bot: BotApi,
    st: GroupState,
    cfg: Settings,
    clock: Clock,
    prompts: PromptCatalog,
    database: Callable[[], asyncpg.Pool],
    identities: IdentityRepository,
    evidence_store: EvidenceRepository,
    archive: ArchiveRepository,
    budget: Budget,
    members: MemberDirectory,
    persona: Persona,
    msg: ChatMsg | None,
    providers: Providers,
    media: MediaProcessor,
    delivery: MessageDelivery,
    directory: Directory,
    window: list[ChatMsg] | None = None,
    scheduled: ScheduledTask | None = None,
    progress: ReplyProgress | None = None,
    deadline: float | None = None,
) -> ReplyOutcome:
    """Bound a model session without forgetting sends when it later fails."""
    progress = progress if progress is not None else ReplyProgress()
    if deadline is None:
        deadline = asyncio.get_running_loop().time() + cfg.conversation.reply_deadline_sec
    try:
        async with asyncio.timeout_at(deadline):
            await generate(
                clock=clock,
                prompts=prompts,
                database=database,
                identities=identities,
                evidence_store=evidence_store,
                archive=archive,
                members=members,
                budget=budget,
                bot=bot,
                st=st,
                cfg=cfg,
                persona=persona,
                msg=msg,
                providers=providers,
                media=media,
                directory=directory,
                delivery=delivery,
                progress=progress,
                window=window,
                scheduled=scheduled,
            )
    except (BudgetExceeded, BudgetUnavailable):
        return progress.finish(ReplyEnd.BUDGET)
    except Exception as e:
        log.warning(
            "group %s: generation failed: %s",
            st.group_id,
            why(e),
            exc_info=not _expected(e),
        )
        return progress.finish(ReplyEnd.TIMEOUT if isinstance(e, TimeoutError) else ReplyEnd.FAILED)
    return progress.finish(ReplyEnd.FINISHED)


def _expected(e: BaseException) -> bool:
    """Whether a generation failure is the kind a log line explains on its own:
    the provider or the network said no. Matched by name so this module stays
    importable without the SDK's error classes at hand."""
    names = {c.__name__ for c in type(e).__mro__}
    return bool(names & {"APIError", "HTTPError", "TimeoutError", "QuotaExhausted", "OSError"})
