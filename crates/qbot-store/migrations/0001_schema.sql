-- The QBot schema.
--
-- Conventions: instants are bigint milliseconds since the Unix epoch; platform ids are bigint;
-- closed sets are text with CHECK constraints; every table that belongs to a group carries
-- group_id, and every query filters by it.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS btree_gist;

-- ---- group state and the chat archive ---------------------------------------------------

CREATE TABLE group_state (
    group_id      bigint PRIMARY KEY,
    muted         boolean NOT NULL DEFAULT false,
    first_seen_ms bigint NOT NULL
);

-- A blocked account cannot start a run. Its lines still enter every run's context.
CREATE TABLE group_block (
    group_id   bigint NOT NULL,
    account_id bigint NOT NULL,
    until_ms   bigint,             -- NULL: until removed
    created_ms bigint NOT NULL,
    PRIMARY KEY (group_id, account_id)
);

-- Stable per-group member numbers, assigned once on first appearance and never reused.
CREATE TABLE member_number (
    group_id   bigint NOT NULL,
    account_id bigint NOT NULL,
    number     integer NOT NULL CHECK (number > 0),
    PRIMARY KEY (group_id, account_id),
    UNIQUE (group_id, number)
);

-- The archive. The unique (group_id, message_id) key is the only dedup gate: a duplicate event
-- stores nothing and has no further effect.
CREATE TABLE chat_line (
    seq        bigserial PRIMARY KEY,
    group_id   bigint NOT NULL,
    -- Dense per-group number from 1, assigned under the group lock. History batches, and the
    -- memory slices cut on the same grid, are ranges of ordinals.
    ordinal    bigint NOT NULL CHECK (ordinal >= 1),
    message_id bigint NOT NULL,
    speaker    text NOT NULL CHECK (speaker IN ('bot', 'member')),
    account_id bigint,
    at_ms      bigint NOT NULL,
    text       text NOT NULL,
    CHECK ((speaker = 'member') = (account_id IS NOT NULL)),
    UNIQUE (group_id, message_id),
    UNIQUE (group_id, ordinal)
);
CREATE INDEX chat_line_group_seq ON chat_line (group_id, seq);

-- ---- runs: the complete, append-only record of every agent run --------------------------

CREATE TABLE run (
    run_id        bigserial PRIMARY KEY,
    group_id      bigint NOT NULL,
    trigger_kind  text NOT NULL CHECK (trigger_kind IN ('addressed', 'wake')),
    trigger       jsonb NOT NULL,
    started_ms    bigint NOT NULL,
    ended_ms      bigint,
    end_reason    text CHECK (end_reason IN (
        'completed', 'delivered', 'step_limit', 'deadline', 'model_error',
        'cancelled', 'environment', 'interrupted')),
    error_class   text,
    input_tokens  bigint,
    cached_tokens bigint,
    output_tokens bigint,
    turns         integer,
    tool_calls    integer,
    sends         integer,
    CHECK ((ended_ms IS NULL) = (end_reason IS NULL))
);
CREATE INDEX run_group ON run (group_id, run_id DESC);
CREATE INDEX run_open ON run (run_id) WHERE ended_ms IS NULL;

-- Items are never updated or deleted. seq is dense from zero; the loader verifies that and
-- replays the items through the transcript invariants.
CREATE TABLE run_item (
    run_id  bigint NOT NULL REFERENCES run (run_id) ON DELETE CASCADE,
    seq     integer NOT NULL CHECK (seq >= 0),
    kind    text NOT NULL,
    payload jsonb NOT NULL,
    PRIMARY KEY (run_id, seq)
);

-- ---- timers: group tasks and background jobs ---------------------------------------------

CREATE TABLE timer (
    id             bigserial PRIMARY KEY,
    kind           text NOT NULL CHECK (kind IN ('wake', 'job')),
    due_ms         bigint NOT NULL,
    state          text NOT NULL CHECK (state IN ('pending', 'claimed', 'done', 'cancelled', 'interrupted')),
    attempts       integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    lease_until_ms bigint,
    outcome        text,
    outcome_detail text,
    created_ms     bigint NOT NULL,
    -- wake (group task) fields
    group_id       bigint,
    intent         text,
    chain_id       bigint,
    chain_depth    integer CHECK (chain_depth >= 0),
    origin         text CHECK (origin IN ('model', 'owner')),
    -- job fields (group_id is optional for jobs)
    job_kind       text CHECK (job_kind IN ('nightly', 'extract', 'decay', 'backup', 'report', 'cleanup')),
    CHECK (kind <> 'wake' OR (group_id IS NOT NULL AND intent IS NOT NULL AND chain_id IS NOT NULL
                              AND chain_depth IS NOT NULL AND origin IS NOT NULL AND job_kind IS NULL)),
    CHECK (kind <> 'job' OR (job_kind IS NOT NULL AND intent IS NULL AND chain_id IS NULL)),
    CHECK ((state = 'done') = (outcome IS NOT NULL)),
    CHECK (lease_until_ms IS NULL OR state = 'claimed')
);
CREATE INDEX timer_due ON timer (due_ms, id) WHERE state = 'pending';
CREATE INDEX timer_group_active ON timer (group_id, due_ms, id)
    WHERE kind = 'wake' AND state IN ('pending', 'claimed');
CREATE INDEX timer_job_lease ON timer (lease_until_ms) WHERE kind = 'job' AND state = 'claimed';

-- The last occurrence of each recurring schedule that was turned into a job. Firing is one
-- statement that advances this and inserts the job, so an occurrence is never fired twice.
CREATE TABLE recurrence (
    name          text PRIMARY KEY,
    last_fired_ms bigint NOT NULL
);

-- ---- usage: informational accounting; nothing reads it to refuse work -------------------

CREATE TABLE usage_event (
    id               bigserial PRIMARY KEY,
    at_ms            bigint NOT NULL,
    group_id         bigint NOT NULL,
    run_id           bigint NOT NULL,
    turn             integer NOT NULL,
    kind             text NOT NULL CHECK (kind IN ('model', 'tool', 'run')),
    provider         text,
    model            text,
    tool             text,
    status           text NOT NULL,
    input_tokens     bigint,
    cached_tokens    bigint,        -- NULL: the provider did not report cache usage
    output_tokens    bigint,
    reasoning_tokens bigint,
    attempts         integer,
    count            integer,       -- run events: tool calls in the run
    latency_ms       bigint NOT NULL
);
CREATE INDEX usage_event_time ON usage_event (at_ms);
CREATE INDEX usage_event_run ON usage_event (run_id);

CREATE VIEW usage_model_daily AS
SELECT at_ms / 86400000 AS day, group_id, provider, model,
       count(*) AS calls,
       count(*) FILTER (WHERE status <> 'ok') AS failed_calls,
       coalesce(sum(input_tokens), 0)::bigint AS input_tokens,
       coalesce(sum(cached_tokens), 0)::bigint AS cached_tokens,
       coalesce(sum(output_tokens), 0)::bigint AS output_tokens,
       coalesce(sum(reasoning_tokens), 0)::bigint AS reasoning_tokens,
       avg(latency_ms)::double precision AS avg_latency_ms
FROM usage_event WHERE kind = 'model'
GROUP BY 1, 2, 3, 4;

CREATE VIEW usage_tool_daily AS
SELECT at_ms / 86400000 AS day, group_id, tool,
       count(*) AS calls,
       count(*) FILTER (WHERE status <> 'ok') AS not_ok,
       avg(latency_ms)::double precision AS avg_latency_ms
FROM usage_event WHERE kind = 'tool'
GROUP BY 1, 2, 3;

-- ---- pictures and voice --------------------------------------------------------------------

-- What pictures were described as, so the same picture is never described (and paid for) twice.
-- Keys are either a platform file id ("p:..."), which is known before anything is downloaded,
-- or the SHA-256 of the picture's bytes ("h:..."), which recognises the same picture arriving
-- under a different id. Voice clips are not cached.
CREATE TABLE media_cache (
    key         text PRIMARY KEY,
    description text NOT NULL CHECK (description <> ''),
    created_ms  bigint NOT NULL
);

-- Where each picture, sticker or clip of a message can be fetched again, by its
-- position among the markers of its kind in the line. Written with the line. Bytes are never
-- stored; these are the platform's references (a file id, a link that may expire).
CREATE TABLE media_ref (
    group_id   bigint NOT NULL,
    message_id bigint NOT NULL,
    kind       text NOT NULL CHECK (kind IN ('image', 'sticker', 'voice')),
    idx        integer NOT NULL CHECK (idx >= 0),
    key        text,
    file       text,
    url        text,
    size_bytes bigint CHECK (size_bytes IS NULL OR size_bytes >= 0),
    PRIMARY KEY (group_id, message_id, kind, idx),
    FOREIGN KEY (group_id, message_id) REFERENCES chat_line (group_id, message_id)
);

-- ---- identity ------------------------------------------------------------------------------

-- A person. Linking merges holders (the loser points at the winner and keeps its history);
-- every change to the set of accounts bumps the revision so a stale confirmation is detectable.
CREATE TABLE holder (
    holder_id   bigserial PRIMARY KEY,
    created_ms  bigint NOT NULL,
    revision    integer NOT NULL DEFAULT 0 CHECK (revision >= 0),
    merged_into bigint REFERENCES holder (holder_id),
    CHECK (merged_into IS NULL OR merged_into <> holder_id)
);

-- A platform login, always attached to a current (unmerged) holder.
CREATE TABLE account (
    account_id    bigint PRIMARY KEY,
    holder_id     bigint NOT NULL REFERENCES holder (holder_id),
    first_seen_ms bigint NOT NULL
);
CREATE INDEX account_holder ON account (holder_id);

-- A name in a group, pointing at one login or at a person. Confidence and status are computed
-- in Rust from the evidence rows and stored for lookup.
CREATE TABLE alias (
    alias_id         bigserial PRIMARY KEY,
    group_id         bigint NOT NULL,
    text             text NOT NULL CHECK (text <> '' AND char_length(text) <= 64),
    target_kind      text NOT NULL CHECK (target_kind IN ('account', 'holder')),
    target_account   bigint REFERENCES account (account_id),
    target_holder    bigint REFERENCES holder (holder_id),
    status           text NOT NULL CHECK (status IN ('candidate', 'confirmed', 'inactive')),
    confidence       real NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
    removed          boolean NOT NULL DEFAULT false,
    last_evidence_ms bigint NOT NULL,
    CHECK ((target_kind = 'account') = (target_account IS NOT NULL)),
    CHECK ((target_kind = 'holder') = (target_holder IS NOT NULL)),
    CHECK (NOT removed OR status = 'inactive')
);
CREATE UNIQUE INDEX alias_account_unique ON alias (group_id, text, target_account) WHERE target_kind = 'account';
CREATE UNIQUE INDEX alias_holder_unique ON alias (group_id, text, target_holder) WHERE target_kind = 'holder';
CREATE INDEX alias_confirmed_lookup ON alias (group_id, text) WHERE status = 'confirmed';

CREATE TABLE alias_evidence (
    alias_id bigint NOT NULL REFERENCES alias (alias_id) ON DELETE CASCADE,
    kind     text NOT NULL CHECK (kind IN ('manual', 'extracted')),
    support  bigint,            -- extracted: the supporting episode; otherwise NULL
    at_ms    bigint NOT NULL,
    CHECK ((kind = 'extracted') = (support IS NOT NULL))
);
CREATE UNIQUE INDEX alias_evidence_unique ON alias_evidence (alias_id, kind, COALESCE(support, -1));

-- An invitation to link two accounts. One pending invitation per account per group is enforced
-- by the two partial unique indexes plus a check across the sides in the same transaction.
CREATE TABLE link_invitation (
    invitation_id      bigserial PRIMARY KEY,
    group_id           bigint NOT NULL,
    initiator          bigint NOT NULL REFERENCES account (account_id),
    target             bigint NOT NULL REFERENCES account (account_id),
    created_by         bigint NOT NULL,
    created_ms         bigint NOT NULL,
    initiator_revision integer NOT NULL,
    target_revision    integer NOT NULL,
    state              text NOT NULL CHECK (state IN ('pending', 'applied', 'cancelled', 'expired')),
    confirmed_by       bigint,
    CHECK (initiator <> target),
    CHECK ((state = 'applied') = (confirmed_by IS NOT NULL))
);
CREATE UNIQUE INDEX invitation_pending_initiator ON link_invitation (group_id, initiator) WHERE state = 'pending';
CREATE UNIQUE INDEX invitation_pending_target ON link_invitation (group_id, target) WHERE state = 'pending';

-- ---- episodes ------------------------------------------------------------------------------

-- The summary of one slice: whole chat batches on the history grid. The exclusion constraint
-- makes it impossible for two episodes of a group to claim the same lines.
CREATE TABLE episode (
    episode_id    bigserial PRIMARY KEY,
    group_id      bigint NOT NULL,
    first_batch   bigint NOT NULL CHECK (first_batch >= 0),
    last_batch    bigint NOT NULL,
    first_ordinal bigint NOT NULL CHECK (first_ordinal >= 1),
    last_ordinal  bigint NOT NULL,
    batch_lines   integer NOT NULL CHECK (batch_lines >= 1),
    first_message bigint NOT NULL,
    last_message  bigint NOT NULL,
    started_ms    bigint NOT NULL,
    ended_ms      bigint NOT NULL,
    line_count    integer NOT NULL CHECK (line_count >= 1),
    participants  bigint[] NOT NULL,
    title         text NOT NULL CHECK (title <> ''),
    summary       text NOT NULL CHECK (summary <> ''),
    evidence      jsonb NOT NULL,
    method        text NOT NULL,
    model         text NOT NULL,
    created_ms    bigint NOT NULL,
    -- Validated names, facts and group knowledge found in the slice (see qbot-memory findings),
    -- and when they were applied; NULL until then.
    findings        jsonb NOT NULL DEFAULT '{}',
    consolidated_ms bigint,
    CHECK (first_batch <= last_batch AND first_ordinal <= last_ordinal),
    EXCLUDE USING gist (group_id WITH =, int8range(first_ordinal, last_ordinal, '[]') WITH &&)
);
CREATE INDEX episode_group_order ON episode (group_id, first_ordinal);

-- The vector recall finds an episode by: one per episode, from the embedding model that made it.
-- After the model or width changes, the nightly run embeds each episode again from its title and
-- summary. Recall is an exact scan filtered by group first; no approximate index.
CREATE TABLE episode_embedding (
    episode_id bigint PRIMARY KEY REFERENCES episode (episode_id) ON DELETE CASCADE,
    model      text NOT NULL,
    dims       integer NOT NULL CHECK (dims >= 1),
    embedding  vector NOT NULL,
    CHECK (vector_dims(embedding) = dims)
);

-- ---- facts and notes -----------------------------------------------------------------------

-- Facts about people and about groups, with time attached (see qbot-memory facts).
CREATE TABLE fact (
    fact_id           bigserial PRIMARY KEY,
    group_id          bigint NOT NULL,
    -- NULL: a fact about the group itself.
    subject_account   bigint REFERENCES account (account_id),
    predicate         text NOT NULL CHECK (predicate <> ''),
    -- What distinguishes values of the predicate; empty for single-valued predicates.
    key               text NOT NULL,
    object            text NOT NULL CHECK (object <> ''),
    label             text,
    status            text NOT NULL CHECK (status IN ('active', 'superseded', 'expired', 'forgotten')),
    supports          integer NOT NULL CHECK (supports >= 1),
    decay             text NOT NULL CHECK (decay IN ('stable', 'default', 'fast')),
    first_seen_ms     bigint NOT NULL,
    last_confirmed_ms bigint NOT NULL,
    ended_ms          bigint,
    CHECK ((status = 'active') = (ended_ms IS NULL) OR status = 'forgotten')
);
-- One active value per subject, predicate and key.
CREATE UNIQUE INDEX fact_one_active ON fact (group_id, coalesce(subject_account, 0), predicate, key)
    WHERE status = 'active';
CREATE INDEX fact_active ON fact (group_id, subject_account) WHERE status = 'active';

-- Each episode supports a fact at most once.
CREATE TABLE fact_evidence (
    fact_id    bigint NOT NULL REFERENCES fact (fact_id) ON DELETE CASCADE,
    episode_id bigint NOT NULL,
    message_id bigint NOT NULL,
    quote      text NOT NULL,
    PRIMARY KEY (fact_id, episode_id)
);

-- Notes members write by hand about a member: shown as written, never derived from or merged
-- with learned memory. Removing a note deletes it.
CREATE TABLE note (
    note_id        bigserial PRIMARY KEY,
    group_id       bigint NOT NULL,
    account_id     bigint NOT NULL REFERENCES account (account_id),
    text           text NOT NULL CHECK (btrim(text) <> ''),
    -- Who wrote it, or last edited it.
    author_account bigint NOT NULL,
    created_ms     bigint NOT NULL,
    updated_ms     bigint NOT NULL CHECK (updated_ms >= created_ms)
);
CREATE INDEX note_by_account ON note (group_id, account_id, note_id);
