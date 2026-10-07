-- A slice extraction gave up on for good: the provider refuses its content, or no valid answer
-- came back in any attempt. It counts as covered, so the group's later slices are still
-- extracted; its lines stay verbatim wherever history shows them and remain searchable. Slices
-- of a group are disjoint, whether they became an episode or were skipped.
CREATE TABLE slice_skip (
    group_id      bigint NOT NULL,
    first_ordinal bigint NOT NULL CHECK (first_ordinal >= 1),
    last_ordinal  bigint NOT NULL,
    reason        text NOT NULL,
    created_ms    bigint NOT NULL,
    CHECK (first_ordinal <= last_ordinal),
    EXCLUDE USING gist (group_id WITH =, int8range(first_ordinal, last_ordinal, '[]') WITH &&)
);
