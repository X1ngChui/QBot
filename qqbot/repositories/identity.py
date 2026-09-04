"""L1/L2 persistence: people, accounts, names, and the trace behind each reference."""

from __future__ import annotations

import uuid
from dataclasses import replace
from datetime import datetime

import asyncpg

from ..db import pool
from ..util import now_local, tz, tz_sql
from ..domain.identity import (
    Alias, AliasEvidence, AliasStatus, AliasType, Entity, EntityStatus, EntityType,
    EvidenceType, IdentityAccount, normalize, platform_weight, usage_weight,
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
        group_id=row["group_id"],
        alias_type=AliasType(row["alias_type"]) if row["alias_type"] else AliasType.NICKNAME,
        confidence=row["confidence"],
        status=AliasStatus(row["status"]),
        valid_from=row["valid_from"],
        valid_to=row["valid_to"],
        last_used_at=row["last_used_at"],
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
            platform, user_id,
        )
        return IdentityAccount(**dict(row)) if row else None

    async def ensure_account(
        self, platform: str, user_id: str, *, seen_at: datetime, name: str | None = None
    ) -> IdentityAccount:
        """Seeing an account guarantees it has an owner.

        The first sighting creates the person along with the account (design doc 12): an
        account always belongs to somebody, even when that somebody currently consists of
        this one account. Leaving an ownerless account behind defers the question of who
        they are to a moment with no context left to answer it.
        """
        try:
            return await self._ensure_account(platform, user_id, seen_at=seen_at, name=name)
        except asyncpg.UniqueViolationError:
            # Lost a first-sighting race (the same new account speaking in two groups
            # at once): the winner's row exists now, and the rollback took this call's
            # entity with it. Rerun lands on the row-exists path.
            return await self._ensure_account(platform, user_id, seen_at=seen_at, name=name)

    async def _ensure_account(
        self, platform: str, user_id: str, *, seen_at: datetime, name: str | None = None
    ) -> IdentityAccount:
        async with pool().acquire() as conn, conn.transaction():
            row = await conn.fetchrow(
                """SELECT id, entity_id, platform, platform_user_id,
                          first_seen_at, last_seen_at
                     FROM identity_account
                    WHERE platform=$1 AND platform_user_id=$2 FOR UPDATE""",
                platform, user_id,
            )
            if row:
                await conn.execute(
                    "UPDATE identity_account SET last_seen_at=$2 WHERE id=$1",
                    row["id"], seen_at,
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
                ent["id"], platform, user_id, seen_at,
            )
            return IdentityAccount(**dict(acc))

    #: The namespace a group's own account lives in. Not "qq": a group id and an account
    #: id are different numbers from different spaces, and one table holding both needs to
    #: be able to say which.
    GROUP_PLATFORM = "qq-group"

    async def group_entity(self, group_id: int) -> uuid.UUID:
        """The group itself, as an entity.

        A group is a thing the system knows facts about - what it is for, what its words
        mean - and once that is admitted, there is no reason for those facts to live in a
        different store from everything else. Making the group an entity gets them the
        whole model for free: evidence, supersession, validity windows, decay, and the
        same command that deletes a wrong fact about a person.

        It reuses identity_account rather than inventing a key, so the uniqueness that
        makes an account resolvable makes a group resolvable too.
        """
        async with pool().acquire() as conn, conn.transaction():
            row = await conn.fetchrow(
                """SELECT entity_id FROM identity_account
                    WHERE platform=$1 AND platform_user_id=$2""",
                self.GROUP_PLATFORM, str(group_id),
            )
            if row:
                return row["entity_id"]
            ent = await conn.fetchval(
                """INSERT INTO entity (entity_type, canonical_name)
                   VALUES ('group', $1) RETURNING id""",
                str(group_id),
            )
            await conn.execute(
                """INSERT INTO identity_account
                       (entity_id, platform, platform_user_id, first_seen_at, last_seen_at)
                   VALUES ($1,$2,$3,NOW(),NOW())
                   ON CONFLICT (platform, platform_user_id) DO NOTHING""",
                ent, self.GROUP_PLATFORM, str(group_id),
            )
            # Re-read rather than return `ent`: on the conflict path a concurrent
            # call won the account row, and returning this call's own entity would
            # hand back an orphan no account references - facts written against it
            # would be invisible to every later read, permanently.
            return await conn.fetchval(
                """SELECT entity_id FROM identity_account
                    WHERE platform=$1 AND platform_user_id=$2""",
                self.GROUP_PLATFORM, str(group_id),
            )

    async def entity(self, entity_id: uuid.UUID) -> Entity | None:
        """Read one person, following the merge pointer.

        The caller always gets the live one. After a merge the old id is still scattered
        through historical facts and episode participants, and asking every caller to
        remember to follow it is a design where one missed spot mixes two people up.
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

    async def merge(self, loser: uuid.UUID, winner: uuid.UUID) -> None:
        """Fold loser into winner. Owner-triggered only (design doc 55).

        The accounts change hands and the entity is marked merged. Facts and aliases are
        left alone: the ids they point at still resolve, because reads follow the pointer.
        Rewriting them would make it impossible to say which person a record was
        originally filed under - which is the only thing that makes a split recoverable.
        """
        if loser == winner:
            raise ValueError("cannot merge an entity into itself")
        async with pool().acquire() as conn, conn.transaction():
            await conn.execute(
                "UPDATE identity_account SET entity_id=$2 WHERE entity_id=$1",
                loser, winner,
            )
            await conn.execute(
                """UPDATE entity
                      SET status='merged', merged_into=$2,
                          revision=revision+1, updated_at=NOW()
                    WHERE id=$1""",
                loser, winner,
            )

    async def split(self, account: IdentityAccount) -> uuid.UUID:
        """Detach one account from the person it is currently filed under.

        A wrong merge is the most damaging thing this system can do to itself (design
        doc 56): everything the two people ever said becomes one person's history, and
        nothing downstream can tell which half came from where. The undo has to exist,
        and it has to be evidence-driven rather than a guess.

        So the aliases move with the account only when the evidence says they belong to
        it: every supporting row must point at a raw event this account produced. A name
        somebody else used, or one written in by hand, stays with the original person -
        moving it would be inventing a fact rather than restoring one. Facts and episodes
        are left in place for the same reason; they carry their own evidence and are the
        offline repair worker's job, not a chat command's.

        Returns the id of the newly created person.
        """
        async with pool().acquire() as conn, conn.transaction():
            new_id = await conn.fetchval(
                """INSERT INTO entity (entity_type, canonical_name)
                   VALUES ('person', $1) RETURNING id""",
                account.platform_user_id,
            )
            await conn.execute(
                "UPDATE identity_account SET entity_id=$2 WHERE id=$1",
                account.id, new_id,
            )
            await conn.execute(
                FAMILY.format(arg="$1") + """
                 UPDATE alias SET target_entity_id=$3, updated_at=NOW()
                    WHERE target_entity_id IN (SELECT id FROM family)
                      AND id IN (
                          SELECT ae.alias_id
                            FROM alias_evidence ae
                            JOIN raw_event r ON r.id = ae.raw_event_id
                           GROUP BY ae.alias_id
                          HAVING bool_and(r.platform = $4
                                          AND r.platform_user_id = $2)
                      )
                      AND id NOT IN (
                          SELECT alias_id FROM alias_evidence
                           WHERE raw_event_id IS NULL
                      )""",
                account.entity_id, account.platform_user_id, new_id, account.platform,
            )
            return new_id

    async def rename(self, entity_id: uuid.UUID, name: str | None) -> None:
        """Set the name this person is filed under.

        Only a label for the operator's benefit: nothing resolves through it, because a
        canonical name that took part in matching would be a second alias table with no
        evidence behind it.
        """
        await pool().execute(
            """UPDATE entity SET canonical_name=$2, revision=revision+1, updated_at=NOW()
                WHERE id=$1""",
            entity_id, name,
        )

    async def accounts_of(self, entity_id: uuid.UUID) -> list[IdentityAccount]:
        rows = await pool().fetch(
            """SELECT id, entity_id, platform, platform_user_id, first_seen_at, last_seen_at
                 FROM identity_account WHERE entity_id=$1 ORDER BY first_seen_at""",
            entity_id,
        )
        return [IdentityAccount(**dict(r)) for r in rows]

    # -- names ------------------------------------------------------------
    async def aliases_for(self, group_id: int, entity_id: uuid.UUID) -> list[Alias]:
        """Every name this person answers to here, strongest evidence first.

        The tie-break on alias_text is not cosmetic. This list is rendered into the
        system block ahead of the history, and prefix caching only matches forward from
        the start - two names of equal confidence coming back in a different order
        between turns would invalidate the entire prompt behind them.
        """
        rows = await pool().fetch(
            FAMILY.format(arg="$2") + """
               SELECT a.* FROM alias a JOIN family ON a.target_entity_id = family.id
                WHERE (a.group_id=$1 OR a.group_id IS NULL)
                  AND a.status <> 'inactive'
                ORDER BY a.confidence DESC, a.alias_text""",
            group_id, entity_id,
        )
        return [_alias(r) for r in rows]

    async def lookup(self, group_id: int, text: str) -> list[Alias]:
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
            group_id, normalize(text),
        )
        out = []
        for row in rows:
            alias = _alias(row)
            # A name bound before a merge still points at the account it was bound to.
            # Handing that id back would resolve the name to a person who no longer
            # exists, which reads downstream as "nobody".
            live = await self.entity(alias.target_entity_id)
            if live is not None and live.id != alias.target_entity_id:
                alias = replace(alias, target_entity_id=live.id)
            out.append(alias)
        return out

    #: The evidence kinds the platform reports on every message. Their weight depends on
    #: endurance, not on arrival - see domain.platform_weight.
    _PLATFORM_EV = (EvidenceType.GROUP_CARD.value, EvidenceType.PLATFORM_IDENTITY.value)

    async def upsert_alias(self, alias: Alias, evidence: list[AliasEvidence]) -> Alias:
        """Write a name, or strengthen one, recording the evidence alongside it.

        Confidence and status are recomputed from the evidence by the domain object
        (Alias.scored); this only persists the result. The rule lives somewhere it can be
        tested on its own rather than spread through SQL.

        Three write rules live here because they need the stored trail:

        - A platform name's evidence is scored by how many distinct days it has been
          seen, so a card worn for three minutes of a renaming game enters at candidate
          weight and only a card that endures into a second day confirms.
        - A retired alias stays retired against automatic evidence: the platform
          re-reports the nickname on every message, and letting that rescore the row
          would walk back a name the owner just struck out. Only a manual action
          revives it.
        - At most one MANUAL evidence row exists per alias, replaced on each manual
          action, so the owner's latest decision is the one the domain reads - setting
          0.4 after 1.0 must mean 0.4, which a max over history cannot express.
        """
        async with pool().acquire() as conn, conn.transaction():
            row = await conn.fetchrow(
                """SELECT * FROM alias
                    WHERE COALESCE(group_id, 0)=COALESCE($1::bigint, 0)
                      AND normalized_text=$2 AND target_entity_id=$3
                    FOR UPDATE""",
                alias.group_id, alias.normalized_text, alias.target_entity_id,
            )
            manual_in = any(e.evidence_type is EvidenceType.MANUAL for e in evidence)

            if (row is not None and not manual_in and len(evidence) == 1
                    and evidence[0].evidence_type.value in self._PLATFORM_EV):
                # The per-message fast path. The platform re-reports the card and the
                # nickname on every message, and nothing scored here can move within a
                # day: day-stability counts only days *before* today, the user count
                # ignores platform evidence entirely, and channel-max fusion is blind
                # to a duplicate of a row already in the trail. So a sighting already
                # filed today only bumps last_used_at - no trail row, no full-trail
                # fetch, no rescore. Without this the trail grew two rows per message
                # and every message paid aggregates over all of them.
                last = await conn.fetchval(
                    """SELECT max(created_at) FROM alias_evidence
                        WHERE alias_id=$1 AND evidence_type=$2""",
                    row["id"], evidence[0].evidence_type.value,
                )
                if (last is not None
                        and last.astimezone(tz()).date() == now_local().date()):
                    await conn.execute(
                        "UPDATE alias SET last_used_at=NOW() WHERE id=$1", row["id"])
                    return _alias(row)

            if row is not None and row["status"] == "inactive" and not manual_in:
                # Dead stays dead. The sighting is still written down - the trail should
                # say the name kept appearing - but it moves nothing.
                for ev in evidence:
                    await conn.execute(
                        """INSERT INTO alias_evidence
                               (alias_id, raw_event_id, evidence_type, evidence_score)
                           VALUES ($1,$2,$3,$4)""",
                        row["id"], ev.raw_event_id, ev.evidence_type.value, ev.score,
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
                        row["id"], EvidenceType.MANUAL.value,
                    )
                    await conn.execute(
                        "UPDATE alias SET valid_to=NULL WHERE id=$1", row["id"],
                    )
                current = _alias(row)
                if current.status is AliasStatus.INACTIVE:
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
                    row["id"], scored.confidence, scored.status.value,
                )
                alias_id = row["id"]
                out = scored
                trail = merged
            else:
                evidence = await self._stability_scored(conn, None, evidence)
                scored = alias.scored(evidence)
                alias_id = await conn.fetchval(
                    """INSERT INTO alias
                           (alias_text, normalized_text, target_entity_id, group_id,
                            alias_type, confidence, status, valid_from, last_used_at)
                       VALUES ($1,$2,$3,$4,$5,$6,$7,NOW(),NOW())
                    RETURNING id""",
                    scored.alias_text, scored.normalized_text, scored.target_entity_id,
                    scored.group_id, scored.alias_type.value,
                    scored.confidence, scored.status.value,
                )
                out = scored
                trail = evidence

            for ev in evidence:
                await conn.execute(
                    """INSERT INTO alias_evidence
                           (alias_id, raw_event_id, evidence_type, evidence_score)
                       VALUES ($1,$2,$3,$4)""",
                    alias_id, ev.raw_event_id, ev.evidence_type.value, ev.score,
                )

            if manual_in or await conn.fetchval(
                "SELECT 1 FROM alias_evidence WHERE alias_id=$1 AND evidence_type=$2 LIMIT 1",
                alias_id, EvidenceType.MANUAL.value,
            ):
                # The owner's number is final until their next action; the usage recount
                # below must not outvote it.
                return out

            # How many different people have been seen using this name. It is counted
            # from the evidence trail rather than stored, because every piece of evidence
            # already points at a message and every message has a sender - the answer was
            # always in there, and a stored counter would be a second thing to keep true.
            #
            # This is the one route by which a name the model only observed becomes
            # certain. Without it, everything it noticed would stay a candidate forever
            # and never reach the prompt, however thoroughly the group had converged on
            # the nickname.
            users = await conn.fetchval(
                """SELECT count(DISTINCT r.platform_user_id)
                     FROM alias_evidence ae JOIN raw_event r ON r.id = ae.raw_event_id
                    WHERE ae.alias_id = $1
                      AND ae.evidence_type IN ('llm_inference','speaker_usage',
                                               'multi_user_usage')""",
                alias_id,
            ) or 0
            if users:
                # The whole trail plus the fresh usage row, never the usage row
                # alone: scored() fuses only what it is handed, so a single-row
                # rescore would replace the multi-channel fusion just computed
                # above with the bare usage weight. Channel-max inside scored()
                # keeps the fresh count ahead of any older multi-user rows
                # already in the trail.
                out = out.scored(trail + [AliasEvidence(EvidenceType.MULTI_USER_USAGE,
                                                        score=usage_weight(users))])
                await conn.execute(
                    """UPDATE alias SET confidence=$2, status=$3, updated_at=NOW()
                        WHERE id=$1""",
                    alias_id, out.confidence, out.status.value,
                )
            return out

    @staticmethod
    async def _stability_scored(
        conn, alias_id: uuid.UUID | None, evidence: list[AliasEvidence],
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
            days = 1 + (await conn.fetchval(
                """SELECT count(DISTINCT (created_at AT TIME ZONE $3)::date)
                     FROM alias_evidence
                    WHERE alias_id=$1 AND evidence_type = ANY($2::text[])
                      AND (created_at AT TIME ZONE $3)::date
                          < (NOW() AT TIME ZONE $3)::date""",
                alias_id, [k.value for k in kinds], zone,
            ) or 0)
        return [
            replace(e, score=platform_weight(e.evidence_type, days))
            if e.evidence_type in kinds and e.score is None else e
            for e in evidence
        ]

    async def set_alias_confidence(
        self, group_id: int, entity_id: uuid.UUID, text: str, confidence: float,
    ) -> Alias | None:
        """Set one name's confidence by hand, across the whole merged family.

        The number is written as the single MANUAL evidence row, so the domain reads it
        as the owner's verdict and automatic sightings cannot outvote it - in either
        direction. Below the confirmation threshold the name stops being usable without
        being struck out; at or above, it is usable on the spot. Returns the updated
        alias, or None if this person does not answer to the name here.
        """
        async with pool().acquire() as conn, conn.transaction():
            rows = await conn.fetch(
                FAMILY.format(arg="$2") + """
                   SELECT a.* FROM alias a JOIN family ON a.target_entity_id = family.id
                    WHERE (a.group_id=$1 OR a.group_id IS NULL) AND a.normalized_text=$3
                    FOR UPDATE OF a""",
                group_id, entity_id, normalize(text),
            )
            if not rows:
                return None
            status = ("confirmed" if confidence >= CONFIRM_THRESHOLD else "candidate")
            out = None
            for row in rows:
                await conn.execute(
                    "DELETE FROM alias_evidence WHERE alias_id=$1 AND evidence_type=$2",
                    row["id"], EvidenceType.MANUAL.value,
                )
                await conn.execute(
                    """INSERT INTO alias_evidence (alias_id, evidence_type, evidence_score)
                       VALUES ($1,$2,$3)""",
                    row["id"], EvidenceType.MANUAL.value, confidence,
                )
                await conn.execute(
                    """UPDATE alias SET confidence=$2, status=$3, valid_to=NULL,
                                        updated_at=NOW()
                        WHERE id=$1""",
                    row["id"], confidence, status,
                )
                out = _alias(row)
            return replace(out, confidence=confidence,
                           status=AliasStatus(status), valid_to=None)

    async def retire_alias(self, alias_id: uuid.UUID) -> None:
        """No longer holds. Not deleted: messages already archived still need it to be
        readable (design doc 18).

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
                alias_id, EvidenceType.MANUAL.value,
            )
            await conn.execute(
                """INSERT INTO alias_evidence (alias_id, evidence_type, evidence_score)
                   VALUES ($1,$2,0.0)""",
                alias_id, EvidenceType.MANUAL.value,
            )

    async def decay_aliases(self, group_id: int, *, unused_days: float,
                            joke_days: float | None = None) -> int:
        """Retire names that never became certain and stopped being used.

        Only candidates. A confirmed name is either something the platform reported, or
        something an owner typed, or something three different people were seen using -
        and none of those stops being true because nobody said it this month. A candidate
        is the model's guess, and a guess nobody has repeated since is the definition of
        one that did not pan out.

        A name the model marked as a joke gets a shorter window. Most of them are true for
        an afternoon, and the alias_type field would otherwise be one the model is asked
        to fill and nothing ever reads.
        """
        rows = await pool().fetch(
            """UPDATE alias
                  SET status = 'inactive', valid_to = NOW(), updated_at = NOW()
                WHERE group_id = $1 AND status = 'candidate'
                  AND COALESCE(last_used_at, valid_from, created_at)
                      < NOW() - (CASE WHEN alias_type = 'joke_name'
                                      THEN COALESCE($3::float, $2::float)
                                      ELSE $2::float END * INTERVAL '1 day')
             RETURNING id""",
            group_id, unused_days, joke_days,
        )
        return len(rows)
