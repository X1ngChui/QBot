-- A run the bot started on its own while watching the conversation: no member and no message.
ALTER TABLE run DROP CONSTRAINT run_trigger_kind_check;
ALTER TABLE run ADD CONSTRAINT run_trigger_kind_check
    CHECK (trigger_kind IN ('addressed', 'wake', 'spontaneous'));
