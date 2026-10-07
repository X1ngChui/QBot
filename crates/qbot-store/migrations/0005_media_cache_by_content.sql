-- Picture descriptions are keyed by `FINGERPRINT:SHA256` only: the describer (provider, model and
-- instructions) and the picture's bytes. Rows under the earlier platform-id (`p:`) and bare-hash
-- (`h:`) keys can no longer be looked up, so they go.
DELETE FROM media_cache WHERE key !~ '^[0-9a-f]{16}:[0-9a-f]{64}$';

COMMENT ON TABLE media_cache IS
    'Picture descriptions by FINGERPRINT:SHA256 (describer, bytes); a derived lookup that only saves model calls.';
