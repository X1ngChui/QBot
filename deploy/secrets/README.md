# Secrets

One file per secret, named in lower case, containing only the value (surrounding whitespace is
trimmed). Docker Compose mounts them at `/run/secrets/<name>`.

| File | Secret name | Used for |
| --- | --- | --- |
| `database_password` | `DATABASE_PASSWORD` | PostgreSQL password (also read by the `postgres` service) |
| `text_api_key` | `TEXT_API_KEY` | Replies and memory extraction |
| `embedding_api_key` | `EMBEDDING_API_KEY` | Episode embeddings |
| `vision_api_key` | `VISION_API_KEY` | Only when `providers.vision.enabled = true`: the key of the picture-describing model |
| `search_api_key` | `SEARCH_API_KEY` | Only when `providers.search.enabled = true`: the web search (Tavily) key |
| `onebot_access_token` | `ONEBOT_ACCESS_TOKEN` | The bearer token NapCat presents on its reverse WebSocket; set `gateway.access_token_secret = ""` to run without authentication (private networks only) |

The secret names are set in the configuration (`database.password_secret`,
`providers.text.api_key_secret`, `providers.embedding.api_key_secret`, `gateway.access_token_secret`) and can be changed there.

Resolution order for a secret `NAME`: the file named by `NAME_FILE`, then the variable `NAME`,
then `/run/secrets/<name>`. A missing or empty secret stops startup with an error that says how
to fix it; there is no fallback. Real secret files are git-ignored.
