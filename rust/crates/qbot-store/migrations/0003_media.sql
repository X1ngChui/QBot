-- What pictures were described as, so the same picture is never described (and paid for) twice.
-- Keys are either a platform file id ("p:..."), which is known before anything is downloaded,
-- or the SHA-256 of the picture's bytes ("h:..."), which recognises the same picture arriving
-- under a different id. Voice clips are not cached.
CREATE TABLE media_cache (
    key         text PRIMARY KEY,
    kind        text NOT NULL CHECK (kind IN ('image')),
    description text NOT NULL CHECK (description <> ''),
    created_ms  bigint NOT NULL
);

-- Where each picture, sticker, clip or forwarded record of a message can be fetched again, by its
-- position among the markers of its kind in the line. Written with the line. Bytes are never
-- stored; these are the platform's references (a file id, a link that may expire).
CREATE TABLE media_ref (
    group_id   bigint NOT NULL,
    message_id bigint NOT NULL,
    kind       text NOT NULL CHECK (kind IN ('image', 'sticker', 'voice', 'forward')),
    idx        integer NOT NULL CHECK (idx >= 0),
    key        text,
    file       text,
    url        text,
    size_bytes bigint CHECK (size_bytes IS NULL OR size_bytes >= 0),
    PRIMARY KEY (group_id, message_id, kind, idx),
    FOREIGN KEY (group_id, message_id) REFERENCES chat_line (group_id, message_id)
);
