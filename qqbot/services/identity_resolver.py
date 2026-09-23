"""Account -> person.

The only write path into L1. Every message that arrives gives the speaking account an
owner first; so does every account it @-mentions - somebody named in a message has not
spoken yet but is already part of the conversation.

Nicknames and group cards become alias evidence here rather than columns. The name the
platform reports is strong evidence (GROUP_CARD / QQ_NICKNAME), but it is still only one
of the names this person goes by - in the model it is the same kind of thing as a
nickname somebody shouted in the group, differing only in the evidence behind it, so
the two are always comparable.
"""

from __future__ import annotations

import logging
import uuid
from datetime import datetime

import asyncpg

from ..domain.ids import GroupId
from ..domain.identity import (
    Alias,
    AliasEvidence,
    AliasType,
    EvidenceType,
    IdentityAccount,
)
from ..repositories import IdentityRepository

log = logging.getLogger("qqbot.identity")

PLATFORM = "qq"


class UnknownAccount(LookupError):
    """Named an account this group has never seen speak.

    Its own class because the command layer has to tell it apart from every other
    failure: it is the one case where the answer is "@ them and try again", not
    "something broke".
    """

    def __init__(self, user_id: str) -> None:
        super().__init__(f"account {user_id} has not been seen")
        self.user_id = user_id


class IdentityResolver:
    """The way in to account identity. Stateless, so it is safe to call concurrently."""

    def __init__(self, repo: IdentityRepository) -> None:
        self._repo = repo

    async def seen(
        self,
        user_id: str,
        *,
        group_id: GroupId,
        at: datetime,
        card: str | None = None,
        nickname: str | None = None,
        raw_event_id: uuid.UUID | None = None,
        _conn: asyncpg.Connection | None = None,
    ) -> IdentityAccount:
        """Record that an account was seen speaking, and file its current names.

        The group card wins over the nickname: a card is how this person asks to be
        addressed in this group, while the nickname is their display name across the
        whole platform. Here, the former is closer to what people actually call them.
        """
        acc = await self._repo.ensure_account(
            PLATFORM,
            user_id,
            seen_at=at,
            name=card or nickname,
            _conn=_conn,
        )
        for text, kind, ev in (
            (card, AliasType.GROUP_CARD, EvidenceType.GROUP_CARD),
            (nickname, AliasType.QQ_NICKNAME, EvidenceType.PLATFORM_IDENTITY),
        ):
            if not (text or "").strip():
                continue
            await self._repo.upsert_alias(
                Alias(
                    alias_text=text.strip(),
                    target_entity_id=None,
                    target_account_id=acc.id,
                    # A group card holds in this group only. A platform nickname is the
                    # same everywhere, but it is still scoped to this group here: a
                    # global alias is the one channel that crosses groups, and no
                    # automatic path is allowed to open one.
                    group_id=group_id,
                    alias_type=kind,
                ),
                [AliasEvidence(ev, raw_event_id)],
                _conn=_conn,
            )
        return acc

    async def account(self, user_id: str) -> IdentityAccount:
        """The account row, or UnknownAccount if this one has never been seen."""
        acc = await self._repo.account_of(PLATFORM, user_id)
        if acc is None:
            raise UnknownAccount(user_id)
        return acc

    async def merge(self, left_account: str, right_account: str) -> bool:
        """Union the two account equivalence classes using deterministic root choice."""

        left = await self.account(left_account)
        right = await self.account(right_account)
        root, changed = await self._repo.merge_accounts(left.id, right.id)
        if changed:
            log.info(
                "merged holders %s and %s under %s",
                left.entity_id,
                right.entity_id,
                root,
            )
        return changed

    async def split(self, account_id: str) -> uuid.UUID:
        """Undo a merge for one account: give it a person of its own again.

        Used by owner repair and authenticated self-service unlink. Returns the new
        holder id.
        """
        acc = await self.account(account_id)
        new_id = await self._repo.split(acc)
        log.info("split account %s out of entity %s into %s", account_id, acc.entity_id, new_id)
        return new_id
