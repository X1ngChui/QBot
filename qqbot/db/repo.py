"""SQL for the ops and infrastructure tables: switches, ledger, blocklist, traces,
schema checks - the state that belongs to running the bot rather than to what it
remembers. The domain aggregates (identity, memory, episodes, jobs) live in
`qqbot.repositories`; a handful of hot-path readers (tools.search_history, the
extraction worker's drain queries) issue their own SQL where the query is the logic.
"""

from __future__ import annotations

from datetime import date

from ..util import today_local, tz_sql
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
        msg_id, plain_text,
    )


async def ensure_schema() -> None:
    """The schema is created by sql/init.sql; this only checks it at startup.

    It confirms, never changes: a schema that does not match the code should fail at
    boot, not when the first message arrives.
    """
    missing = await pool().fetch(
        """SELECT t.name FROM unnest(ARRAY[
               'raw_event','entity','identity_account','alias','alias_evidence',
               'memory_fact','memory_fact_evidence','memory_candidate',
               'episode','episode_participant','episode_event',
               'memory_job','embedding_index',
               'cost_ledger','group_state','group_blocklist','image_cache',
               'reply_trace'
           ]) AS t(name)
           LEFT JOIN information_schema.tables i
                  ON i.table_name = t.name AND i.table_schema = 'public'
          WHERE i.table_name IS NULL""",
    )
    if missing:
        raise RuntimeError(
            "missing tables: " + ", ".join(r["name"] for r in missing))

    # init.sql only runs on a fresh database, so a server that missed a migration has
    # the table but not a later-added column - and without this check the mismatch
    # surfaces as a caught-and-logged failure inside a background worker, hours later,
    # looking like a group with nothing worth extracting. Listed by hand because there is
    # no way to derive what the code writes; the list only needs the columns that are not
    # in the original CREATE.
    absent = await pool().fetch(
        """SELECT c.tbl, c.col FROM (VALUES
               ('memory_candidate','batch_event_id'),
               ('memory_candidate','batch_size'),
               ('memory_fact','object_key'),
               ('raw_event','plain_text'),
               ('group_state','first_seen_at'),
               ('image_cache','file_id'),
               ('cost_ledger','day'),
               ('cost_ledger','user_id'),
               ('group_blocklist','blocked_until')
           ) AS c(tbl, col)
           LEFT JOIN information_schema.columns i
                  ON i.table_name = c.tbl AND i.column_name = c.col
                 AND i.table_schema = 'public'
          WHERE i.column_name IS NULL""",
    )
    if absent:
        raise RuntimeError(
            "schema is behind the code, missing: "
            + ", ".join(f"{r['tbl']}.{r['col']}" for r in absent))

    # The column check alone is not enough where the code upserts: ledger_add's
    # ON CONFLICT names the five-column primary key, and a hand migration that adds
    # the column but keeps the old key passes the check above while every upsert
    # fails - caught, so the ledger silently stops accruing and the budget restores
    # understated after a restart. Verify the key itself.
    pk = await pool().fetchval(
        """SELECT array_agg(a.attname ORDER BY x.ord)
             FROM pg_constraint c
             JOIN unnest(c.conkey) WITH ORDINALITY AS x(attnum, ord) ON true
             JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = x.attnum
            WHERE c.conrelid = 'cost_ledger'::regclass AND c.contype = 'p'""",
    )
    want = ["day", "group_id", "kind", "model", "user_id"]
    if list(pk or []) != want:
        raise RuntimeError(
            f"cost_ledger primary key is behind the code: expected {want}, "
            f"found {list(pk or [])}")

    # Same reasoning for the unique indexes that back ON CONFLICT upserts but
    # live as separate CREATE INDEX statements in init.sql - a hand migration
    # that runs the CREATE TABLE block and stops there passes every check above
    # while every inbound insert (raw_event) or trace write (reply_trace) fails.
    no_index = await pool().fetch(
        """SELECT n.name FROM unnest(ARRAY[
               'raw_event_platform_key','reply_trace_reply'
           ]) AS n(name)
           LEFT JOIN pg_indexes p
                  ON p.indexname = n.name AND p.schemaname = 'public'
          WHERE p.indexname IS NULL""",
    )
    if no_index:
        raise RuntimeError(
            "missing unique indexes (ON CONFLICT depends on them): "
            + ", ".join(r["name"] for r in no_index))


async def recent_messages(group_id: int, *, limit: int) -> list:
    """The newest messages of one group, returned oldest first.

    What the reply path's in-memory window is rebuilt from after a restart: the stored
    reading already has pictures described, so the window comes back full instead of the
    bot rejoining a conversation it was part of thirty seconds earlier with no idea what
    is being discussed.

    Ordered by (occurred_at, id) so two messages sharing a timestamp cannot swap places
    between two calls.
    """
    rows = await pool().fetch(
        """SELECT id, platform_event_id, platform_user_id, occurred_at, payload,
                  plain_text
             FROM raw_event
            WHERE group_id=$1 AND event_type='message'
            ORDER BY occurred_at DESC, id DESC LIMIT $2""",
        group_id, limit,
    )
    return list(reversed(rows))


async def image_cache_get(key: str) -> str | None:
    """The stored description, or None while there is none yet. A row whose upload
    landed before its describing call holds '' - reported as a miss, not a hit."""
    row = await pool().fetchrow(
        """UPDATE image_cache SET hit_count = hit_count + 1, last_seen = now()
            WHERE key=$1 RETURNING description""",
        key,
    )
    return (row["description"] or None) if row else None


async def image_cache_put(key: str, description: str) -> None:
    """Store the description. Overwrites on conflict: the row may have been created by
    the upload half with an empty description, and the same key is the same picture -
    a fresher describing call is never worse than the placeholder it replaces."""
    await pool().execute(
        """INSERT INTO image_cache (key, description) VALUES ($1,$2)
           ON CONFLICT (key) DO UPDATE
             SET description = EXCLUDED.description, last_seen = now()""",
        key,
        description,
    )


async def image_cache_file(key: str) -> str | None:
    """Where the vision backend filed this picture, if it was ever uploaded. A hint:
    the backend expires files, and a dead id can fail the request it rides in - which
    is why the prompt only ever attaches ids from recent messages."""
    return await pool().fetchval(
        "SELECT file_id FROM image_cache WHERE key=$1", key)


async def image_cache_set_file(key: str, file_id: str) -> None:
    await pool().execute(
        """INSERT INTO image_cache (key, file_id) VALUES ($1,$2)
           ON CONFLICT (key) DO UPDATE
             SET file_id = EXCLUDED.file_id, last_seen = now()""",
        key,
        file_id,
    )


async def image_cache_stats() -> dict:
    row = await pool().fetchrow(
        "SELECT count(*) AS n, COALESCE(sum(hit_count),0) AS hits FROM image_cache"
    )
    return dict(row) if row else {"n": 0, "hits": 0}


# -- group_state (per-group switches and watermarks) ------------------------
# One typed column per field, so the schema itself says what runtime state the system
# keeps - no magic strings inside a jsonb blob.


async def group_switches(group_id: int) -> tuple[bool, dict]:
    """This group's persisted switches: (muted, {blocked account: lapses at}).

    Two tables, one read: the mute flag is an attribute of the group, while who is
    blocked is a one-to-many relation and so lives in its own table. A block that
    has already lapsed is simply not loaded; its row waits for blocked_now or the
    next timed lapse in the group to sweep it, and is invisible until then.
    """
    muted = await pool().fetchval(
        "SELECT muted FROM group_state WHERE group_id=$1", group_id)
    rows = await pool().fetch(
        """SELECT user_id, blocked_until FROM group_blocklist
            WHERE group_id=$1 AND (blocked_until IS NULL OR blocked_until > NOW())""",
        group_id)
    return bool(muted), {r["user_id"]: r["blocked_until"] for r in rows}


async def set_group_muted(group_id: int, muted: bool) -> None:
    await pool().execute(
        """INSERT INTO group_state (group_id, muted) VALUES ($1,$2)
           ON CONFLICT (group_id) DO UPDATE SET muted=$2, updated_at=NOW()""",
        group_id, muted,
    )


async def block(group_id: int, user_ids: list[str] | str, *, until=None) -> None:
    """Block one person - every account they hold - until `until`, or for good.

    Takes a list because a merge makes several accounts one person, and a decision
    about a person that only reached the account that was @-ed is not a decision:
    the alt keeps talking, keeps being archived, keeps feeding memory. The rows stay
    account-keyed so the hot path can keep testing a plain mapping
    (state.GroupState), and expanding to the person happens here, at write time.

    Re-blocking overwrites the expiry: the newest decision is the decision, so
    /block again without a duration turns a timed block permanent, and with one
    restarts the clock.
    """
    ids = [user_ids] if isinstance(user_ids, str) else list(user_ids)
    if not ids:
        return
    await pool().executemany(
        """INSERT INTO group_blocklist (group_id, user_id, blocked_until)
           VALUES ($1,$2,$3)
           ON CONFLICT (group_id, user_id)
             DO UPDATE SET blocked_until = EXCLUDED.blocked_until""",
        [(group_id, uid, until) for uid in ids],
    )


async def unblock(group_id: int, user_ids: list[str] | str) -> bool:
    """Lift the block from every account of one person.

    True if anything was actually removed - the command reports the difference.
    """
    ids = [user_ids] if isinstance(user_ids, str) else list(user_ids)
    if not ids:
        return False
    tag = await pool().execute(
        "DELETE FROM group_blocklist WHERE group_id=$1 AND user_id = ANY($2::text[])",
        group_id, ids,
    )
    return not tag.endswith(" 0")


async def groups_blocking(user_ids: list[str]) -> list[tuple[int, object]]:
    """Groups where any of these accounts is live-blocked, with the block's expiry.

    What a merge needs to know: the accounts became one person, so wherever one of
    them was blocked, all of them now are - and for how long. Per group the
    strongest standing block wins: a permanent one (NULL) dominates, otherwise the
    latest expiry; already-lapsed rows count for nothing.
    """
    if not user_ids:
        return []
    rows = await pool().fetch(
        """SELECT group_id,
                  CASE WHEN bool_or(blocked_until IS NULL) THEN NULL
                       ELSE max(blocked_until) END AS lapses
             FROM group_blocklist
            WHERE user_id = ANY($1::text[])
              AND (blocked_until IS NULL OR blocked_until > NOW())
            GROUP BY group_id""",
        user_ids,
    )
    return [(r["group_id"], r["lapses"]) for r in rows]


async def unblock_expired(group_id: int) -> None:
    """Sweep this group's lapsed timed blocks. Called when one is noticed - there
    is no scheduler for something the hot path detects for free."""
    await pool().execute(
        """DELETE FROM group_blocklist
            WHERE group_id=$1 AND blocked_until IS NOT NULL AND blocked_until <= NOW()""",
        group_id,
    )


async def unread_since_extract(group_id: int) -> tuple[int, object]:
    """How many messages this group has that no extraction has read, and the newest one's
    arrival time.

    The watermark records what was actually *read*, not what was queued, so this count is
    the honest answer to "is there anything to extract" - and it is what stops a retried
    or hand-triggered job from paying to re-read a batch it already paid for.

    The WHERE clause is the same watermark predicate workers/memory._next_unread fetches
    by (that copy documents the created_at-vs-occurred_at axis choice); a change to one
    must reach the other, or the gate and the fetch disagree about what "unread" means.
    """
    row = await pool().fetchrow(
        """SELECT count(*) AS n, max(created_at) AS newest FROM raw_event
            WHERE group_id=$1 AND event_type='message'
              AND created_at > COALESCE(
                  (SELECT last_extract_at FROM group_state WHERE group_id=$1), 'epoch')""",
        group_id,
    )
    return (row["n"] or 0), row["newest"]


async def mark_extracted(group_id: int, upto) -> None:
    """Move the watermark to the newest message an extraction actually read.

    Never backwards: two passes can overlap, and the later-finishing one must not reopen
    messages the other has already read.
    """
    await pool().execute(
        """INSERT INTO group_state (group_id, last_extract_at) VALUES ($1,$2)
           ON CONFLICT (group_id) DO UPDATE
             SET last_extract_at = GREATEST(
                     COALESCE(group_state.last_extract_at, 'epoch'), EXCLUDED.last_extract_at),
                 updated_at = NOW()""",
        group_id, upto,
    )


async def reset_extract_watermark(group_id: int, *, keep: int) -> None:
    """Pull the watermark back so exactly the newest `keep` messages count unread.

    The deliberate exception to mark_extracted's never-backwards rule, in its own
    function so the exception cannot be reached by accident: only /relearn calls it,
    and /relearn's whole meaning is "read it again" - a gate that answers "nothing
    new" to an owner asking for a re-read is the gate malfunctioning.

    Pulled back *this far and no further*, because extraction now drains oldest-
    first from the watermark: a bare NULL here would send the next drain through
    the entire archive - a re-read of months at model prices, from one command.

    Known, accepted race: a drain pass already in flight marks forward (GREATEST)
    after this reset and re-advances the watermark, shrinking what the forced
    re-read sees. Only reachable when /relearn lands mid-drain (around the
    nightly cron or a retry window); the owner's remedy is the command again.
    """
    await pool().execute(
        """UPDATE group_state
              SET last_extract_at = (
                      SELECT created_at FROM raw_event
                       WHERE group_id=$1 AND event_type='message'
                       ORDER BY created_at DESC OFFSET $2 LIMIT 1),
                  updated_at = NOW()
            WHERE group_id=$1""",
        group_id, keep,
    )


async def note_group_seen(group_id: int) -> bool:
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
        group_id,
    )
    return row is not None


async def groups_first_seen_on(day: str | date) -> list[int]:
    """Groups whose first message arrived on this day, for the daily report."""
    rows = await pool().fetch(
        """SELECT group_id FROM group_state
            WHERE first_seen_at IS NOT NULL
              AND (first_seen_at AT TIME ZONE $2)::date = $1::date
            ORDER BY first_seen_at""",
        _as_date(day), tz_sql(),
    )
    return [r["group_id"] for r in rows]


async def groups_with_state() -> list[int]:
    rows = await pool().fetch("SELECT group_id FROM group_state")
    return [r["group_id"] for r in rows]


async def muted_groups() -> list[int]:
    """Read from the table, not the in-memory registry: a group muted before the
    last restart and quiet since is exactly the one the daily report must not
    forget to list."""
    rows = await pool().fetch("SELECT group_id FROM group_state WHERE muted")
    return [r["group_id"] for r in rows]


# -- reply_trace ------------------------------------------------------------


async def trace_add(group_id: int, reply_event_id: str, content: str) -> None:
    """File one reply's trajectory digest. Idempotent per reply: a replayed write is
    the same note."""
    await pool().execute(
        """INSERT INTO reply_trace (group_id, reply_event_id, content)
           VALUES ($1,$2,$3)
           ON CONFLICT (group_id, reply_event_id) DO NOTHING""",
        group_id, reply_event_id, content,
    )


async def traces_for(group_id: int, reply_event_ids: list[str]) -> dict[str, str]:
    """The stored trajectories for these replies, for the window rebuild to re-seat
    each one in front of the reply it fed."""
    if not reply_event_ids:
        return {}
    rows = await pool().fetch(
        """SELECT reply_event_id, content FROM reply_trace
            WHERE group_id=$1 AND reply_event_id = ANY($2::text[])""",
        group_id, reply_event_ids,
    )
    return {r["reply_event_id"]: r["content"] for r in rows}


# -- cost_ledger ------------------------------------------------------------


async def ledger_add(
    *,
    group_id: str | None,
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


async def top_spenders(group_id: int, *, k: int) -> list[dict]:
    """This group's costliest people this calendar month, merged accounts as one.

    The read-side person primitive, written out once so later person-level readings
    copy it rather than reinvent it: the ledger stores the causing *account* (append-
    only, never rewritten), and the person is resolved at query time by joining
    identity_account and grouping on entity_id - a merge repoints the account rows,
    so every past charge follows the person the moment the owner declares them one.
    An account the identity layer has never seen groups as itself.
    """
    today = _as_date(today_local())
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
        group_id, today.replace(day=1), today, k,
    )
    return [dict(r) for r in rows]


def _group(group_id: str | int | None) -> int | None:
    """A group id as the ledger stores it.

    Everything above the repositories carries it as a string, because that is what the
    platform hands over and what a config key looks like; the column is a bigint like
    every other group_id in the schema. Converting in one place keeps a write from
    failing at runtime with a type error nobody sees until the bill is wrong.

    None becomes 0, the row for costs that belong to no single group.
    """
    return 0 if group_id is None else int(group_id)


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
        today.replace(day=1), today, kind, model,
    )
    return int(v or 0)


async def day_cost(day: str | date) -> float:
    v = await pool().fetchval(
        "SELECT COALESCE(sum(cny),0) FROM cost_ledger WHERE day=$1", _as_date(day)
    )
    return float(v or 0.0)


async def day_breakdown(day: str | date, group_id: str | None = None) -> list[dict]:
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
