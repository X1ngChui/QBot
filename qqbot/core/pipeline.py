"""The message pipeline (section 2): what happens to a message between arriving and
being answered. (`gateway/` is the protocol layer - OneBot in, typed events out.)

  arrive -> dedup -> ingest -> free media lookups -> trigger
  -> context slice -> reply task (paid media -> reply)

Every addressed message gets exactly one reply task, cut loose at the moment it
arrives with its own slice of the conversation. Tasks run concurrently: each
quotes and @s its own initiator and bills its own asker (task-local budget
attribution), so two people asking at once each get their own answer instead of
the later ask absorbing the earlier one. The order is what keeps the cost down:
everything before the trigger is free, and nothing is paid for until the bot has
decided to answer - which, since it only speaks when spoken to, is settled by a
nickname match.

The archive runs off the hot path in the background, because the text has to reach the
history whether or not anything is answered.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import replace
from datetime import datetime

from ..db import repo
from ..gateway.ingest import ingestor
from ..gateway.onebot import GroupMessage
from ..settings import Settings, config
from ..util import now_local, tz, why
from . import agreement, engine, perms, prompt, trigger
from .botapi import BotApi
from .budget import BUDGET
from .command_catalog import PREFIXES as COMMANDS
from .media import MEDIA
from .segments import ImageRef, ParsedMessage, parse_segments
from .ratelimit import DedupSet
from .state import REGISTRY, ChatMsg

log = logging.getLogger("qqbot.pipeline")

#: How long a reply waits for media (the settle and the backlog). A deliberating
#: describe measures 10-20s; the person is already waiting for an answer about the
#: picture, so waiting beats answering that it could not be seen. Timing out is
#: still safe either way - the work completes and lands for the next turn. Only a
#: replying message waits at all: the media tasks persist their own results, so a
#: message that draws no reply costs nothing here.
MEDIA_WAIT_PAID_SEC = 25.0


class Inbound:
    __slots__ = ("msg", "parsed", "media_task", "archive_task", "heard")

    def __init__(self, msg: ChatMsg, parsed: ParsedMessage, media_task, archive_task=None):
        self.msg = msg
        self.parsed = parsed
        self.media_task = media_task
        self.archive_task = archive_task
        #: The text as it arrived, before any media patch. The patch task mutates
        #: msg in place on its own schedule, so ordering alone cannot keep the
        #: trigger reading what was typed; only a snapshot can.
        self.heard = msg.text


class Gateway:
    def __init__(self) -> None:
        self._dedup: DedupSet | None = None
        #: In-flight reply tasks, one per addressed message. Cancelled at
        #: shutdown: a half-generated reply is money already spent either way,
        #: and holding the restart for a 30s model call helps nobody.
        self._replies: set[asyncio.Task] = set()
        #: Fire-and-forget tasks (archive writes, media patches) still in flight.
        #: Tracked so shutdown can wait them out; every restart is a deploy, and an
        #: archive insert dropped by the closing pool is a message lost for good.
        self._loose: set[asyncio.Task] = set()

    def _track(self, coro) -> asyncio.Task:
        t = asyncio.create_task(coro)
        self._loose.add(t)
        t.add_done_callback(self._loose.discard)
        return t

    def _dedup_set(self) -> DedupSet:
        ttl = config().default.gateway.dedup_ttl_sec
        if self._dedup is None:
            self._dedup = DedupSet(ttl)
        self._dedup.ttl = ttl
        return self._dedup

    async def shutdown(self) -> None:
        """Stop the in-flight reply tasks and wait for them.

        Cancelling alone does not wait: the gather keeps shutdown from returning while a
        task is still inside a query, with the pool about to close underneath it.
        """
        tasks = list(self._replies)
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        # The loose tasks are not cancelled - they are writes that should land -
        # only waited for, briefly: a hung one must not hold the whole shutdown.
        if self._loose:
            await asyncio.wait(self._loose, timeout=5)
        self._replies.clear()

    # -- producer ---------------------------------------------------------
    async def handle(self, bot: BotApi, event) -> None:
        group_id = str(event.group_id)
        user_id = str(event.user_id)
        cfg, _persona = config().for_group(group_id)

        msg_id = str(event.message_id)
        if self._dedup_set().seen(msg_id):
            return
        try:
            st = await REGISTRY.get(group_id)
        except Exception:
            # Marked seen, then failed before doing anything with the message: give
            # the mark back, or the adapter's replay of this event is swallowed and
            # the message is neither archived nor answered.
            self._dedup_set().discard(msg_id)
            raise
        # A blocked account is NOT dropped here: its messages arrive, archive and
        # feed memory like anyone's, so the window stays coherent around them - a
        # hole where a person used to be reads as broken context (the owner's
        # call; the price is that a blocked account still feeds memory). The one
        # thing withheld is the reply, at the dispatch gate.
        segments = [
            {"type": seg.type, "data": dict(seg.data)} for seg in event.get_message()
        ]
        parsed = parse_segments(segments, str(bot.self_id))

        # The adapter pops a leading or trailing @me segment off the message and reports it
        # as event.to_me, so the segments alone cannot tell us we were addressed - the one
        # path that must always answer would never fire. Trust to_me, and put the marker
        # back so the prompt still shows the bot was spoken to.
        if getattr(event, "to_me", False) and not parsed.at_bot:
            parsed.at_bot = True
            parsed.parts.insert(0, "@我")

        # The adapter does the same to a quote: _check_reply resolves the reply segment,
        # moves it to event.reply, and deletes it from the message - so parse_segments
        # never sees one. Trust event.reply the same way to_me is trusted. (Tests cannot
        # cover this: their events are built by hand and skip the adapter's preprocessing.)
        if (reply := getattr(event, "reply", None)) is not None and not parsed.reply_to:
            parsed.reply_to = str(getattr(reply, "message_id", "") or "") or None

        text = parsed.render()
        if text.lstrip().startswith(COMMANDS):
            return  # routed to the command matchers instead
        if not text and not parsed.refs:
            return
        if len(text) > cfg.gateway.max_msg_len:
            text = text[: cfg.gateway.max_msg_len]

        sender = event.sender
        nickname = (getattr(sender, "card", "") or getattr(sender, "nickname", "") or user_id).strip()

        # The event's own timestamp when it carries one, so the line's [MM-dd HH:mm]
        # stamp and the archive's occurred_at agree - late-delivered messages after
        # a NapCat outage would otherwise stamp as "now", and a restart-rebuilt
        # window would re-stamp them differently than the live one did.
        when = getattr(event, "time", 0) or 0
        msg = ChatMsg(
            msg_id=msg_id,
            user_id=user_id,
            nickname=nickname,
            text=text,
            ts=datetime.fromtimestamp(when, tz()) if when else now_local(),
            is_owner=user_id in cfg.owners,
            mentions=list(parsed.mentions),
            reply_to=parsed.reply_to,
            # Kept beyond the describe: pending is unpaid work and gets cleared,
            # but the references stay for the window's lifetime so inspect_image
            # can reopen a picture whose one-line description is already in.
            image_refs=[x for x in parsed.refs if isinstance(x, ImageRef)],
        )

        # Before this message joins the deque: after a restart the window is rebuilt from
        # the archive, so the bot rejoins a conversation knowing what was being discussed
        # rather than starting blind. Only the message path can do this - it is the one
        # place that knows which account is the bot and who its owners are.
        try:
            await st.load_history(self_id=str(bot.self_id), owners=set(cfg.owners))
        except Exception:
            self._dedup_set().discard(msg_id)   # same rule: fail unmarked
            raise

        # Archive unconditionally, off the hot path.
        inbound = GroupMessage.from_event(event, segments, bot.self_id, plain_text=text)
        archive_task = self._track(
            self._archive(inbound, at_accounts=list(parsed.mentions))
        )

        # Free lookups and the pictures run now; only voice waits for a reply. The free
        # half has to happen on arrival because an unresolved mention archives as a bare
        # account number; the pictures happen now because the link is freshest, the
        # upload is free, and the describing call carries its own cache, rate limit and
        # budget gate - see media.resolve for the full schedule.
        media_task = None
        if parsed.refs:
            media_task = self._track(self._resolve_and_patch(
                parsed, msg, bot=bot, group_id=group_id, cfg=cfg,
                archive_task=archive_task,
            ))

        item = Inbound(msg, parsed, media_task, archive_task)
        # Hold on to anything still unresolved and expensive. A picture is usually
        # asked about in the message *after* it - a separate task by then, whose
        # backlog settle is what pays for it - see ChatMsg.pending.
        if parsed.needs_model:
            msg.pending = parsed
        st.add(msg)

        # Decided on the text as it arrived (Inbound.heard), never the resolved
        # form: patched content - a forwarded conversation whose body names the
        # bot, a mention rendering to a card that matches a nickname - can contain
        # the trigger word without anyone having typed it at the bot, and being
        # spoken to means something somebody typed deliberately. A typed nickname
        # is in the arrival text and an @ is a parse-time flag, so no legitimate
        # trigger needs the resolved form. The media tasks keep running and patch
        # the window on their own; the paid settle in the reply task waits for
        # them before the prompt reads the text.
        decision = trigger.decide(replace(msg, text=item.heard), parsed.at_bot,
                                  st=st, cfg=cfg)
        if not decision.reply:
            # Being addressed and still saying nothing is never normal, and silence is the
            # one symptom that looks identical whether the bot chose not to speak or
            # something broke. Not being addressed at all is the ordinary case.
            if parsed.at_bot:
                log.info("group %s: addressed but not replying (%s)", group_id, decision.reason)
            else:
                log.debug("group %s: no reply (%s)", group_id, decision.reason)
            return

        # The context slice is cut here, at arrival, and travels with the task:
        # no await sits between st.add above and this line, so the slice ends
        # exactly at the message being answered, and whatever arrives while the
        # task is generating can neither leak in nor steal the reply's target.
        window = prompt.history_window(st, [msg], cfg)
        task = asyncio.create_task(self._reply(bot, group_id, item, decision, window))
        self._replies.add(task)
        task.add_done_callback(self._replies.discard)

    @staticmethod
    async def _archive(inbound: GroupMessage, *, at_accounts: list[str]) -> None:
        """Hand the message to the inbound chain: L0 write, identity, reference.

        Everything here is free - a write and some lookups - which is why it can run on
        every message rather than only on the ones that draw a reply. The paid step,
        extraction, happens elsewhere entirely: the nightly drain reads what this wrote.

        Failures are logged and swallowed: a message that cannot be archived must still
        reach the reply path, because going mute is a worse failure than forgetting.
        """
        try:
            await ingestor().ingest(inbound, at_accounts=at_accounts)
        except Exception:
            log.exception("failed to archive message %s", inbound.message_id)

    async def _reply(self, bot: BotApi, group_id: str, item: Inbound,
                     decision: trigger.Decision, window: list[ChatMsg]) -> None:
        """One reply attempt for one addressed message, start to finish.

        Tasks run concurrently. Each carries its own context slice (cut at
        arrival), quotes and @s its own initiator, and bills its own asker
        through the budget's task-local attribution - so simultaneous asks
        answer independently, in whatever order the model finishes them.
        """
        cfg, persona = config().for_group(group_id)
        st = await REGISTRY.get(group_id)

        if await BUDGET.exceeded(cfg.budget.daily_cny_cap):
            # After the trigger on purpose: it fires once per suppressed reply, not
            # once per message all afternoon - hundreds of identical warnings would
            # walk the actionable failures out of the ring the daily report reads.
            log.warning("group %s: daily budget cap of %.2f reached, staying quiet "
                        "until the day rolls over", group_id, cfg.budget.daily_cny_cap)
            return

        who = decision.initiator
        # /block withholds exactly the reply. Checked through blocked_now, never
        # `in`: a timed block lapses the moment this check notices it has.
        if who and await st.blocked_now(who):
            log.debug("group %s: no reply, initiator %s is blocked", group_id, who)
            return
        # The consent gate, before anything is paid for: a member who has not
        # accepted the user agreement gets the agreement itself instead of a
        # reply - at most once per cooldown - and nothing is spent on their
        # behalf. Of the commands only /agree answers before consent
        # (commands._gate holds the rest); archiving is untouched, owners are
        # exempt.
        if (who and not perms.is_owner(who, cfg.owners)
                and not await agreement.ok(group_id, who)):
            if agreement.should_prompt(group_id, who):
                try:
                    await bot.send_group_msg(
                        group_id=int(group_id),
                        message=[{"type": "at", "data": {"qq": who}},
                                 {"type": "text",
                                  "data": {"text": " " + agreement.text()}}])
                except Exception as e:
                    log.warning("group %s: agreement prompt failed: %s",
                                group_id, why(e))
            return

        # Every yuan this reply spends - the transcribes and backlog describes it
        # forces, the tool calls, the model tokens - is booked to the initiator
        # the trigger decision already named. An attribution, not a charge: the
        # budget stays shared, this only feeds the /top leaderboard's ledger
        # column.
        try:
            with BUDGET.attribute(who):
                # Only now is anything paid for. Understanding a picture is worth money
                # exactly when the model is about to read the message it is in - which,
                # since the bot only speaks when spoken to, is a question that has
                # already been answered by here.
                await self._settle_media([item], group_id, bot=bot, cfg=cfg, who=who)
                # And whatever is still unread in the history about to be sent: a
                # question about a voice clip refers to the message before it, which
                # was never worth paying for on its own.
                await self._settle_backlog(
                    st, group_id, bot=bot, cfg=cfg, batch=[item.msg], who=who,
                )
                await engine.respond(
                    bot=bot, st=st, cfg=cfg, persona=persona,
                    batch=[item.msg], window=window,
                    reply_to=decision.initiator_msg_id,
                    initiator=decision.initiator,
                )
        except asyncio.CancelledError:
            raise
        except Exception:
            # A bare task has no worker loop above it to log for it.
            log.exception("group %s: reply task failed", group_id)

    async def _settle_backlog(
        self, st, group_id: str, *, bot: BotApi, cfg, batch: list[ChatMsg],
        who: str | None = None,
    ) -> None:
        """Understand the media still unread in the history this reply will be given.

        People post a picture and ask about it in the *next* message, so the thing being
        asked about usually sits in a message that drew no reply and was never paid for.
        Voice more so: asking what a clip said is very nearly the only way a voice clip
        is ever discussed.

        The bound is the prompt's own history window, not a count of its own. A picture is
        worth understanding exactly when the model is about to read the message it is in;
        one that has already fallen out of the window would be paid for and never seen.
        """
        stale = [
            m for m in prompt.history_window(st, batch, cfg)
            if m.pending is not None and not m.is_bot
        ]
        # Each attempt owns its own persistence and survives this wait; `pending`
        # is cleared inside it only when every paid slot settled, so a slow or
        # failed try is simply retried next turn - against warm caches and the
        # in-flight registry, so a retry never starts a second paid call for the
        # same picture. All tasks start together and share one bounded wait:
        # waiting them out one at a time stalled this group's only worker for up
        # to N windows while the queue backed up behind it.
        tasks = [
            self._track(self._resolve_and_patch(
                msg.pending, msg, bot=bot, group_id=group_id, cfg=cfg,
                allow_models=True, note="understood a message from the backlog",
                who=who))
            for msg in stale
        ]
        if tasks:
            await asyncio.wait(tasks, timeout=MEDIA_WAIT_PAID_SEC)

    async def _resolve_and_patch(
        self, pm, msg: ChatMsg, *, bot: BotApi, group_id: str, cfg: Settings,
        allow_models: bool = False, images_now: bool = True,
        archive_task=None, note: str | None = None,
        after: asyncio.Task | None = None, who: str | None = None,
    ) -> None:
        """Resolve one message's segments and persist whatever came back.

        The task owns its result. However long a describing or transcribing call
        takes, its outcome lands on the ChatMsg, in image_cache and in the archive
        the moment it completes - no waiter has to survive long enough to collect
        it, and nobody cancels it. Letting the reply's settle collect and cancel
        these tasks instead would abort paid describing calls mid-flight and leave
        empty cache rows behind - any image slower than the wait window could then
        never be described at all.
        """
        if after is not None:
            # Order the writers of msg.text: the arrival task and this one patch the
            # same message, and unordered, whichever rendered *worse* could finish
            # last and win - then be persisted. Waiting the earlier task out first
            # makes this render the final word. Bounded in practice: both attempts
            # share the describe flight, so the wait is the flight, not a second one.
            await asyncio.wait({after})
        try:
            # `who` is the reply initiator when this resolve was forced by a reply
            # (the paid settle and the backlog); the archival arrival pass leaves it
            # unset and the poster pays for their own picture's description. Either
            # way the attribution covers everything paid inside, the single-flight
            # describe included (create_task copies the context).
            with BUDGET.attribute(who if who is not None else msg.user_id):
                resolved = await MEDIA.resolve(
                    pm, bot=bot, group_id=group_id, cfg=cfg,
                    allow_models=allow_models, images_now=images_now,
                )
        except Exception as e:
            log.warning("group %s: media resolution failed: %s", group_id, why(e))
            return
        _carry_file_ids(pm, msg)
        if allow_models and MEDIA.settled(pm, resolved):
            # The paid content is in (or was refused with a cached verdict); the
            # backlog has nothing left to buy for this message. A transient failure
            # comes back as an Unsettled fallback (or an absent slot) instead, and
            # keeps pending alive so the next turn can try again - clearing it
            # regardless would make the first rate-limited burst permanent, empty
            # markers no money can fix.
            msg.pending = None
        new_text = pm.render(resolved)
        # The arrival path truncated once (gateway.max_msg_len); a patch must not
        # undo it. pm.parts holds the full original text, so an unbounded render
        # would put a 10k-char message back into the window and the archive - past
        # the one per-line bound the no-token-budget prompt layout relies on.
        if len(new_text) > cfg.gateway.max_msg_len:
            new_text = new_text[: cfg.gateway.max_msg_len]
        if new_text and new_text != msg.text:
            msg.text = new_text
            await self._backfill(msg.msg_id, new_text, archive_task)
            if note:
                log.info("group %s: %s", group_id, note)

    async def _settle_media(
        self, batch: list[Inbound], group_id: str, *, bot: BotApi,
        cfg: Settings, who: str | None = None,
    ) -> None:
        """Give this batch's media a bounded moment to land before the prompt is built.

        The tasks persist their own results (_resolve_and_patch), so this only waits:
        a task that finishes in time has already patched the message; one that does
        not keeps running and patches it for the next turn. Nothing is cancelled.
        Runs only on a replying batch - it starts the paid voice work (arrival
        covers pictures) and waits out the free arrival tasks alongside it, so the
        prompt reads the resolved text without a batch that draws no reply ever
        having stalled the worker.
        """
        for item in batch:
            if item.parsed.needs_model and item.msg.pending is not None:
                item.media_task = self._track(self._resolve_and_patch(
                    item.parsed, item.msg, bot=bot, group_id=group_id, cfg=cfg,
                    allow_models=True, archive_task=item.archive_task,
                    after=item.media_task, who=who))
        tasks = [i.media_task for i in batch if i.media_task is not None]
        if not tasks:
            return
        await asyncio.wait(tasks, timeout=MEDIA_WAIT_PAID_SEC)
        for item in batch:
            if item.media_task is not None and item.media_task.done():
                item.media_task = None

    @staticmethod
    async def _backfill(msg_id: str, text: str, archive_task=None) -> None:
        # The row has to exist first. Both start from handle(), and a cached image
        # resolves instantly, so without this the UPDATE can beat the INSERT and quietly
        # match nothing.
        if archive_task is not None:
            try:
                await archive_task
            except Exception:
                pass
        try:
            await repo.backfill_plain_text(msg_id, text)
        except Exception:
            log.exception("failed to backfill plain_text for %s", msg_id)


def _carry_file_ids(pm, msg) -> None:
    """Copy where each picture was filed from the parsed segments onto the ChatMsg.

    The refs live on the parsed message, which is dropped once media settles; the
    ChatMsg is what the window keeps and what the prompt reads. Runs after every
    settle because an id can arrive on either pass, and overwriting with the same
    list is harmless.
    """
    fids = [r.file_id for r in pm.refs if isinstance(r, ImageRef) and r.file_id]
    if fids:
        msg.images = fids


GATEWAY = Gateway()
