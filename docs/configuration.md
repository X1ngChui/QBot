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

The active schema accepts exactly nine top-level sections. Unknown keys, invalid IANA
zones, invalid cron expressions and incoherent database pool ranges fail startup.
Settings, personas, prompts and predicates form one immutable startup bundle. Group
persona files cannot override runtime behavior.

| Section | Operator decisions |
| --- | --- |
| `bot` | Owners, timezone and nickname triggers |
| `conversation` | History message count, reply deadline, send count, text length and evidence retention |
| `backends` | Model accounts, providers, request resources and search quality |
| `media` | Image/audio admission, per-group understanding pace and description freshness |
| `memory` | Episode and unconfirmed-name retention |
| `budget` | Daily and per-reply accounted-spend stop-loss |
| `tasks` | Wakeup horizon, pending ceilings, chain depth and daily group allowance |
| `maintenance` | Cron schedules and backup/cache/job/task retention |
| `runtime` | Reply capacity, database pool and nondefault bundle paths |

Start with [the example](../config/settings.yaml.example). The exhaustive
[field reference](configuration-reference.md) is generated from the active schema;
it includes defaults and validation constraints. Regenerate it with
`python scripts/config_reference.py --write`, or run that command without `--write`
to check for drift. It never loads private settings.

### Applying changes

Restart after changing any part of the bundle. Clients, worker resources and scheduled
jobs retain the startup configuration. Changes to application code additionally require
an image rebuild; restarting an old image does not load edited Python source.

The backend names `deepseek`, `openai_responses` and `local` select Responses adapters;
there is no Chat Completions fallback. Extraction shares the text account and may override
only its model, reasoning grade and request deadline. ASR is a fixed local SenseVoice
service with a model directory and native thread count, not a remote-provider selector.
Embedding width is an adapter/storage contract, not a YAML setting. Changing the embedding
model hides old model-tagged vectors until they are rebuilt.

### Preferences versus mechanism limits

`conversation.history_messages` is the history preference. Chunk size and deque headroom
are derived from it; it is not a token or byte budget. The single reply deadline includes
queueing, media, model calls, tools, sends and echo observation. ACKs remain recorded even
if later work times out. An uncertain send is not automatically replayed.

`runtime.reply_capacity` currently bounds active plus queued addressed replies. Full or
expired work ends silently. Active slots are also limited by text-backend concurrency.
Persistent wakeups retain their own worker admission while the common-inbox refactor is
in progress; this setting must not be treated as a combined timer/reply capacity yet.

The program owns finite session turns, total tool calls, result retention, parsing limits,
ASR queue size, member-cache capacity, echo waits, job claims and maintenance phase waits.
These are not an alternative advanced configuration tree. A free model still has finite
session fuel. Attachment expiry comes from the selected provider's contract.

Budget limits are stop-loss thresholds on already recorded spend, not prepaid reservations:
concurrent calls can overshoot. Every new paid attempt checks the ledger; unreadable totals
fail closed. An ambiguous accounting write preserves the completed result but blocks later
paid requests for that Runtime. Restarting does not repair a missing charge. Local ASR is
not stopped by the paid budget.

Maintenance cron uses APScheduler's syntax in `bot.timezone`. Prefer weekday names such as
`mon` rather than numeric weekdays. Backup freshness follows the next scheduled run plus
bounded maintenance completion time, not a fixed number of hours. Backup verification
checks the dump catalog and minimum size; it is not a full restore rehearsal.

### One-time migration

Runtime loading accepts only the new schema, without old-key aliases or fallback reads.
The offline converter supports the frozen pre-refactor contract documented in the
[130-field migration table](configuration-migration.md). Older, unknown formats require
an explicit separate conversion; they are not guessed.

Run locally with development dependencies installed:

```sh
python scripts/migrate_config.py path/to/settings.yaml
python scripts/migrate_config.py path/to/settings.yaml --apply
```

The first command only validates and reports field paths and dispositions, never values.
`--apply` validates again, keeps a recoverable byte-for-byte backup under
`backups/config-migrations/`, detects concurrent edits, and atomically replaces the YAML.
Quotes, comments, BOM and line endings are retained. Comments are preserved verbatim, not
rewritten into new semantic explanations: review notes attached to moved or retired fields.

Explicit nondefault values of retired settings stop conversion. Accept a retirement only
when its replacement mechanism is understood, with `--retire old.field` for that field.
The converter preserves nondefault retained choices and sparse input instead of expanding
all defaults. The old two history factors are multiplied into one message count; the
new sliding cadence is derived and need not match the old cadence.

Configuration conversion does not migrate PostgreSQL. The ownership/attempt-count SQL
migration is separate. Before production changes, take and verify a fresh backup and
follow the deployment procedure; a test against an empty schema is not a migration rehearsal.

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

## NapCat

`napcat/onebot11.json.template` is the OneBot configuration NapCat needs: one reverse
WebSocket client pointing at `ws://bot:8080/onebot/v11/ws` with `messagePostFormat`
set to `array`. See [operations.md](operations.md) for where it goes.
