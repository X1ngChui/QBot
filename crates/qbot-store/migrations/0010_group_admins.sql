-- Delegated group admins: members a bot owner lets use /mute and /block in one group. Only a bot
-- owner adds or removes one, and the grant belongs to that account alone: a linked account, in
-- this group or another, gets nothing from it. A delegated admin blocks only ordinary members and
-- never overrides a bot owner's block.
CREATE TABLE group_admin (
    group_id   bigint NOT NULL,
    account_id bigint NOT NULL REFERENCES account (account_id),
    granted_by bigint NOT NULL,
    granted_ms bigint NOT NULL,
    PRIMARY KEY (group_id, account_id)
);

-- Who set a block: a bot owner's block (every block before this change) a delegated admin can
-- neither change nor lift; a bot owner overrides any block.
ALTER TABLE group_block ADD COLUMN by_owner boolean NOT NULL DEFAULT true;
