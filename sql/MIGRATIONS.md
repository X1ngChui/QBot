# Hand migrations

`init.sql` only runs on an empty `data/pg`, so every schema change after first start
is applied by hand to the live database (and to the `qbot-pgtest` test database).
`ensure_schema` (qqbot/db/repo.py) refuses to boot until the live schema has the
tables, columns, unique indexes, the cost_ledger primary key and the vector width
the code expects - but it checks by name, so this file records the actual
statements, newest first. Types mirror
`init.sql`, which is always the authoritative shape of a fresh database.

Apply with:

```bash
docker exec qbot-postgres-1 psql -U qqbot -d qqbot -c "<statement>"
```

## 2026-09-11 — uploaded originals carry their upload time

```sql
ALTER TABLE image_cache ADD COLUMN IF NOT EXISTS file_uploaded_at TIMESTAMPTZ;
```

A NULL reads as "too old to trust", and re-uploading needs the bytes - which for a
picture whose link has expired means a get_image the platform may never answer. So
the rows are stamped from each picture's first sighting, which is when the upload
happened; only pictures first seen beyond the backend's retention stay NULL:

```sql
WITH first AS (
  SELECT CASE WHEN seg->>'type'='mface' THEN seg->'data'->>'emoji_id'
              ELSE lower(substring(coalesce(seg->'data'->>'file', seg->'data'->>'file_id','')
                                   from '[0-9a-fA-F]{32}')) END AS key,
         min(occurred_at) AS first_seen
    FROM raw_event, jsonb_array_elements(payload->'segments') seg
   WHERE seg->>'type' IN ('image','mface') GROUP BY 1)
UPDATE image_cache c SET file_uploaded_at = f.first_seen
  FROM first f
 WHERE c.key = f.key AND c.file_id IS NOT NULL AND c.file_uploaded_at IS NULL
   AND f.first_seen > now() - interval '30 days';
```

## 2026-09-09 — image descriptions expire

```sql
ALTER TABLE image_cache ADD COLUMN IF NOT EXISTS described_at TIMESTAMPTZ;
ALTER TABLE image_cache ADD COLUMN IF NOT EXISTS refused BOOLEAN NOT NULL DEFAULT FALSE;
```

The cache had no expiry of any kind, so the first description a picture ever got
was served forever - including the placeholder written when a content filter
declined to look at one. Existing rows keep `described_at` NULL deliberately:
their descriptions are of unknown age and count as expired, so each is rewritten
the next time that picture is actually posted again. Nothing is refreshed in
bulk, and a picture never seen twice is never paid for twice.

`refused` is not backfilled either - the old placeholder rows are
indistinguishable from real descriptions in the data, and they expire anyway.

## 2026-09-08 — reserved-bracket markers (data rewrite, no schema change)

```sql
-- No DDL. Transcript markers moved from ASCII square brackets to a reserved
-- bracket pair that is stripped from every string a member can type, so a
-- marker in a transcript can only have been written by this system; the
-- stored derived readings in raw_event.plain_text and image_cache.description
-- were rewritten
-- once by scripts/migrate_markers.py (best-effort pattern rules documented in
-- the script). Run it inside the bot container while the bot is stopped or
-- idle, and only with memory_candidate empty of pending rows - consolidation
-- validates quotes against a re-rendered transcript:
--   docker compose run --rm bot python scripts/migrate_markers.py
```

## 2026-09-07 — namesake serials

```sql
-- New table: run the CREATE TABLE member_seq block from init.sql verbatim
-- (the UNIQUE (group_id, seq) constraint is part of the block and the
-- assignment relies on it).
```

## 2026-09-04 — the user agreement gate

```sql
-- Same-day reshapes: the first cut was keyed by user alone (consent is per
-- (group, user)), the second lacked the version column. Drop and re-run the
-- CREATE TABLE user_agreement block from init.sql verbatim, or on the
-- two-column shape:
ALTER TABLE user_agreement ADD COLUMN IF NOT EXISTS version INT NOT NULL DEFAULT 1;
```

## 2026-09-04 — the exit guard stops punishing

A flagged reply is simply dropped; no member is auto-blocked, so the strike
table goes.

```sql
DROP TABLE IF EXISTS censor_offense;
```

## 2026-09-04 — alias evidence fast-path probe

```sql
CREATE INDEX IF NOT EXISTS alias_evidence_probe
    ON alias_evidence (alias_id, evidence_type, created_at);
DROP INDEX IF EXISTS alias_evidence_alias;
```

## 2026-09-03 — timed blocks

```sql
ALTER TABLE group_blocklist ADD COLUMN IF NOT EXISTS blocked_until TIMESTAMPTZ;
```

## 2026-09-02 — spend attribution (/top)

```sql
ALTER TABLE cost_ledger ADD COLUMN IF NOT EXISTS user_id VARCHAR(64) NOT NULL DEFAULT '';
ALTER TABLE cost_ledger DROP CONSTRAINT cost_ledger_pkey;
ALTER TABLE cost_ledger ADD PRIMARY KEY (day, group_id, kind, model, user_id);
```

## 2026-08-31 — exact replay for validation

```sql
ALTER TABLE memory_candidate ADD COLUMN IF NOT EXISTS batch_size INT;
CREATE INDEX IF NOT EXISTS raw_event_group_created
    ON raw_event (group_id, created_at) WHERE event_type = 'message';
```

## 2026-08-30 — retrieval traces

```sql
-- New table: run the CREATE TABLE reply_trace block from init.sql verbatim,
-- AND the CREATE UNIQUE INDEX reply_trace_reply statement below it - the
-- trace upsert's ON CONFLICT depends on the index, not the table.
```

## Earlier (pre-ledger; reconstructed from ensure_schema's column list)

```sql
ALTER TABLE raw_event        ADD COLUMN IF NOT EXISTS plain_text TEXT;
ALTER TABLE group_state      ADD COLUMN IF NOT EXISTS first_seen_at TIMESTAMPTZ;
ALTER TABLE image_cache      ADD COLUMN IF NOT EXISTS file_id VARCHAR(64);
ALTER TABLE memory_fact      ADD COLUMN IF NOT EXISTS object_key TEXT;
ALTER TABLE memory_candidate ADD COLUMN IF NOT EXISTS batch_event_id UUID REFERENCES raw_event(id);
ALTER TABLE cost_ledger      ADD COLUMN IF NOT EXISTS day DATE;
```
