-- The archive's dedup identity is QQ's own per-group message sequence, not NapCat's message id.
--
-- NapCat's `message_id` is a 31-bit hash of QQ's internal message id: unique in practice for a
-- while, never ordered, and two messages of a group collide with a chance that grows with the
-- square of the archive (about 17% at 28,000 lines). A colliding message was taken for a
-- duplicate and dropped. Group message events also carry `real_seq`, the sequence QQ's server
-- gives each message of a group: unique within the group, the same on every redelivery.
--
-- A line with a sequence is unique by it; two lines may now share a message id. A line without
-- one (a notice, whose id is derived from its content; an event that lacks the field; every
-- line archived before this migration) is still unique by its message id, as before.
--
-- At the deploy boundary a message archived just before (without a sequence) and delivered again
-- just after (with one) would be stored twice. NapCat does not replay history on reconnect, so
-- this needs a redelivery within the seconds of the restart; it is accepted rather than guarded.

ALTER TABLE chat_line ADD COLUMN platform_seq bigint CHECK (platform_seq >= 0);

-- Media references point at their line by the line's own key, not by a message id.
ALTER TABLE media_ref ADD COLUMN line_seq bigint;
UPDATE media_ref r SET line_seq = l.seq
FROM chat_line l
WHERE l.group_id = r.group_id AND l.message_id = r.message_id;
ALTER TABLE media_ref ALTER COLUMN line_seq SET NOT NULL;
ALTER TABLE media_ref DROP CONSTRAINT media_ref_group_id_message_id_fkey;
ALTER TABLE media_ref DROP CONSTRAINT media_ref_pkey;
ALTER TABLE media_ref DROP COLUMN message_id;
ALTER TABLE media_ref ADD PRIMARY KEY (line_seq, kind, idx);
ALTER TABLE media_ref ADD FOREIGN KEY (line_seq) REFERENCES chat_line (seq);

ALTER TABLE chat_line DROP CONSTRAINT chat_line_group_id_message_id_key;
CREATE UNIQUE INDEX chat_line_platform_seq ON chat_line (group_id, platform_seq)
    WHERE platform_seq IS NOT NULL;
CREATE UNIQUE INDEX chat_line_unsequenced_message ON chat_line (group_id, message_id)
    WHERE platform_seq IS NULL;
-- Lookups by message id (a quote, a picture the model opens) take the newest such line.
CREATE INDEX chat_line_message ON chat_line (group_id, message_id, seq);
