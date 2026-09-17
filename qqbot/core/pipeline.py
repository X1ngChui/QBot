"""The message pipeline: what happens to a message between arriving and
being answered. (`gateway/` is the protocol layer - OneBot in, typed events out.)

  arrive -> dedup -> ingest -> free media lookups -> trigger
  -> context slice -> reply task (paid media -> reply)

Every addressed message gets exactly one reply task, cut loose at the moment it
arrives with its own slice of the conversation. Tasks run concurrently: each
answers its own message and bills its own asker (task-local budget attribution), so
two people asking at once each get their own answer instead of the later ask
absorbing the earlier one. The order is what keeps the cost down:
everything before the trigger is free, and nothing is paid for until the bot has
decided to answer - which, since it only speaks when spoken to, is settled by a
nickname match.

The archive runs off the hot path in the background, because the text has to reach the
history whether or not anything is answered.
"""

from __future__ import annotations

import asyncio
import logging
import uuid
from dataclasses import replace
from datetime import datetime

from ..db import repo
from ..gateway.ingest import ingestor
from ..gateway.onebot import GroupMessage, Sender
from ..settings import Settings, config
from ..util import cut_text, display_name, now_local, sysmark, tz, why
from . import agreement, engine, perms, prompt, trigger
from .botapi import BotApi
from .budget import BUDGET
from .command_catalog import PREFIXES as COMMANDS
from .media import MEDIA
from .members import MEMBERS
from .ratelimit import DedupSet
from .segments import ParsedMessage, parse_segments
from .state import REGISTRY, ChatMsg

log = logging.getLogger("qqbot.pipeline")


async def note_console_reply(*, group_id: str | int, self_id: str, text: str,
                             message_id: str = "", reply_to: str = "",
                             name: str = "",
                             addressee: tuple[str, str] | None = None) -> None:
    """A command's answer, entered into the window and the archive like any
    other line the bot speaks.

    Off the record, /who's card or /stats' table would land in the group but reach
    neither the window nor L0, and the next question about it ("what does that note
    mean?") would meet a model that had never seen it - the one speaker in the room
    whose words vanish. Both writes mirror the engine's own send path, the @ of
    the asker included (`addressee` is their account and display name): it is
    what the group read, and what tells a later turn whom the answer was for. A
    missing platform id falls back to a synthetic one, which costs only the
    quote-pointer render if someone replies to that exact message.
    """
    now = now_local()
    mid = message_id or f"cmd-{uuid.uuid4().hex[:12]}"
    at = [addressee] if addressee and addressee[0] else []
    if (st := REGISTRY.loaded(str(group_id))) is not None:
        st.add(ChatMsg(msg_id=mid, user_id=str(self_id), nickname=name,
                       text=text, ts=now, is_bot=True, reply_to=reply_to or None,
                       at=list(at)))
    await ingestor().record_own_reply(
        group_id=int(group_id), self_id=str(self_id), message_id=mid,
        text=text, at=now, name=name, reply_to=reply_to, addressees=at)


class Inbound:
    __slots__ = ("msg", "parsed", "media_task", "archive_task", "heard")

    def __init__(self, msg: ChatMsg, parsed: ParsedMessage,
                 media_task: asyncio.Task | None,
                 archive_task: asyncio.Task | None = None):
        self.msg = msg
        self.parsed = parsed
        self.media_task = media_task
        self.archive_task = archive_task
        #: What was typed, and only that: the text segments, before any media
        #: patch. The patch task mutates msg in place on its own schedule, so
        #: ordering alone cannot keep the trigger reading what was typed; only a
        #: snapshot can - and a snapshot of the render would still carry a share
        #: card's title or a file name, which nobody typed at the bot.
        self.heard = parsed.typed_text


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
            await asyncio.wait(self._loose,
                               timeout=config().default.gateway.shutdown_wait_sec)
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
            await self._admit(bot, event, group_id=group_id, user_id=user_id,
                              msg_id=msg_id, cfg=cfg)
        except Exception:
            # Marked seen, then failed before the message reached the window or
            # the archive: give the mark back, or the adapter's replay of this
            # event is swallowed and the message is neither archived nor answered.
            self._dedup_set().discard(msg_id)
            raise

    async def _admit(self, bot: BotApi, event, *, group_id: str, user_id: str,
                     msg_id: str, cfg: Settings) -> None:
        """Everything handle() does under the dedup mark: parse, window, archive,
        decide. Split out so one guard covers the whole stretch - the parser and
        the history load can both raise, and either would otherwise leave the
        mark held on a message nothing was done with."""
        st = await REGISTRY.get(group_id)
        # A blocked account is NOT dropped here: its messages arrive, archive and
        # feed memory like anyone's, so the window stays coherent around them - a
        # hole where a person used to be reads as broken context. The price is that
        # a blocked account still feeds memory; the one thing withheld is the reply,
        # at the dispatch gate.
        segments = [
            {"type": seg.type, "data": dict(seg.data)} for seg in event.get_message()
        ]
        parsed = parse_segments(segments, str(bot.self_id), limits=cfg.prompt)

        # The adapter pops a leading or trailing @me segment off the message and reports it
        # as event.to_me, so the segments alone cannot show the bot was addressed - the one
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
        # A command is answered by the command matchers, never by the reply
        # path - but the typed line still enters the window and the archive:
        # the console's answer quotes it, and a record with the answer but not
        # the question would show the model an @ with no antecedent. Whole
        # word: the matchers require whitespace after the name, so "/topology"
        # is chat, not "/top".
        is_command = bool(text.strip()) and text.split(maxsplit=1)[0] in COMMANDS
        if not text and not parsed.refs:
            return
        text = cut_text(text, cfg.gateway.max_msg_len)

        sender = event.sender
        # The reply path reads the live event, not gateway.Sender - so it defangs
        # here, in step with Sender.parse doing the same for the archived copy.
        # A sender with neither card nor nickname is shown by the generic member
        # word, never as the bare account number the prompt is told it will not see.
        nickname = display_name(getattr(sender, "card", ""),
                                getattr(sender, "nickname", ""), "成员")

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
            reply_to=parsed.reply_to,
            # Kept beyond the describe: pending is unpaid work and gets cleared,
            # but the references stay for the window's lifetime so open_images
            # can open any picture by number, forwarded ones included.
            image_refs=parsed.pictures,
        )

        # Before this message joins the deque: after a restart the window is rebuilt from
        # the archive, so the bot rejoins a conversation knowing what was being discussed
        # rather than starting blind. Only the message path can do this - it is the one
        # place that knows which account is the bot and who its owners are.
        await st.load_history(self_id=str(bot.self_id), owners=set(cfg.owners))

        # Archive unconditionally, off the hot path.
        # Hold on to anything still unresolved and expensive. A picture is usually
        # asked about in the message *after* it - a separate task by then, whose
        # backlog settle is what pays for it - see ChatMsg.pending.
        if parsed.needs_model:
            msg.pending = parsed
        if not st.add(msg):
            # Already in the window: the adapter replayed a message across a
            # restart, and the rebuilt history holds both it and its answer.
            # Before any task starts: a voice clip has no result cache, and a
            # replay must not pay for it twice.
            log.info("group %s: message %s replayed, already handled", group_id, msg_id)
            return

        inbound = GroupMessage.from_event(event, segments, bot.self_id, plain_text=text)
        archive_task = self._track(
            self._archive(inbound, at_accounts=list(parsed.mentions))
        )

        # Everything the message points at resolves now. The free lookups have to,
        # because an unresolved mention archives as a bare account number; the
        # pictures and voice because the link is freshest, the upload is free, and
        # each paid call carries its own cache, rate limit and budget gate - see
        # media.resolve.
        media_task = None
        if parsed.refs:
            media_task = self._track(self._resolve_and_patch(
                parsed, msg, bot=bot, group_id=group_id, cfg=cfg,
                archive_task=archive_task,
            ))

        if is_command:
            return

        item = Inbound(msg, parsed, media_task, archive_task)

        # Decided on what was typed (Inbound.heard), never the render or the
        # resolved form: a forwarded conversation whose body names the bot, a
        # share card whose title does, a mention rendering to a card that matches
        # a nickname - all can contain the trigger word without anyone having
        # typed it at the bot, and being spoken to means something somebody typed
        # deliberately. A typed nickname is in the text segments and an @ is a
        # parse-time flag, so no legitimate trigger needs more. The media tasks
        # keep running and patch the window on their own; the paid settle in the
        # reply task waits for them before the prompt reads the text.
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
        window = prompt.history_window(st, msg, cfg)
        task = asyncio.create_task(self._reply(bot, group_id, item, decision, window))
        self._replies.add(task)
        task.add_done_callback(self._replies.discard)

    async def handle_notice(self, bot: BotApi, event) -> None:
        """Group notices, transcribed: a recall, a join, a leave, a ban or a
        poke becomes one bracketed line in the window and the archive, so the
        conversation around it stays readable ("he deleted it" no longer
        points at nothing). Never a reply: these lines skip the trigger
        entirely - a poke whose target name contains the bot's nickname must
        not read as being addressed.
        """
        group_id = str(getattr(event, "group_id", "") or "")
        if not group_id:
            return
        actor, text = await self._notice_line(bot, group_id, event)
        if not text:
            return
        if actor == str(bot.self_id):
            # The bot as the event's subject - its message recalled, itself
            # added, removed or muted - is not transcribed: archiving would
            # mint an identity entity for the bot (the invariant
            # record_own_reply keeps), and a window rebuilt after a restart
            # would re-read the line as one the bot spoke.
            return
        # Notices carry no message id; the dedup set and the archive's
        # platform key both need one stable per event, so it is synthesized
        # from what identifies the event. Recalls add the recalled message's
        # id and pokes their target: the timestamp has second resolution, and
        # an admin mass-recalling one author lands many events in one second.
        when = int(getattr(event, "time", 0) or 0)
        mark = str(getattr(event, "message_id", "")
                   or getattr(event, "target_id", "") or "")
        nid = (f"notice-{getattr(event, 'notice_type', '')}"
               f"-{group_id}-{actor}-{when}" + (f"-{mark}" if mark else ""))
        if self._dedup_set().seen(nid):
            return
        cfg, _persona = config().for_group(group_id)
        try:
            st = await REGISTRY.get(group_id)
            await st.load_history(self_id=str(bot.self_id), owners=set(cfg.owners))
        except Exception:
            self._dedup_set().discard(nid)   # same rule as handle(): fail unmarked
            raise
        ts = datetime.fromtimestamp(when, tz()) if when else now_local()
        await self._transcribe(bot, group_id, actor=actor, text=text, msg_id=nid, ts=ts)

    async def _transcribe(self, bot: BotApi, group_id: str, *, actor: str,
                          text: str, msg_id: str, ts: datetime) -> None:
        """One system-written line about a member, into the window and the archive.

        The line is attributed to the member it is about and worded as something
        that happened to them, the way a ban or a recall is - never as words the
        bot spoke. What the bot is recorded as saying, the model takes as its own
        voice and repeats when the same question comes back, so a gate's notice
        filed as bot speech would be repeated to a member who has since satisfied
        the gate.

        The window line falls back to the generic member word for a name the
        member list does not know; the archived sender carries the card or
        nothing, because the ingest chain files sender names into the alias table
        as platform-reported, and a placeholder written there would stick.
        """
        cfg, _persona = config().for_group(group_id)
        st = await REGISTRY.get(group_id)
        card = await MEMBERS.name_of(bot, group_id, actor)
        st.add(ChatMsg(msg_id=msg_id, user_id=actor, nickname=card or "成员", text=text,
                       ts=ts, is_owner=actor in cfg.owners))
        inbound = GroupMessage(
            message_id=msg_id, group_id=int(group_id),
            sender=Sender(user_id=actor, nickname=card or ""),
            segments=[{"type": "text", "data": {"text": text}}],
            self_id=str(bot.self_id), occurred_at=ts, sub_type="notice",
            plain_text=text)
        self._track(self._archive(inbound, at_accounts=[]))

    async def _notice_line(self, bot: BotApi, group_id: str,
                           event) -> tuple[str, str]:
        """(actor account, transcript line) for one notice; ("", "") for the
        kinds deliberately not transcribed (title changes, honors, uploads)."""
        ntype = str(getattr(event, "notice_type", "") or "")
        uid = str(getattr(event, "user_id", "") or "")
        if not uid or uid == "0":
            # user_id 0 is the whole-group gesture (mute-all and its lift):
            # group state, not a member event - transcribing it would credit
            # a phantom account "0".
            return "", ""
        if ntype == "group_recall":
            op = str(getattr(event, "operator_id", "") or "")
            if op and op != uid:
                return uid, sysmark("一条消息被管理员撤回")
            return uid, sysmark("撤回了自己的一条消息")
        if ntype == "group_increase":
            return uid, sysmark("加入了本群")
        if ntype == "group_decrease":
            sub = str(getattr(event, "sub_type", "") or "")
            return uid, sysmark("被移出了本群" if sub == "kick" else "退出了本群")
        if ntype == "group_ban":
            # sub_type is the authoritative direction where present; the
            # duration alone misreads shapes that mark a lift with -1.
            sub = str(getattr(event, "sub_type", "") or "")
            dur = int(getattr(event, "duration", 0) or 0)
            if sub == "lift_ban" or (not sub and dur <= 0):
                return uid, sysmark("被解除禁言")
            if dur <= 0:
                return uid, sysmark("被禁言")
            if dur >= 60:
                return uid, sysmark(f"被禁言 {dur // 60} 分钟")
            return uid, sysmark(f"被禁言 {dur} 秒")
        if ntype == "notify" and str(getattr(event, "sub_type", "")) == "poke":
            target = str(getattr(event, "target_id", "") or "")
            if target == str(bot.self_id):
                return uid, sysmark("戳了戳你")
            if not target:
                return uid, sysmark("戳了戳别人")
            tname = await MEMBERS.name_of(bot, group_id, target)
            return uid, sysmark(f"戳了戳 {tname}" if tname else "戳了戳别人")
        return "", ""

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
        arrival), answers its own message, and bills its own asker
        through the budget's task-local attribution - so simultaneous asks
        answer independently, in whatever order the model finishes them.
        """
        cfg, persona = config().for_group(group_id)
        st = await REGISTRY.get(group_id)
        try:
            await self._answer(bot, group_id, item, decision, window, cfg=cfg,
                               persona=persona, st=st)
        except Exception:
            # A bare task has no worker loop above it to log for it.
            log.exception("group %s: reply task failed", group_id)

    async def _answer(self, bot: BotApi, group_id: str, item: Inbound,
                      decision: trigger.Decision, window: list[ChatMsg], *,
                      cfg: Settings, persona, st) -> None:
        """The body of one reply task, under _reply's guard."""
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
        # accepted the user agreement gets a one-line pointer at /terms and
        # /agree instead of a reply - at most once per cooldown, and one line
        # rather than the full text, which re-sent every cooldown reads as
        # spam - and nothing is spent on their behalf. Of the commands only
        # /agree and /terms answer before consent (commands._gate holds the
        # rest); archiving is untouched, owners are exempt.
        if (who and not perms.is_owner(who, cfg.owners)
                and not await agreement.ok(group_id, who)):
            if agreement.should_prompt(group_id, who):
                try:
                    sent = await bot.send_group_msg(
                        group_id=int(group_id),
                        message=[{"type": "at", "data": {"qq": who}},
                                 {"type": "text",
                                  "data": {"text": " " + agreement.POINTER}}])
                    # On the record as a notice about the member, not as a line
                    # the bot spoke: the model repeats what it reads as its own
                    # earlier answer, and this one must not come back once the
                    # member has consented. Filed under the platform id of the
                    # sent message, so a quote of the pointer still resolves.
                    mid = (str((sent or {}).get("message_id") or "")
                           or f"consent-{uuid.uuid4().hex[:12]}")
                    await self._transcribe(
                        bot, group_id, actor=who, msg_id=mid, ts=now_local(),
                        text=sysmark("被提示先同意用户协议"))
                except Exception as e:
                    log.warning("group %s: agreement prompt failed: %s",
                                group_id, why(e))
            return

        # Every yuan this reply spends - the transcribes and backlog describes it
        # forces, the tool calls, the model tokens - is booked to the initiator
        # the trigger decision already named. An attribution, not a charge: the
        # budget stays shared, this only feeds the /top leaderboard's ledger
        # column.
        with BUDGET.attribute(who):
            # Only now is anything paid for. Understanding a picture is worth money
            # exactly when the model is about to read the message it is in - which,
            # since the bot only speaks when spoken to, is a question that has
            # already been answered by here.
            await self._settle(item, window, group_id, bot=bot, cfg=cfg, who=who)
            await engine.respond(
                bot=bot, st=st, cfg=cfg, persona=persona,
                msg=item.msg, window=window,
                track=self._track,
            )

    async def _settle(self, item: Inbound, window: list[ChatMsg], group_id: str, *,
                      bot: BotApi, cfg: Settings, who: str | None) -> None:
        """Give the paid media this reply will read a bounded moment to land before
        the prompt is built: the message being answered, and whatever is still
        unread in the history about to be sent - a question about a voice clip
        refers to the message before it, which was never worth paying for on its
        own. Both sets start together and share one wait (gateway.media_wait_sec).

        The tasks persist their own results (_resolve_and_patch), so this only
        waits: a task that finishes in time has already patched its message; one
        that does not keeps running and patches it for the next turn. Nothing is
        cancelled.
        """
        tasks = self._settle_media(item, group_id, bot=bot, cfg=cfg, who=who)
        tasks += self._settle_backlog(window, group_id, bot=bot, cfg=cfg, who=who)
        if tasks:
            await asyncio.wait(tasks, timeout=cfg.gateway.media_wait_sec)
        if item.media_task is not None and item.media_task.done():
            item.media_task = None

    def _settle_backlog(
        self, window: list[ChatMsg], group_id: str, *, bot: BotApi, cfg: Settings,
        who: str | None = None,
    ) -> list[asyncio.Task]:
        """Start understanding the media still unread in the history this reply
        will be given; the tasks, for the caller's one bounded wait.

        People post a picture and ask about it in the *next* message, so the thing being
        asked about usually sits in a message that drew no reply and was never paid for.
        Voice more so: asking what a clip said is very nearly the only way a voice clip
        is ever discussed.

        The bound is the reply's own window - the slice cut at arrival, the very
        lines the prompt will carry - not a count of its own and not a fresh cut:
        a picture is worth understanding exactly when the model is about to read
        the message it is in, and a slice cut again here, after more messages
        arrived, could pay for pictures the prompt will not show and skip ones
        it will.
        """
        stale = [m for m in window if m.pending is not None and not m.is_bot]
        # Each attempt owns its own persistence and survives the wait; `pending`
        # is cleared inside it only when every paid slot settled, so a slow or
        # failed try is simply retried next turn - against warm caches and the
        # in-flight registry, so a retry never starts a second paid call for the
        # same picture.
        return [
            self._track(self._resolve_and_patch(
                msg.pending, msg, bot=bot, group_id=group_id, cfg=cfg,
                note="understood a message from the backlog", who=who))
            for msg in stale
        ]

    async def _resolve_and_patch(
        self, pm, msg: ChatMsg, *, bot: BotApi, group_id: str, cfg: Settings,
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
                resolved = await MEDIA.resolve(pm, bot=bot, group_id=group_id, cfg=cfg)
        except Exception as e:
            log.warning("group %s: media resolution failed: %s", group_id, why(e))
            return
        if MEDIA.settled(pm, resolved):
            # The paid content is in (or was refused with a cached verdict); nothing
            # is left to buy for this message - and that verdict now matters on the
            # arrival pass too: a voice transcript has no result cache, so leaving
            # pending set after a successful arrival transcription would let the
            # next reply's backlog pay for the same clip again. A transient failure
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
        new_text = cut_text(new_text, cfg.gateway.max_msg_len)
        if new_text and new_text != msg.text:
            msg.text = new_text
            await self._backfill(msg.msg_id, new_text, archive_task)
            if note:
                log.info("group %s: %s", group_id, note)

    def _settle_media(
        self, item: Inbound, group_id: str, *, bot: BotApi,
        cfg: Settings, who: str | None = None,
    ) -> list[asyncio.Task]:
        """Start the second attempt at the answered message's own paid media; the
        task, for the caller's one bounded wait.

        Runs only for a message that drew a reply: one whose paid content the
        arrival pass could not settle (rate limited, over the cap, a transient
        failure) gets this attempt on the asker's account, so a message that draws
        no reply costs nothing more, and one that does reads the resolved text
        when it can be had.
        """
        if item.parsed.needs_model and item.msg.pending is not None:
            item.media_task = self._track(self._resolve_and_patch(
                item.parsed, item.msg, bot=bot, group_id=group_id, cfg=cfg,
                archive_task=item.archive_task, after=item.media_task, who=who))
        return [item.media_task] if item.media_task is not None else []

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


GATEWAY = Gateway()
