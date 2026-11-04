-- Active memory rebuilt (docs/memory-design.md).
--
-- Identity: every member line keeps the display name it was sent under, so what a member is and
-- was shown as comes from the archive. Names people use are *uses*, each with how the line used
-- it (their own name, addressing them, talking about them, or set by a person with /name);
-- whom a name refers to is read from all its uses together. A name a person removed is
-- suppressed for that member until a person sets it again.
--
-- Memories: one short statement about a member or the group, with its evidence lines. A later
-- statement may replace one (the old is kept as replaced) or end it. Durability sets how fast
-- it loses rank, never how true it is.

ALTER TABLE chat_line ADD COLUMN sender_name text
    CHECK (sender_name IS NULL OR (sender_name <> '' AND char_length(sender_name) <= 128));
CREATE INDEX chat_line_sender_name ON chat_line (group_id, account_id, at_ms)
    WHERE sender_name IS NOT NULL;

CREATE TABLE name_use (
    group_id   bigint NOT NULL,
    account_id bigint NOT NULL REFERENCES account (account_id),
    -- qbot_memory::names::name_key of the text.
    key        text NOT NULL CHECK (key <> '' AND char_length(key) <= 64),
    text       text NOT NULL CHECK (text <> '' AND char_length(text) <= 64),
    basis      text NOT NULL CHECK (basis IN ('set', 'own', 'addressed', 'about')),
    -- The episode and line a learned use comes from; NULL for a name set by a person.
    episode_id bigint,
    message_id bigint,
    quote      text,
    at_ms      bigint NOT NULL,
    CHECK ((basis = 'set') = (episode_id IS NULL)),
    CHECK ((episode_id IS NULL) = (message_id IS NULL)),
    CHECK ((episode_id IS NULL) = (quote IS NULL))
);
-- One use per member, name and episode (and one set use).
CREATE UNIQUE INDEX name_use_once ON name_use (group_id, account_id, key, coalesce(episode_id, -1));
CREATE INDEX name_use_key ON name_use (group_id, key);

CREATE TABLE name_removed (
    group_id   bigint NOT NULL,
    account_id bigint NOT NULL REFERENCES account (account_id),
    key        text NOT NULL,
    removed_ms bigint NOT NULL,
    PRIMARY KEY (group_id, account_id, key)
);

CREATE TABLE memory (
    memory_id       bigserial PRIMARY KEY,
    group_id        bigint NOT NULL,
    -- NULL: a memory about the group.
    subject_account bigint REFERENCES account (account_id),
    -- For a group term: the term as members write it.
    term            text CHECK (term IS NULL OR (term <> '' AND char_length(term) <= 64)),
    text            text NOT NULL CHECK (text <> '' AND char_length(text) <= 300),
    -- qbot_memory::memories::memory_key of the term and text: what "the same memory" means.
    key             text NOT NULL CHECK (key <> ''),
    durability      text NOT NULL CHECK (durability IN ('lasting', 'ongoing', 'current')),
    state           text NOT NULL CHECK (state IN ('current', 'replaced', 'ended')),
    replaced_by     bigint REFERENCES memory (memory_id) ON DELETE SET NULL,
    -- When it was replaced or said to have ended.
    closed_ms       bigint,
    first_ms        bigint NOT NULL,
    last_ms         bigint NOT NULL,
    legacy_fact     bigint,
    CHECK (subject_account IS NULL OR term IS NULL),
    CHECK ((state = 'current') = (closed_ms IS NULL)),
    CHECK (state = 'replaced' OR replaced_by IS NULL),
    CHECK (first_ms <= last_ms)
);
CREATE INDEX memory_subject ON memory (group_id, subject_account);
CREATE UNIQUE INDEX memory_one_current ON memory (group_id, coalesce(subject_account, 0), key)
    WHERE state = 'current';

-- Each episode supports a memory at most once.
CREATE TABLE memory_evidence (
    memory_id      bigint NOT NULL REFERENCES memory (memory_id) ON DELETE CASCADE,
    episode_id     bigint NOT NULL,
    message_id     bigint NOT NULL,
    -- Who wrote the line; NULL when a migrated line can no longer be found.
    author_account bigint,
    quote          text NOT NULL,
    at_ms          bigint NOT NULL,
    PRIMARY KEY (memory_id, episode_id)
);

-- ---- carried over ---------------------------------------------------------------------------

-- The comparison form of qbot_memory::{names::name_key, memories::memory_key}.
CREATE FUNCTION pg_temp.fold(t text) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT btrim(regexp_replace(lower(normalize(t, NFKC)), '\s+', ' ', 'g'), ' ')
$$;

-- Names a person set stay set; names a person removed stay removed. Names learned from chat
-- are not carried: their evidence never said how a line used the name.
INSERT INTO name_use (group_id, account_id, key, text, basis, at_ms)
SELECT DISTINCT ON (a.group_id, a.account_id, pg_temp.fold(a.text))
       a.group_id, a.account_id, pg_temp.fold(a.text), a.text, 'set', min(e.at_ms)
FROM alias a JOIN alias_evidence e ON e.alias_id = a.alias_id AND e.kind = 'manual'
WHERE NOT a.removed AND char_length(pg_temp.fold(a.text)) BETWEEN 1 AND 64
GROUP BY a.group_id, a.account_id, a.text
ORDER BY a.group_id, a.account_id, pg_temp.fold(a.text);
INSERT INTO name_removed (group_id, account_id, key, removed_ms)
SELECT group_id, account_id, pg_temp.fold(text), max(last_evidence_ms)
FROM alias WHERE removed AND pg_temp.fold(text) <> ''
GROUP BY group_id, account_id, pg_temp.fold(text);

-- Facts become memories: a member fact as `predicate: value`, a group term with its meaning, a
-- topic as its text. Durability follows the old decay class.
INSERT INTO memory (group_id, subject_account, term, text, key, durability, state, first_ms,
                    last_ms, legacy_fact)
SELECT f.group_id, f.subject_account, t.term, t.text,
       pg_temp.fold(coalesce(t.term || ': ', '') || t.text),
       CASE f.decay WHEN 'stable' THEN 'lasting' WHEN 'fast' THEN 'current' ELSE 'ongoing' END,
       'current', least(f.first_seen_ms, f.last_confirmed_ms), f.last_confirmed_ms, f.fact_id
FROM fact f
CROSS JOIN LATERAL (
    SELECT CASE WHEN f.subject_account IS NULL AND f.predicate = 'group_term'
                THEN left(coalesce(f.label, f.key), 64) END AS term,
           left(CASE WHEN f.subject_account IS NULL THEN f.object
                     ELSE replace(f.predicate, '_', ' ') || ': ' || f.object END, 300) AS text
) t
WHERE f.status = 'active'
ORDER BY f.fact_id
ON CONFLICT DO NOTHING;

INSERT INTO memory_evidence (memory_id, episode_id, message_id, author_account, quote, at_ms)
SELECT m.memory_id, e.episode_id, e.message_id,
       (SELECT l.account_id FROM chat_line l
        WHERE l.group_id = m.group_id AND l.message_id = e.message_id
        ORDER BY l.seq DESC LIMIT 1),
       e.quote, coalesce(ep.ended_ms, m.last_ms)
FROM fact_evidence e
JOIN memory m ON m.legacy_fact = e.fact_id
LEFT JOIN episode ep ON ep.episode_id = e.episode_id;

-- Of competing claims (one slot of one subject), the most recently supported stays current and
-- replaces the others.
WITH ranked AS (
    SELECT m.memory_id, m.last_ms,
           first_value(m.memory_id) OVER w AS winner,
           first_value(m.last_ms) OVER w AS winner_ms,
           count(*) OVER (PARTITION BY f.group_id, coalesce(f.subject_account, 0), f.slot) AS n
    FROM memory m JOIN fact f ON f.fact_id = m.legacy_fact
    WINDOW w AS (PARTITION BY f.group_id, coalesce(f.subject_account, 0), f.slot
                 ORDER BY m.last_ms DESC, m.memory_id DESC)
)
UPDATE memory m SET state = 'replaced', replaced_by = r.winner,
                    closed_ms = greatest(r.winner_ms, m.last_ms)
FROM ranked r
WHERE m.memory_id = r.memory_id AND r.n > 1 AND r.memory_id <> r.winner;

-- A fact said to have ended after its last support has ended.
UPDATE memory m SET state = 'ended', closed_ms = f.ended_ms
FROM fact f
WHERE f.fact_id = m.legacy_fact AND m.state = 'current'
  AND f.ended_ms IS NOT NULL AND f.ended_ms > f.last_confirmed_ms;

ALTER TABLE memory DROP COLUMN legacy_fact;

-- Episodes keep what the earlier extraction found, as a record; the new findings start empty
-- and every stored episode counts as applied.
ALTER TABLE episode ADD COLUMN legacy_findings jsonb;
UPDATE episode SET legacy_findings = findings, findings = '{}' WHERE findings <> '{}';
UPDATE episode SET consolidated_ms = created_ms WHERE consolidated_ms IS NULL;

DROP TABLE alias_evidence;
DROP TABLE alias;
DROP TABLE fact_evidence;
DROP TABLE fact;
