-- A group's topics stand side by side: one slot per topic (qbot_memory::facts::topic_slot), so a
-- group talking about something new has not stopped being about what it was about; topics now
-- compete only for room. A topic row's key already is its normalized value, and keys were unique
-- among a group's active topics, so the new slots are too.
UPDATE fact SET slot = 'group_topic:' || key
WHERE subject_account IS NULL AND predicate = 'group_topic';
