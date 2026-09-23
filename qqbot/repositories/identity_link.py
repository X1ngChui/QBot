"""Durable two-account ownership challenges for self-service identity linking."""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum

from ..db import pool
from ..domain.ids import GroupId
from ..domain.identity import IdentityAccount
from .identity import IdentityRepository, lock_identity_topology


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
        token_hash: str,
        initiator: IdentityAccount,
        target: IdentityAccount,
        created_event_id: uuid.UUID,
        expires_at: datetime,
        max_pending: int,
        max_pending_per_account: int,
    ) -> LinkChallenge:
        async with pool().acquire() as conn, conn.transaction():
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
                    WHERE status='pending' AND expires_at <= NOW()"""
            )
            pending = await conn.fetchval(
                "SELECT count(*) FROM account_link_challenge WHERE status='pending'"
            )
            if pending >= max_pending:
                raise LinkChallengeError("当前待确认的账号关联请求过多，请稍后重试。")
            per_account = await conn.fetchrow(
                """SELECT
                       count(*) FILTER (
                           WHERE initiator_account_id=$1 OR target_account_id=$1
                       ) AS initiator_pending,
                       count(*) FILTER (
                           WHERE initiator_account_id=$2 OR target_account_id=$2
                       ) AS target_pending
                     FROM account_link_challenge
                    WHERE status='pending'""",
                initiator.id,
                target.id,
            )
            if max(per_account["initiator_pending"], per_account["target_pending"]) >= (
                max_pending_per_account
            ):
                raise LinkChallengeError("相关账号的待确认关联请求已达上限。")
            existing = await conn.fetchval(
                """SELECT 1 FROM account_link_challenge
                    WHERE group_id=$1 AND status='pending'
                      AND LEAST(initiator_account_id, target_account_id)
                          = LEAST($2::uuid, $3::uuid)
                      AND GREATEST(initiator_account_id, target_account_id)
                          = GREATEST($2::uuid, $3::uuid)""",
                group_id.to_db(),
                initiator.id,
                target.id,
            )
            if existing:
                raise LinkChallengeError("这两个账号已有待确认的关联请求。")
            row = await conn.fetchrow(
                """INSERT INTO account_link_challenge
                       (group_id, token_hash, initiator_account_id, target_account_id,
                        initiator_entity_id, target_entity_id,
                        initiator_entity_revision, target_entity_revision,
                        created_event_id, expires_at)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                RETURNING *""",
                group_id.to_db(),
                token_hash,
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
        token_hash: str,
        actor: IdentityAccount,
        confirmed_event_id: uuid.UUID,
        identities: IdentityRepository,
    ) -> LinkChallenge:
        invalidated: LinkChallengeError | None = None
        applied: LinkChallenge | None = None
        async with pool().acquire() as conn, conn.transaction():
            row = await conn.fetchrow(
                """SELECT * FROM account_link_challenge
                    WHERE token_hash=$1 FOR UPDATE""",
                token_hash,
            )
            if row is None or GroupId(row["group_id"]) != group_id:
                raise LinkChallengeError("验证码无效或不属于本群。")
            if actor.id != row["target_account_id"]:
                raise LinkChallengeError("只有被邀请的目标账号可以确认。")
            if row["status"] == LinkStatus.APPLIED.value:
                return self._challenge(row)
            if row["status"] != LinkStatus.PENDING.value:
                raise LinkChallengeError("该关联请求已失效。")

            if row["expires_at"] <= datetime.now(row["expires_at"].tzinfo):
                invalidated = LinkChallengeError("关联请求已过期，请重新发起。")
            else:
                await lock_identity_topology(conn)
                accounts = await conn.fetch(
                    """SELECT id, entity_id FROM identity_account
                        WHERE id=ANY($1::uuid[]) ORDER BY id FOR UPDATE""",
                    [row["initiator_account_id"], row["target_account_id"]],
                )
                current = {item["id"]: item["entity_id"] for item in accounts}
                if (
                    current.get(row["initiator_account_id"]) != row["initiator_entity_id"]
                    or current.get(row["target_account_id"]) != row["target_entity_id"]
                ):
                    invalidated = IdentityChanged("账号关联关系已变化，请重新发起。")
                else:
                    entities = await conn.fetch(
                        """SELECT id, revision FROM entity
                            WHERE id=ANY($1::uuid[]) ORDER BY id FOR UPDATE""",
                        [row["initiator_entity_id"], row["target_entity_id"]],
                    )
                    revisions = {item["id"]: item["revision"] for item in entities}
                    if (
                        revisions.get(row["initiator_entity_id"])
                        != row["initiator_entity_revision"]
                        or revisions.get(row["target_entity_id"]) != row["target_entity_revision"]
                    ):
                        invalidated = IdentityChanged("账号关联关系已变化，请重新发起。")

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
                applied = self._challenge(result)

        if invalidated is not None:
            raise invalidated
        if applied is None:
            raise RuntimeError("link confirmation completed without a result")
        return applied

    async def cancel(
        self,
        *,
        group_id: GroupId,
        token_hash: str,
        actor: IdentityAccount,
    ) -> bool:
        row = await pool().fetchrow(
            """UPDATE account_link_challenge
                  SET status='cancelled', cancelled_at=NOW()
                WHERE token_hash=$1 AND group_id=$2 AND status='pending'
                  AND $3 = ANY(ARRAY[initiator_account_id, target_account_id])
            RETURNING id""",
            token_hash,
            group_id.to_db(),
            actor.id,
        )
        return row is not None
