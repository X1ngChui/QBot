-- Offline conversion from the account-owned task and single-note schema.
-- Stop the runtime and verify a fresh backup before applying this transaction.
-- This is an explicit one-time conversion, not a startup migration or fallback.
BEGIN;

-- Creator-dependent goals cannot be interpreted safely as anonymous group goals.
-- Retain their history, but require pending work to be established under the group contract.
UPDATE scheduled_task SET status='failed', outcome='scope_changed', finished_at=clock_timestamp()
    WHERE status='pending';
DROP INDEX scheduled_task_pending_creator;
ALTER TABLE scheduled_task DROP COLUMN creator_id;
CREATE INDEX scheduled_task_active_group ON scheduled_task (group_id, due_at, id)
    WHERE status IN ('pending', 'running');

WITH legacy_notes AS (
    SELECT id, first_value(id) OVER (
        PARTITION BY group_id, subject_entity_id, subject_account_id
        ORDER BY created_at, id
    ) AS note_key
    FROM memory_fact WHERE predicate='note' AND object_key IS NULL
)
UPDATE memory_fact AS fact SET object_key=legacy.note_key::text
    FROM legacy_notes AS legacy WHERE fact.id=legacy.id;

ALTER TABLE memory_fact ADD CONSTRAINT fact_note_key_valid
    CHECK (predicate <> 'note' OR
        (object_key IS NOT NULL AND object_key ~
         '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'));

-- Pending invitations require fresh recipient confirmation after conversion.
UPDATE account_link_challenge SET status='expired' WHERE status='pending';
ALTER TABLE account_link_challenge DROP COLUMN token_hash;
DROP INDEX account_link_pending_target;
CREATE UNIQUE INDEX account_link_pending_target ON account_link_challenge (group_id,target_account_id)
    WHERE status='pending';
DROP INDEX account_link_pending_initiator;
CREATE UNIQUE INDEX account_link_pending_initiator ON account_link_challenge (group_id,initiator_account_id)
    WHERE status='pending';
ALTER TABLE account_link_challenge ADD CONSTRAINT account_link_created_event_key
    UNIQUE (created_event_id);
ALTER TABLE account_link_challenge ADD CONSTRAINT account_link_confirmed_event_key
    UNIQUE (confirmed_event_id);

COMMIT;
