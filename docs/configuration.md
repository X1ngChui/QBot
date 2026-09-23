# Configuration

Behaviour is configured in `config/`, credentials in `.env`, and runtime state lives in
the database. This page is the reference for every file. The committed templates
(`.env.example`, `config/settings.yaml.example`, `config/personas/*.example`) carry the
same information as comments next to each key; the real files are ignored by git.

## `.env`

Read by Docker Compose. A missing credential fails `docker compose up`.

| Variable | Purpose |
| --- | --- |
| `PG_PASSWORD` | PostgreSQL password, used only between the containers |
| `TEXT_API_KEY` | The text and vision backends (one account serves both by default) |
| `MEDIA_API_KEY` | The embedding provider |
| `SEARCH_API_KEY` | The search backend |
| `NAPCAT_ACCOUNT` | The QQ number NapCat logs in as. Leave empty for the first login, then set it so restarts reuse the saved session. |
| `REGISTRY` | Docker registry mirror, default `docker.io` |
| `BUILD_PROXY`, `BUILD_NO_PROXY` | HTTP proxy for image builds only. Never used at runtime. |

Credential variables are named after the capability they serve, not the vendor. Which
vendor serves a capability is decided in `settings.yaml`, where each capability names
its variable with `credential_env`. To split text and vision across two accounts, add a
variable here and in `docker-compose.yml`, then name it in the vision block.

The bot container also reads `DATABASE_URL`, `DATABASE_PASSWORD`, `CONFIG_DIR`,
`NAPCAT_DATA_DIR`, `BACKUP_DIR`, `LOG_DIR`, `LOG_LEVEL`, `HOST` and `PORT`. Compose sets
them; you only touch them when running outside Docker.

## `config/settings.yaml`

Validated with pydantic. Unknown keys are rejected, so a typo fails the load instead of
silently doing nothing. Every setting is global. Per-group files contain only persona identity
and standing context.

### Applying changes

The application loads and validates one immutable configuration bundle at startup. The
bundle includes settings, personas, prompts, predicates and the agreement text. A missing
file, unknown key, duplicate template key or invalid value prevents startup; no partially
validated configuration becomes active.

Any configuration change requires a process restart. Provider clients, concurrency gates,
local ASR resources, the memory worker, scheduler jobs and prompt catalogs all retain the
same validated startup snapshot for their lifetime. Code changes additionally require an
image rebuild.

Retired configuration names are not dual-read. Validation names the unsupported key so it
can be replaced directly: `llm` → `capabilities`, `backend` → `provider`, `base_url` →
`endpoint`, and `api_key_env` → `credential_env`.

### Top level

| Key | Meaning |
| --- | --- |
| `owners` | QQ numbers, as strings, of the people who hold the operator console and receive the daily report. |
| `timezone` | IANA name. Governs the clock line in the prompt, the cron jobs, the daily report and the day boundary the budget resets on. |
| `personas_dir` | Directory of persona files, relative to `config/`. |
| `prompts_dir` | Directory containing the versioned `prompts.yaml` template bundle. |
| `predicates_file` | The predicate table. |

### `trigger`

| Key | Meaning |
| --- | --- |
| `nicknames` | Every name the bot answers to. Matched as whole words. A nickname that is also an ordinary word will trigger whenever the group talks about that word; handle it in the persona, or keep only unambiguous names. |

### `gateway`

| Key | Default | Meaning |
| --- | --- | --- |
| `shutdown_wait_sec` | 5 | Seconds shutdown waits for in-flight media patches |

### `media`

| Key | Default | Meaning |
| --- | --- | --- |
| `wait_sec` | 25 | Seconds a reply waits for shared media tickets before continuing; slow work keeps running and patches later |
| `protocol_timeout_sec` | 10 | Seconds allowed for one NapCat media API call |
| `http_timeout_sec` | 20 | Seconds allowed for one media download |
| `unreadable_retry_sec` | 600 | Seconds before retrying media marked unreadable |

### `members`

| Key | Default | Meaning |
| --- | --- | --- |
| `cache_ttl_sec` | 1800 | Seconds a fetched group member list remains fresh |

### `commands`

| Key | Default | Meaning |
| --- | --- | --- |
| `roster_max_entries` | 60 | Account rows shown by one owner `/members` response |
| `top_default_entries`, `top_max_entries` | 5, 20 | Default and maximum spending rows shown by `/top` |

### `identity_link`

| Key | Default | Meaning |
| --- | --- | --- |
| `challenge_ttl_sec` | 600 | Lifetime of one `/link` confirmation request |
| `max_pending_challenges` | 1000 | Global ceiling on durable pending requests |
| `max_pending_per_account` | 3 | Pending requests one initiating account may hold |
| `challenge_code_length` | 8 | Decimal digits in a newly issued confirmation code |

### `diagnostics`

| Key | Default | Meaning |
| --- | --- | --- |
| `debug_max_rounds` | 50 | Largest accepted `/debug start N` capture |
| `log_tail_default_lines`, `log_tail_max_lines` | 15, 60 | Default and maximum `/log` line count |
| `log_tail_scan_bytes` | 65536 | Maximum suffix of the log file scanned for one response |
| `error_ring_entries` | 200 | Process-local recent-error ring capacity |
| `error_message_chars` | 300 | Stored characters per recent error |
| `daily_report_recent_errors` | 8 | Recent errors included in the owner report |

### `tools`

| Key | Default | Meaning |
| --- | --- | --- |
| `max_calls_per_round` | 8 | Tool calls accepted from one model round |
| `max_rounds` | 20 | Safety ceiling on model rounds in one tool loop |
| `send_messages.max_messages_per_call` | 4 | Independent QQ messages allowed in one terminal call |
| `send_messages.max_text_chars_per_message` | 2000 | Text characters allowed in each QQ message |
| `web_search.count` | 5 | Results requested from one search |
| `web_search.depth` | basic | Vendor search depth; `advanced` consumes two credits |
| `search_history.context_lines` | 5 | Archive lines included before and after each hit |
| `search_history.max_hits` | 8 | Matching archive messages returned by one search |
| `search_history.max_query_terms` | 8 | Terms accepted in one search expression |
| `search_history.max_result_chars` | 12000 | Characters returned by one search |
| `recall_events.context_episodes` | 2 | Episodes included before and after each hit |
| `recall_events.max_hits` | 5 | Similarity hits selected before temporal context is added |
| `read_url.max_content_chars` | 8000 | Page-text characters returned by one read |
| `open_images.max_images` | 6 | Images accepted by one call |

### `capabilities`

Five capabilities are configured independently. `provider` is a closed schema value, not
an arbitrary registry string. The provider adapter hides platform-specific request and
response behavior; core code sees only capability contracts.

| Key | Meaning |
| --- | --- |
| `http_retries` | Retries for the plain-HTTP backends (embedding, search, file upload) |
| `retry_after_cap_sec` | The longest a vendor's `Retry-After` header may hold a call |

**`capabilities.text`** — replies and memory extraction.

| Key | Meaning |
| --- | --- |
| `provider`, `endpoint`, `credential_env`, `model` | The account and reply model. `deepseek`, `openai_responses` and `local` all use a Responses endpoint; there is no Chat Completions fallback. |
| `reasoning_effort` | `off`, `low`, `high` or `max`, translated into the backend's own Responses parameter. Thinking bills at output price. |
| `max_concurrency` | Concurrent model calls across every group (restart) |
| `timeout_sec`, `retries` | Per call |
| `extract.model`, `extract.reasoning_effort`, `extract.timeout_sec` | Extraction's process-owned model, grade and deadline on the same account; empty model means the reply model. Restart to apply. |

**`capabilities.vision`** — picture descriptions for the archive. The backend must expose the
Responses image-input shape as well as text responses.

| Key | Meaning |
| --- | --- |
| `provider`, `endpoint`, `credential_env`, `model`, `reasoning_effort`, `timeout_sec` | As above |
| `description_ttl_days` | How long a stored description stays current; past it, a reposted picture is described again. 0 disables expiry. |
| `file_max_age_days` | How long an uploaded original is trusted to still exist at the backend; past it `open_images` uploads again. Keep it under the backend's retention. |
| `max_images_per_min` | Pace gate for description calls |
| `max_concurrency` | Concurrent paid description calls across all groups (restart) |
| `max_output_tokens` | Shared reasoning and visible-output ceiling for one description |
| `max_image_mb` | Largest picture handled |

**`capabilities.asr`** — fixed in-process SenseVoice CPU transcription. It has no
provider selector, endpoint, credential, remote model, request timeout or billing mode.

| Key | Meaning |
| --- | --- |
| `model_dir` | Model bundle fetched by `scripts/fetch_asr_model.sh` (restart) |
| `threads` | Native recognizer CPU threads (restart) |
| `queue_capacity` | Global bounded FIFO admission queue (restart) |
| `max_audio_sec` | Longest clip accepted |
| `max_clips_per_min` | Per-group admission guard in front of the global queue |

**`capabilities.embedding`** — vectors for episode recall.

| Key | Meaning |
| --- | --- |
| `model`, `dimensions` | `dimensions` must match the `VECTOR(n)` column in `sql/init.sql`. Vectors are stored under the model that produced them and searched only under the current one, so changing the model hides existing vectors until the nightly pass rebuilds them. |

**`capabilities.search`** — web search and page reading.

| Key | Meaning |
| --- | --- |
| `monthly_quota` | Credits per calendar month over every group. |
| `proxy` | HTTP proxy for the search client only; empty means direct |

### `budget`

| Key | Meaning |
| --- | --- |
| `daily_cny_cap` | Daily spend over every group. At the cap the bot stops answering until the day rolls over. |
| `per_reply_cny` | What one reply may spend, tool loop included. Reaching it ends the tool loop with one final round answered from what was already fetched. |

### `prompt`

Counts, not tokens.

| Key | Meaning |
| --- | --- |
| `window_chunks`, `evict_chunk` | The history window holds `window_chunks × evict_chunk` messages and evicts a whole chunk at a time |
| `forward_lines`, `forward_depth`, `forward_chars` | How much of a forwarded chat record is rendered |
| `evidence_result_chars`, `evidence_total_chars`, `evidence_ttl_days` | Per-item bound, total bound and retention for structured evidence supporting nearby follow-ups |
| `evidence_request_chars` | Bound on the sanitized request summary stored in each evidence item |

### `memory`

Restart to apply.

| Key | Meaning |
| --- | --- |
| `extract_window` | Messages per extraction chunk |
| `batch_gap_min` | A full chunk is trimmed back to the last conversation gap of at least this many minutes |
| `drain_floor` | Fewer unconsumed events than this are left for the next night |
| `max_passes` | Chunks one nightly drain may process |
| `known_episodes` | Recorded episodes the extractor is reminded of |
| `roster_aliases_per_account` | Confirmed aliases shown beside one exact account in extraction context |
| `episode_ttl_days` | Age after which an episode leaves semantic recall and loses its rebuildable vectors; provenance remains |
| `alias_unused_days`, `joke_unused_days` | How long an unconfirmed name, or one marked as a joke, survives unused |
| `job_lease_min` | How long a claimed job stays claimed |
| `worker_idle_sec` | Idle polling interval for the background worker |
| `embedding_page_size` | Episode rows embedded in one page |
| `worker_retry_backoff_sec` | Retry delays for failed memory jobs |

### `schedule`

| Key | Meaning |
| --- | --- |
| `nightly_cron` | The nightly pipeline: extraction, decay, backup, media cleanup (restart) |
| `report_cron` | The daily report (restart) |
| `backup_keep` | Dumps kept |
| `napcat_cache_days` | Age past which NapCat's media cache is deleted |
| `extract_drain_hours`, `decay_drain_min`, `drain_poll_sec` | How long the pipeline waits for each stage's own jobs before moving on; embedding drains independently |
| `misfire_grace_sec` | How late a missed trigger may still fire (restart) |
| `backup_stale_hours` | Age past which the report flags the newest dump |
| `completed_job_keep_days` | Age after which completed memory-job audit rows are pruned |

### `database`

Restart to apply.

| Key | Meaning |
| --- | --- |
| `pool_min`, `pool_max` | Connection pool size |
| `command_timeout_sec` | Ceiling on any single statement |

### `agreement`

| Key | Meaning |
| --- | --- |
| `version` | Acceptances are stored against it; bump it to re-ask everyone |
| `file` | The agreement text shown by `/terms`, relative to `config/`. Must exist and be non-empty. |
| `prompt_every_sec` | How often one unconsenting member is re-shown the pointer |

## Personas

`config/personas/default.yaml` is the default persona. `config/personas/group_<id>.yaml`
applies to one group and states only what differs; everything else is inherited.

| Field | Meaning |
| --- | --- |
| `name` | The bot's name in the transcript |
| `system_prompt` | The persona text. Replaces the default entirely. |
| `system_prompt_extra` | Paragraphs appended to the inherited `system_prompt` |
| `group_knowledge` | Standing facts about the group: what it is for, its jargon, its running jokes. Shown to the model every turn and to the extractor as established fact. Leave empty rather than writing notes to yourself. |

The prompt bodies are Chinese because that is what the model reads. Tone, length and
formatting are constrained here; the code only strips Markdown from the output.

## Predicates

`config/predicates.yaml` defines what may be recorded about a person. Each entry is the
whole definition: the extraction model is offered exactly these names, the validator
accepts exactly these names, and the roster is rendered with these verbs.

| Field | Meaning |
| --- | --- |
| `verb` | How the fact reads in Chinese; optional `{{object}}` marks where the object goes if not verb-first |
| `cardinality` | `single` (a new value closes the old one) or `multi` |
| `decay` | `stable`, `default` or `fast`, mapped to `half_life_days` |
| `kind` | `attribute`, `preference` or `relation` |
| `opposite` | Recording this retracts the named predicate about the same object |
| `rule` | The line shown to the extraction model: one sentence of meaning and the boundary against neighbouring predicates |

The file's header comment states what a predicate has to be to earn a place (checkable
from a verbatim sentence, still true next week, orthogonal, worth reading) and what is
deliberately absent (in-jokes, inferred attributes, personality labels, sensitive
categories).

## Prompts

All runtime prompt wording lives in the single versioned
`config/prompts/prompts.yaml` bundle. The closed manifest in
`qqbot/prompting/templates.py` defines each logical template's role and exact slots;
extra or malformed. See [config/prompts/README.md](../config/prompts/README.md) for the
contract and safe generation workflow.

## Agreement

`config/agreement.txt` is shown verbatim by `/terms`. Acceptance is recorded per group,
account and `agreement.version`.

## NapCat

`napcat/onebot11.json.template` is the OneBot configuration NapCat needs: one reverse
WebSocket client pointing at `ws://bot:8080/onebot/v11/ws` with `messagePostFormat`
set to `array`. See [operations.md](operations.md) for where it goes.
