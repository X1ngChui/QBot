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
