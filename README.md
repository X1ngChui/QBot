# QBot

QBot is an AI member for QQ group chats. It answers when it is @-mentioned, called by
name, or quoted; it keeps a structured, per-group memory of the people and events it
reads about; it understands pictures and voice messages; and it spends money only
within limits you set.

[中文说明](README.zh-CN.md)

## Features

- **Speaks when addressed or when a requested timer fires.** An @, a whole-word
  nickname or a quote submits an independent reply. A member can also ask it to schedule
  a one-shot group task; at the due time it re-reads the current conversation before
  deciding whether to reply or schedule another bounded check. Other messages are
  archived silently.
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
  boolean query, recall past episodes by meaning, read a web page, open pictures,
  and create, list or cancel its initiator's group timers without a command.
- **Spending and timer bounds.** A daily spending cap, a per-reply cap and a monthly
  search allowance bound paid work. Durable timers have separate limits on frequency,
  pending count, future horizon and self-renewal. Addressed replies and timers share a
  bounded inbox and one end-to-end deadline per reply; queueing counts against it.
  Finite session steps and payload bounds also apply to free backends. Spending caps
  stop new paid requests based on accounted usage, not strict concurrent prepayment.
- **Per-group everything.** Persona, group knowledge, memory, block list and mute switch
  are all scoped to the group.
- **Immediate access.** Members can address the bot and use member commands without a
  separate registration or consent step.
- **An operator console in chat.** Inspect and correct memory, block or mute, read
  usage, and capture model calls for debugging.

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
| `config/settings.yaml` | Global settings in nine sections: bot, conversation, backends, media, memory, budget, tasks, maintenance, runtime |
| `config/personas/default.yaml` | The default persona: name, system prompt, group knowledge |
| `config/personas/group_<id>.yaml` | Per-group persona: identity, prompt additions and standing context |
| `config/predicates.yaml` | What may be recorded about a person |
| `config/prompts/prompts.yaml` | Versioned bundle containing every runtime prompt template |

The complete configuration bundle is validated at startup. Changes to settings,
personas, prompts or predicates take effect after a restart. The
reference is in [docs/configuration.md](docs/configuration.md), with a generated
[complete field table](docs/configuration-reference.md) and
[one-time migration map](docs/configuration-migration.md). Old configuration keys are
not accepted by the runtime.

## Commands

Commands use one meaning for every caller; authorization only permits or rejects the
action. Person commands target the exact account by default and use explicit `--all` for
the current linked account set.

| Command | Purpose |
| --- | --- |
| `/help` | Show the shared command catalogue and authorization labels |
| `/who`, `/note`, `/alias`, `/forget` | Inspect and correct exact-account or explicit linked-set records |
| `/link`, `/unlink` | Confirm an alternate account or detach the current exact account |
| `/card`, `/stats`, `/top` | Group memory and usage |
| `/members`, `/merge`, `/split` | Owner directory and identity repair |
| `/block`, `/mute` | Owner reply controls with explicit subcommands |
| `/debug`, `/log` | Maintenance |

See [docs/commands.md](docs/commands.md) for usage and permissions.

## Operations

Deployment, backups, restore, rollback, schema changes, scheduled jobs, the daily
report, debugging and the behavioural evaluation scripts are described in
[docs/operations.md](docs/operations.md).

## Development

The tests use pytest and need no QQ connection or paid API. Database cases require an
explicit disposable PostgreSQL address; without it, those cases are skipped. Pyright
checks annotated production code and maintained Python scripts (`bot.py`, `qqbot/`,
`scripts/`) in basic mode, including nominal identifiers such as `GroupId` rather
than treating them as interchangeable with `str`. Tests are exercised by pytest,
not included in the Pyright scope.

```bash
python -m venv .venv
.venv/bin/pip install -r requirements.txt -r requirements-dev.txt

ROOT="$(pwd -W 2>/dev/null || pwd)"
docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qbot_test -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw \
  -p 127.0.0.1:15432:5432 \
  -v "$ROOT/sql/init.sql:/docker-entrypoint-initdb.d/01-init.sql:ro" \
  -v "$ROOT/tests/fixtures/test_db_marker.sql:/docker-entrypoint-initdb.d/02-test-marker.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

export QBOT_TEST_DATABASE_URL=postgresql://qbot_test@127.0.0.1:15432/qbot_test
export QBOT_TEST_DATABASE_PASSWORD=testpw
until docker exec qbot-pgtest pg_isready -h 127.0.0.1 -U qbot_test -d qbot_test; do
  sleep 1
done
. .venv/bin/activate
python -m pytest
python -m ruff check .
python -m pyright
```

Run pytest, Ruff and Pyright as independent checks. The type check needs no running
services or provider credentials; database tests use the disposable database above.

Database tests verify the dedicated role, database and safety marker before destructive
operations. Never point them at production, and do not run them concurrently against
the same database.

See [CONTRIBUTING.md](CONTRIBUTING.md) for conventions and [tests/README.md](tests/README.md)
for what each suite covers.

## Project layout

| Path | Contents |
| --- | --- |
| `bot.py` | Entry point; serves the OneBot reverse WebSocket |
| `qqbot/plugin.py` | Thin NoneBot lifecycle and event adapter |
| `qqbot/runtime.py` | Process composition root and ordered lifecycle ownership |
| `qqbot/operations/scheduled.py` | Framework-independent nightly and reporting job bodies |
| `qqbot/configuration/` | Validated immutable settings, prompt/predicate bundles and persona merge |
| `qqbot/gateway/` | OneBot normalization, bounded parsing and archive-first admission |
| `qqbot/commands/` | Command catalogue, authorization and handlers |
| `qqbot/conversation/` | Shared reply inbox, sessions, snapshots, prompts and tools |
| `qqbot/delivery/` | Protocol sends, output validation and own-message observation |
| `qqbot/media/` | Bounded media work, downloads, descriptions and transcription coordination |
| `qqbot/domain/` | Typed ingress plus the memory model: identities, aliases, facts, episodes, evidence |
| `qqbot/repositories/` | Narrow archive, identity, memory, ledger, group-policy, media-cache, evidence and job stores |
| `qqbot/services/` | Extraction, validation, consolidation, budget and member/roster services |
| `qqbot/workers/` | Background memory worker and durable timer worker |
| `qqbot/providers/` | Provider abstractions and one module per backend |
| `qqbot/plugins/` | Thin scheduled-job registration adapters |
| `qqbot/db/` | Owned connection pool, exclusive Runtime lease and schema checks |
| `config/` | Configuration templates, prompts and predicates |
| `sql/` | Database schema and the schema changelog |
| `scripts/` | Deployment, preflight, model download, evaluations |
| `tests/` | The test suites |
| `docs/` | Architecture, configuration, commands, operations |

## License

[MIT](LICENSE)
