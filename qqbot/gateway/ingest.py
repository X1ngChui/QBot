"""Atomic append-once admission and member identity projection for inbound events."""

from __future__ import annotations

import uuid
from collections.abc import Sequence
from dataclasses import dataclass

import asyncpg

from ..db import pool
from ..domain.archive import AuthorKind
from ..domain.ingress import InboundEvent
from ..services import IdentityResolver


@dataclass(frozen=True, slots=True)
class Ingested:
    """The authoritative archive id and optional member entity created at admission."""

    raw_event_id: uuid.UUID
    speaker_entity_id: uuid.UUID | None


class Ingestor:
    def __init__(self, identity: IdentityResolver) -> None:
        self._identity = identity

    async def ingest(
        self,
        event: InboundEvent,
        *,
        at_accounts: Sequence[str] = (),
    ) -> Ingested | None:
        """Append once and atomically apply the event's required identity writes.

        None means the platform key already exists. Callers must treat that conflict as
        final admission denial and perform no window, media, command or reply effects.
        """

        async with pool().acquire() as conn, conn.transaction():
            raw_id = await self._record(conn, event)
            if raw_id is None:
                return None
            if event.author_kind is AuthorKind.BOT:
                return Ingested(raw_event_id=raw_id, speaker_entity_id=None)

            speaker = await self._identity.seen(
                event.sender.user_id,
                group_id=event.group_id,
                at=event.occurred_at,
                card=event.sender.card,
                nickname=event.sender.nickname,
                raw_event_id=raw_id,
                _conn=conn,
            )
            for account in dict.fromkeys(at_accounts):
                if account:
                    await self._identity.seen(
                        account,
                        group_id=event.group_id,
                        at=event.occurred_at,
                        raw_event_id=raw_id,
                        _conn=conn,
                    )
            return Ingested(raw_event_id=raw_id, speaker_entity_id=speaker.entity_id)

    @staticmethod
    async def _record(
        conn: asyncpg.Connection,
        event: InboundEvent,
    ) -> uuid.UUID | None:
        """Claim the platform event key without mutating an existing row."""

        return await conn.fetchval(
            """INSERT INTO raw_event
                   (platform, event_type, group_id, platform_user_id,
                    platform_event_id, occurred_at, payload, plain_text, archive_schema)
               VALUES ('qq',$1,$2,$3,$4,$5,$6,$7,1)
               ON CONFLICT (platform, platform_event_id)
                 WHERE platform_event_id IS NOT NULL
                 DO NOTHING
            RETURNING id""",
            event.event_type,
            event.group_id.to_db(),
            event.sender.user_id,
            event.message_id,
            event.occurred_at,
            event.as_payload(),
            event.plain_text or None,
        )
