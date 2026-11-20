-- Memory is read once a day by each group's batch, which divides the chat into episodes where the
-- conversations divide: an episode is no longer a fixed number of whole batches of lines.
ALTER TABLE episode
    DROP COLUMN first_batch,
    DROP COLUMN last_batch,
    DROP COLUMN batch_lines;
-- Dropping the batch columns dropped the check that named them with the ordinals.
ALTER TABLE episode ADD CONSTRAINT episode_range CHECK (first_ordinal <= last_ordinal);

-- What a group's view of its history hangs on between two batches: when its last batch ran (who
-- is who and what the group knows are shown as of then), and where a run the provider found too
-- long moved the start of the raw part (0: not moved since the batch).
CREATE TABLE history_window (
    group_id   bigint PRIMARY KEY,
    batched_ms bigint NOT NULL,
    cut        bigint NOT NULL DEFAULT 0 CHECK (cut >= 0)
);
-- The groups as of now: their view holds still from here until their first batch.
INSERT INTO history_window (group_id, batched_ms)
SELECT DISTINCT group_id, (extract(epoch FROM now()) * 1000)::bigint FROM chat_line;

-- Extraction is the daily batch, one job for every group; filled batches no longer queue a
-- group's own.
DELETE FROM timer WHERE kind = 'job' AND job_kind = 'extract' AND state IN ('pending', 'claimed');
