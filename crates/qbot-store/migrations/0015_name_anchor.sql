-- A learned name use says whether the chat itself anchored it to the member: the member answered
-- the line that used the name. A name others use for a member without any anchor (this one, the
-- member's own word, a person's /name, or the member's display name) is kept but not shown to
-- the model until a second conversation uses it for the same member (docs/memory.md section 4).
-- Uses recorded before this are unanchored.
ALTER TABLE name_use ADD COLUMN anchored boolean NOT NULL DEFAULT false;
