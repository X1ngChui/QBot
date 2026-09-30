"""Self-service linking through one addressed invitation per group recipient."""

from __future__ import annotations

from qqbot.domain.ids import AccountId
import uuid
from datetime import timedelta

from qqbot.clock import Clock
from qqbot.domain.ids import GroupId
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.identity_link import (
    IdentityLinkRepository,
    LinkChallenge,
    LinkChallengeError,
)
from qqbot.services.identity_limits import ChallengeLimits
from qqbot.services.identity_resolver import IdentityResolver


class IdentityLinkService:
    def __init__(
        self,
        policy: ChallengeLimits,
        resolver: IdentityResolver,
        identities: IdentityRepository,
        challenges: IdentityLinkRepository,
        *,
        clock: Clock,
    ) -> None:
        self._clock = clock
        self._policy = policy
        self._resolver = resolver
        self._identities = identities
        self._challenges = challenges

    async def issue(
        self,
        *,
        group_id: GroupId,
        initiator_user_id: AccountId,
        target_user_id: AccountId,
        created_event_id: uuid.UUID,
    ) -> LinkChallenge:
        initiator = await self._resolver.account(initiator_user_id)
        target = await self._resolver.account(target_user_id)
        if initiator.id == target.id:
            raise LinkChallengeError("不能把账号与自身关联。")
        if initiator.entity_id == target.entity_id:
            raise LinkChallengeError("这两个账号已经关联。")
        return await self._challenges.issue(
            group_id=group_id,
            initiator=initiator,
            target=target,
            created_event_id=created_event_id,
            expires_at=self._clock.now() + timedelta(seconds=self._policy.challenge_ttl_sec),
            max_pending=self._policy.max_pending_challenges,
        )

    async def confirm(
        self,
        *,
        group_id: GroupId,
        actor_user_id: AccountId,
        confirmed_event_id: uuid.UUID,
    ) -> LinkChallenge:
        actor = await self._resolver.account(actor_user_id)
        return await self._challenges.confirm(
            group_id=group_id,
            actor=actor,
            confirmed_event_id=confirmed_event_id,
            identities=self._identities,
        )

    async def cancel(
        self,
        *,
        group_id: GroupId,
        actor_user_id: AccountId,
        cancelled_event_id: uuid.UUID,
    ) -> bool:
        actor = await self._resolver.account(actor_user_id)
        return await self._challenges.cancel(
            group_id=group_id,
            actor=actor,
            cancelled_event_id=cancelled_event_id,
        )
