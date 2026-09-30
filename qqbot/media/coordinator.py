"""Bounded per-message media admission, retry state and late archive patches."""

from __future__ import annotations

import asyncio
import logging
import uuid
import weakref
from dataclasses import dataclass, field
from enum import StrEnum
from typing import TYPE_CHECKING

from qqbot.configuration import Settings
from qqbot.repositories.archive import ArchiveRepository
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.gateway.botapi import BotApi
from qqbot.gateway.segments import ParsedMessage
from qqbot.media.result import Resolution
from qqbot.media.service import MediaProcessor
from qqbot.services.budget import Budget
from qqbot.util import why

if TYPE_CHECKING:
    from qqbot.conversation.state import ChatMsg

log = logging.getLogger("qqbot.media")


class MediaStatus(StrEnum):
    PENDING = "pending"
    RETRYABLE = "retryable"
    FINAL = "final"


@dataclass(slots=True)
class MediaTicket:
    """One admitted message's media work, owned outside the chat transcript."""

    raw_event_id: uuid.UUID
    message_id: MessageId
    parsed: ParsedMessage
    message_ref: weakref.ReferenceType[ChatMsg]
    bot: BotApi
    group_id: GroupId
    cfg: Settings
    default_who: AccountId
    status: MediaStatus = MediaStatus.PENDING
    task: asyncio.Task[None] | None = None
    resolved: dict[int, Resolution] = field(default_factory=dict)


class MediaCoordinator:
    """Own per-message resolution, retries, patches, and shutdown draining."""

    def __init__(
        self,
        processor: MediaProcessor,
        *,
        budget: Budget,
        archive: ArchiveRepository,
        capacity: int = 32,
    ) -> None:
        if capacity < 1:
            raise ValueError("media capacity must be positive")
        self._archive = archive
        self._budget = budget
        self._processor = processor
        self._capacity = capacity
        self._closed = False
        self._tickets: dict[uuid.UUID, MediaTicket] = {}
        self.high_water = 0
        self.overloaded = 0

    def admit(
        self,
        raw_event_id: uuid.UUID,
        parsed: ParsedMessage,
        message: ChatMsg,
        *,
        bot: BotApi,
        group_id: GroupId,
        cfg: Settings,
    ) -> MediaTicket | None:
        """Admit bounded arrival work; overload never creates waiting tasks."""

        if self._closed:
            return None
        current = self._tickets.get(raw_event_id)
        if current is not None:
            return current
        if len(self._tickets) >= self._capacity:
            self._tickets = {
                key: ticket
                for key, ticket in self._tickets.items()
                if ticket.message_ref() is not None or ticket.task is not None
            }
        if len(self._tickets) >= self._capacity:
            self.overloaded += 1
            return None
        ticket = MediaTicket(
            raw_event_id=raw_event_id,
            message_id=message.msg_id,
            parsed=parsed,
            message_ref=weakref.ref(message),
            bot=bot,
            group_id=group_id,
            cfg=cfg,
            default_who=message.user_id,
        )
        self._tickets[raw_event_id] = ticket
        self.high_water = max(self.high_water, len(self._tickets))
        self._start(ticket, who=ticket.default_who)
        return ticket

    def ticket(self, raw_event_id: uuid.UUID) -> MediaTicket | None:
        return self._tickets.get(raw_event_id)

    @property
    def processor(self) -> MediaProcessor:
        return self._processor

    def _start(self, ticket: MediaTicket, *, who: AccountId | None) -> asyncio.Task[None] | None:
        if self._closed or ticket.status is MediaStatus.FINAL:
            return None
        if ticket.task is not None and not ticket.task.done():
            return ticket.task
        if ticket.message_ref() is None:
            self._tickets.pop(ticket.raw_event_id, None)
            return None
        task = asyncio.create_task(self._resolve(ticket, who=who))
        ticket.task = task

        def finished(done: asyncio.Task[None]) -> None:
            if ticket.task is done:
                ticket.task = None
            if ticket.status is MediaStatus.FINAL or ticket.message_ref() is None:
                self._tickets.pop(ticket.raw_event_id, None)

        task.add_done_callback(finished)
        return task

    async def _resolve(self, ticket: MediaTicket, *, who: AccountId | None) -> None:
        message = ticket.message_ref()
        if message is None:
            ticket.status = MediaStatus.FINAL
            return
        try:
            with self._budget.attribute(who):
                resolved = await self._processor.resolve(
                    ticket.parsed,
                    bot=ticket.bot,
                    group_id=ticket.group_id,
                    cfg=ticket.cfg,
                    previous=ticket.resolved,
                )
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            ticket.status = MediaStatus.RETRYABLE
            log.warning(
                "group %s: media resolution failed: %s",
                ticket.group_id,
                why(exc),
            )
            return

        ticket.resolved = resolved
        ticket.status = (
            MediaStatus.FINAL
            if self._processor.settled(ticket.parsed, resolved)
            else MediaStatus.RETRYABLE
        )
        new_text = ticket.parsed.render({slot: r.text for slot, r in resolved.items() if r.text})
        if new_text and new_text != message.text:
            message.text = new_text
            try:
                await self._archive.backfill_plain_text(ticket.message_id, new_text)
            except Exception:
                log.exception("failed to backfill plain_text for %s", ticket.message_id)

    async def settle(
        self,
        messages: list[ChatMsg],
        *,
        wait_sec: float,
        who: AccountId | None,
        cfg: Settings | None = None,
    ) -> None:
        """Start retryable work in a frozen reply window and wait once, bounded."""

        tasks: set[asyncio.Task[None]] = set()
        for message in messages:
            if message.raw_event_id is None or message.is_bot:
                continue
            ticket = self._tickets.get(message.raw_event_id)
            if ticket is None:
                continue
            if cfg is not None:
                ticket.cfg = cfg
            if task := self._start(ticket, who=who):
                tasks.add(task)
        if tasks:
            await asyncio.wait(tasks, timeout=wait_sec)

    def abort(self) -> None:
        self._closed = True
        for ticket in self._tickets.values():
            if ticket.task is not None:
                ticket.task.cancel()

    async def close(self, *, timeout: float) -> None:
        """Drain admitted patch tasks, then cancel any that exceed shutdown's bound."""

        self._closed = True
        tasks = {
            ticket.task
            for ticket in self._tickets.values()
            if ticket.task is not None and not ticket.task.done()
        }
        if tasks:
            _done, pending = await asyncio.wait(tasks, timeout=timeout)
            for task in pending:
                task.cancel()
            if pending:
                await asyncio.gather(*pending, return_exceptions=True)
        self._tickets.clear()
