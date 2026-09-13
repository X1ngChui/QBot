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
| `MEDIA_API_KEY` | The embedding backend, and API-based speech backends if configured |
| `SEARCH_API_KEY` | The search backend |
| `NAPCAT_ACCOUNT` | The QQ number NapCat logs in as. Leave empty for the first login, then set it so restarts reuse the saved session. |
| `REGISTRY` | Docker registry mirror, default `docker.io` |
| `BUILD_PROXY`, `BUILD_NO_PROXY` | HTTP proxy for image builds only. Never used at runtime. |

Credential variables are named after the capability they serve, not the vendor. Which
vendor serves a capability is decided in `settings.yaml`, where each capability names
its variable with `api_key_env`. To split text and vision across two accounts, add a
variable here and in `docker-compose.yml`, then name it in the vision block.

The bot container also reads `DATABASE_URL`, `DATABASE_PASSWORD`, `CONFIG_DIR`,
`NAPCAT_DATA_DIR`, `BACKUP_DIR`, `LOG_DIR`, `LOG_LEVEL`, `HOST` and `PORT`. Compose sets
them; you only touch them when running outside Docker.

## `config/settings.yaml`

Validated with pydantic. Unknown keys are rejected, so a typo fails the load instead of
silently doing nothing. Every value here can be overridden per group in a persona file
except where noted.

### Applying changes

`/reload` re-reads the settings, the personas, the prompts, the predicates and the
agreement. If the new configuration is invalid the old one stays in force. The
following need a restart:

- `schedule.nightly_cron`, `schedule.report_cron`, `schedule.misfire_grace_sec`: the
  jobs are registered with the scheduler at startup.
- The whole `database` block: the connection pool is built once.
- The whole `memory` block and `llm.text.extract`: the memory worker resolves them when
  it is constructed.
- `llm.text.max_concurrency`: the semaphore is built once.
- `timezone`, for the already-registered cron jobs only.

Changing code never takes effect through `/reload`; the image must be rebuilt.

### Top level

| Key | Meaning |
| --- | --- |
| `owners` | QQ numbers, as strings, of the people who hold the operator console and receive the daily report. A persona may override the list for its group, but the commands that affect every group at once (`/merge`, `/split`, `/reload`, `/debug`, `/log`) answer only to this default list. |
| `timezone` | IANA name. Governs the clock line in the prompt, the cron jobs, the daily report and the day boundary the budget resets on. |
| `personas_dir` | Directory of persona files, relative to `config/`. |
| `prompts_dir` | Directory of prompt files. |
| `predicates_file` | The predicate table. |

### `trigger`

| Key | Meaning |
| --- | --- |
| `nicknames` | Every name the bot answers to. Matched as whole words. A nickname that is also an ordinary word will trigger whenever the group talks about that word; handle it in the persona, or keep only unambiguous names. |

### `gateway`

The deadlines and the shutdown wait are read from the top-level file only, because
they size clients shared by every group.

| Key | Default | Meaning |
| --- | --- | --- |
| `dedup_ttl_sec` | 300 | How long a platform message id is remembered for deduplication |
| `max_msg_len` | 2000 | Longest reply sent to the group |
| `media_wait_sec` | 25 | How long a reply waits for a picture or clip to be understood before building the prompt without it |
| `member_cache_ttl_sec` | 1800 | How long a fetched member list is reused |
| `protocol_call_timeout_sec` | 10 | Deadline for NapCat media calls |
| `media_http_timeout_sec` | 20 | Deadline for downloading a picture or clip |
| `unreadable_retry_sec` | 600 | How long a picture no route could read is left alone before another attempt |
| `shutdown_wait_sec` | 5 | How long shutdown waits for in-flight archive writes |

### `llm`

Five capabilities, each with its own backend, endpoint, model and credential name.
`backend` selects the implementation class from `qqbot/providers/registry.py` and can
only be set in the top-level file. A group may override endpoint, model and grades.

| Key | Meaning |
| --- | --- |
| `http_retries` | Retries for the plain-HTTP backends (embedding, search, file upload) |
| `retry_after_cap_sec` | The longest a vendor's `Retry-After` header may hold a call |

**`llm.text`** — replies and memory extraction.

| Key | Meaning |
| --- | --- |
| `backend`, `base_url`, `api_key_env`, `model` | The account and the reply model |
| `reasoning_effort` | `off`, `low`, `high` or `max`, translated into the backend's own parameter. Thinking bills at output price. |
| `max_concurrency` | Concurrent model calls across every group (restart) |
| `timeout_sec`, `retries` | Per call |
| `extract.model`, `extract.reasoning_effort`, `extract.timeout_sec` | Extraction's own model, grade and deadline on the same account; empty model means the reply model. Restart to apply. |

**`llm.vision`** — picture descriptions for the archive.

| Key | Meaning |
| --- | --- |
| `backend`, `base_url`, `api_key_env`, `model`, `reasoning_effort`, `timeout_sec` | As above |
| `description_ttl_days` | How long a stored description stays current; past it, a reposted picture is described again. 0 disables expiry. |
| `file_max_age_days` | How long an uploaded original is trusted to still exist at the backend; past it `open_images` uploads again. Keep it under the backend's retention. |
| `max_images_per_min` | Pace gate for description calls |
| `max_image_mb` | Largest picture handled |

**`llm.asr`** — voice transcription. The default backend `sherpa` runs in-process and
has no endpoint or credential.

| Key | Meaning |
| --- | --- |
| `backend` | `sherpa` (in-process), `dashscope` or `openai_compat` |
| `model` | For `sherpa`, only a label in the ledger |
| `model_dir` | The model bundle fetched by `scripts/fetch_asr_model.sh` (sherpa only) |
| `threads` | CPU threads (sherpa only) |
| `max_audio_sec` | Longest clip transcribed |
| `max_clips_per_min` | Pace gate; a CPU guard for the free backend, a spending guard for a paid one |
| `timeout_sec` | API backends only |

**`llm.embedding`** — vectors for episode recall.

| Key | Meaning |
| --- | --- |
| `model`, `dimensions` | `dimensions` must match the `VECTOR(n)` column in `sql/init.sql`. Vectors are stored under the model that produced them and searched only under the current one, so changing the model hides existing vectors until the nightly pass rebuilds them. |

**`llm.search`** — web search and page reading.

| Key | Meaning |
| --- | --- |
| `count` | Results per search, at most 20 |
| `depth` | `basic` (one credit) or `advanced` (two) |
| `monthly_quota` | Credits per calendar month over every group. A persona override is ignored. |
| `proxy` | HTTP proxy for the search client only; empty means direct |

### `budget`

| Key | Meaning |
| --- | --- |
| `daily_cny_cap` | Daily spend over every group. At the cap the bot stops answering until the day rolls over. A persona override is ignored. |
| `per_reply_cny` | What one reply may spend, tool loop included. Reaching it ends the tool loop with one final round answered from what was already fetched. |

### `prompt`

Counts, not tokens.

| Key | Meaning |
| --- | --- |
| `window_chunks`, `evict_chunk` | The history window holds `window_chunks × evict_chunk` messages and evicts a whole chunk at a time |
| `forward_lines`, `forward_depth`, `forward_chars` | How much of a forwarded chat record is rendered |
| `trace_result_chars`, `trace_total_chars` | How much of the retrieval trace beside each bot reply survives |
| `provenance_items`, `provenance_query_chars` | The provenance marker on the bot's archived lines |

### `retrieval`

| Key | Meaning |
| --- | --- |
| `history_context` | Lines of surrounding conversation around each `search_history` hit, each way |
| `episode_context` | Neighbouring episodes around each `recall_events` hit, each way |
| `history_hits`, `max_query_terms` | Hits per search and terms per query expression |
| `history_chars` | Longest `search_history` answer; a cut answer says so on its last line |
| `url_content_chars` | Longest page text `read_url` returns |
| `max_tool_calls_per_round` | Tool calls one model round may carry |
| `open_images_max` | Pictures one `open_images` call may fetch |
| `max_rounds` | Tripwire on the tool loop, only reachable with a backend that bills zero |

### `memory`

Restart to apply.

| Key | Meaning |
| --- | --- |
| `extract_window` | Messages per extraction chunk; also how far `/relearn` rewinds |
| `batch_gap_min` | A full chunk is trimmed back to the last conversation gap of at least this many minutes |
| `drain_floor` | Fewer unread messages than this are left for the next night |
| `max_passes` | Chunks one nightly drain may process |
| `known_episodes` | Recorded episodes the extractor is reminded of |
| `alias_unused_days`, `joke_unused_days` | How long an unconfirmed name, or one marked as a joke, survives unused |
| `job_lease_min` | How long a claimed job stays claimed |

### `schedule`

| Key | Meaning |
| --- | --- |
| `nightly_cron` | The nightly pipeline: extraction, decay, backup, media cleanup (restart) |
| `report_cron` | The daily report (restart) |
| `backup_keep` | Dumps kept |
| `napcat_cache_days` | Age past which NapCat's media cache is deleted |
| `extract_drain_hours`, `decay_drain_min`, `drain_poll_sec` | How long the pipeline waits for each stage's jobs to finish before moving on |
| `misfire_grace_sec` | How late a missed trigger may still fire (restart) |
| `backup_stale_hours` | Age past which the report flags the newest dump |

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
| `overrides` | Any subtree of `settings.yaml`. Backends and the two global caps cannot be overridden. |

The prompt bodies are Chinese because that is what the model reads. Tone, length and
formatting are constrained here; the code only strips Markdown from the output.

## Predicates

`config/predicates.yaml` defines what may be recorded about a person. Each entry is the
whole definition: the extraction model is offered exactly these names, the validator
accepts exactly these names, and the roster is rendered with these verbs.

| Field | Meaning |
| --- | --- |
| `verb` | How the fact reads in Chinese; `{}` marks where the object goes if not verb-first |
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

Every instruction text the model reads is a file under `config/prompts/`, one per key
in the manifest in `qqbot/settings.py`. A missing or empty file fails the load. See
[config/prompts/README.md](../config/prompts/README.md) for what each file does and how
to change one safely.

## Agreement

`config/agreement.txt` is shown verbatim by `/terms`. Acceptance is recorded per group,
account and `agreement.version`.

## NapCat

`napcat/onebot11.json.template` is the OneBot configuration NapCat needs: one reverse
WebSocket client pointing at `ws://bot:8080/onebot/v11/ws` with `messagePostFormat`
set to `array`. See [operations.md](operations.md) for where it goes.
