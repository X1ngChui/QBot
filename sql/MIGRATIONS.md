# Schema changelog

`init.sql` is the schema of a fresh database and only runs when the data directory is
empty. Every later change is applied by hand to existing databases and recorded here,
newest first. At boot the bot checks that the live schema has the tables, columns,
unique indexes and vector width the code expects and refuses to start until it does;
it checks names, not the statements that get you there, which is why this file exists.

Apply a statement with:

```bash
docker exec qbot-postgres-1 psql -U qqbot -d qqbot -c "<statement>"
```

Types mirror `init.sql`. Where an entry says to run a block from `init.sql`, copy it
verbatim: some `ON CONFLICT` clauses depend on the unique index that follows the table.

## 2026-09-17 — provider-scoped image file handles

```sql
ALTER TABLE image_cache ADD COLUMN IF NOT EXISTS file_provider VARCHAR(32);
```

Existing handles intentionally keep a null provider and are uploaded again on first use;
a file id issued by one provider must never be sent to another.

## 2026-09-17 — structured, expiring reply evidence

```sql
ALTER TABLE reply_trace ADD COLUMN IF NOT EXISTS memo JSONB;
ALTER TABLE reply_trace ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;
ALTER TABLE reply_trace ALTER COLUMN content SET DEFAULT '';
UPDATE reply_trace
   SET expires_at = created_at + INTERVAL '30 days'
 WHERE memo IS NULL AND expires_at IS NULL;
CREATE INDEX IF NOT EXISTS reply_trace_expiry
    ON reply_trace (expires_at)
    WHERE expires_at IS NOT NULL;
```

The table name stays stable for a low-risk migration. New writes use only versioned JSON;
legacy text remains readable until its assigned expiry, then the nightly sweep removes it.

## 2026-09-17 — member numbers replace namesake serials

```sql
DROP TABLE IF EXISTS member_seq;
UPDATE raw_event SET plain_text = regexp_replace(plain_text, '⟦同名\d+⟧', '', 'g')
 WHERE plain_text LIKE '%⟦同名%';
UPDATE reply_trace SET content = regexp_replace(content, '⟦同名\d+⟧', '', 'g')
 WHERE content LIKE '%⟦同名%';
```

Members sharing a name are told apart by per-prompt member numbers, which are never
stored. The serial table goes, and the serial tags written into archived text and
retrieval traces are removed.

## 2026-09-11 — batch width and description stamps become mandatory

```sql
UPDATE memory_candidate SET batch_size = 60 WHERE batch_size IS NULL;
ALTER TABLE memory_candidate ALTER COLUMN batch_size SET NOT NULL;

UPDATE image_cache SET described_at = TIMESTAMPTZ '2026-07-31 00:00+08'
 WHERE description <> '' AND described_at IS NULL;
ALTER TABLE image_cache ADD CONSTRAINT image_cache_described_stamped
    CHECK (description = '' OR described_at IS NOT NULL);
```

Candidates staged before batches recorded their width were cut at sixty rows.
Descriptions without a stamp are of unknown age and are stamped as expired.

Archived lines written under earlier marker forms (a namesake serial in parentheses,
a picture description in square brackets) are rewritten to the reserved-bracket
grammar:

```sql
UPDATE raw_event SET plain_text = regexp_replace(plain_text,
           '@([^\s(]+)\((\d+)\)', '@\1⟦同名\2⟧', 'g')
 WHERE plain_text ~ '@[^\s(]+\(\d+\)';
UPDATE raw_event SET plain_text = regexp_replace(plain_text,
           '\[图片:([^\]]*)\]', '⟦图片:\1⟧', 'g')
 WHERE plain_text LIKE '%[图片:%';
```

## 2026-09-11 — uploaded originals carry their upload time

```sql
ALTER TABLE image_cache ADD COLUMN IF NOT EXISTS file_uploaded_at TIMESTAMPTZ;
```

A NULL means the upload is too old to trust and the picture is uploaded again when
opened. Existing rows can be stamped from each picture's first sighting:

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

Existing rows keep `described_at` NULL: their descriptions count as expired and are
rewritten the next time the picture is posted. Nothing is refreshed in bulk.

## 2026-09-08 — reserved-bracket markers

No DDL. Transcript markers moved from ASCII square brackets to the reserved pair
`⟦ ⟧`; `raw_event.plain_text` and `image_cache.description` were rewritten once.

## 2026-09-07 — namesake serials

New table: run the `CREATE TABLE member_seq` block from `init.sql`, including its
`UNIQUE (group_id, seq)` constraint.

## 2026-09-04 — user agreement

New table: run the `CREATE TABLE user_agreement` block from `init.sql`. On a database
that already has the table without the version column:

```sql
ALTER TABLE user_agreement ADD COLUMN IF NOT EXISTS version INT NOT NULL DEFAULT 1;
```

## 2026-09-04 — output moderation strike table removed

```sql
DROP TABLE IF EXISTS censor_offense;
```

## 2026-09-04 — alias evidence probe index

```sql
CREATE INDEX IF NOT EXISTS alias_evidence_probe
    ON alias_evidence (alias_id, evidence_type, created_at);
DROP INDEX IF EXISTS alias_evidence_alias;
```

## 2026-09-03 — timed blocks

```sql
ALTER TABLE group_blocklist ADD COLUMN IF NOT EXISTS blocked_until TIMESTAMPTZ;
```

## 2026-09-02 — spend attribution

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

New table: run the `CREATE TABLE reply_trace` block from `init.sql` and the
`CREATE UNIQUE INDEX reply_trace_reply` statement that follows it.

## Earlier

```sql
ALTER TABLE raw_event        ADD COLUMN IF NOT EXISTS plain_text TEXT;
ALTER TABLE group_state      ADD COLUMN IF NOT EXISTS first_seen_at TIMESTAMPTZ;
ALTER TABLE image_cache      ADD COLUMN IF NOT EXISTS file_id VARCHAR(64);
ALTER TABLE memory_fact      ADD COLUMN IF NOT EXISTS object_key TEXT;
ALTER TABLE memory_candidate ADD COLUMN IF NOT EXISTS batch_event_id UUID REFERENCES raw_event(id);
ALTER TABLE cost_ledger      ADD COLUMN IF NOT EXISTS day DATE;

CREATE UNIQUE INDEX IF NOT EXISTS raw_event_platform_key
    ON raw_event (platform, platform_event_id)
    WHERE platform_event_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS alias_unique_in_scope
    ON alias (COALESCE(group_id, 0), normalized_text, target_entity_id);
CREATE UNIQUE INDEX IF NOT EXISTS fact_one_current
    ON memory_fact (COALESCE(group_id, 0), subject_entity_id, predicate,
                    COALESCE(object_key, ''))
    WHERE valid_to IS NULL AND status = 'active';
CREATE UNIQUE INDEX IF NOT EXISTS job_pending_once
    ON memory_job (job_type, (payload->>'group_id'))
    WHERE status = 'pending';
```
