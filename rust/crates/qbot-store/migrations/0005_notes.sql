-- Notes people write by hand about a member. Not memory: nothing reads or derives from them
-- except their display (see qbot-memory notes). Removing a note deletes it.

CREATE TABLE note (
    note_id        bigserial PRIMARY KEY,
    group_id       bigint NOT NULL,
    account_id     bigint NOT NULL REFERENCES account (account_id),
    text           text NOT NULL CHECK (btrim(text) <> ''),
    -- Who wrote it, or last edited it.
    author_account bigint NOT NULL,
    created_ms     bigint NOT NULL,
    updated_ms     bigint NOT NULL CHECK (updated_ms >= created_ms)
);
CREATE INDEX note_by_account ON note (group_id, account_id, note_id);
