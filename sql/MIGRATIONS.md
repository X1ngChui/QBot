# Hand migrations

`init.sql` only runs on an empty `data/pg`, so every schema change after first start
is applied by hand to the live database (and to the `qbot-pgtest` test database).
`ensure_schema` (qqbot/db/repo.py) refuses to boot until the live schema has the
tables, columns and the cost_ledger primary key the code expects - but it checks by
name, so this file records the actual statements, newest first. Types mirror
`init.sql`, which is always the authoritative shape of a fresh database.

Apply with:

```bash
docker exec qbot-postgres-1 psql -U qqbot -d qqbot -c "<statement>"
```

## 2026-09-04 — the user agreement gate

```sql
-- New table: run the CREATE TABLE user_agreement block from init.sql verbatim.
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
