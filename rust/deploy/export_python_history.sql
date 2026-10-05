-- Export the Python bot's chat history for `qbot import-history`, one JSON object per line.
-- Read-only: run against the Python bot's database with
--
--   psql -X -q -A -t -v ON_ERROR_STOP=1 -U qqbot -d qqbot -f export_python_history.sql > history.jsonl
--
-- The output is the group chat itself: keep it as private as the database and delete it after
-- the import.

BEGIN TRANSACTION READ ONLY ISOLATION LEVEL REPEATABLE READ;

-- Picture descriptions, keyed by the picture's file hash.
SELECT json_build_object('description', json_build_object('key', key, 'text', description))
FROM image_cache
WHERE description IS NOT NULL AND description <> '' AND NOT refused;

-- Messages, oldest first: the order they are numbered in.
SELECT json_build_object('message', json_build_object(
    'group_id', group_id,
    'user_id', platform_user_id,
    'at_ms', (extract(epoch FROM occurred_at) * 1000)::bigint,
    'payload', payload))
FROM raw_event
WHERE event_type = 'message'
ORDER BY occurred_at, created_at, id;

COMMIT;
