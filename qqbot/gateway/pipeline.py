"""Archive-first group ingress, command routing and concurrent reply dispatch."""

from __future__ import annotations


import asyncio
import logging
from dataclasses import replace
from zoneinfo import ZoneInfo

from qqbot.conversation.scheduler import ReplyScheduler
from qqbot.conversation.session import AddressedMessage, ReplyRequest

from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import AccountId, MessageId
from qqbot.domain.ingress import InboundEvent
from qqbot.gateway.ingest import Ingestor
from qqbot.gateway.onebot import GroupMessage
from qqbot.gateway.onebot import notice_from_event
from qqbot.clock import Clock
from qqbot.configuration import ConfigBundle, Settings
from qqbot.util import display_name
from qqbot.util import sysmark
from qqbot.conversation import prompt
from qqbot.gateway import trigger
from qqbot.gateway.botapi import BotApi
from qqbot.commands.catalog import find as find_command
from qqbot.commands.router import CommandRequest
from qqbot.commands.router import CommandRouter
from qqbot.delivery.service import GroupDelivery
from qqbot.media.coordinator import MediaCoordinator
from qqbot.services.members import MemberDirectory
from qqbot.delivery.segments import from_onebot
from qqbot.gateway.segments import at_mentions
from qqbot.gateway.segments import parse_segments
from qqbot.conversation.state import ChatMsg
from qqbot.conversation.state import Registry

log = logging.getLogger("qqbot.pipeline")


class Gateway:
    def __init__(
        self,
        *,
        bundle: ConfigBundle,
        clock: Clock,
        members: MemberDirectory,
        ingestor: Ingestor,
        registry: Registry,
        router: CommandRouter,
        delivery: GroupDelivery,
        media: MediaCoordinator,
        replies: ReplyScheduler[ReplyRequest],
    ) -> None:
        self._members = members
        self._clock = clock
        self._bundle = bundle
        self._closing = False
        self._admissions: dict[asyncio.Task, int] = {}
        self._replies = replies
        self._ingestor = ingestor
        self._registry = registry
        self._router = router
        self._delivery = delivery
        self._media = media

    def quiesce(self) -> None:
        self._closing = True

    def abort(self) -> None:
        self.quiesce()
        for task in self._admissions:
            task.cancel()

    async def shutdown(self) -> None:
        """Quiesce ingress before joining all owned reply executions."""
        self._closing = True
        admissions = set(self._admissions) - {asyncio.current_task()}
        if admissions:
            _, pending = await asyncio.wait(admissions, timeout=5.0)
            for task in pending:
                task.cancel()
            if pending:
                await asyncio.gather(*pending, return_exceptions=True)

    async def drain(self) -> None:
        """Wait for accepted replies; inbound observation stays independently available."""
        await self._replies.drain()

    async def handle(self, bot: BotApi, event) -> None:
        """Normalize an ordinary or self-authored message and route it once."""

        inbound = GroupMessage.from_event(event, str(bot.self_id), clock=self._clock)
        cfg, _persona = self._bundle.for_group(inbound.group_id)
        self_name = self._bundle.persona_for(inbound.group_id).name or (
            cfg.bot.nicknames[0] if cfg.bot.nicknames else "机器人"
        )
        await self._admit(
            bot,
            inbound.with_bot_mention(self_name),
            cfg=cfg,
            self_name=self_name,
        )

    async def _admit(self, bot: BotApi, inbound: InboundEvent, **kwargs) -> None:
        if self._closing:
            return
        task = asyncio.current_task()
        assert task is not None
        self._admissions[task] = self._admissions.get(task, 0) + 1
        try:
            await self._admit_once(bot, inbound, **kwargs)
        finally:
            depth = self._admissions[task] - 1
            if depth:
                self._admissions[task] = depth
            else:
                del self._admissions[task]

    async def _admit_once(
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
            display_zone=ZoneInfo(cfg.bot.timezone),
            self_name=self_name,
        )
        if inbound.reply_to_message_id and not parsed.reply_to:
            parsed.reply_to = inbound.reply_to_message_id
        reply_to = MessageId(parsed.reply_to) if parsed.reply_to else None
        rendered = inbound.plain_text if inbound.event_type == "notice" else parsed.render()
        text = rendered
        command_name = self._command_name(inbound.typed_text)
        is_bot = inbound.author_kind is AuthorKind.BOT
        direct_mentions = [
            (AccountId(account), label)
            for account, label in at_mentions(
                segments,
                self_id=inbound.self_id,
                self_name=self_name,
            )
        ]

        st = await self._registry.get(group_id)
        # A replay after restart is already present in this load. The append-once claim
        # below then rejects it before this event can enter the live deque a second time.
        await st.load_history(self_id=str(bot.self_id), owners=set(cfg.bot.owners))

        archived = replace(
            inbound,
            plain_text=text,
            reply_to_message_id=reply_to,
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
            accounts = [str(account) for account, _ in direct_mentions]
            live_names = await self._members.names_of(bot, group_id, accounts)
            direct_mentions = [
                (account, label or live_names.get(account) or "成员")
                for account, label in direct_mentions
            ]
        if inbound.event_type == "notice":
            live_name = await self._members.name_of(bot, group_id, inbound.sender.user_id)
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
            is_owner=not is_bot and inbound.sender.user_id in cfg.bot.owners,
            reply_to=reply_to,
            image_refs=parsed.pictures,
            mentions=[] if is_bot else direct_mentions,
            at=direct_mentions if is_bot else [],
            outbound=from_onebot(segments) if is_bot else (),
        )
        st.add(msg)
        if is_bot:
            self._delivery.echo.publish(str(bot.self_id), group_id, msg)

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

        decision = trigger.decide(
            replace(msg, text=inbound.typed_text),
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

        window = tuple(prompt.history_window(st, msg))
        if not self._closing:
            self._replies.submit(
                ReplyRequest(bot, group_id, AddressedMessage(msg, decision.initiator, window)),
                deadline=asyncio.get_running_loop().time() + cfg.conversation.reply_deadline_sec,
            )

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
            clock=self._clock,
            self_id=bot.self_id,
            plain_text=text,
        )
        if notice is None or notice.sender.user_id in {"0", str(bot.self_id)}:
            return
        group_id = notice.group_id
        cfg, _persona = self._bundle.for_group(group_id)
        self_name = self._bundle.persona_for(group_id).name or (
            cfg.bot.nicknames[0] if cfg.bot.nicknames else "机器人"
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
