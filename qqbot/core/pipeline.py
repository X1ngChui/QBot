"""Archive-first group ingress, command routing and concurrent reply dispatch."""

from __future__ import annotations

import asyncio
import logging
from dataclasses import replace

from ..domain.archive import AuthorKind
from ..domain.ids import AccountId, GroupId
from ..domain.ingress import InboundEvent
from ..gateway.ingest import Ingestor
from ..gateway.onebot import GroupMessage, notice_from_event
from ..providers.base import Providers
from ..services import Directory
from ..settings import Settings, config
from ..util import cut_text, display_name, sysmark, why
from . import agreement, engine, perms, prompt, trigger
from .botapi import BotApi
from .budget import BUDGET
from .command_catalog import find as find_command
from .commands import CommandRequest, CommandRouter
from .delivery import GroupDelivery
from .media import MediaCoordinator
from .members import MEMBERS
from .outbound import AtSegment, TextSegment, from_onebot
from .segments import ParsedMessage, at_mentions, parse_segments
from .state import ChatMsg, Registry

log = logging.getLogger("qqbot.pipeline")


class Inbound:
    __slots__ = ("msg", "parsed", "heard")

    def __init__(
        self,
        msg: ChatMsg,
        parsed: ParsedMessage,
        *,
        heard: str,
    ) -> None:
        self.msg = msg
        self.parsed = parsed
        # The trigger reads only what the member typed, never a mutable media render.
        self.heard = heard


class Gateway:
    def __init__(
        self,
        *,
        ingestor: Ingestor,
        registry: Registry,
        router: CommandRouter,
        delivery: GroupDelivery,
        media: MediaCoordinator,
        providers: Providers,
        directory: Directory,
    ) -> None:
        # One task per addressed message. Generation is concurrent; GroupDelivery alone
        # serializes the final protocol batches within each group.
        self._replies: set[asyncio.Task] = set()
        self._ingestor = ingestor
        self._registry = registry
        self._router = router
        self._delivery = delivery
        self._media = media
        self._providers = providers
        self._directory = directory

    async def shutdown(self) -> None:
        """Cancel and join every reply generation task."""

        replies = list(self._replies)
        for task in replies:
            task.cancel()
        if replies:
            await asyncio.gather(*replies, return_exceptions=True)
        self._replies.clear()

    async def handle(self, bot: BotApi, event) -> None:
        """Normalize an ordinary or self-authored message and route it once."""

        inbound = GroupMessage.from_event(event, str(bot.self_id))
        cfg, _persona = config().for_group(inbound.group_id)
        self_name = config().persona_for(inbound.group_id).name or (
            cfg.trigger.nicknames[0] if cfg.trigger.nicknames else "机器人"
        )
        await self._admit(
            bot,
            inbound.with_bot_mention(self_name),
            cfg=cfg,
            self_name=self_name,
        )

    async def _admit(
        self,
        bot: BotApi,
        inbound: InboundEvent,
        *,
        cfg: Settings,
        self_name: str,
    ) -> None:
        """Parse, append once, then perform all event-specific side effects."""

        group_id = inbound.group_id
        segments = inbound.segments
        parsed = parse_segments(
            segments,
            inbound.self_id,
            limits=cfg.prompt,
            self_name=self_name,
        )
        if inbound.reply_to_message_id and not parsed.reply_to:
            parsed.reply_to = inbound.reply_to_message_id
        rendered = inbound.plain_text if inbound.event_type == "notice" else parsed.render()
        text = cut_text(
            rendered,
            cfg.tools.send_messages.max_text_chars_per_message,
        )
        command_name = self._command_name(inbound.typed_text)
        is_bot = inbound.author_kind is AuthorKind.BOT
        direct_mentions = at_mentions(
            segments,
            self_id=inbound.self_id,
            self_name=self_name,
        )

        st = await self._registry.get(group_id)
        # A replay after restart is already present in this load. The append-once claim
        # below then rejects it before this event can enter the live deque a second time.
        await st.load_history(self_id=str(bot.self_id), owners=set(cfg.owners))

        archived = replace(
            inbound,
            plain_text=text,
            reply_to_message_id=parsed.reply_to,
        )
        admitted = await self._ingestor.ingest(
            archived,
            at_accounts=parsed.mentions,
        )
        if admitted is None:
            log.info(
                "group %s: platform event %s already admitted",
                group_id,
                inbound.message_id,
            )
            return

        if not text and not parsed.refs:
            return

        if is_bot and direct_mentions:
            accounts = [account for account, _ in direct_mentions]
            live_names = await MEMBERS.names_of(bot, group_id, accounts)
            direct_mentions = [
                (account, label or live_names.get(account) or "成员")
                for account, label in direct_mentions
            ]
        if inbound.event_type == "notice":
            live_name = await MEMBERS.name_of(bot, group_id, inbound.sender.user_id)
            nickname = live_name or "成员"
        else:
            nickname = (
                self_name
                if is_bot
                else display_name(inbound.sender.card, inbound.sender.nickname, "成员")
            )

        msg = ChatMsg(
            msg_id=inbound.message_id,
            user_id=inbound.sender.user_id,
            nickname=nickname,
            text=text,
            ts=inbound.occurred_at,
            raw_event_id=admitted.raw_event_id,
            is_bot=is_bot,
            is_owner=not is_bot and inbound.sender.user_id in cfg.owners,
            reply_to=parsed.reply_to,
            image_refs=parsed.pictures,
            mentions=[] if is_bot else direct_mentions,
            at=direct_mentions if is_bot else [],
            outbound=from_onebot(segments) if is_bot else (),
        )
        st.add(msg)

        if parsed.refs:
            self._media.admit(
                admitted.raw_event_id,
                parsed,
                msg,
                bot=bot,
                group_id=group_id,
                cfg=cfg,
            )

        # Self-observation and notices share admission and projection, then stop before
        # command mutation or model dispatch.
        if is_bot or inbound.event_type == "notice":
            return

        if command_name is not None:
            await self._router.dispatch(
                bot,
                CommandRequest(
                    name=command_name,
                    message_id=inbound.message_id,
                    raw_event_id=admitted.raw_event_id,
                    group_id=group_id,
                    user_id=inbound.sender.user_id,
                    self_id=inbound.self_id,
                    text=inbound.typed_text,
                    mentions=tuple(AccountId(account) for account in parsed.mentions),
                ),
            )
            return

        item = Inbound(msg, parsed, heard=inbound.typed_text)
        decision = trigger.decide(
            replace(msg, text=item.heard),
            parsed.at_bot,
            st=st,
            cfg=cfg,
        )
        if not decision.reply:
            if parsed.at_bot:
                log.info(
                    "group %s: addressed but not replying (%s)",
                    group_id,
                    decision.reason,
                )
            else:
                log.debug("group %s: no reply (%s)", group_id, decision.reason)
            return

        window = prompt.history_window(st, msg, cfg)
        task = asyncio.create_task(self._reply(bot, group_id, item, decision, window))
        self._replies.add(task)
        task.add_done_callback(self._replies.discard)

    @staticmethod
    def _command_name(typed_text: str) -> str | None:
        """Return an exact catalog command from the first complete typed token."""

        token = typed_text.strip().split(maxsplit=1)[0] if typed_text.strip() else ""
        if not token.startswith("/"):
            return None
        spec = find_command(token)
        return spec.name if spec is not None and token == spec.name else None

    async def handle_notice(self, bot: BotApi, event) -> None:
        """Normalize supported notices and send them through the same admission route."""

        text = self._notice_line(str(bot.self_id), event)
        if not text:
            return
        notice = notice_from_event(
            event,
            self_id=bot.self_id,
            plain_text=text,
        )
        if notice is None or notice.sender.user_id in {"0", str(bot.self_id)}:
            return
        group_id = notice.group_id
        cfg, _persona = config().for_group(group_id)
        self_name = config().persona_for(group_id).name or (
            cfg.trigger.nicknames[0] if cfg.trigger.nicknames else "机器人"
        )
        await self._admit(bot, notice, cfg=cfg, self_name=self_name)

    @staticmethod
    def _notice_line(self_id: str, event) -> str:
        """Return the stable transcript projection for one supported notice."""

        ntype = str(getattr(event, "notice_type", "") or "")
        user_id = str(getattr(event, "user_id", "") or "")
        if ntype == "group_recall":
            operator = str(getattr(event, "operator_id", "") or "")
            if operator and operator != user_id:
                return sysmark("一条消息被管理员撤回")
            return sysmark("撤回了自己的一条消息")
        if ntype == "group_increase":
            return sysmark("加入了本群")
        if ntype == "group_decrease":
            subtype = str(getattr(event, "sub_type", "") or "")
            return sysmark("被移出了本群" if subtype == "kick" else "退出了本群")
        if ntype == "group_ban":
            subtype = str(getattr(event, "sub_type", "") or "")
            duration = int(getattr(event, "duration", 0) or 0)
            if subtype == "lift_ban" or (not subtype and duration <= 0):
                return sysmark("被解除禁言")
            if duration <= 0:
                return sysmark("被禁言")
            if duration >= 60:
                return sysmark(f"被禁言 {duration // 60} 分钟")
            return sysmark(f"被禁言 {duration} 秒")
        if ntype == "notify" and str(getattr(event, "sub_type", "")) == "poke":
            target = str(getattr(event, "target_id", "") or "")
            return sysmark("戳了戳你" if target == self_id else "戳了戳别人")
        return ""

    async def _reply(
        self,
        bot: BotApi,
        group_id: GroupId,
        item: Inbound,
        decision: trigger.Decision,
        window: list[ChatMsg],
    ) -> None:
        """One reply attempt for one addressed message, start to finish.

        Tasks run concurrently. Each carries its own context slice (cut at
        arrival), answers its own message, and bills its own asker
        through the budget's task-local attribution - so simultaneous asks
        answer independently, in whatever order the model finishes them.
        """
        cfg, persona = config().for_group(group_id)
        st = await self._registry.get(group_id)
        try:
            await self._answer(
                bot, group_id, item, decision, window, cfg=cfg, persona=persona, st=st
            )
        except Exception:
            # A bare task has no worker loop above it to log for it.
            log.exception("group %s: reply task failed", group_id)

    async def _answer(
        self,
        bot: BotApi,
        group_id: GroupId,
        item: Inbound,
        decision: trigger.Decision,
        window: list[ChatMsg],
        *,
        cfg: Settings,
        persona,
        st,
    ) -> None:
        """The body of one reply task, under _reply's guard."""
        if await BUDGET.exceeded(cfg.budget.daily_cny_cap):
            # After the trigger on purpose: it fires once per suppressed reply, not
            # once per message all afternoon - hundreds of identical warnings would
            # walk the actionable failures out of the ring the daily report reads.
            log.warning(
                "group %s: daily budget cap of %.2f reached, staying quiet "
                "until the day rolls over",
                group_id,
                cfg.budget.daily_cny_cap,
            )
            return

        who = decision.initiator
        # /block withholds exactly the reply; owners stay exempt even when a
        # holder rule starts covering them after an identity merge.
        if who and not perms.is_owner(who, cfg.owners) and await st.blocked_now(who):
            log.debug("group %s: no reply, initiator %s is blocked", group_id, who)
            return
        # The consent gate, before anything is paid for: a member who has not
        # accepted the user agreement gets a one-line pointer at /terms and
        # /agree instead of a reply - at most once per cooldown, and one line
        # rather than the full text, which re-sent every cooldown reads as
        # spam - and nothing is spent on their behalf. Of the commands only
        # /agree and /terms answer before consent (CommandRouter holds the rest);
        # archiving is untouched, owners are exempt.
        if who and not perms.is_owner(who, cfg.owners) and not await agreement.ok(group_id, who):
            if agreement.should_prompt(group_id, who):
                try:
                    await self._delivery.deliver(
                        bot,
                        group_id=group_id,
                        messages=(
                            (
                                AtSegment(who),
                                TextSegment(" " + agreement.POINTER),
                            ),
                        ),
                    )
                except Exception as e:
                    log.warning("group %s: agreement prompt failed: %s", group_id, why(e))
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
            await self._settle(item, window, cfg=cfg, who=who)
            await engine.respond(
                bot=bot,
                st=st,
                cfg=cfg,
                persona=persona,
                msg=item.msg,
                providers=self._providers,
                media=self._media.processor,
                delivery=self._delivery,
                directory=self._directory,
                window=window,
            )

    async def _settle(
        self,
        item: Inbound,
        window: list[ChatMsg],
        *,
        cfg: Settings,
        who: str | None,
    ) -> None:
        """Give media in this reply's frozen window one bounded wait."""

        await self._media.settle(
            [*window, item.msg],
            wait_sec=cfg.media.wait_sec,
            who=who,
            cfg=cfg,
        )
