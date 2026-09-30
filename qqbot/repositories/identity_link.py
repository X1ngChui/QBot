"""Durable two-account ownership challenges for self-service identity linking."""

from __future__ import annotations

import uuid
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum

import asyncpg

from qqbot.domain.ids import GroupId
from qqbot.domain.identity import IdentityAccount
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.identity import lock_identity_topology


class LinkStatus(StrEnum):
    PENDING = "pending"
    APPLIED = "applied"
    CANCELLED = "cancelled"
    EXPIRED = "expired"


@dataclass(frozen=True, slots=True)
class LinkChallenge:
    id: uuid.UUID
    group_id: GroupId
    initiator_account_id: uuid.UUID
    target_account_id: uuid.UUID
    initiator_entity_id: uuid.UUID
    target_entity_id: uuid.UUID
    initiator_entity_revision: int
    target_entity_revision: int
    status: LinkStatus
    expires_at: datetime


class LinkChallengeError(ValueError):
    pass


class IdentityChanged(LinkChallengeError):
    pass


class IdentityLinkRepository:
    """Challenge state and atomic finalization around the shared union primitive."""

    def __init__(self, *, database: Callable[[], asyncpg.Pool]) -> None:
        self._database = database

    @staticmethod
    def _challenge(row) -> LinkChallenge:
        return LinkChallenge(
            id=row["id"],
            group_id=GroupId(row["group_id"]),
            initiator_account_id=row["initiator_account_id"],
            target_account_id=row["target_account_id"],
            initiator_entity_id=row["initiator_entity_id"],
            target_entity_id=row["target_entity_id"],
            initiator_entity_revision=row["initiator_entity_revision"],
            target_entity_revision=row["target_entity_revision"],
            status=LinkStatus(row["status"]),
            expires_at=row["expires_at"],
        )

    async def issue(
        self,
        *,
        group_id: GroupId,
        initiator: IdentityAccount,
        target: IdentityAccount,
        created_event_id: uuid.UUID,
        expires_at: datetime,
        max_pending: int,
    ) -> LinkChallenge:
        async with self._database().acquire() as conn, conn.transaction():
            await conn.execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
                "account-link-capacity",
            )
            await lock_identity_topology(conn)
            accounts = await conn.fetch(
                """SELECT id, entity_id FROM identity_account
                    WHERE id=ANY($1::uuid[]) ORDER BY id FOR UPDATE""",
                [initiator.id, target.id],
            )
            if len(accounts) != 2:
                raise LinkChallengeError("账号记录已变化，请重新发起。")
            current = {row["id"]: row["entity_id"] for row in accounts}
            initiator_entity_id = current[initiator.id]
            target_entity_id = current[target.id]
            if initiator_entity_id == target_entity_id:
                raise LinkChallengeError("这两个账号已经关联。")
            entities = await conn.fetch(
                """SELECT id, revision FROM entity
                    WHERE id=ANY($1::uuid[]) ORDER BY id FOR UPDATE""",
                [initiator_entity_id, target_entity_id],
            )
            if len(entities) != 2:
                raise LinkChallengeError("账号关联关系已变化，请重新发起。")
            revisions = {row["id"]: row["revision"] for row in entities}
            await conn.execute(
                """UPDATE account_link_challenge
                      SET status='expired'
                    WHERE status='pending' AND expires_at <= clock_timestamp()"""
            )
            pending = await conn.fetchval(
                "SELECT count(*) FROM account_link_challenge WHERE status='pending'"
            )
            if pending >= max_pending:
                raise LinkChallengeError("当前待确认的账号关联请求过多，请稍后重试。")
            occupied = await conn.fetchval(
                """SELECT EXISTS (
                    SELECT 1 FROM account_link_challenge WHERE group_id=$1 AND status='pending'
                      AND (initiator_account_id=ANY($2::uuid[])
                           OR target_account_id=ANY($2::uuid[]))
                )""",
                group_id.to_db(),
                [initiator.id, target.id],
            )
            if occupied:
                raise LinkChallengeError("相关账号在本群已有待处理的关联邀请，请先处理该邀请。")
            row = await conn.fetchrow(
                """INSERT INTO account_link_challenge
                       (group_id, initiator_account_id, target_account_id,
                        initiator_entity_id, target_entity_id,
                        initiator_entity_revision, target_entity_revision,
                        created_event_id, expires_at, created_at)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,clock_timestamp())
                RETURNING *""",
                group_id.to_db(),
                initiator.id,
                target.id,
                initiator_entity_id,
                target_entity_id,
                revisions[initiator_entity_id],
                revisions[target_entity_id],
                created_event_id,
                expires_at,
            )
            return self._challenge(row)

    async def confirm(
        self,
        *,
        group_id: GroupId,
        actor: IdentityAccount,
        confirmed_event_id: uuid.UUID,
        identities: IdentityRepository,
    ) -> LinkChallenge:
        invalidated: LinkChallengeError | None = None
        async with self._database().acquire() as conn, conn.transaction():
            await lock_identity_topology(conn)
            event_time = await conn.fetchval(
                """SELECT created_at FROM raw_event WHERE id=$1 AND group_id=$2
                      AND platform='qq' AND platform_user_id=$3""",
                confirmed_event_id,
                group_id.to_db(),
                actor.platform_user_id,
            )
            if event_time is None:
                raise LinkChallengeError("确认消息不属于本群当前账号，无法确认邀请。")
            replay = await conn.fetchrow(
                """SELECT * FROM account_link_challenge WHERE confirmed_event_id=$1
                      AND group_id=$2 AND target_account_id=$3 AND status='applied'""",
                confirmed_event_id,
                group_id.to_db(),
                actor.id,
            )
            if replay is not None:
                return self._challenge(replay)
            row = await conn.fetchrow(
                """WITH candidate AS MATERIALIZED (
                    SELECT * FROM account_link_challenge
                    WHERE group_id=$1 AND target_account_id=$2 AND status='pending'
                      AND created_at <= $3 FOR UPDATE
                ) SELECT candidate.*,expires_at <= clock_timestamp() AS expired
                    FROM candidate""",
                group_id.to_db(),
                actor.id,
                event_time,
            )
            if row is None:
                raise LinkChallengeError("本群没有该账号可确认的待处理邀请。")

            invalidated = await self._invalidation(conn, row)
            if invalidated is not None:
                await conn.execute(
                    """UPDATE account_link_challenge
                          SET status='expired' WHERE id=$1""",
                    row["id"],
                )
            else:
                await identities.merge(
                    row["initiator_entity_id"],
                    row["target_entity_id"],
                    _conn=conn,
                )
                result = await conn.fetchrow(
                    """UPDATE account_link_challenge
                          SET status='applied', confirmed_event_id=$2,
                              confirmed_at=NOW()
                        WHERE id=$1
                    RETURNING *""",
                    row["id"],
                    confirmed_event_id,
                )
                return self._challenge(result)

        raise invalidated

    @staticmethod
    async def _invalidation(conn, row) -> LinkChallengeError | None:
        if row["expired"]:
            return LinkChallengeError("关联邀请已过期，请重新发起。")
        accounts = await conn.fetch(
            """SELECT id,entity_id FROM identity_account
                WHERE id=ANY($1::uuid[]) ORDER BY id FOR UPDATE""",
            [row["initiator_account_id"], row["target_account_id"]],
        )
        current = {item["id"]: item["entity_id"] for item in accounts}
        if (
            current.get(row["initiator_account_id"]) != row["initiator_entity_id"]
            or current.get(row["target_account_id"]) != row["target_entity_id"]
        ):
            return IdentityChanged("账号关联关系已变化，请重新发起。")
        entities = await conn.fetch(
            """SELECT id,revision FROM entity
                WHERE id=ANY($1::uuid[]) ORDER BY id FOR UPDATE""",
            [row["initiator_entity_id"], row["target_entity_id"]],
        )
        revisions = {item["id"]: item["revision"] for item in entities}
        if (
            revisions.get(row["initiator_entity_id"]) != row["initiator_entity_revision"]
            or revisions.get(row["target_entity_id"]) != row["target_entity_revision"]
        ):
            return IdentityChanged("账号关联关系已变化，请重新发起。")
        return None

    async def cancel(
        self,
        *,
        group_id: GroupId,
        actor: IdentityAccount,
        cancelled_event_id: uuid.UUID,
    ) -> bool:
        row = await self._database().fetchrow(
            """UPDATE account_link_challenge
                  SET status='cancelled', cancelled_at=clock_timestamp()
                WHERE group_id=$1 AND status='pending'
                  AND (initiator_account_id=$2 OR target_account_id=$2)
                  AND created_at <= (
                      SELECT created_at FROM raw_event WHERE id=$3 AND group_id=$1
                        AND platform='qq' AND platform_user_id=$4
                  )
            RETURNING id""",
            group_id.to_db(),
            actor.id,
            cancelled_event_id,
            actor.platform_user_id,
        )
        return row is not None
