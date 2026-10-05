-- A member number, once assigned, names that account in that group for good: prompts, scheduled
-- task intents and run transcripts refer to members by number. Rows are only ever inserted.
CREATE FUNCTION member_number_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'member numbers are never changed or removed (%)', TG_OP;
END
$$;

CREATE TRIGGER member_number_immutable
    BEFORE UPDATE OR DELETE OR TRUNCATE ON member_number
    FOR EACH STATEMENT EXECUTE FUNCTION member_number_immutable();
