# QBot

QBot is an AI member for QQ group chats. It answers when it is @-mentioned, called by
name, or quoted; it keeps a structured, per-group memory of the people and events it
reads about; it understands pictures and voice messages; and it spends money only
within limits you set.

[中文说明](README.zh-CN.md)

## Features

- **Speaks only when spoken to.** A reply happens on an @, a nickname used as a whole
  word, or a quote of one of the bot's own messages. Everything else is read and
  archived in silence.
- **Structured memory.** Facts about members and about the group are extracted nightly
  by a language model, validated by code against verbatim quotes, and stored with
  evidence, confidence and an expiry. Wrong entries can be deleted by number.
- **Identity, not nicknames.** Accounts, display names and people are kept apart.
  Two accounts can be merged into one person; every person in a prompt wears a member
  number, so members sharing a name are never confused.
- **QQ-native replies.** The model sends each reply through a tool call and decides
  whom to @ and which message to reply to, or sends a plain message.
- **Pictures and voice.** Every picture is described in one line and archived as text;
  the reply model can fetch the originals it wants to look at. Voice clips are
  transcribed on arrival, on the CPU, at no cost.
- **Tools.** The reply model can search the web, search the group's own archive with a
  boolean query, recall past episodes by meaning, read a web page, and open pictures.
- **Money is the only limit.** A daily spending cap, a per-reply cap and a monthly
  search allowance. There are no token budgets and no call quotas.
- **Per-group everything.** Persona, group knowledge, memory, block list, mute switch
  and user-agreement consent are all scoped to the group.
- **Consent gate.** Members are answered only after they accept a user agreement, whose
  text and version you control.
- **An operator console in chat.** Inspect and correct memory, block or mute, read
  usage, reload configuration, and capture model calls for debugging.

## How it works

```text
QQ  <->  NapCat (OneBot v11)  <-- reverse WebSocket -->  bot  <-- asyncpg -->  PostgreSQL + pgvector
```

Three containers. NapCat is the QQ protocol client and connects to the bot over a
reverse WebSocket. The bot is a NoneBot2 application. PostgreSQL holds the archive,
the memory model, the job queue and the cost ledger.

The bot deals in five capabilities: text, vision, speech recognition, embedding and
web search. Which provider serves each capability, with which model and credential,
is declared in `config/settings.yaml`. The defaults use DeepSeek for text and vision,
sherpa-onnx with SenseVoice in-process for speech recognition, Alibaba DashScope for
embeddings, and Tavily for search. Text and vision backends speak the Responses API;
tool calls, results and reasoning continuation stay local to each reply task rather than
using a provider-side conversation. Adding a provider means writing one subclass and
registering it.

Read [docs/architecture.md](docs/architecture.md) for the full picture.

## Requirements

- Docker and Docker Compose
- A QQ account for the bot (a dedicated account is strongly recommended)
- API keys for the providers named in your configuration
- Python 3.12 if you want to run the test suite or the evaluation scripts locally

Third-party QQ protocol clients violate Tencent's terms of service and the account may
be banned. Use an account you can afford to lose.

## Quick start

```bash
git clone <this repository> qbot
cd qbot

cp .env.example .env
cp config/settings.yaml.example config/settings.yaml
cp config/personas/default.yaml.example config/personas/default.yaml
```

Edit the three files:

- `.env`: API keys, the PostgreSQL password, and the QQ number NapCat logs in as.
- `config/settings.yaml`: the owner accounts, the bot's nicknames, the providers.
- `config/personas/default.yaml`: the bot's name and personality.

Then fetch the speech-recognition model, start the containers, log NapCat in, and
verify the providers before going live:

```bash
bash scripts/fetch_asr_model.sh          # once; ~250 MB into models/

docker compose up -d postgres napcat
docker compose logs -f napcat             # scan the QR code, or open http://127.0.0.1:6099

# After the first login, point NapCat at the bot (see docs/operations.md):
#   merge napcat/onebot11.json.template into data/napcat/config/onebot11_<QQ>.json
docker compose restart napcat

docker compose build bot
docker compose run --rm bot python scripts/preflight.py   # one real call per provider
docker compose up -d bot
```

A group is served from its first message. There is no allow list; new groups appear in
the daily report.

Both real configuration files are ignored by git because they name real accounts.
Only the `.example` templates are committed.

## Configuration

Behaviour lives in `config/`, credentials in `.env`, runtime state in the database.

| File | Purpose |
| --- | --- |
| `.env` | Credentials and infrastructure settings read by Docker Compose |
| `config/settings.yaml` | Global settings: owners, trigger, providers, budget, prompt window, memory, schedule |
| `config/personas/default.yaml` | The default persona: name, system prompt, group knowledge |
| `config/personas/group_<id>.yaml` | Per-group persona: identity, prompt additions and standing context |
| `config/predicates.yaml` | What may be recorded about a person |
| `config/prompts/prompts.yaml` | Versioned bundle containing every runtime prompt template |
| `config/agreement.txt` | The user agreement shown by `/terms` |

`/reload` applies reloadable edits atomically. If a process-owned setting changed, it
rejects the whole candidate and reports which paths require a restart. The reference is
in [docs/configuration.md](docs/configuration.md).

## Commands

Commands are typed in the group, prefixed with `/`. Owners hold the whole console.
Members can read and correct their own record, view read-only statistics, and accept
the agreement. Anything a member may not run is ignored without a reply.

| Command | Purpose |
| --- | --- |
| `/help` | List the commands you may run |
| `/agree`, `/terms` | Accept or read the user agreement |
| `/who`, `/note`, `/alias`, `/forget` | Inspect and correct what is known about a member |
| `/card` | What is known about the group itself |
| `/merge`, `/split` | Declare that two accounts are one person, or undo it |
| `/block`, `/unblock`, `/mute`, `/unmute` | Stop answering a member, or the whole group |
| `/stats`, `/groupstats`, `/top` | Spending and usage |
| `/reload`, `/debug`, `/log` | Maintenance |

See [docs/commands.md](docs/commands.md) for usage and permissions.

## Operations

Deployment, backups, restore, rollback, schema changes, scheduled jobs, the daily
report, debugging and the behavioural evaluation scripts are described in
[docs/operations.md](docs/operations.md).

## Development

The test suite runs against a throwaway PostgreSQL and needs no QQ connection:

```bash
python -m venv .venv
.venv/bin/pip install -r requirements.txt -r requirements-dev.txt

docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qqbot -e POSTGRES_USER=qqbot -e POSTGRES_PASSWORD=testpw \
  -p 15432:5432 \
  -v "$PWD/sql/init.sql:/docker-entrypoint-initdb.d/init.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

.venv/bin/python tests/run_all.py        # every suite, then ruff
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for conventions and [tests/README.md](tests/README.md)
for what each suite covers.

## Project layout

| Path | Contents |
| --- | --- |
| `bot.py` | Entry point; serves the OneBot reverse WebSocket |
| `qqbot/plugin.py` | NoneBot plugin wiring and startup order |
| `qqbot/settings.py` | Configuration models, persona merge, `/reload` |
| `qqbot/gateway/` | Inbound message handling: segments, dedup, archiving |
| `qqbot/core/` | Trigger, pipeline, prompt assembly, reply engine, tools, media, budget, commands catalogue |
| `qqbot/domain/` | The memory model: identities, aliases, facts, episodes, evidence |
| `qqbot/repositories/` | Database access for the memory model |
| `qqbot/services/` | Extraction, validation, consolidation, the member directory |
| `qqbot/workers/` | The background memory worker |
| `qqbot/providers/` | Provider abstractions and one module per backend |
| `qqbot/plugins/` | The command handlers and the scheduled jobs |
| `qqbot/db/` | Connection pool, schema check, archive and ledger access |
| `config/` | Configuration templates, prompts, predicates, agreement |
| `sql/` | Database schema and the schema changelog |
| `scripts/` | Deployment, preflight, model download, evaluations |
| `tests/` | The test suites |
| `docs/` | Architecture, configuration, commands, operations |

## License

[MIT](LICENSE)
