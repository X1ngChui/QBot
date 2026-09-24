"""SQL for the ops and infrastructure tables: switches, ledger, blocklist, traces,
schema checks - the state that belongs to running the bot rather than to what it
remembers. The domain aggregates (identity, memory, episodes, jobs, the archive's
read side) live in `qqbot.repositories`; a handful of hot-path readers
(tools.search_history) issue their own SQL where the query is the logic.
"""

from __future__ import annotations

import re
import uuid
from datetime import date, timedelta

from ..domain.evidence import EvidenceMemo
from ..domain.ids import GroupId
from ..repositories.archive import ArchiveRepository
from ..settings import config
from ..util import now_local, today_local, tz_sql
from .pool import pool

# -- raw events -------------------------------------------------------------


async def backfill_plain_text(msg_id: str, plain_text: str) -> None:
    """Fill in the reading once media has been understood.

    Updates the derived plain_text column and nothing else: payload is the append-only
    record of what the platform sent, and the reading is the one field allowed to change
    after the fact - which is exactly why it lives outside the blob.
    """
    await pool().execute(
        """UPDATE raw_event SET plain_text = $2
            WHERE platform = 'qq' AND platform_event_id = $1""",
        msg_id,
        plain_text,
    )


_REQUIRED_SCHEMA_TABLES = {
    "account_link_challenge",
    "alias",
    "alias_evidence",
    "cost_ledger",
    "embedding_index",
    "entity",
    "episode",
    "episode_event",
    "group_blocklist",
    "group_state",
    "identity_account",
    "image_cache",
    "memory_candidate",
    "memory_extraction",
    "memory_extraction_event",
    "memory_fact",
    "memory_fact_evidence",
    "memory_job",
    "raw_event",
    "reply_trace",
    "user_agreement",
}
_REQUIRED_SCHEMA_COLUMNS = {
    "account_link_challenge": {
        "initiator_account_id",
        "target_account_id",
        "initiator_entity_revision",
        "target_entity_revision",
        "token_hash",
    },
    "alias": {"target_entity_id", "target_account_id"},
    "episode": {"extraction_id", "summary"},
    "group_blocklist": {"id", "user_id", "entity_id", "blocked_until"},
    "identity_account": {"id", "entity_id", "platform", "platform_user_id"},
    "memory_extraction": {"status", "snapshot"},
    "memory_fact": {"subject_entity_id", "subject_account_id"},
    "raw_event": {"archive_schema", "payload", "plain_text"},
}
_REQUIRED_NOT_NULL_COLUMNS = {
    "account_link_challenge": {
        "group_id",
        "token_hash",
        "initiator_account_id",
        "target_account_id",
        "initiator_entity_id",
        "target_entity_id",
        "initiator_entity_revision",
        "target_entity_revision",
        "status",
        "created_event_id",
        "created_at",
        "expires_at",
    },
    "reply_trace": {"memo", "expires_at"},
}
_SCHEMA_CONSTRAINTS = {
    "account_link_distinct_accounts": ("account_link_challenge", "c"),
    "account_link_revision_valid": ("account_link_challenge", "c"),
    "account_link_status_valid": ("account_link_challenge", "c"),
    "alias_exactly_one_target": ("alias", "c"),
    "fact_exactly_one_subject": ("memory_fact", "c"),
    "group_blocklist_exactly_one_target": ("group_blocklist", "c"),
    "identity_account_platform_platform_user_id_key": ("identity_account", "u"),
    "memory_extraction_event_extraction_id_ordinal_key": ("memory_extraction_event", "u"),
    "memory_extraction_event_raw_event_id_key": ("memory_extraction_event", "u"),
    "memory_extraction_live_snapshot_v2": ("memory_extraction", "c"),
    "memory_extraction_status_valid": ("memory_extraction", "c"),
    "raw_event_archive_schema_valid": ("raw_event", "c"),
}
_SCHEMA_CHECK_DEFINITIONS = {
    "account_link_distinct_accounts": "CHECK (initiator_account_id <> target_account_id)",
    "account_link_revision_valid": (
        "CHECK (initiator_entity_revision >= 1 AND target_entity_revision >= 1)"
    ),
    "account_link_status_valid": (
        "CHECK (status = ANY (ARRAY['pending', 'applied', 'cancelled', 'expired']))"
    ),
    "alias_exactly_one_target": (
        "CHECK (num_nonnulls(target_entity_id, target_account_id) = 1)"
    ),
    "fact_exactly_one_subject": (
        "CHECK (num_nonnulls(subject_entity_id, subject_account_id) = 1)"
    ),
    "group_blocklist_exactly_one_target": "CHECK (num_nonnulls(user_id, entity_id) = 1)",
    "identity_account_platform_platform_user_id_key": "UNIQUE (platform, platform_user_id)",
    "memory_extraction_event_extraction_id_ordinal_key": "UNIQUE (extraction_id, ordinal)",
    "memory_extraction_event_raw_event_id_key": "UNIQUE (raw_event_id)",
    "memory_extraction_live_snapshot_v2": (
        "CHECK (status = 'applied' OR status = 'extracting' AND snapshot IS NULL "
        "OR status = 'staged' AND snapshot IS NOT NULL "
        "AND snapshot @> '{\"version\": 2}'::jsonb)"
    ),
    "memory_extraction_status_valid": (
        "CHECK (status = ANY (ARRAY['extracting', 'staged', 'applied']))"
    ),
    "raw_event_archive_schema_valid": "CHECK (archive_schema >= 1)",
}
_SCHEMA_INDEXES = {
    "account_link_pending_pair": (
        "account_link_challenge",
        ("group_id", "LEAST(initiator_account_id, target_account_id)",
         "GREATEST(initiator_account_id, target_account_id)"),
        "status='pending'",
    ),
    "alias_unique_account_scope": (
        "alias", ("COALESCE(group_id, 0)", "normalized_text", "target_account_id"),
        "target_account_id IS NOT NULL",
    ),
    "alias_unique_entity_scope": (
        "alias", ("COALESCE(group_id, 0)", "normalized_text", "target_entity_id"),
        "target_entity_id IS NOT NULL",
    ),
    "fact_one_current_account": (
        "memory_fact",
        ("COALESCE(group_id, 0)", "subject_account_id", "predicate",
         "COALESCE(object_key, '')"),
        "subject_account_id IS NOT NULL AND valid_to IS NULL AND status='active'",
    ),
    "fact_one_current_entity": (
        "memory_fact",
        ("COALESCE(group_id, 0)", "subject_entity_id", "predicate",
         "COALESCE(object_key, '')"),
        "subject_entity_id IS NOT NULL AND valid_to IS NULL AND status='active'",
    ),
    "group_blocklist_account": (
        "group_blocklist", ("group_id", "user_id"), "user_id IS NOT NULL",
    ),
    "group_blocklist_holder": (
        "group_blocklist", ("group_id", "entity_id"), "entity_id IS NOT NULL",
    ),
    "raw_event_platform_key": (
        "raw_event", ("platform", "platform_event_id"), "platform_event_id IS NOT NULL",
    ),
    "reply_trace_reply": ("reply_trace", ("group_id", "reply_event_id"), ""),
}
_RETIRED_SCHEMA_TABLES = {"episode_participant", "schema_migration"}
_RETIRED_SCHEMA_COLUMNS = {"reply_trace": {"content"}}


def _schema_expression(value: str | None) -> str:
    """Compare catalog-deparsed expressions independent of harmless casts and spacing."""

    without_casts = re.sub(r"::(?:bigint|text|character varying)", "", value or "", flags=re.I)
    return re.sub(r"\s+", "", without_casts).lower()


async def check_schema(conn, *, schema: str, embedding_dimensions: int) -> None:
    """Validate the current schema contract without executing DDL."""

    rows = await conn.fetch(
        """SELECT table_name, column_name, is_nullable
             FROM information_schema.columns
            WHERE table_schema=$1""",
        schema,
    )
    columns: dict[str, set[str]] = {}
    not_null: dict[str, set[str]] = {}
    for row in rows:
        table = row["table_name"]
        column = row["column_name"]
        columns.setdefault(table, set()).add(column)
        if row["is_nullable"] == "NO":
            not_null.setdefault(table, set()).add(column)

    missing = sorted(_REQUIRED_SCHEMA_TABLES - columns.keys())
    missing.extend(
        f"{table}.{column}"
        for table, expected in _REQUIRED_SCHEMA_COLUMNS.items()
        for column in sorted(expected - columns.get(table, set()))
    )
    missing.extend(
        f"{table}.{column} NOT NULL"
        for table, expected in _REQUIRED_NOT_NULL_COLUMNS.items()
        for column in sorted(expected - not_null.get(table, set()))
    )
    constraint_rows = await conn.fetch(
        """SELECT c.conname, t.relname AS table_name, c.contype, c.convalidated,
                  pg_get_constraintdef(c.oid, true) AS definition
             FROM pg_constraint c
             JOIN pg_class t ON t.oid=c.conrelid
             JOIN pg_namespace n ON n.oid=t.relnamespace
            WHERE n.nspname=$1""",
        schema,
    )
    constraints = {row["conname"]: row for row in constraint_rows}
    for name, (table, kind) in _SCHEMA_CONSTRAINTS.items():
        row = constraints.get(name)
        if (row is None or row["table_name"] != table or row["contype"] != kind.encode()
                or not row["convalidated"]
                or _schema_expression(row["definition"])
                   != _schema_expression(_SCHEMA_CHECK_DEFINITIONS[name])):
            missing.append(f"{name} definition")
    index_rows = await conn.fetch(
        """SELECT idx.relname AS index_name, t.relname AS table_name,
                  i.indisunique, i.indisvalid, i.indisready, i.indislive, am.amname,
                  ARRAY(SELECT pg_get_indexdef(i.indexrelid, pos, true)
                          FROM generate_series(1, i.indnkeyatts) AS pos ORDER BY pos) AS keys,
                  pg_get_expr(i.indpred, i.indrelid, true) AS predicate
             FROM pg_index i
             JOIN pg_class idx ON idx.oid=i.indexrelid
             JOIN pg_class t ON t.oid=i.indrelid
             JOIN pg_am am ON am.oid=idx.relam
             JOIN pg_namespace n ON n.oid=idx.relnamespace
            WHERE n.nspname=$1""",
        schema,
    )
    indexes = {row["index_name"]: row for row in index_rows}
    for name, (table, keys, predicate) in _SCHEMA_INDEXES.items():
        row = indexes.get(name)
        if (row is None or row["table_name"] != table or row["amname"] != "btree"
                or not row["indisunique"] or not row["indisvalid"]
                or not row["indisready"] or not row["indislive"]
                or tuple(map(_schema_expression, row["keys"]))
                   != tuple(map(_schema_expression, keys))
                or _schema_expression(row["predicate"]) != _schema_expression(predicate)):
            missing.append(f"{name} definition")
    retired = sorted(_RETIRED_SCHEMA_TABLES & columns.keys())
    retired_columns = [
        f"{table}.{column}"
        for table, forbidden in _RETIRED_SCHEMA_COLUMNS.items()
        for column in sorted(forbidden & columns.get(table, set()))
    ]
    width = await conn.fetchval(
        """SELECT a.atttypmod FROM pg_attribute a
             JOIN pg_class c ON c.oid=a.attrelid
             JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname=$1 AND c.relname='embedding_index'
              AND a.attname='embedding' AND NOT a.attisdropped""",
        schema,
    )

    details = []
    if missing:
        details.append("missing " + ", ".join(missing))
    if retired:
        details.append("retired tables present: " + ", ".join(retired))
    if retired_columns:
        details.append("retired columns present: " + ", ".join(retired_columns))
    if width != embedding_dimensions:
        details.append(
            f"embedding_index.embedding is VECTOR({width}) but configuration "
            f"requires VECTOR({embedding_dimensions})"
        )
    if details:
        raise RuntimeError(
            "database schema is incompatible; apply the manual schema update first ("
            + "; ".join(details)
            + ")"
        )


async def ensure_schema() -> None:
    """Refuse startup unless the manually managed schema matches this runtime."""

    dimensions = config().default.capabilities.embedding.dimensions
    async with pool().acquire() as conn:
        await check_schema(conn, schema="public", embedding_dimensions=dimensions)


async def recent_messages(group_id: GroupId, *, limit: int) -> list:
    """The newest canonical archived messages of one group, oldest first."""

    return await ArchiveRepository().recent(group_id, limit=limit)


async def image_cache_get(key: str, *, max_age: timedelta | None = None) -> str | None:
    """The stored description, or None while there is none yet. A row whose upload
    landed before its describing call holds '' - reported as a miss, not a hit.

    `max_age` asks only for a description still worth trusting: an older one is
    reported as a miss so the caller pays to write a fresh one. Callers that may not
    spend leave it unset - outside the paid describing path a stale description
    still beats a bare marker.

    The sighting counts either way: hit_count measures how often a picture comes
    back, which is what decides whether describing it again is worth anything.
    """
    row = await pool().fetchrow(
        """UPDATE image_cache SET hit_count = hit_count + 1, last_seen = now()
            WHERE key=$1 RETURNING description, described_at""",
        key,
    )
    if not row or not row["description"]:
        return None
    if max_age is not None and row["described_at"] < now_local() - max_age:
        return None
    return row["description"]


async def image_cache_put(key: str, description: str, *, refused: bool = False) -> None:
    """Store the description and stamp when it was written.

    Overwrites on conflict: the row may have been created by the upload half with an
    empty description, or hold one this call was made to replace because it had aged
    out - the same key is the same picture, and the newer description is the one the
    current model wrote.

    `refused` marks a placeholder standing in for a picture the backend declined to
    look at, so the two are told apart in the report. It expires like any other
    description; a later backend may well look at it.
    """
    await pool().execute(
        """INSERT INTO image_cache (key, description, refused, described_at)
                VALUES ($1,$2,$3,now())
           ON CONFLICT (key) DO UPDATE
             SET description = EXCLUDED.description, refused = EXCLUDED.refused,
                 described_at = now(), last_seen = now()""",
        key,
        description,
        refused,
    )


async def image_cache_file(
    key: str,
    *,
    provider: str,
    max_age: timedelta | None = None,
) -> str | None:
    """Return a fresh file handle issued by this exact provider, if any."""

    row = await pool().fetchrow(
        """SELECT file_id, file_provider, file_uploaded_at
             FROM image_cache WHERE key=$1""",
        key,
    )
    if not row or not row["file_id"] or row["file_provider"] != provider:
        return None
    if max_age is not None:
        at = row["file_uploaded_at"]
        if at is None or at < now_local() - max_age:
            return None
    return row["file_id"]


async def image_cache_set_file(key: str, file_id: str, *, provider: str) -> None:
    await pool().execute(
        """INSERT INTO image_cache (key, file_id, file_provider, file_uploaded_at)
           VALUES ($1,$2,$3,now())
           ON CONFLICT (key) DO UPDATE
             SET file_id = EXCLUDED.file_id,
                 file_provider = EXCLUDED.file_provider,
                 file_uploaded_at = now(), last_seen = now()""",
        key,
        file_id,
        provider,
    )


async def image_cache_stats() -> dict:
    row = await pool().fetchrow(
        """SELECT count(*) AS n, COALESCE(sum(hit_count),0) AS hits,
                  count(*) FILTER (WHERE refused) AS refused
             FROM image_cache"""
    )
    return dict(row) if row else {"n": 0, "hits": 0, "refused": 0}


# -- group_state (per-group switches) ----------------------------------------
# One typed column per field, so the schema itself says what runtime state the system
# keeps - no magic strings inside a jsonb blob.


async def group_muted(group_id: GroupId) -> bool:
    """Return this group's persisted mute switch."""

    muted = await pool().fetchval(
        "SELECT muted FROM group_state WHERE group_id=$1", _group(group_id)
    )
    return bool(muted)


async def set_group_muted(group_id: GroupId, muted: bool) -> None:
    await pool().execute(
        """INSERT INTO group_state (group_id, muted) VALUES ($1,$2)
           ON CONFLICT (group_id) DO UPDATE SET muted=$2, updated_at=NOW()""",
        _group(group_id),
        muted,
    )


async def block(group_id: GroupId, user_ids: list[str] | str, *, until=None) -> None:
    """Create or replace exact-account block rules."""

    ids = [user_ids] if isinstance(user_ids, str) else list(user_ids)
    if not ids:
        return
    await pool().executemany(
        """INSERT INTO group_blocklist (group_id, user_id, blocked_until)
           VALUES ($1,$2,$3)
           ON CONFLICT (group_id, user_id) WHERE user_id IS NOT NULL
             DO UPDATE SET blocked_until = EXCLUDED.blocked_until""",
        [(_group(group_id), user_id, until) for user_id in ids],
    )


async def block_holder(group_id: GroupId, entity_id: uuid.UUID, *, until=None) -> None:
    """Create or replace a dynamic linked-holder block rule."""

    await pool().execute(
        """INSERT INTO group_blocklist (group_id, entity_id, blocked_until)
           VALUES ($1,$2,$3)
           ON CONFLICT (group_id, entity_id) WHERE entity_id IS NOT NULL
             DO UPDATE SET blocked_until = EXCLUDED.blocked_until""",
        _group(group_id),
        entity_id,
        until,
    )


async def unblock(group_id: GroupId, user_ids: list[str] | str) -> bool:
    """Delete exact-account block rules."""

    ids = [user_ids] if isinstance(user_ids, str) else list(user_ids)
    if not ids:
        return False
    tag = await pool().execute(
        "DELETE FROM group_blocklist WHERE group_id=$1 AND user_id=ANY($2::text[])",
        _group(group_id),
        ids,
    )
    return not tag.endswith(" 0")


async def unblock_holder(group_id: GroupId, entity_id: uuid.UUID) -> bool:
    """Delete every holder rule that currently resolves to this linked set."""

    tag = await pool().execute(
        """WITH RECURSIVE family AS (
               SELECT $2::uuid AS id
               UNION ALL
               SELECT e.id FROM entity e JOIN family f ON e.merged_into=f.id
           )
           DELETE FROM group_blocklist
            WHERE group_id=$1 AND entity_id IN (SELECT id FROM family)""",
        _group(group_id),
        entity_id,
    )
    return not tag.endswith(" 0")


async def blocked(group_id: GroupId, user_id: str) -> bool:
    """Resolve exact and linked-holder rules against current identity membership."""

    return bool(
        await pool().fetchval(
            """WITH RECURSIVE family AS (
               SELECT entity_id AS id FROM identity_account
                WHERE platform='qq' AND platform_user_id=$2
               UNION ALL
               SELECT e.id FROM entity e JOIN family f ON e.merged_into=f.id
           )
           SELECT EXISTS (
               SELECT 1 FROM group_blocklist b
                WHERE b.group_id=$1
                  AND (b.blocked_until IS NULL OR b.blocked_until > NOW())
                  AND (b.user_id=$2 OR b.entity_id IN (SELECT id FROM family))
           )""",
            _group(group_id),
            user_id,
        )
    )


async def block_rules(group_id: GroupId) -> list[dict]:
    """Return active rules in their current exact-account or holder scope."""

    rows = await pool().fetch(
        """WITH RECURSIVE resolved AS (
               SELECT b.id AS rule_id, b.user_id, b.blocked_until, b.created_at,
                      e.id AS current_entity_id, e.merged_into
                 FROM group_blocklist b
                 LEFT JOIN entity e ON e.id=b.entity_id
                WHERE b.group_id=$1
                  AND (b.blocked_until IS NULL OR b.blocked_until > NOW())
               UNION ALL
               SELECT r.rule_id, r.user_id, r.blocked_until, r.created_at,
                      e.id, e.merged_into
                 FROM resolved r
                 JOIN entity e ON e.id=r.merged_into
           )
           SELECT rule_id AS id, user_id,
                  CASE WHEN user_id IS NULL THEN current_entity_id END AS entity_id,
                  blocked_until, created_at
             FROM resolved
            WHERE user_id IS NOT NULL OR merged_into IS NULL
            ORDER BY created_at, rule_id""",
        _group(group_id),
    )
    combined: dict[tuple[str | None, uuid.UUID | None], dict] = {}
    for row in rows:
        item = dict(row)
        item.pop("created_at")
        key = (item["user_id"], item["entity_id"])
        previous = combined.get(key)
        if previous is None:
            combined[key] = item
            continue
        old_until = previous["blocked_until"]
        new_until = item["blocked_until"]
        previous["blocked_until"] = (
            None if old_until is None or new_until is None else max(old_until, new_until)
        )
    return list(combined.values())


async def note_group_seen(group_id: GroupId) -> bool:
    """Record that this group exists. True only for the call that first saw it.

    The insert is the claim: whoever creates the row is the discoverer, and everyone
    after them conflicts and gets False. There is no allowlist, so this row is the only
    thing that distinguishes a group the bot has served for months from one that added
    it a minute ago.
    """
    row = await pool().fetchval(
        """INSERT INTO group_state (group_id, first_seen_at) VALUES ($1, NOW())
           ON CONFLICT (group_id) DO NOTHING
           RETURNING group_id""",
        _group(group_id),
    )
    return row is not None


async def groups_first_seen_on(day: str | date) -> list[GroupId]:
    """Groups whose first message arrived on this day, for the daily report."""
    rows = await pool().fetch(
        """SELECT group_id FROM group_state
            WHERE first_seen_at IS NOT NULL
              AND (first_seen_at AT TIME ZONE $2)::date = $1::date
            ORDER BY first_seen_at""",
        _as_date(day),
        tz_sql(),
    )
    return [GroupId(r["group_id"]) for r in rows]


async def groups_with_state() -> list[GroupId]:
    rows = await pool().fetch("SELECT group_id FROM group_state")
    return [GroupId(r["group_id"]) for r in rows]


async def has_agreed(group_id: GroupId, user_id: str, version: int) -> bool:
    """Whether this account accepted the agreement, at this version or later,
    in this group. An older acceptance does not count: bumping the version is
    how the owner voids it."""
    return await pool().fetchval(
        "SELECT EXISTS(SELECT 1 FROM user_agreement"
        " WHERE group_id=$1 AND user_id=$2 AND version >= $3)",
        _group(group_id),
        user_id,
        version,
    )


async def record_agreement(group_id: GroupId, user_id: str, version: int) -> bool:
    """File one acceptance of this version; True when it changed anything - a
    first acceptance or an upgrade - False when already at this version or
    later."""
    return bool(
        await pool().fetchval(
            """INSERT INTO user_agreement (group_id, user_id, version)
           VALUES ($1,$2,$3)
           ON CONFLICT (group_id, user_id)
           DO UPDATE SET version = EXCLUDED.version, agreed_at = NOW()
                   WHERE user_agreement.version < EXCLUDED.version
        RETURNING TRUE""",
            _group(group_id),
            user_id,
            version,
        )
    )


async def holder_ids_for_accounts(user_ids: list[str]) -> dict[str, uuid.UUID]:
    """Return the current holder id for each known exact account."""
    want = sorted({u for u in user_ids if u})
    if not want:
        return {}
    rows = await pool().fetch(
        """SELECT platform_user_id, entity_id FROM identity_account
            WHERE platform='qq' AND platform_user_id = ANY($1::text[])""",
        want,
    )
    return {r["platform_user_id"]: r["entity_id"] for r in rows}


async def linked_account_ids(user_id: str) -> list[str]:
    """Return every exact account currently linked to this one."""
    rows = await pool().fetch(
        """SELECT b.platform_user_id FROM identity_account a
             JOIN identity_account b ON b.entity_id = a.entity_id AND b.platform = 'qq'
            WHERE a.platform='qq' AND a.platform_user_id=$1""",
        user_id,
    )
    found = [r["platform_user_id"] for r in rows]
    return found if user_id in found else [*found, user_id]


async def muted_groups() -> list[GroupId]:
    """Read from the table, not the in-memory registry: a group muted before the
    last restart and quiet since is exactly the one the daily report must not
    forget to list."""
    rows = await pool().fetch("SELECT group_id FROM group_state WHERE muted")
    return [GroupId(r["group_id"]) for r in rows]


# -- reply evidence ----------------------------------------------------------


async def evidence_add(group_id: GroupId, reply_event_id: str, memo: EvidenceMemo) -> None:
    """Store one structured memo; a replayed reply keeps its original evidence."""

    await pool().execute(
        """INSERT INTO reply_trace
               (group_id, reply_event_id, memo, expires_at)
           VALUES ($1,$2,$3,$4)
           ON CONFLICT (group_id, reply_event_id) DO NOTHING""",
        _group(group_id),
        reply_event_id,
        memo.to_dict(),
        memo.expires_at,
    )


async def evidence_for(group_id: GroupId, reply_event_ids: list[str]) -> dict[str, str]:
    """Render unexpired structured memos for prompt replay."""

    if not reply_event_ids:
        return {}
    rows = await pool().fetch(
        """SELECT reply_event_id, memo FROM reply_trace
            WHERE group_id=$1 AND reply_event_id = ANY($2::text[])
              AND memo IS NOT NULL AND expires_at > NOW()""",
        _group(group_id),
        reply_event_ids,
    )
    rendered: dict[str, str] = {}
    for row in rows:
        try:
            content = EvidenceMemo.from_dict(row["memo"]).render()
        except (TypeError, ValueError):
            continue
        if content:
            rendered[row["reply_event_id"]] = content
    return rendered


async def evidence_prune() -> int:
    """Delete expired evidence in one bounded nightly database operation."""

    result = await pool().execute(
        "DELETE FROM reply_trace WHERE expires_at IS NOT NULL AND expires_at <= NOW()"
    )
    return int(result.rpartition(" ")[2])


# -- cost_ledger ------------------------------------------------------------


async def ledger_add(
    *,
    group_id: GroupId | None,
    kind: str,
    model: str,
    in_hit: int = 0,
    in_miss: int = 0,
    out: int = 0,
    calls: int = 1,
    cny: float = 0.0,
    user_id: str = "",
) -> None:
    """Add one call to the day's running total.

    An upsert rather than an append: the table's whole point is that one day, one
    group, one kind and one causing account have exactly one row, which is what makes
    a billing figure a lookup rather than a scan. `user_id` is the account whose
    action caused the spend ('' when no single account did); the caller usually
    leaves it to BUDGET.attribute rather than passing it.

    The day comes from here rather than from CURRENT_DATE, so the boundary is midnight
    in the configured timezone rather than midnight wherever the container thinks it is.
    Those are the same day for most of the day and different for the hours that matter -
    a budget that resets eight hours early is a budget nobody set.
    """
    await pool().execute(
        """INSERT INTO cost_ledger
               (day, group_id, kind, model, user_id, in_hit, in_miss, out, calls, cny)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
           ON CONFLICT (day, group_id, kind, model, user_id) DO UPDATE
             SET calls   = cost_ledger.calls   + EXCLUDED.calls,
                 in_hit  = cost_ledger.in_hit  + EXCLUDED.in_hit,
                 in_miss = cost_ledger.in_miss + EXCLUDED.in_miss,
                 out     = cost_ledger.out     + EXCLUDED.out,
                 cny     = cost_ledger.cny     + EXCLUDED.cny""",
        _as_date(today_local()),
        _group(group_id),
        kind,
        model,
        user_id or "",
        in_hit,
        in_miss,
        out,
        calls,
        cny,
    )


async def top_spenders(group_id: GroupId, *, k: int, all_linked: bool = False) -> list[dict]:
    """This group's costliest exact accounts or explicitly linked holders."""

    today = _as_date(today_local())
    if not all_linked:
        rows = await pool().fetch(
            """SELECT user_id AS person, ARRAY[user_id] AS accounts,
                      sum(cny) AS cny, sum(calls) AS calls
                 FROM cost_ledger
                WHERE group_id=$1 AND user_id <> ''
                  AND day >= $2 AND day <= $3
                GROUP BY user_id
                ORDER BY sum(cny) DESC, user_id
                LIMIT $4""",
            _group(group_id),
            today.replace(day=1),
            today,
            k,
        )
        return [dict(row) for row in rows]
    rows = await pool().fetch(
        """SELECT COALESCE(ia.entity_id::text, l.user_id) AS person,
                  array_agg(DISTINCT l.user_id) AS accounts,
                  sum(l.cny) AS cny, sum(l.calls) AS calls
             FROM cost_ledger l
             LEFT JOIN identity_account ia
                    ON ia.platform = 'qq' AND ia.platform_user_id = l.user_id
            WHERE l.group_id = $1 AND l.user_id <> ''
              AND l.day >= $2 AND l.day <= $3
            GROUP BY person
            ORDER BY sum(l.cny) DESC, person
            LIMIT $4""",
        _group(group_id),
        today.replace(day=1),
        today,
        k,
    )
    return [dict(row) for row in rows]


def _group(group_id: GroupId | None) -> int:
    """Encode an optional domain group id for the ledger and SQL parameters."""
    return 0 if group_id is None else group_id.to_db()


def _as_date(day: str | date) -> date:
    """Callers pass 'YYYY-MM-DD'; asyncpg binds a date column as datetime.date."""
    return day if isinstance(day, date) else date.fromisoformat(day)


async def month_calls(kind: str, model: str) -> int:
    """Calls of one kind and model booked in the calendar month holding today.

    The quota meter for a capability with a monthly free allowance: the ledger already
    counts every call, so the allowance is read back rather than tracked in a second
    place that could drift. Month boundaries follow the configured timezone, like the
    day boundaries every ledger row is filed under.
    """
    today = _as_date(today_local())
    v = await pool().fetchval(
        """SELECT COALESCE(sum(calls),0) FROM cost_ledger
            WHERE day >= $1 AND day <= $2 AND kind=$3 AND model=$4""",
        today.replace(day=1),
        today,
        kind,
        model,
    )
    return int(v or 0)


async def day_cost(day: str | date) -> float:
    v = await pool().fetchval(
        "SELECT COALESCE(sum(cny),0) FROM cost_ledger WHERE day=$1", _as_date(day)
    )
    return float(v or 0.0)


async def day_breakdown(day: str | date, group_id: GroupId | None = None) -> list[dict]:
    """Cost and call counts for one day, by kind and model.

    Without group_id this covers every group, which is what the budget is measured
    against - the cap is shared, not per-group.
    """
    rows = await pool().fetch(
        """SELECT kind, model, sum(calls) AS calls, sum(in_hit) AS in_hit,
                  sum(in_miss) AS in_miss, sum(out) AS out, sum(cny) AS cny
             FROM cost_ledger WHERE day=$1 AND ($2::bigint IS NULL OR group_id=$2)
            GROUP BY kind, model ORDER BY sum(cny) DESC""",
        _as_date(day),
        None if group_id is None else _group(group_id),
    )
    return [dict(r) for r in rows]
