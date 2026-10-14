-- Every provider call is recorded, whatever the work: reply runs, extraction and picture
-- description, model turns, embeddings, web searches and page reads. A provider call row says
-- which work it served (`purpose`) and what it was (`kind`); only a reply run's calls have a run.

ALTER TABLE usage_event
    ALTER COLUMN group_id DROP NOT NULL,
    ALTER COLUMN run_id DROP NOT NULL,
    ALTER COLUMN turn DROP NOT NULL;

-- HTTP requests a call made: retries, batches and uploads each count.
ALTER TABLE usage_event RENAME COLUMN attempts TO requests;

ALTER TABLE usage_event
    ADD COLUMN purpose text CHECK (purpose IN ('reply', 'extraction', 'picture')),
    -- Plan credits a search provider reported; NULL when it reported none.
    ADD COLUMN credits bigint;

-- Every model row so far was a reply run's.
UPDATE usage_event SET purpose = 'reply' WHERE kind = 'model';

ALTER TABLE usage_event DROP CONSTRAINT usage_event_kind_check;
ALTER TABLE usage_event
    ADD CONSTRAINT usage_event_kind_check
        CHECK (kind IN ('model', 'embedding', 'search', 'extract', 'tool', 'run')),
    -- Run and tool rows belong to a run; a provider call row says which work it served (NULL
    -- only for a call made outside any attributed work, which is logged as a wiring mistake).
    ADD CONSTRAINT usage_event_run_rows
        CHECK (kind IN ('model', 'embedding', 'search', 'extract')
               OR (group_id IS NOT NULL AND run_id IS NOT NULL AND turn IS NOT NULL
                   AND purpose IS NULL)),
    ADD CONSTRAINT usage_event_reply_calls
        CHECK (run_id IS NULL OR kind IN ('tool', 'run') OR purpose = 'reply');

DROP VIEW usage_model_daily;
CREATE VIEW usage_provider_daily AS
SELECT at_ms / 86400000 AS day, group_id, purpose, kind, provider, model,
       count(*) AS calls,
       coalesce(sum(requests), 0)::bigint AS requests,
       count(*) FILTER (WHERE status <> 'ok') AS failed_calls,
       coalesce(sum(input_tokens), 0)::bigint AS input_tokens,
       sum(cached_tokens)::bigint AS cached_tokens,
       coalesce(sum(output_tokens), 0)::bigint AS output_tokens,
       coalesce(sum(reasoning_tokens), 0)::bigint AS reasoning_tokens,
       sum(credits)::bigint AS credits,
       avg(latency_ms)::double precision AS avg_latency_ms
FROM usage_event WHERE kind IN ('model', 'embedding', 'search', 'extract')
GROUP BY 1, 2, 3, 4, 5, 6;
