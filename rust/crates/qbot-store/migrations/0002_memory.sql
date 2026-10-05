-- Identity and episodic memory.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS btree_gist;

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

-- One vector per episode per embedding model, so switching models hides old vectors until they
-- are rebuilt. Recall is an exact scan filtered by group first; no approximate index.
CREATE TABLE episode_embedding (
    episode_id bigint NOT NULL REFERENCES episode (episode_id) ON DELETE CASCADE,
    model      text NOT NULL,
    dims       integer NOT NULL CHECK (dims >= 1),
    embedding  vector NOT NULL,
    PRIMARY KEY (episode_id, model),
    CHECK (vector_dims(embedding) = dims)
);
