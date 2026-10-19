-- Repeating group tasks. A repeating task is one wake timer: after each occurrence the scheduler
-- returns it to pending at the next occurrence of its rule, so nothing depends on the model
-- scheduling it again. The rule is an interval of whole minutes, every day at a time, or chosen
-- weekdays (bit 0 Monday .. bit 6 Sunday) at a time; a time is minutes after midnight in
-- repeat_zone, the IANA zone the rule was set in. last_* is how the latest occurrence ended.
-- Existing rows are one-shot tasks and jobs: every new column is NULL for them.
ALTER TABLE timer
    ADD COLUMN repeat_kind         text CHECK (repeat_kind IN ('interval', 'daily', 'weekly')),
    ADD COLUMN repeat_interval_ms  bigint CHECK (repeat_interval_ms > 0 AND repeat_interval_ms % 60000 = 0),
    ADD COLUMN repeat_minute       integer CHECK (repeat_minute BETWEEN 0 AND 1439),
    ADD COLUMN repeat_days         smallint CHECK (repeat_days BETWEEN 1 AND 127),
    ADD COLUMN repeat_zone         text,
    ADD COLUMN last_due_ms         bigint,
    ADD COLUMN last_outcome        text CHECK (last_outcome IN ('ran', 'skipped_muted')),
    ADD COLUMN last_outcome_detail text,
    ADD CONSTRAINT timer_repeat_shape CHECK (
        CASE repeat_kind
            WHEN 'interval' THEN repeat_interval_ms IS NOT NULL AND repeat_minute IS NULL AND repeat_days IS NULL
            WHEN 'daily' THEN repeat_interval_ms IS NULL AND repeat_minute IS NOT NULL AND repeat_days IS NULL
            WHEN 'weekly' THEN repeat_interval_ms IS NULL AND repeat_minute IS NOT NULL AND repeat_days IS NOT NULL
            ELSE repeat_interval_ms IS NULL AND repeat_minute IS NULL AND repeat_days IS NULL AND repeat_zone IS NULL
        END
        AND (repeat_kind IS NULL OR (kind = 'wake' AND repeat_zone IS NOT NULL))),
    ADD CONSTRAINT timer_last_run CHECK (
        (last_due_ms IS NULL) = (last_outcome IS NULL)
        AND (last_outcome IS NULL OR repeat_kind IS NOT NULL)
        AND (last_outcome_detail IS NULL OR last_outcome IS NOT NULL));
