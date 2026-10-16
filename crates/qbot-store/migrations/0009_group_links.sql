-- Links between accounts belong to the group they were made in.
--
-- A link was a merge of global holders, so two accounts linked in one group were shown as one
-- person, under each other's names, in every group both are in. What is learned or stored in a
-- group stays in it: a link is now an edge recorded with its group, a person in a group is the
-- accounts linked to each other there, and the holder is gone. Names point at one account.
--
-- Existing links are kept where the group they were made in is on record: an applied invitation,
-- or an owner's `/link [at:N] [at:M]` in the archive (member numbers of that group). Both
-- accounts must still share a holder. A link with no such record is dropped and can be made
-- again in its group.

CREATE TABLE account_link (
    group_id     bigint NOT NULL,
    low_account  bigint NOT NULL REFERENCES account (account_id),
    high_account bigint NOT NULL REFERENCES account (account_id),
    created_ms   bigint NOT NULL,
    PRIMARY KEY (group_id, low_account, high_account),
    CHECK (low_account < high_account)
);
CREATE INDEX account_link_high ON account_link (group_id, high_account);

WITH made AS (
    SELECT group_id, initiator AS a, target AS b, created_ms
    FROM link_invitation WHERE state = 'applied'
    UNION ALL
    SELECT l.group_id, ma.account_id, mb.account_id, l.at_ms
    FROM (
        SELECT group_id, at_ms,
               regexp_match(text, '^\s*(?:\[reply:[^]]*\]\s*)?/link\s*\[at:(\d+)\]\s*\[at:(\d+)\]\s*$') AS m
        FROM chat_line WHERE speaker = 'member'
    ) l
    JOIN member_number ma ON ma.group_id = l.group_id AND ma.number = l.m[1]::integer
    JOIN member_number mb ON mb.group_id = l.group_id AND mb.number = l.m[2]::integer
    WHERE l.m IS NOT NULL
)
INSERT INTO account_link (group_id, low_account, high_account, created_ms)
SELECT made.group_id, least(a, b), greatest(a, b), min(made.created_ms)
FROM made
JOIN account x ON x.account_id = made.a
JOIN account y ON y.account_id = made.b
WHERE a <> b AND x.holder_id = y.holder_id
GROUP BY made.group_id, least(a, b), greatest(a, b);

-- A name given to a holder becomes a name of each of its accounts numbered in the alias's group.
CREATE TEMPORARY TABLE holder_alias_account ON COMMIT DROP AS
WITH RECURSIVE up(start, id, next) AS (
    SELECT holder_id, holder_id, merged_into FROM holder
    UNION ALL
    SELECT up.start, h.holder_id, h.merged_into FROM up JOIN holder h ON h.holder_id = up.next
)
SELECT al.alias_id AS old_alias, al.group_id, al.text, acc.account_id, al.status, al.confidence,
       al.removed, al.last_evidence_ms
FROM alias al
JOIN up ON up.start = al.target_holder AND up.next IS NULL
JOIN account acc ON acc.holder_id = up.id
JOIN member_number mn ON mn.group_id = al.group_id AND mn.account_id = acc.account_id
WHERE al.target_kind = 'holder';

INSERT INTO alias (group_id, text, target_kind, target_account, status, confidence, removed,
                   last_evidence_ms)
SELECT DISTINCT ON (group_id, text, account_id)
       group_id, text, 'account', account_id, status, confidence, removed, last_evidence_ms
FROM holder_alias_account
ORDER BY group_id, text, account_id, confidence DESC
ON CONFLICT DO NOTHING;

INSERT INTO alias_evidence (alias_id, kind, support, at_ms)
SELECT a.alias_id, e.kind, e.support, e.at_ms
FROM holder_alias_account h
JOIN alias_evidence e ON e.alias_id = h.old_alias
JOIN alias a ON a.group_id = h.group_id AND a.text = h.text AND a.target_kind = 'account'
            AND a.target_account = h.account_id
ON CONFLICT DO NOTHING;

-- Where an account already had the name, the stronger of the two stands.
UPDATE alias a SET
    confidence = greatest(a.confidence, h.confidence),
    status = CASE WHEN 'confirmed' IN (a.status, h.status) THEN 'confirmed' ELSE a.status END,
    removed = a.removed AND h.removed,
    last_evidence_ms = greatest(a.last_evidence_ms, h.last_evidence_ms)
FROM holder_alias_account h
WHERE a.group_id = h.group_id AND a.text = h.text AND a.target_kind = 'account'
  AND a.target_account = h.account_id;
UPDATE alias SET status = 'inactive' WHERE removed AND status <> 'inactive';

DELETE FROM alias WHERE target_kind = 'holder';
DROP INDEX alias_account_unique;
DROP INDEX alias_holder_unique;
ALTER TABLE alias DROP COLUMN target_holder;
ALTER TABLE alias DROP COLUMN target_kind;
ALTER TABLE alias RENAME COLUMN target_account TO account_id;
ALTER TABLE alias ALTER COLUMN account_id SET NOT NULL;
CREATE UNIQUE INDEX alias_unique ON alias (group_id, text, account_id);

-- An invitation remembers who each side was linked to in its group, instead of holder
-- revisions; one made before this change no longer applies.
UPDATE link_invitation SET state = 'cancelled' WHERE state = 'pending';
ALTER TABLE link_invitation DROP COLUMN initiator_revision;
ALTER TABLE link_invitation DROP COLUMN target_revision;
ALTER TABLE link_invitation ADD COLUMN initiator_linked bigint[] NOT NULL DEFAULT '{}';
ALTER TABLE link_invitation ADD COLUMN target_linked bigint[] NOT NULL DEFAULT '{}';
ALTER TABLE link_invitation ALTER COLUMN initiator_linked DROP DEFAULT;
ALTER TABLE link_invitation ALTER COLUMN target_linked DROP DEFAULT;

ALTER TABLE account DROP COLUMN holder_id;
DROP TABLE holder;
