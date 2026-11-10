-- Names as a ledger of references (docs/memory-design.md).
--
-- A reference is one recorded use of an expression for a person: the line, the expression, who
-- it named (a member, the bot, someone outside the group, or nobody the reader could place),
-- how the line used it and what showed who was meant. Whom a name belongs to is read from all
-- its references together. An expression used again and again for someone nobody could place
-- is ruled on from its uses across conversations (name_ruling). Names a person set are kept
-- apart (name_set), and a statement about a person known only by an unplaced name is held
-- (held_claim) until the name is some member's.

CREATE TABLE name_reference (
    reference_id    bigserial PRIMARY KEY,
    group_id        bigint NOT NULL,
    -- The episode whose reading recorded it, and the line: its message, its ordinal in the
    -- group's chat and who wrote it.
    episode_id      bigint NOT NULL,
    message_id      bigint NOT NULL,
    ordinal         bigint NOT NULL CHECK (ordinal >= 0),
    speaker_account bigint NOT NULL REFERENCES account (account_id),
    -- qbot_memory::names::name_key of the text.
    key             text NOT NULL CHECK (key <> '' AND char_length(key) <= 64),
    text            text NOT NULL CHECK (text <> '' AND char_length(text) <= 64),
    who             text NOT NULL CHECK (who IN ('member', 'bot', 'outside', 'unknown')),
    account_id      bigint REFERENCES account (account_id),
    -- How the line uses it: the member for themselves, speaking to the person, or of them.
    basis           text NOT NULL CHECK (basis IN ('own', 'addressed', 'about')),
    -- What showed who was meant.
    grounds         text NOT NULL CHECK (grounds IN ('marker', 'answer', 'content', 'self',
                                                     'listed', 'ruling', 'none')),
    -- The chat's structure ties the expression to the member: the line is said to them, or
    -- they reply to it.
    anchored        boolean NOT NULL DEFAULT false,
    shown_by        bigint[] NOT NULL DEFAULT '{}',
    at_ms           bigint NOT NULL,
    CHECK ((who = 'member') = (account_id IS NOT NULL)),
    -- One reading of an expression per episode and line.
    UNIQUE (group_id, episode_id, message_id, key)
);
CREATE INDEX name_reference_key ON name_reference (group_id, key);
CREATE INDEX name_reference_member ON name_reference (group_id, account_id)
    WHERE account_id IS NOT NULL;

CREATE TABLE name_set (
    group_id   bigint NOT NULL,
    account_id bigint NOT NULL REFERENCES account (account_id),
    key        text NOT NULL CHECK (key <> '' AND char_length(key) <= 64),
    text       text NOT NULL CHECK (text <> '' AND char_length(text) <= 64),
    at_ms      bigint NOT NULL,
    PRIMARY KEY (group_id, account_id, key)
);
CREATE INDEX name_set_key ON name_set (group_id, key);

CREATE TABLE name_ruling (
    group_id   bigint NOT NULL,
    key        text NOT NULL CHECK (key <> '' AND char_length(key) <= 64),
    text       text NOT NULL CHECK (text <> '' AND char_length(text) <= 64),
    verdict    text NOT NULL CHECK (verdict IN ('member', 'outside', 'general', 'unknown')),
    account_id bigint REFERENCES account (account_id),
    -- The lines the ruling rests on, and what they show.
    lines      bigint[] NOT NULL DEFAULT '{}',
    reason     text NOT NULL DEFAULT '',
    -- The unplaced uses the expression had when it was ruled on.
    seen       integer NOT NULL CHECK (seen >= 0),
    at_ms      bigint NOT NULL,
    PRIMARY KEY (group_id, key),
    CHECK ((verdict = 'member') = (account_id IS NOT NULL))
);

CREATE TABLE held_claim (
    held_id        bigserial PRIMARY KEY,
    group_id       bigint NOT NULL,
    key            text NOT NULL CHECK (key <> '' AND char_length(key) <= 64),
    name           text NOT NULL CHECK (name <> '' AND char_length(name) <= 64),
    text           text NOT NULL CHECK (text <> '' AND char_length(text) <= 300),
    durability     text NOT NULL CHECK (durability IN ('lasting', 'ongoing', 'current')),
    episode_id     bigint NOT NULL,
    message_id     bigint NOT NULL,
    author_account bigint,
    quote          text NOT NULL,
    at_ms          bigint NOT NULL,
    -- The member it is remembered about, once the name was theirs; NULL while held.
    filed_account  bigint REFERENCES account (account_id),
    UNIQUE (group_id, episode_id, message_id, text)
);
CREATE INDEX held_claim_waiting ON held_claim (group_id) WHERE filed_account IS NULL;

-- ---- carried over ---------------------------------------------------------------------------

INSERT INTO name_set (group_id, account_id, key, text, at_ms)
SELECT group_id, account_id, key, text, at_ms FROM name_use WHERE basis = 'set';

-- A learned use becomes a reference where its line is still in the archive. What showed who
-- was meant follows what the earlier rules required: the member's own word; a line said to the
-- member; for talk about them, their reply to the line where there was one, else the reading.
INSERT INTO name_reference (group_id, episode_id, message_id, ordinal, speaker_account, key, text,
                            who, account_id, basis, grounds, anchored, at_ms)
SELECT u.group_id, u.episode_id, u.message_id, l.ordinal, l.account_id, u.key, u.text,
       'member', u.account_id, u.basis,
       CASE WHEN u.basis = 'own' THEN 'self'
            WHEN u.basis = 'addressed' THEN 'marker'
            WHEN u.anchored THEN 'answer'
            ELSE 'content' END,
       u.anchored AND u.basis <> 'own', u.at_ms
FROM name_use u
CROSS JOIN LATERAL (
    SELECT c.ordinal, c.account_id FROM chat_line c
    WHERE c.group_id = u.group_id AND c.message_id = u.message_id AND c.account_id IS NOT NULL
    ORDER BY c.seq DESC LIMIT 1
) l
WHERE u.basis <> 'set'
  AND ((u.basis = 'own') = (l.account_id = u.account_id))
ON CONFLICT DO NOTHING;

DROP TABLE name_use;
