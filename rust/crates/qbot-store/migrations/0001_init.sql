-- Schema v2. Forward-only migrations; there is no compatibility with the Python schema.
--
-- Conventions: instants are bigint milliseconds since the Unix epoch (UnixMillis); platform ids
-- are bigint; closed sets are text with CHECK constraints; every table that belongs to a group
-- carries group_id and every query filters by it.

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
