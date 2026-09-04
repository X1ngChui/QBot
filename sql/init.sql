-- QQ group-chat bot long-term memory: the physical model.
--
-- Layers as in design doc section 6.1:
--   L0 raw_event                     raw events, append-only, never modified
--   L1 entity / identity_account     person and account kept apart; the account is the
--                                    strong identity
--      alias / alias_evidence        names, with scope, evidence and status
--   L3 memory_fact / _evidence       temporal semantic facts
--      memory_candidate              LLM output, staged before validation
--   L4 episode / _participant/_event episodic memory
--   L6 embedding_index               vector projections, decoupled from what they project
--      memory_job                    the background job queue
--
-- L2 (reference-resolution traces) is deliberately absent: every reference this system
-- receives is an @ or a quote, where the platform states the account outright, so
-- resolution is not a judgement and a trace of it would have no reader.
--
-- Group isolation (design goal 4): on every table that carries a group_id, the group_id
-- is the first column of its indexes, and no retrieval path exists that crosses groups.
-- alias.group_id may be NULL for a global name - the one cross-group channel, and only
-- an owner may open it by hand; everything the LLM writes carries a group id.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;   -- gen_random_uuid()

-- ---------------------------------------------------------------- L0

CREATE TABLE IF NOT EXISTS raw_event (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    platform          VARCHAR(32)  NOT NULL,
    event_type        VARCHAR(64)  NOT NULL,
    group_id          BIGINT,
    platform_user_id  VARCHAR(128),
    platform_event_id VARCHAR(128),
    occurred_at       TIMESTAMPTZ  NOT NULL,
    -- The platform message verbatim: append-only, never modified. The derived reading
    -- lives in the plain_text column instead - it gets backfilled once a picture is
    -- understood, and something updatable has no place inside a blob that claims to be
    -- immutable.
    payload           JSONB        NOT NULL,
    plain_text        TEXT,
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- One platform message lands exactly once; this is what stops a replay after a
-- reconnect.
CREATE UNIQUE INDEX IF NOT EXISTS raw_event_platform_key
    ON raw_event (platform, platform_event_id)
    WHERE platform_event_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS raw_event_group_time
    ON raw_event (group_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS raw_event_speaker
    ON raw_event (group_id, platform_user_id, occurred_at DESC);
-- unread_since_extract filters on created_at (the ingest watermark), which the
-- occurred_at indexes cannot serve: without this the count is a full-group scan,
-- paid synchronously on every incoming message, over a table that only ever grows.
CREATE INDEX IF NOT EXISTS raw_event_group_created
    ON raw_event (group_id, created_at)
    WHERE event_type = 'message';

-- ---------------------------------------------------------------- L1

CREATE TABLE IF NOT EXISTS entity (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    entity_type    VARCHAR(32)  NOT NULL,
    canonical_name VARCHAR(256),
    status         VARCHAR(32)  NOT NULL DEFAULT 'active',
    -- After a merge this points at the survivor, and reads follow it one hop. Physical
    -- deletion would leave the historical records dangling.
    merged_into    UUID REFERENCES entity(id),
    revision       BIGINT       NOT NULL DEFAULT 1,
    created_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS entity_alive ON entity (entity_type) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS identity_account (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    entity_id        UUID        NOT NULL REFERENCES entity(id),
    platform         VARCHAR(32) NOT NULL,
    platform_user_id VARCHAR(128) NOT NULL,
    first_seen_at    TIMESTAMPTZ,
    last_seen_at     TIMESTAMPTZ,
    UNIQUE (platform, platform_user_id)
);

CREATE INDEX IF NOT EXISTS identity_account_entity ON identity_account (entity_id);

CREATE TABLE IF NOT EXISTS alias (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    alias_text         VARCHAR(256) NOT NULL,
    -- The form with case, whitespace and full-width folded away; matching goes through
    -- this, display always uses alias_text.
    normalized_text    VARCHAR(256) NOT NULL,
    target_entity_id   UUID         NOT NULL REFERENCES entity(id),
    group_id           BIGINT,
    alias_type         VARCHAR(32),
    confidence         REAL         NOT NULL,
    status             VARCHAR(32)  NOT NULL DEFAULT 'candidate',
    valid_from         TIMESTAMPTZ,
    valid_to           TIMESTAMPTZ,
    last_used_at       TIMESTAMPTZ,
    created_at         TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- One row per (group, name, entity): repeated sightings of the same binding upsert into
-- one row instead of accumulating duplicates. The same name may still point at several
-- entities - that ambiguity is real, and lookup hands it to the caller undecided.
CREATE UNIQUE INDEX IF NOT EXISTS alias_unique_in_scope
    ON alias (COALESCE(group_id, 0), normalized_text, target_entity_id);
-- Name resolution filters on normalized_text with (group_id = X OR group_id IS NULL);
-- the text is the selective column, so it leads and the group test rides along.
CREATE INDEX IF NOT EXISTS alias_lookup
    ON alias (normalized_text)
    WHERE status <> 'inactive';
CREATE INDEX IF NOT EXISTS alias_by_entity ON alias (target_entity_id);

CREATE TABLE IF NOT EXISTS alias_evidence (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    alias_id       UUID        NOT NULL REFERENCES alias(id) ON DELETE CASCADE,
    raw_event_id   UUID        REFERENCES raw_event(id),
    evidence_type  VARCHAR(64) NOT NULL,
    evidence_score REAL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Serves both the plain by-alias reads and upsert_alias's fast-path probe
-- (newest evidence of one type for one alias, an index-only backward scan).
CREATE INDEX IF NOT EXISTS alias_evidence_probe
    ON alias_evidence (alias_id, evidence_type, created_at);

-- ---------------------------------------------------------------- L3

CREATE TABLE IF NOT EXISTS memory_fact (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id          BIGINT,
    subject_entity_id UUID         NOT NULL REFERENCES entity(id),
    predicate         VARCHAR(128) NOT NULL,
    -- What distinguishes rows of a multi-valued predicate (likes, by the thing liked;
    -- term, by the word defined). NULL for single-valued predicates. This used to be
    -- folded into the predicate itself ("likes:<object>") - two values in one column, which
    -- first normal form forbids.
    object_key        TEXT,
    object_entity_id  UUID         REFERENCES entity(id),
    object_value      JSONB,
    memory_type       VARCHAR(64)  NOT NULL,
    confidence        REAL         NOT NULL,
    status            VARCHAR(32)  NOT NULL DEFAULT 'active',
    -- Facts carry time. "Quit the game" stamps valid_to on the old fact; it does not
    -- delete it.
    valid_from        TIMESTAMPTZ,
    valid_to          TIMESTAMPTZ,
    first_observed_at TIMESTAMPTZ,
    last_confirmed_at TIMESTAMPTZ,
    revision          BIGINT       NOT NULL DEFAULT 1,
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS fact_subject
    ON memory_fact (group_id, subject_entity_id, status);
-- One current fact per subject and predicate (and, for multi-valued predicates, per
-- object key). The database guarantees the invariant directly: violating it is a
-- unique-index conflict, not a race between writers.
CREATE UNIQUE INDEX IF NOT EXISTS fact_one_current
    ON memory_fact (COALESCE(group_id, 0), subject_entity_id, predicate,
                    COALESCE(object_key, ''))
    WHERE valid_to IS NULL AND status = 'active';

CREATE TABLE IF NOT EXISTS memory_fact_evidence (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    fact_id        UUID        NOT NULL REFERENCES memory_fact(id) ON DELETE CASCADE,
    raw_event_id   UUID        NOT NULL REFERENCES raw_event(id),
    relation       VARCHAR(32) NOT NULL,   -- supports | contradicts
    evidence_score REAL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS fact_evidence_fact ON memory_fact_evidence (fact_id);

-- Everything the LLM produces lands here first, and reaches memory_fact / alias only
-- through the Validator (design doc section 6.3).
CREATE TABLE IF NOT EXISTS memory_candidate (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id        BIGINT,
    source_event_id UUID        REFERENCES raw_event(id),
    -- The last message of the batch this came from, and how many rows that batch
    -- held. Validation has to run against the same batch the model read: the quote,
    -- the name and the term must all appear word for word in the messages it was
    -- shown. Consolidation is a separate queued job that runs later, and re-fetching
    -- the unread messages by then reads past a watermark that has moved, rejecting
    -- correctly-quoted records as if invented. Anchor plus size name the exact set;
    -- batches are cut at conversation gaps, so their length varies.
    batch_event_id  UUID        REFERENCES raw_event(id),
    batch_size      INT,
    candidate_type  VARCHAR(64) NOT NULL,
    payload         JSONB       NOT NULL,
    confidence      REAL,
    status          VARCHAR(32) NOT NULL DEFAULT 'pending',
    reject_reason   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS candidate_pending
    ON memory_candidate (group_id, created_at) WHERE status = 'pending';

-- ---------------------------------------------------------------- L4

CREATE TABLE IF NOT EXISTS episode (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id     BIGINT       NOT NULL,
    episode_type VARCHAR(64),
    title        VARCHAR(256),
    summary      TEXT         NOT NULL,
    started_at   TIMESTAMPTZ,
    ended_at     TIMESTAMPTZ,
    importance   REAL,
    confidence   REAL,
    status       VARCHAR(32)  NOT NULL DEFAULT 'active',
    revision     BIGINT       NOT NULL DEFAULT 1,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS episode_group_time ON episode (group_id, started_at DESC);

CREATE TABLE IF NOT EXISTS episode_participant (
    episode_id UUID NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    entity_id  UUID NOT NULL REFERENCES entity(id),
    role       VARCHAR(64),
    PRIMARY KEY (episode_id, entity_id)
);

CREATE INDEX IF NOT EXISTS episode_participant_entity ON episode_participant (entity_id);

CREATE TABLE IF NOT EXISTS episode_event (
    episode_id   UUID NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    raw_event_id UUID NOT NULL REFERENCES raw_event(id),
    PRIMARY KEY (episode_id, raw_event_id)
);

-- ---------------------------------------------------------------- L6

-- Vectors are decoupled from the objects they project: switching embedding models
-- rebuilds this one table (design doc section 6.1).
--
-- 2048 dimensions, text-embedding-v4. Measured on Chinese, the model separates related
-- from unrelated sentences by a cosine margin of 0.345 at 1024 dims and 0.388 at 2048;
-- the larger won.
--
-- The price is no ANN index: pgvector's hnsw caps at 2000 dims. Going without one is
-- deliberate - retrieval always filters by group_id first (the btree below), the
-- remaining set is in the hundreds, and an exact scan is both more accurate than an
-- approximation and indistinguishably fast. Dimension reduction becomes a conversation
-- the day one group outgrows that.
CREATE TABLE IF NOT EXISTS embedding_index (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id        BIGINT,
    object_type     VARCHAR(32)  NOT NULL,
    object_id       UUID         NOT NULL,
    embedding       VECTOR(2048) NOT NULL,
    embedding_model VARCHAR(128) NOT NULL,
    embedding_version INTEGER    NOT NULL DEFAULT 1,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    UNIQUE (object_type, object_id, embedding_model, embedding_version)
);

-- Group isolation holds in vector search too: filter by group, then measure distance -
-- never a store-wide nearest neighbour. The same index is the performance guarantee:
-- it confines the exact scan to one group.
CREATE INDEX IF NOT EXISTS embedding_group ON embedding_index (group_id, object_type);

CREATE TABLE IF NOT EXISTS memory_job (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    job_type     VARCHAR(64) NOT NULL,
    payload      JSONB       NOT NULL,
    status       VARCHAR(32) NOT NULL DEFAULT 'pending',
    priority     INTEGER     NOT NULL DEFAULT 0,
    retry_count  INTEGER     NOT NULL DEFAULT 0,
    max_retry    INTEGER     NOT NULL DEFAULT 5,
    available_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    locked_at    TIMESTAMPTZ,
    locked_by    VARCHAR(128),
    last_error   TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at  TIMESTAMPTZ
);

-- Claim order: higher priority first, then first come first served. SKIP LOCKED leans
-- on this.
CREATE INDEX IF NOT EXISTS job_claimable
    ON memory_job (priority DESC, available_at)
    WHERE status = 'pending';

-- Identical pending work is one job: submit() relies on this to collapse double
-- submissions (two messages crossing the extraction threshold at once).
CREATE UNIQUE INDEX IF NOT EXISTS job_pending_once
    ON memory_job (job_type, (payload->>'group_id'))
    WHERE status = 'pending';

-- ---------------------------------------------------------------- runtime

-- The bot's own working notes: which lookups fed one reply, digested. Not the
-- group's memory - nothing here was said in the group - so it lives beside
-- raw_event rather than inside it, and search_history / extraction never read it.
-- The window rebuild re-seats each entry in front of the reply it fed. Kept
-- forever like L0, by the owner's call: rows are small and disk is not the
-- constraint.
CREATE TABLE IF NOT EXISTS reply_trace (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id       BIGINT      NOT NULL,
    reply_event_id VARCHAR(64) NOT NULL,
    content        TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX IF NOT EXISTS reply_trace_reply
    ON reply_trace (group_id, reply_event_id);

-- Budget and usage, aggregated per day, group, kind and causing account: one
-- authoritative billing shape.
CREATE TABLE IF NOT EXISTS cost_ledger (
    day      DATE         NOT NULL,
    -- 0 means belonging to no group (a global search call, say). 0 rather than NULL:
    -- NULLs in a primary key never compare equal, ON CONFLICT stops matching, and the
    -- day's second global record quietly starts a second row - which silently breaks
    -- the one-row-per-day-group-kind shape this table exists for.
    group_id BIGINT       NOT NULL DEFAULT 0,
    kind     VARCHAR(32)  NOT NULL,
    model    VARCHAR(128) NOT NULL,
    -- The account whose action caused the spend: whoever addressed the bot for a
    -- reply, whoever posted the picture or clip for vision/ASR. '' for spend no
    -- single account caused (extraction reads everybody). An account id, not an
    -- entity id: entities merge and split after the fact, and the ledger is
    -- append-only - person-level readings aggregate through identity_account at
    -- query time, which is what makes them follow a later merge for free.
    user_id  VARCHAR(64)  NOT NULL DEFAULT '',
    calls    BIGINT       NOT NULL DEFAULT 0,
    in_hit   BIGINT       NOT NULL DEFAULT 0,
    in_miss  BIGINT       NOT NULL DEFAULT 0,
    out      BIGINT       NOT NULL DEFAULT 0,
    cny      NUMERIC(12,6) NOT NULL DEFAULT 0,
    PRIMARY KEY (day, group_id, kind, model, user_id)
);

-- Per-group runtime switches and watermarks, one typed column per field: the schema
-- itself says what runtime state the system keeps, with names and types a generic
-- kv/jsonb blob cannot offer.
CREATE TABLE IF NOT EXISTS group_state (
    group_id        BIGINT      PRIMARY KEY,
    muted           BOOLEAN     NOT NULL DEFAULT FALSE,
    -- When this group first reached the bot. Written on the first message, and the row's
    -- absence is what makes a group new: there is no allowlist to add a group to, so this
    -- is the only record that one appeared.
    first_seen_at   TIMESTAMPTZ,
    -- How far memory extraction has read. In the database rather than in memory: a
    -- restart must not send a half-accumulated batch back to zero.
    last_extract_at TIMESTAMPTZ,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- A group blocking members is a one-to-many relation, so it is a table - not an array
-- column inside group_state.
CREATE TABLE IF NOT EXISTS group_blocklist (
    group_id   BIGINT      NOT NULL,
    user_id    VARCHAR(128) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- NULL means blocked until somebody lifts it; a timestamp means the block
    -- lapses on its own at that moment (checked lazily, no sweeper).
    blocked_until TIMESTAMPTZ,
    PRIMARY KEY (group_id, user_id)
);

-- Image description cache. Keyed by image md5 or sticker id, deliberately without a
-- group: the same picture is the same picture wherever it is posted.
CREATE TABLE IF NOT EXISTS image_cache (
    key         VARCHAR(64) PRIMARY KEY,
    -- Empty until the describing call has run: the file_id can land first, and a row
    -- exists from whichever write happens first.
    description TEXT        NOT NULL DEFAULT '',
    -- Where the vision backend filed the original picture (Files API). Referenced by
    -- reply prompts to show the model the actual pixels; NULL when never uploaded, and
    -- stale once the backend's retention lapses - readers treat it as a hint.
    file_id     VARCHAR(64),
    hit_count   BIGINT      NOT NULL DEFAULT 0,
    last_seen   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
