-- Maintenance schedules (the nightly run, the daily report) are repeating jobs: one job timer
-- with a rule, which the scheduler returns to pending at its next occurrence after each run,
-- as it does a repeating group task. The table that remembered which occurrence of a cron
-- schedule had been turned into a job is no longer needed; the scheduler creates each
-- schedule's job when it starts, due at its next occurrence.
DROP TABLE recurrence;

-- The stages of the nightly run were never queued by themselves.
DELETE FROM timer WHERE job_kind IN ('decay', 'backup', 'cleanup');
ALTER TABLE timer
    DROP CONSTRAINT timer_job_kind_check,
    ADD CONSTRAINT timer_job_kind_check CHECK (job_kind IN ('nightly', 'extract', 'report'));

-- A job may carry a rule too, and its latest occurrence ends as a job does.
ALTER TABLE timer
    DROP CONSTRAINT timer_repeat_shape,
    ADD CONSTRAINT timer_repeat_shape CHECK (
        CASE repeat_kind
            WHEN 'interval' THEN repeat_interval_ms IS NOT NULL AND repeat_minute IS NULL AND repeat_days IS NULL
            WHEN 'daily' THEN repeat_interval_ms IS NULL AND repeat_minute IS NOT NULL AND repeat_days IS NULL
            WHEN 'weekly' THEN repeat_interval_ms IS NULL AND repeat_minute IS NOT NULL AND repeat_days IS NOT NULL
            ELSE repeat_interval_ms IS NULL AND repeat_minute IS NULL AND repeat_days IS NULL AND repeat_zone IS NULL
        END
        AND (repeat_kind IS NULL OR repeat_zone IS NOT NULL)),
    DROP CONSTRAINT timer_last_outcome_check,
    ADD CONSTRAINT timer_last_outcome_check CHECK (
        last_outcome IS NULL
        OR (kind = 'wake' AND last_outcome IN ('ran', 'skipped_muted'))
        OR (kind = 'job' AND last_outcome IN ('job_ok', 'job_failed')));
