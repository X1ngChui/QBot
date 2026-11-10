-- What the extraction before the active-memory rebuild found was kept beside each episode as a
-- record. Nothing reads it: memories carried over cite their lines in memory_evidence, and
-- earlier episodes are read again by the current extraction (memory.reread_per_run).
ALTER TABLE episode DROP COLUMN legacy_findings;
