"""L1 persistence: people, accounts, names, and the evidence behind each name."""

from __future__ import annotations

import uuid
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from dataclasses import replace
from datetime import datetime

import asyncpg

from ..db import pool
from ..domain.ids import GroupId
from ..util import now_local, tz, tz_sql
from ..domain.identity import (
    Alias,
    AliasEvidence,
    AliasStatus,
    AliasType,
    Entity,
    EntityStatus,
    EntityType,
    EvidenceType,
    IdentityAccount,
    normalize,
    platform_weight,
    usage_weight,
)
from ..domain.identity.alias import CONFIRM_THRESHOLD


#: Every entity id that resolves to a given one: itself, plus everything ever merged
#: into it, transitively. Merging deliberately does not rewrite the rows that point at
#: the loser - that would erase which person a record was originally filed under, which
#: is the only thing that makes a split recoverable - so every read follows the pointer
#: instead. A read that skips this sees only half of a merged person.
FAMILY = """WITH RECURSIVE family AS (
                   SELECT id FROM entity WHERE id = {arg}
                   UNION ALL
                   SELECT e.id FROM entity e JOIN family f ON e.merged_into = f.id
               )"""


def _entity(row) -> Entity:
    return Entity(
        id=row["id"],
        entity_type=EntityType(row["entity_type"]),
        canonical_name=row["canonical_name"],
        status=EntityStatus(row["status"]),
        merged_into=row["merged_into"],
        revision=row["revision"],
        created_at=row["created_at"],
        updated_at=row["updated_at"],
    )


def _alias(row) -> Alias:
    return Alias(
        id=row["id"],
        alias_text=row["alias_text"],
        target_entity_id=row["target_entity_id"],
        target_account_id=row["target_account_id"],
        group_id=GroupId(row["group_id"]) if row["group_id"] is not None else None,
        alias_type=AliasType(row["alias_type"]) if row["alias_type"] else AliasType.NICKNAME,
        confidence=row["confidence"],
        status=AliasStatus(row["status"]),
        valid_from=row["valid_from"],
        valid_to=row["valid_to"],
        last_used_at=row["last_used_at"],
    )


@asynccontextmanager
async def _write_connection(
    conn: asyncpg.Connection | None,
) -> AsyncIterator[asyncpg.Connection]:
    """Use a caller transaction or own one for an ordinary repository write."""

    if conn is not None:
        yield conn
        return
    async with pool().acquire() as owned, owned.transaction():
        yield owned


async def lock_identity_topology(conn: asyncpg.Connection) -> None:
    """Serialize the tiny identity graph while a union, detach, or snapshot runs."""

    await conn.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        "identity-topology",
    )


class IdentityRepository:
    """People, accounts and names.

    Every method that looks a name up requires group_id: which group a name holds in is a
    property of the name, not a detail a caller may leave out. Global names come along
    through the `group_id IS NULL` in the SQL, so no caller has to know they exist.
    """

    # -- people and accounts ----------------------------------------------
    async def account_of(self, platform: str, user_id: str) -> IdentityAccount | None:
        row = await pool().fetchrow(
            """SELECT id, entity_id, platform, platform_user_id, first_seen_at, last_seen_at
                 FROM identity_account WHERE platform=$1 AND platform_user_id=$2""",
            platform,
            user_id,
        )
        return IdentityAccount(**dict(row)) if row else None

    async def account_by_id(self, account_id: uuid.UUID) -> IdentityAccount | None:
        row = await pool().fetchrow(
            """SELECT id, entity_id, platform, platform_user_id,
                      first_seen_at, last_seen_at
                 FROM identity_account WHERE id=$1""",
            account_id,
        )
        return IdentityAccount(**dict(row)) if row else None

    async def account_names(self, group_id: GroupId) -> list[tuple[IdentityAccount, str]]:
        """Confirmed literal names that identify one exact account in this group."""

        rows = await pool().fetch(
            """SELECT ia.id, ia.entity_id, ia.platform, ia.platform_user_id,
                      ia.first_seen_at, ia.last_seen_at, a.alias_text
                 FROM alias a
                 JOIN identity_account ia ON ia.id=a.target_account_id
                WHERE (a.group_id=$1 OR a.group_id IS NULL)
                  AND a.status='confirmed' AND a.valid_to IS NULL
                ORDER BY length(a.alias_text) DESC, a.alias_text, ia.id""",
            group_id.to_db(),
        )
        return [
            (
                IdentityAccount(
                    id=row["id"],
                    entity_id=row["entity_id"],
                    platform=row["platform"],
                    platform_user_id=row["platform_user_id"],
                    first_seen_at=row["first_seen_at"],
                    last_seen_at=row["last_seen_at"],
                ),
                row["alias_text"],
            )
            for row in rows
        ]

    async def ensure_account(
        self,
        platform: str,
        user_id: str,
        *,
        seen_at: datetime,
        name: str | None = None,
        _conn: asyncpg.Connection | None = None,
    ) -> IdentityAccount:
        """Seeing an account guarantees it has an owner.

        The first sighting creates the person along with the account: an
        account always belongs to somebody, even when that somebody currently consists of
        this one account. Leaving an ownerless account behind defers the question of who
        they are to a moment with no context left to answer it.
        """
        if _conn is not None:
            return await self._ensure_account(
                platform,
                user_id,
                seen_at=seen_at,
                name=name,
                _conn=_conn,
            )
        try:
            return await self._ensure_account(
                platform,
                user_id,
                seen_at=seen_at,
                name=name,
            )
        except asyncpg.UniqueViolationError:
            # Lost a first-sighting race (the same new account speaking in two groups
            # at once): the winner's row exists now, and the rollback took this call's
            # entity with it. Rerun lands on the row-exists path.
            return await self._ensure_account(platform, user_id, seen_at=seen_at, name=name)

    async def _ensure_account(
        self,
        platform: str,
        user_id: str,
        *,
        seen_at: datetime,
        name: str | None = None,
        _conn: asyncpg.Connection | None = None,
    ) -> IdentityAccount:
        async with _write_connection(_conn) as conn:
            await conn.execute(
                "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
                f"{platform}:{user_id}",
            )
            row = await conn.fetchrow(
                """SELECT id, entity_id, platform, platform_user_id,
                          first_seen_at, last_seen_at
                     FROM identity_account
                    WHERE platform=$1 AND platform_user_id=$2 FOR UPDATE""",
                platform,
                user_id,
            )
            if row:
                await conn.execute(
                    "UPDATE identity_account SET last_seen_at=$2 WHERE id=$1",
                    row["id"],
                    seen_at,
                )
                return IdentityAccount(**dict(row) | {"last_seen_at": seen_at})

            ent = await conn.fetchrow(
                """INSERT INTO entity (entity_type, canonical_name)
                   VALUES ('person', $1) RETURNING id""",
                name,
            )
            acc = await conn.fetchrow(
                """INSERT INTO identity_account
                       (entity_id, platform, platform_user_id, first_seen_at, last_seen_at)
                   VALUES ($1, $2, $3, $4, $4)
                RETURNING id, entity_id, platform, platform_user_id,
                          first_seen_at, last_seen_at""",
                ent["id"],
                platform,
                user_id,
                seen_at,
            )
            return IdentityAccount(**dict(acc))

    #: The namespace a group's own account lives in. Not "qq": a group id and an account
    #: id are different numbers from different spaces, and one table holding both needs to
    #: be able to say which.
    GROUP_PLATFORM = "qq-group"

    async def group_entity(
        self,
        group_id: GroupId,
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> uuid.UUID:
        """The group itself, as an entity.

        A group is a thing the system knows facts about - what it is for, what its words
        mean - and once that is admitted, there is no reason for those facts to live in a
        different store from everything else. Making the group an entity gets them the
        whole model for free: evidence, supersession, validity windows, decay, and the
        same command that deletes a wrong fact about a person.

        It reuses identity_account rather than inventing a key, so the uniqueness that
        makes an account resolvable makes a group resolvable too.
        """
        async with _write_connection(_conn) as conn:
            # One creator per group at a time: two first calls racing here would
            # both insert an entity, and the loser's would stay behind as an orphan
            # no account references.
            await conn.execute("SELECT pg_advisory_xact_lock($1)", group_id.to_db())
            row = await conn.fetchrow(
                """SELECT entity_id FROM identity_account
                    WHERE platform=$1 AND platform_user_id=$2""",
                self.GROUP_PLATFORM,
                group_id,
            )
            if row:
                return row["entity_id"]
            ent = await conn.fetchval(
                """INSERT INTO entity (entity_type, canonical_name)
                   VALUES ('group', $1) RETURNING id""",
                group_id,
            )
            await conn.execute(
                """INSERT INTO identity_account
                       (entity_id, platform, platform_user_id, first_seen_at, last_seen_at)
                   VALUES ($1,$2,$3,NOW(),NOW())""",
                ent,
                self.GROUP_PLATFORM,
                group_id,
            )
            return ent

    async def entity(self, entity_id: uuid.UUID) -> Entity | None:
        """Read one person, following the merge pointer.

        The caller always gets the live one. After a merge, explicit holder-scoped
        records may still reference an old root; centralizing pointer traversal prevents
        one missed call site from splitting an equivalence class.
        """
        row = await pool().fetchrow(
            """WITH RECURSIVE chase AS (
                   SELECT * FROM entity WHERE id = $1
                   UNION ALL
                   SELECT e.* FROM entity e JOIN chase c ON e.id = c.merged_into
               )
               SELECT * FROM chase WHERE merged_into IS NULL LIMIT 1""",
            entity_id,
        )
        return _entity(row) if row else None

    async def merge_accounts(
        self,
        left_account_id: uuid.UUID,
        right_account_id: uuid.UUID,
    ) -> tuple[uuid.UUID, bool]:
        """Lock two exact accounts, then union their current holder roots."""

        if left_account_id == right_account_id:
            account = await self.account_by_id(left_account_id)
            if account is None:
                raise LookupError(f"unknown identity account {left_account_id}")
            return account.entity_id, False
        async with pool().acquire() as conn, conn.transaction():
            await lock_identity_topology(conn)
            rows = await conn.fetch(
                """SELECT id, entity_id FROM identity_account
                    WHERE id=ANY($1::uuid[]) ORDER BY id FOR UPDATE""",
                [left_account_id, right_account_id],
            )
            if len(rows) != 2:
                raise LookupError("identity account disappeared during merge")
            roots = {row["id"]: row["entity_id"] for row in rows}
            left = roots[left_account_id]
            right = roots[right_account_id]
            if left == right:
                return left, False
            return await self.merge(left, right, _conn=conn), True

    async def merge(
        self,
        left: uuid.UUID,
        right: uuid.UUID,
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> uuid.UUID:
        """Union two live holder roots and return the deterministic survivor."""

        if left == right:
            return left
        async with _write_connection(_conn) as conn:
            await lock_identity_topology(conn)
            rows = await conn.fetch(
                """SELECT id, created_at, merged_into
                     FROM entity
                    WHERE id=ANY($1::uuid[])
                    ORDER BY id
                    FOR UPDATE""",
                [left, right],
            )
            if len(rows) != 2 or any(row["merged_into"] is not None for row in rows):
                raise ValueError("identity roots changed during merge")
            ordered = sorted(rows, key=lambda row: (row["created_at"], row["id"]))
            winner, loser = ordered[0]["id"], ordered[1]["id"]
            await conn.execute(
                "UPDATE identity_account SET entity_id=$2 WHERE entity_id=$1",
                loser,
                winner,
            )
            await conn.execute(
                """UPDATE entity
                      SET revision=revision+1, updated_at=NOW()
                    WHERE id=$1""",
                winner,
            )
            await conn.execute(
                """UPDATE entity
                      SET status='merged', merged_into=$2,
                          revision=revision+1, updated_at=NOW()
                    WHERE id=$1""",
                loser,
                winner,
            )
            return winner

    async def split(
        self,
        account: IdentityAccount,
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> uuid.UUID:
        """Detach one exact account while every other linked account stays together."""

        async with _write_connection(_conn) as conn:
            await lock_identity_topology(conn)
            current = await conn.fetchrow(
                """SELECT id, entity_id, platform, platform_user_id,
                          first_seen_at, last_seen_at
                     FROM identity_account WHERE id=$1 FOR UPDATE""",
                account.id,
            )
            if current is None:
                raise LookupError(f"unknown identity account {account.id}")
            await conn.execute(
                "SELECT id FROM entity WHERE id=$1 FOR UPDATE",
                current["entity_id"],
            )
            count = await conn.fetchval(
                "SELECT count(*) FROM identity_account WHERE entity_id=$1",
                current["entity_id"],
            )
            if count < 2:
                raise ValueError("account is not linked")
            new_id = await conn.fetchval(
                """INSERT INTO entity (entity_type, canonical_name)
                   VALUES ('person', $1) RETURNING id""",
                current["platform_user_id"],
            )
            await conn.execute(
                "UPDATE identity_account SET entity_id=$2 WHERE id=$1",
                account.id,
                new_id,
            )
            await conn.execute(
                """UPDATE entity
                      SET revision=revision+1, updated_at=NOW()
                    WHERE id=$1""",
                current["entity_id"],
            )
            return new_id

    async def accounts_of(self, entity_id: uuid.UUID) -> list[IdentityAccount]:
        rows = await pool().fetch(
            """SELECT id, entity_id, platform, platform_user_id, first_seen_at, last_seen_at
                 FROM identity_account WHERE entity_id=$1 ORDER BY first_seen_at""",
            entity_id,
        )
        return [IdentityAccount(**dict(r)) for r in rows]

    # -- names ------------------------------------------------------------
    async def aliases_for_account(self, group_id: GroupId, account_id: uuid.UUID) -> list[Alias]:
        """Names attached to one exact account, strongest evidence first."""

        rows = await pool().fetch(
            """SELECT * FROM alias
                WHERE target_account_id=$2
                  AND (group_id=$1 OR group_id IS NULL)
                  AND status <> 'inactive' AND valid_to IS NULL
                ORDER BY confidence DESC, alias_text""",
            group_id.to_db(),
            account_id,
        )
        return [_alias(r) for r in rows]

    async def aliases_for(self, group_id: GroupId, entity_id: uuid.UUID) -> list[Alias]:
        """Names shared by a holder or attached to any currently linked account."""

        rows = await pool().fetch(
            FAMILY.format(arg="$2")
            + """
               SELECT DISTINCT a.* FROM alias a
                 LEFT JOIN family f ON a.target_entity_id = f.id
                 LEFT JOIN identity_account ia ON a.target_account_id = ia.id
                WHERE (a.group_id=$1 OR a.group_id IS NULL)
                  AND a.status <> 'inactive' AND a.valid_to IS NULL
                  AND (f.id IS NOT NULL OR ia.entity_id=$2)
                ORDER BY a.confidence DESC, a.alias_text""",
            group_id.to_db(),
            entity_id,
        )
        return [_alias(r) for r in rows]

    async def lookup(self, group_id: GroupId, text: str) -> list[Alias]:
        """Who this name might mean in this group.

        More than one row means it is ambiguous, and what to do about that is the
        caller's decision. Nothing here picks one: the cost of picking wrong - a record
        filed under the wrong person - is far higher than the cost of not answering.
        """
        rows = await pool().fetch(
            """SELECT * FROM alias
                WHERE normalized_text=$2
                  AND (group_id=$1 OR group_id IS NULL)
                  AND status='confirmed' AND valid_to IS NULL
                ORDER BY confidence DESC, alias_text""",
            group_id.to_db(),
            normalize(text),
        )
        out = []
        for row in rows:
            alias = _alias(row)
            if alias.target_entity_id is not None:
                live = await self.entity(alias.target_entity_id)
                if live is not None and live.id != alias.target_entity_id:
                    alias = replace(alias, target_entity_id=live.id)
            out.append(alias)
        return out

    async def holder_for_alias(self, alias: Alias) -> uuid.UUID:
        """Resolve either alias target shape to its current holder."""

        if alias.target_account_id is not None:
            entity_id = await pool().fetchval(
                "SELECT entity_id FROM identity_account WHERE id=$1",
                alias.target_account_id,
            )
            if entity_id is None:
                raise LookupError(f"unknown alias account {alias.target_account_id}")
            return entity_id
        live = await self.entity(alias.target_entity_id)
        if live is None:
            raise LookupError(f"unknown alias holder {alias.target_entity_id}")
        return live.id

    #: The evidence kinds the platform reports on every message. Their weight depends on
    #: endurance, not on arrival - see domain.platform_weight.
    _PLATFORM_EV = (EvidenceType.GROUP_CARD.value, EvidenceType.PLATFORM_IDENTITY.value)

    async def upsert_alias(
        self,
        alias: Alias,
        evidence: list[AliasEvidence],
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> Alias:
        """Write a name, or strengthen one, recording the evidence alongside it.

        Confidence and status are recomputed from the evidence by the domain object
        (Alias.scored); this only persists the result. The rule lives somewhere it can be
        tested on its own rather than spread through SQL.

        Three write rules live here because they need the stored trail:

        - A platform name's evidence is scored by how many distinct days it has been
          seen, so a card worn for three minutes of a renaming game enters at candidate
          weight and only a card that endures into a second day confirms.
        - A name an owner struck out stays struck out against automatic evidence: the
          platform re-reports the nickname on every message, and letting that rescore
          the row would walk back the owner's decision. The pin is the MANUAL row
          retire_alias leaves behind, not the inactive status by itself - a name that
          merely aged out of use carries no pin, so the next sighting revives it.
        - At most one MANUAL evidence row exists per alias, replaced on each manual
          action, so the owner's latest decision is the one the domain reads - setting
          0.4 after 1.0 must mean 0.4, which a max over history cannot express.
        """
        if _conn is not None:
            return await self._upsert_alias(alias, evidence, _conn=_conn)
        try:
            return await self._upsert_alias(alias, evidence)
        except asyncpg.UniqueViolationError:
            # Lost a first-sighting race on alias_unique_in_scope (the same new name
            # arriving on two concurrent messages): the winner's row exists now, and
            # the rollback took this call's insert with it. Rerun lands on the
            # row-exists path and strengthens the winner instead.
            return await self._upsert_alias(alias, evidence)

    async def _upsert_alias(
        self,
        alias: Alias,
        evidence: list[AliasEvidence],
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> Alias:
        async with _write_connection(_conn) as conn:
            row = await conn.fetchrow(
                """SELECT * FROM alias
                    WHERE COALESCE(group_id, 0)=COALESCE($1::bigint, 0)
                      AND normalized_text=$2
                      AND target_entity_id IS NOT DISTINCT FROM $3
                      AND target_account_id IS NOT DISTINCT FROM $4
                    FOR UPDATE""",
                alias.group_id.to_db() if alias.group_id is not None else None,
                alias.normalized_text,
                alias.target_entity_id,
                alias.target_account_id,
            )
            manual_in = any(e.evidence_type is EvidenceType.MANUAL for e in evidence)

            if (
                row is not None
                and not manual_in
                and len(evidence) == 1
                and evidence[0].evidence_type.value in self._PLATFORM_EV
            ):
                # The per-message fast path. The platform re-reports the card and the
                # nickname on every message, and nothing scored here can move within a
                # day: day-stability counts only days *before* today, the user count
                # ignores platform evidence entirely, and channel-max fusion is blind
                # to a duplicate of a row already in the trail. So a sighting already
                # filed today only bumps last_used_at - no trail row, no full-trail
                # fetch, no rescore - where every message would otherwise add two
                # trail rows and pay aggregates over all of them.
                last = await conn.fetchval(
                    """SELECT max(created_at) FROM alias_evidence
                        WHERE alias_id=$1 AND evidence_type=$2""",
                    row["id"],
                    evidence[0].evidence_type.value,
                )
                if last is not None and last.astimezone(tz()).date() == now_local().date():
                    await conn.execute("UPDATE alias SET last_used_at=NOW() WHERE id=$1", row["id"])
                    return _alias(row)

            if (
                row is not None
                and row["status"] == "inactive"
                and not manual_in
                and await conn.fetchval(
                    """SELECT 1 FROM alias_evidence
                            WHERE alias_id=$1 AND evidence_type=$2 LIMIT 1""",
                    row["id"],
                    EvidenceType.MANUAL.value,
                )
            ):
                # Struck out by hand: dead stays dead. The sighting is still written
                # down - the trail should say the name kept appearing - but it moves
                # nothing. An inactive row without the pin was retired by decay, and
                # falls through: the name is being used again, so it re-enters as a
                # candidate and earns its way back like any other sighting.
                for ev in evidence:
                    await conn.execute(
                        """INSERT INTO alias_evidence
                               (alias_id, raw_event_id, evidence_type, evidence_score)
                           VALUES ($1,$2,$3,$4)""",
                        row["id"],
                        ev.raw_event_id,
                        ev.evidence_type.value,
                        ev.score,
                    )
                return _alias(row)

            if row is not None:
                evidence = await self._stability_scored(conn, row["id"], evidence)
                if manual_in:
                    # The owner's new verdict replaces the old one, and revives the row
                    # if it was retired: a manual action on a dead name is what bringing
                    # it back deliberately looks like.
                    await conn.execute(
                        "DELETE FROM alias_evidence WHERE alias_id=$1 AND evidence_type=$2",
                        row["id"],
                        EvidenceType.MANUAL.value,
                    )
                current = _alias(row)
                if current.status is AliasStatus.INACTIVE:
                    # Revived, whether by the owner or by a fresh sighting of a name
                    # decay had retired: the validity window reopens and the rescore
                    # below decides what it is worth now.
                    await conn.execute(
                        "UPDATE alias SET valid_to=NULL WHERE id=$1",
                        row["id"],
                    )
                    current = replace(current, status=AliasStatus.CANDIDATE, valid_to=None)
                # The stored MANUAL row rides along into scoring - it is what keeps an
                # owner's earlier verdict authoritative over this sighting. When this
                # call is itself manual, the old row was deleted above and the new one
                # arrives in `evidence`.
                old_ev = await conn.fetch(
                    "SELECT evidence_type, evidence_score FROM alias_evidence WHERE alias_id=$1",
                    row["id"],
                )
                merged = [
                    AliasEvidence(EvidenceType(e["evidence_type"]), score=e["evidence_score"])
                    for e in old_ev
                ] + evidence
                scored = current.scored(merged)
                await conn.execute(
                    """UPDATE alias SET confidence=$2, status=$3, last_used_at=NOW(),
                                        updated_at=NOW()
                        WHERE id=$1""",
                    row["id"],
                    scored.confidence,
                    scored.status.value,
                )
                alias_id = row["id"]
                out = scored
                trail = merged
            else:
                evidence = await self._stability_scored(conn, None, evidence)
                scored = alias.scored(evidence)
                alias_id = await conn.fetchval(
                    """INSERT INTO alias
                           (alias_text, normalized_text, target_entity_id, target_account_id,
                            group_id, alias_type, confidence, status, valid_from, last_used_at)
                       VALUES ($1,$2,$3,$4,$5,$6,$7,$8,NOW(),NOW())
                    RETURNING id""",
                    scored.alias_text,
                    scored.normalized_text,
                    scored.target_entity_id,
                    scored.target_account_id,
                    scored.group_id.to_db() if scored.group_id is not None else None,
                    scored.alias_type.value,
                    scored.confidence,
                    scored.status.value,
                )
                out = scored
                trail = evidence

            for ev in evidence:
                await conn.execute(
                    """INSERT INTO alias_evidence
                           (alias_id, raw_event_id, evidence_type, evidence_score)
                       VALUES ($1,$2,$3,$4)""",
                    alias_id,
                    ev.raw_event_id,
                    ev.evidence_type.value,
                    ev.score,
                )

            if manual_in or await conn.fetchval(
                "SELECT 1 FROM alias_evidence WHERE alias_id=$1 AND evidence_type=$2 LIMIT 1",
                alias_id,
                EvidenceType.MANUAL.value,
            ):
                # The owner's number is final until their next action; the usage recount
                # below must not outvote it.
                return out

            # How many different people have been seen using this name. It is counted
            # from the evidence trail rather than stored, because every piece of evidence
            # already points at a message and every message has a sender - the answer was
            # always in there, and a stored counter would be a second thing to keep true.
            #
            # This promotes an observed name into an identity key. Candidate names
            # remain visible as context, but cannot resolve accounts until confirmed.
            users = (
                await conn.fetchval(
                    """SELECT count(DISTINCT r.platform_user_id)
                     FROM alias_evidence ae JOIN raw_event r ON r.id = ae.raw_event_id
                    WHERE ae.alias_id = $1
                      AND ae.evidence_type IN ('llm_inference','speaker_usage',
                                               'multi_user_usage')""",
                    alias_id,
                )
                or 0
            )
            if users:
                # The whole trail plus the fresh usage row, never the usage row
                # alone: scored() fuses only what it is handed, so a single-row
                # rescore would replace the multi-channel fusion just computed
                # above with the bare usage weight. Channel-max inside scored()
                # keeps the fresh count ahead of any older multi-user rows
                # already in the trail.
                out = out.scored(
                    trail
                    + [AliasEvidence(EvidenceType.MULTI_USER_USAGE, score=usage_weight(users))]
                )
                await conn.execute(
                    """UPDATE alias SET confidence=$2, status=$3, updated_at=NOW()
                        WHERE id=$1""",
                    alias_id,
                    out.confidence,
                    out.status.value,
                )
            return out

    @staticmethod
    async def _stability_scored(
        conn,
        alias_id: uuid.UUID | None,
        evidence: list[AliasEvidence],
    ) -> list[AliasEvidence]:
        """Platform evidence rescored by how many distinct days the name has been worn.

        The trail is already there - every sighting files an evidence row - so endurance
        is counted, not stored. A first-day name computes to one day and enters below the
        confirmation line; see domain.platform_weight for why.
        """
        kinds = {EvidenceType.GROUP_CARD, EvidenceType.PLATFORM_IDENTITY}
        if not any(e.evidence_type in kinds for e in evidence):
            return evidence
        days = 1
        if alias_id is not None:
            zone = tz_sql()
            days = 1 + (
                await conn.fetchval(
                    """SELECT count(DISTINCT (created_at AT TIME ZONE $3)::date)
                     FROM alias_evidence
                    WHERE alias_id=$1 AND evidence_type = ANY($2::text[])
                      AND (created_at AT TIME ZONE $3)::date
                          < (NOW() AT TIME ZONE $3)::date""",
                    alias_id,
                    [k.value for k in kinds],
                    zone,
                )
                or 0
            )
        return [
            replace(e, score=platform_weight(e.evidence_type, days))
            if e.evidence_type in kinds and e.score is None
            else e
            for e in evidence
        ]

    async def set_alias_confidence(
        self,
        group_id: GroupId,
        entity_id: uuid.UUID,
        text: str,
        confidence: float,
    ) -> Alias | None:
        """Set matching aliases across one linked holder view."""

        rows = await pool().fetch(
            FAMILY.format(arg="$2")
            + """
               SELECT DISTINCT a.* FROM alias a
                 LEFT JOIN family f ON a.target_entity_id = f.id
                 LEFT JOIN identity_account ia ON a.target_account_id = ia.id
                WHERE a.group_id=$1 AND a.normalized_text=$3
                  AND (f.id IS NOT NULL OR ia.entity_id=$2)""",
            group_id.to_db(),
            entity_id,
            normalize(text),
        )
        return await self._set_alias_confidence(rows, confidence)

    async def set_account_alias_confidence(
        self,
        group_id: GroupId,
        account_id: uuid.UUID,
        text: str,
        confidence: float,
    ) -> Alias | None:
        """Set one exact account alias confidence."""

        rows = await pool().fetch(
            """SELECT * FROM alias
                WHERE group_id=$1 AND target_account_id=$2 AND normalized_text=$3""",
            group_id.to_db(),
            account_id,
            normalize(text),
        )
        return await self._set_alias_confidence(rows, confidence)

    @staticmethod
    async def _set_alias_confidence(rows, confidence: float) -> Alias | None:
        if not rows:
            return None
        status = "confirmed" if confidence >= CONFIRM_THRESHOLD else "candidate"
        async with pool().acquire() as conn, conn.transaction():
            out = None
            for source in rows:
                row = await conn.fetchrow(
                    "SELECT * FROM alias WHERE id=$1 FOR UPDATE", source["id"]
                )
                await conn.execute(
                    "DELETE FROM alias_evidence WHERE alias_id=$1 AND evidence_type=$2",
                    row["id"],
                    EvidenceType.MANUAL.value,
                )
                await conn.execute(
                    """INSERT INTO alias_evidence (alias_id, evidence_type, evidence_score)
                       VALUES ($1,$2,$3)""",
                    row["id"],
                    EvidenceType.MANUAL.value,
                    confidence,
                )
                await conn.execute(
                    """UPDATE alias SET confidence=$2, status=$3, valid_to=NULL,
                                        updated_at=NOW()
                        WHERE id=$1""",
                    row["id"],
                    confidence,
                    status,
                )
                out = _alias(row)
            return replace(
                out,
                confidence=confidence,
                status=AliasStatus(status),
                valid_to=None,
            )

    async def retire_alias(self, alias_id: uuid.UUID) -> None:
        """No longer holds. Not deleted: messages already archived still need it to be
        readable.

        The zero-weight MANUAL row is the pin that makes retirement stick: the platform
        re-reports the name on the very next message, and without the pin that rescore
        would walk the retirement straight back.
        """
        async with pool().acquire() as conn, conn.transaction():
            await conn.execute(
                """UPDATE alias SET status='inactive', valid_to=NOW(), updated_at=NOW()
                    WHERE id=$1""",
                alias_id,
            )
            await conn.execute(
                "DELETE FROM alias_evidence WHERE alias_id=$1 AND evidence_type=$2",
                alias_id,
                EvidenceType.MANUAL.value,
            )
            await conn.execute(
                """INSERT INTO alias_evidence (alias_id, evidence_type, evidence_score)
                   VALUES ($1,$2,0.0)""",
                alias_id,
                EvidenceType.MANUAL.value,
            )

    async def decay_aliases(
        self, group_id: GroupId, *, unused_days: float, joke_days: float | None = None
    ) -> int:
        """Retire names that never became certain and stopped being used.

        Only candidates. A confirmed name is either something the platform reported,
        something entered explicitly, or something three different people were seen using -
        and none of those stops being true because nobody said it this month. A candidate
        is the model's guess, and a guess nobody has repeated since is the definition of
        one that did not pan out.

        A name the model marked as a joke gets a shorter window. Most of them are true for
        an afternoon, and the alias_type field would otherwise be one the model is asked
        to fill and nothing ever reads.

        A candidate carrying a MANUAL row is not a guess: an authorized caller set it
        below the confirmation line on purpose, and that verdict does not lapse for want
        of use.
        """
        rows = await pool().fetch(
            """UPDATE alias
                  SET status = 'inactive', valid_to = NOW(), updated_at = NOW()
                WHERE group_id = $1 AND status = 'candidate'
                  AND COALESCE(last_used_at, valid_from, created_at)
                      < NOW() - (CASE WHEN alias_type = 'joke_name'
                                      THEN COALESCE($3::float, $2::float)
                                      ELSE $2::float END * INTERVAL '1 day')
                  AND NOT EXISTS (SELECT 1 FROM alias_evidence
                                   WHERE alias_id = alias.id
                                     AND evidence_type = $4)
             RETURNING id""",
            group_id.to_db(),
            unused_days,
            joke_days,
            EvidenceType.MANUAL.value,
        )
        return len(rows)
