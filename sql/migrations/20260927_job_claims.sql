-- Offline migration: stop the old runtime and verify a fresh backup first.
-- ALTER without IF NOT EXISTS deliberately rejects partial or repeated application.
BEGIN;
SET LOCAL lock_timeout = '5s';
ALTER TABLE memory_job ADD COLUMN claim_token uuid;
ALTER TABLE memory_job ADD COLUMN lease_until timestamp with time zone;

-- Old executions are reclaimable, not replayed under their obsolete worker identity.
UPDATE memory_job SET claim_token=gen_random_uuid(), lease_until=clock_timestamp(),
    locked_at=COALESCE(locked_at, clock_timestamp()), locked_by=COALESCE(locked_by, 'pre-fencing')
WHERE status='running';
UPDATE memory_job SET locked_at=NULL, locked_by=NULL WHERE status<>'running';

ALTER TABLE memory_job ADD CONSTRAINT memory_job_status_valid
    CHECK (status IN ('pending', 'running', 'done', 'dead'));
ALTER TABLE memory_job ADD CONSTRAINT memory_job_claim_valid CHECK (
    (status='running' AND num_nonnulls(claim_token, lease_until, locked_at, locked_by)=4)
    OR (status<>'running' AND num_nonnulls(claim_token, lease_until, locked_at, locked_by)=0)
);
ALTER TABLE memory_job ADD CONSTRAINT memory_job_retries_valid
    CHECK (retry_count >= 0 AND max_retry >= 0);
CREATE UNIQUE INDEX job_expired ON memory_job (lease_until, id) WHERE status='running';
-- Attempts before this migration cannot be reconstructed; the new counter starts here.
ALTER TABLE memory_extraction ADD COLUMN model_attempts integer NOT NULL DEFAULT 0;
ALTER TABLE memory_extraction ADD CONSTRAINT memory_extraction_attempts_valid
    CHECK (model_attempts >= 0);
ALTER TABLE memory_extraction DROP CONSTRAINT memory_extraction_status_valid;
ALTER TABLE memory_extraction ADD CONSTRAINT memory_extraction_status_valid
    CHECK (status IN ('extracting', 'staged', 'applied', 'failed'));
ALTER TABLE memory_extraction DROP CONSTRAINT memory_extraction_live_snapshot_v2;
ALTER TABLE memory_extraction ADD CONSTRAINT memory_extraction_live_snapshot_v2 CHECK (
    status='applied' OR status='failed' OR status='extracting' AND snapshot IS NULL
    OR status='staged' AND snapshot IS NOT NULL AND snapshot @> '{"version": 2}'::jsonb
);
COMMIT;
