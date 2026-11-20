-- What is known about the people of a group and the group itself is one store. A memory is a name
-- a member is called by or a statement about a member (possibly with a second member) or the
-- group, from chat, from a ruling over a name's uses, or entered by hand. What supports it are
-- observations: one line telling one thing about one subject. What a person asked to be
-- forgotten is kept as a suppression, so it is never learned again.
--
-- Everything learned or entered before is dropped and the store starts clean: names, memories,
-- held statements, rulings and notes. Episodes stay; what their reading found was kept with
-- them and goes.
DROP TABLE held_claim;
DROP TABLE name_ruling;
DROP TABLE name_reference;
DROP TABLE name_set;
DROP TABLE name_removed;
DROP TABLE memory_evidence;
DROP TABLE memory;
DROP TABLE note;
ALTER TABLE episode DROP COLUMN findings, DROP COLUMN consolidated_ms;

CREATE TABLE memory (
    memory_id       bigserial PRIMARY KEY,
    group_id        bigint NOT NULL,
    -- NULL: the group.
    subject_account bigint REFERENCES account (account_id),
    kind            text NOT NULL CHECK (kind IN ('name', 'statement')),
    -- A second member a statement involves.
    with_account    bigint REFERENCES account (account_id),
    -- For a group term: the term as members write it.
    term            text CHECK (term IS NULL OR (term <> '' AND char_length(term) <= 64)),
    -- The statement, or the name as most recently written.
    text            text NOT NULL CHECK (text <> '' AND char_length(text) <= 200),
    -- What "the same memory" means: qbot_memory's name_key for a name, memory_key otherwise.
    key             text NOT NULL CHECK (key <> ''),
    durability      text NOT NULL CHECK (durability IN ('lasting', 'ongoing', 'current')),
    origin          text NOT NULL CHECK (origin IN ('chat', 'ruling', 'hand')),
    -- Who entered it by hand.
    set_by          bigint,
    state           text NOT NULL CHECK (state IN ('current', 'replaced', 'ended')),
    replaced_by     bigint REFERENCES memory (memory_id) ON DELETE SET NULL,
    -- When it was replaced or ended.
    closed_ms       bigint,
    first_ms        bigint NOT NULL,
    last_ms         bigint NOT NULL,
    CHECK (subject_account IS NOT NULL OR kind = 'statement'),
    CHECK (subject_account IS NULL OR term IS NULL),
    CHECK (with_account IS NULL OR (subject_account IS NOT NULL AND with_account <> subject_account)),
    CHECK ((origin = 'hand') = (set_by IS NOT NULL)),
    CHECK ((state = 'current') = (closed_ms IS NULL)),
    CHECK (state = 'replaced' OR replaced_by IS NULL),
    CHECK (first_ms <= last_ms)
);
CREATE INDEX memory_subject ON memory (group_id, subject_account);
-- One current memory per subject, kind and key.
CREATE UNIQUE INDEX memory_one_current ON memory (group_id, coalesce(subject_account, 0), kind, key)
    WHERE state = 'current';

CREATE TABLE observation (
    observation_id  bigserial PRIMARY KEY,
    group_id        bigint NOT NULL,
    -- The memory it supports; NULL while pending.
    memory_id       bigint REFERENCES memory (memory_id) ON DELETE CASCADE,
    episode_id      bigint NOT NULL REFERENCES episode (episode_id) ON DELETE CASCADE,
    message_id      bigint NOT NULL,
    ordinal         bigint NOT NULL CHECK (ordinal >= 1),
    author_account  bigint NOT NULL,
    quote           text NOT NULL,
    at_ms           bigint NOT NULL,
    -- Whom it is about: a member, the group, or a name nobody has placed yet.
    about           text NOT NULL CHECK (about IN ('member', 'group', 'name')),
    about_account   bigint,
    about_name      text,
    kind            text NOT NULL CHECK (kind IN ('name', 'statement')),
    -- The name as written, or the statement.
    text            text NOT NULL CHECK (text <> ''),
    -- A name's use: how the line used it, what showed who was meant.
    basis           text CHECK (basis IN ('own', 'addressed', 'about')),
    grounds         text CHECK (grounds IN ('marker', 'answer', 'content', 'self', 'listed', 'ruling', 'none')),
    anchored        boolean,
    shown_by        bigint[],
    -- A statement's term, durability and second member.
    term            text,
    durability      text CHECK (durability IN ('lasting', 'ongoing', 'current')),
    with_account    bigint,
    CHECK ((about = 'member') = (about_account IS NOT NULL)),
    CHECK ((about = 'name') = (about_name IS NOT NULL)),
    CHECK ((kind = 'name') = (basis IS NOT NULL AND grounds IS NOT NULL AND anchored IS NOT NULL)),
    CHECK ((kind = 'statement') = (durability IS NOT NULL))
);
CREATE INDEX observation_memory ON observation (memory_id);
CREATE INDEX observation_pending ON observation (group_id) WHERE memory_id IS NULL;

CREATE TABLE suppression (
    suppression_id  bigserial PRIMARY KEY,
    group_id        bigint NOT NULL,
    subject_account bigint,
    kind            text NOT NULL CHECK (kind IN ('name', 'statement')),
    -- What was forgotten, kept only so the batch can recognize it said again; never shown.
    text            text NOT NULL,
    key             text NOT NULL,
    messages        bigint[] NOT NULL,
    by_account      bigint NOT NULL,
    at_ms           bigint NOT NULL
);
CREATE UNIQUE INDEX suppression_once ON suppression (group_id, coalesce(subject_account, 0), kind, key);
