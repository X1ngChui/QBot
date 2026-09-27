"""Self-service account linking with proof from both authenticated accounts."""

from __future__ import annotations

from qqbot.clock import Clock

import hashlib
import secrets
import string
import uuid
from datetime import timedelta

import asyncpg

from qqbot.domain.ids import GroupId
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.identity_link import IdentityLinkRepository
from qqbot.repositories.identity_link import LinkChallenge
from qqbot.repositories.identity_link import LinkChallengeError
from qqbot.services.identity_limits import ChallengeLimits
from qqbot.services.identity_resolver import IdentityResolver

TOKEN_RETRIES = 5


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

    @staticmethod
    def _hash(code: str) -> str:
        return hashlib.sha256(code.encode("ascii")).hexdigest()

    def _token_hash(self, code: str) -> str:
        if (
            len(code) != self._policy.challenge_code_length
            or not code.isascii()
            or not code.isdecimal()
        ):
            raise LinkChallengeError("验证码格式无效。")
        return self._hash(code)

    def _code(self) -> str:
        return "".join(
            secrets.choice(string.digits) for _ in range(self._policy.challenge_code_length)
        )

    async def issue(
        self,
        *,
        group_id: GroupId,
        initiator_user_id: str,
        target_user_id: str,
        created_event_id: uuid.UUID,
    ) -> tuple[LinkChallenge, str]:
        initiator = await self._resolver.account(initiator_user_id)
        target = await self._resolver.account(target_user_id)
        if initiator.id == target.id:
            raise LinkChallengeError("不能把账号与自身关联。")
        if initiator.entity_id == target.entity_id:
            raise LinkChallengeError("这两个账号已经关联。")
        expires_at = self._clock.now() + timedelta(seconds=self._policy.challenge_ttl_sec)
        for _ in range(TOKEN_RETRIES):
            code = self._code()
            try:
                challenge = await self._challenges.issue(
                    group_id=group_id,
                    token_hash=self._token_hash(code),
                    initiator=initiator,
                    target=target,
                    created_event_id=created_event_id,
                    expires_at=expires_at,
                    max_pending=self._policy.max_pending_challenges,
                    max_pending_per_account=self._policy.max_pending_per_account,
                )
            except asyncpg.UniqueViolationError:
                continue
            return challenge, code
        raise LinkChallengeError("无法生成唯一验证码，请重试。")

    async def confirm(
        self,
        *,
        group_id: GroupId,
        actor_user_id: str,
        code: str,
        confirmed_event_id: uuid.UUID,
    ) -> LinkChallenge:
        actor = await self._resolver.account(actor_user_id)
        return await self._challenges.confirm(
            group_id=group_id,
            token_hash=self._token_hash(code),
            actor=actor,
            confirmed_event_id=confirmed_event_id,
            identities=self._identities,
        )

    async def cancel(
        self,
        *,
        group_id: GroupId,
        actor_user_id: str,
        code: str,
    ) -> bool:
        actor = await self._resolver.account(actor_user_id)
        return await self._challenges.cancel(
            group_id=group_id,
            token_hash=self._token_hash(code),
            actor=actor,
        )
