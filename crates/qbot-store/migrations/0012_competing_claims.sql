-- Beliefs have one confidence, the probability that the claim holds now, computed on read
-- (qbot_memory::belief), and nothing is deleted for having little of it.
--
-- Facts: a new value no longer supersedes the old one and an unrepeated fact no longer expires.
-- Claims of a subject sharing a *slot* compete, and the later statement counts against the
-- earlier one. Every row gains its slot; `key` becomes the normalized value of every claim;
-- `ended_ms` is the latest time the claim was said to hold no longer. Superseded and expired
-- facts become claims again: one replaced by a competitor is contradicted by it (its end is
-- dropped), one said to have ended keeps that time, one that expired was merely not repeated.
-- A value that was superseded and later recorded again is one claim with the evidence of both.
--
-- Names: confirmed means vouched for by a person; a name learned from chat is a lead with the
-- credibility of its distinct supporting episodes, never expired.

-- The comparison form of qbot_memory::facts::normalize_key.
CREATE FUNCTION pg_temp.normalize_value(t text) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT btrim(regexp_replace(lower(normalize(t, NFKC)), '\s+', ' ', 'g'), ' ')
$$;

ALTER TABLE fact DROP CONSTRAINT fact_check;
ALTER TABLE fact DROP CONSTRAINT fact_status_check;
DROP INDEX fact_one_active;
ALTER TABLE fact ADD COLUMN slot text;

-- The slots of qbot_memory::facts::{slot, term_slot, TOPIC_SLOT} for the predicate table as
-- shipped: the single-valued predicates, the likes/dislikes pair, and the rest multi-valued (a
-- multi-valued row's key already is its normalized value; a group term's key was its term).
UPDATE fact SET slot = CASE
    WHEN subject_account IS NULL AND predicate = 'group_topic' THEN 'group_topic'
    WHEN subject_account IS NULL THEN predicate || ':' || key
    WHEN predicate IN ('lives_in', 'from_place', 'works_as', 'works_at', 'studies_at',
                       'majors_in', 'birthday', 'preferred_name') THEN predicate
    WHEN predicate IN ('likes', 'dislikes') THEN 'dislikes/likes:' || key
    ELSE predicate || ':' || key
END;
UPDATE fact SET key = pg_temp.normalize_value(object)
WHERE subject_account IS NULL
   OR predicate IN ('lives_in', 'from_place', 'works_as', 'works_at', 'studies_at',
                    'majors_in', 'birthday', 'preferred_name');

-- Observations are made at their episode's end, so a claim superseded by a competitor ended at
-- the end of an episode that supports the competitor.
UPDATE fact f SET ended_ms = NULL
WHERE f.status = 'expired'
   OR (f.status = 'superseded' AND EXISTS (
        SELECT 1 FROM fact c
        JOIN fact_evidence e ON e.fact_id = c.fact_id
        JOIN episode ep ON ep.episode_id = e.episode_id
        WHERE c.group_id = f.group_id
          AND c.subject_account IS NOT DISTINCT FROM f.subject_account
          AND c.slot = f.slot
          AND (c.predicate <> f.predicate OR c.key <> f.key)
          AND ep.ended_ms = f.ended_ms));

-- One row per claim: the first row of each, with the evidence and span of all.
CREATE TEMPORARY TABLE fact_merge ON COMMIT DROP AS
SELECT fact_id, first_value(fact_id) OVER (
    PARTITION BY group_id, coalesce(subject_account, 0), slot, predicate, key ORDER BY fact_id
) AS keep
FROM fact WHERE status <> 'forgotten';
DELETE FROM fact_merge WHERE fact_id = keep;

INSERT INTO fact_evidence (fact_id, episode_id, message_id, quote)
SELECT m.keep, e.episode_id, e.message_id, e.quote
FROM fact_evidence e JOIN fact_merge m USING (fact_id)
ON CONFLICT DO NOTHING;
UPDATE fact k SET first_seen_ms = least(k.first_seen_ms, x.first_seen),
                  last_confirmed_ms = greatest(k.last_confirmed_ms, x.last_confirmed),
                  ended_ms = greatest(k.ended_ms, x.ended)
FROM (
    SELECT m.keep, min(f.first_seen_ms) AS first_seen, max(f.last_confirmed_ms) AS last_confirmed,
           max(f.ended_ms) AS ended
    FROM fact_merge m JOIN fact f USING (fact_id)
    GROUP BY m.keep
) x
WHERE k.fact_id = x.keep;
UPDATE fact k SET supports = greatest(1, (SELECT count(*) FROM fact_evidence e WHERE e.fact_id = k.fact_id))
WHERE k.fact_id IN (SELECT keep FROM fact_merge);
DELETE FROM fact WHERE fact_id IN (SELECT fact_id FROM fact_merge);

UPDATE fact SET status = 'active' WHERE status IN ('superseded', 'expired');
ALTER TABLE fact ALTER COLUMN slot SET NOT NULL;
ALTER TABLE fact ADD CONSTRAINT fact_slot_check CHECK (slot <> '');
ALTER TABLE fact ADD CONSTRAINT fact_status_check CHECK (status IN ('active', 'forgotten'));
CREATE UNIQUE INDEX fact_one_claim ON fact (group_id, coalesce(subject_account, 0), slot, predicate, key)
    WHERE status = 'active';

-- Names: confirmed by provenance, leads at the credibility (n + 1) / (n + 2) of their n
-- supporting episodes; expired leads come back. Expiry marked a lead removed, as a person's
-- removal does, and only ever leads: a removed name nobody set by hand is taken for an expired
-- lead (a person can remove it again).
UPDATE alias a SET removed = false
WHERE a.removed
  AND NOT EXISTS (SELECT 1 FROM alias_evidence e WHERE e.alias_id = a.alias_id AND e.kind = 'manual');
UPDATE alias a SET
    status = CASE WHEN x.manual THEN 'confirmed' ELSE 'candidate' END,
    confidence = CASE WHEN x.manual THEN 1 ELSE (x.episodes + 1)::real / (x.episodes + 2) END
FROM (
    SELECT al.alias_id,
           bool_or(e.kind = 'manual') IS TRUE AS manual,
           count(DISTINCT e.support) AS episodes
    FROM alias al LEFT JOIN alias_evidence e ON e.alias_id = al.alias_id
    GROUP BY al.alias_id
) x
WHERE a.alias_id = x.alias_id AND NOT a.removed;
