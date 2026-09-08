"""The inbound chain: what happens when a group message arrives.

    raw_event is written (L0)
        +-- the speaker and everyone @-ed gets an owner (L1)

There is deliberately no reference-resolution step in between: every mention this system
receives is an @ or a quote, where the platform states the account outright, so there is
nothing to resolve. The references that would need judgement - a bare name, a pronoun -
are left alone, because deciding whether a name in a sentence means a member or
somebody's colleague is interpretation, and this layer reproduces the conversation
rather than interpreting it.

The chain does only free work (design doc 52): write, look up, resolve. Paid extraction
does not happen here at all - the nightly drain (schedule.extract_cron) reads the day's
transcript in one sitting, through the job queue so half-learned work survives the
process stopping.
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass
from datetime import datetime

from ..db import pool
from ..repositories import IdentityRepository
from ..services import IdentityResolver
from .onebot import GroupMessage, Sender

log = logging.getLogger("qqbot.ingest")


@dataclass(frozen=True, slots=True)
class Ingested:
    """What the inbound chain produced. Both fields are worked out on the way through, so
    reporting them costs nothing."""

    raw_event_id: uuid.UUID
    speaker_entity_id: uuid.UUID


class Ingestor:
    def __init__(self, identity: IdentityResolver) -> None:
        self._identity = identity

    async def ingest(
        self, msg: GroupMessage, *, at_accounts: list[str] = (),
    ) -> Ingested:
        """Archive one message and make sure everybody in it has an owner."""
        raw_id = await self._record(msg)

        speaker = await self._identity.seen(
            msg.sender.user_id, group_id=msg.group_id, at=msg.occurred_at,
            card=msg.sender.card, nickname=msg.sender.nickname, raw_event_id=raw_id,
        )

        # Everyone @-ed gets an owner too: they have not spoken yet, but they are already
        # part of this conversation, and the next line saying "he's right" means them.
        # The @ carries the account id outright, so there is nothing here to resolve.
        for uid in dict.fromkeys(at_accounts):
            if uid:
                await self._identity.seen(
                    uid, group_id=msg.group_id, at=msg.occurred_at, raw_event_id=raw_id,
                )

        # No extraction trigger here any more: reading the day's transcript is the
        # nightly drain's job (schedule.extract_cron), at off-peak prices and in
        # gap-aligned batches. The message path only writes.
        return Ingested(raw_event_id=raw_id, speaker_entity_id=speaker.entity_id)

    async def record_own_reply(
        self, *, group_id: int, self_id: str, message_id: str, text: str,
        at: datetime, name: str = "", reply_to: str = "",
    ) -> uuid.UUID:
        """Write down what the bot itself said.

        NapCat is configured not to report the bot's own messages, so nothing else ever
        writes them - and memory is read back out of the archive, so an archive without
        them holds only one side of every conversation the bot took part in.

        When extraction reads the archive these lines render marked as the bot's
        own - readable for coherence, and excluded from evidence (see
        MemoryWorker._render): what the bot said is never proof about the people
        it said it to.

        No identity is created for it. The bot is not a group member with a history to be
        learned, and giving it an entity would put it in its own roster.
        """
        return await self._record(GroupMessage(
            message_id=message_id,
            group_id=group_id,
            sender=Sender(user_id=self_id, nickname=name),
            segments=[{"type": "text", "data": {"text": text}}],
            self_id=self_id,
            occurred_at=at,
            plain_text=text,
            # The quote pointer travels into the payload the same way a member's
            # does, so the restart-rebuilt window renders the line identically.
            reply_to_message_id=reply_to or None,
        ))

    async def _record(self, msg: GroupMessage) -> uuid.UUID:
        """L0 is append-only. The unique index on the platform message id is what stops a
        replay after a reconnect from landing twice."""
        return await pool().fetchval(
            """INSERT INTO raw_event
                   (platform, event_type, group_id, platform_user_id,
                    platform_event_id, occurred_at, payload, plain_text)
               VALUES ('qq','message',$1,$2,$3,$4,$5,$6)
               ON CONFLICT (platform, platform_event_id)
                 WHERE platform_event_id IS NOT NULL
                 DO UPDATE SET occurred_at = raw_event.occurred_at
            RETURNING id""",
            msg.group_id, msg.sender.user_id, msg.message_id,
            msg.occurred_at, msg.as_payload(), msg.plain_text or None,
        )

_INGESTOR: Ingestor | None = None


def ingestor() -> Ingestor:
    """The shared instance. Built lazily and once; every part of it is stateless."""
    global _INGESTOR
    if _INGESTOR is None:
        _INGESTOR = Ingestor(identity=IdentityResolver(IdentityRepository()))
    return _INGESTOR
