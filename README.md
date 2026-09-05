# QQ 群聊 AI Bot

A group-chat member, not an assistant: it answers when addressed, remembers the
group and its people, and stays silent the moment any limit is reached. Design
rationale lives in [docs/QQ-AI-Bot-设计文档.md](docs/QQ-AI-Bot-设计文档.md)
(English: [docs/design-en.md](docs/design-en.md)). Three containers:

```text
QQ <-> napcat <-OneBot v11 reverse WS-> bot <-asyncpg-> postgres
```

Everything about behaviour lives in `config/`; credentials live in `.env`; runtime
state lives in the DB.

The code knows four capabilities - text, vision, ASR, search - and reaches them
through `providers()`. Which platform serves each one, on which endpoint, with
which model and credential, is stated only in `config/settings.yaml`.

Each backend is its own subclass of the ABC in `qqbot/providers/base.py`, because platforms
differ in more than their URL and those differences should be forced into a named class
rather than accumulate as flags. Adding one is: write the subclass, add a line to
`registry.py`, name it in config.

## Layout

| Path | What |
| --- | --- |
| `bot.py` | entrypoint; NoneBot2 serves the reverse WS at `/onebot/v11/ws` |
| `qqbot/plugin.py` | plugin wiring: startup order, the message handler |
| `qqbot/settings.py` | pydantic config models, two-layer merge, `/reload` |
| `qqbot/core/pipeline.py` | the pipeline: dedup, archive, one reply task per addressed message |
| `qqbot/core/trigger.py` | whether this message is addressed to the bot |
| `qqbot/core/prompt.py` | cache-friendly prompt ordering |
| `qqbot/core/retrieval.py` | what the prompt reads: the roster, the group's facts, this turn's episodes |
| `qqbot/domain/`, `qqbot/repositories/`, `qqbot/services/` | the memory model: people, names, facts, episodes, evidence |
| `qqbot/gateway/` | the platform edge and the inbound chain |
| `qqbot/workers/memory.py` | the background extractor and consolidator |
| `qqbot/core/media.py` | image/voice understanding, cache, limits |
| `qqbot/core/budget.py` | the single daily spend gate |
| `qqbot/providers/base.py` | the capability ABCs - names no vendor |
| `qqbot/providers/openai_compat.py` | shared plumbing for chat-protocol backends, quirks as hooks |
| `qqbot/providers/deepseek.py`, `dashscope.py`, `tavily.py` | one module per backend |
| `qqbot/providers/registry.py` | backend name from config -> class |
| `qqbot/plugins/tasks.py` | the five scheduled jobs |
| `scripts/preflight.py` | one real minimal call per capability, run before going live |

## How it behaves

- **The trigger is being addressed** - an @ or a nickname matching as a whole
  word (jieba token plus an ASCII boundary check, so a Latin-lettered nickname
  cannot match inside a longer Latin word). Everything else is read, archived,
  and left alone; the bot never speaks uninvited.
- **Replies require consent.** A member who has not accepted the user agreement
  (`config/agreement.txt`) gets the agreement text instead of a reply, at most
  once per cooldown; `/agree` records acceptance permanently, per group and
  account, and is the one command that answers before consent. Reading and archiving are untouched;
  owners are exempt. A `/block`ed member is the converse: read and remembered
  as always, only never answered - context stays coherent either way.
- **Memory is one store of facts.** What is known about a person and what is
  known about the group are rows in `memory_fact` - the group is an entity too -
  each with a verbatim quote as evidence, a validity window, and a predicate
  from a closed set. Extraction runs once nightly, draining the day's messages
  oldest-first in chunks cut at conversation gaps; names, facts and episodes all
  arrive through function calling, a validator writes, and the model may only
  propose. A wrong entry is deleted by number (`/forget`), and everything
  expires: a fact survives one half-life per supporting event, so what a group
  repeats stays and a passing remark fades in a fortnight.
- **Money is the only limit.** The daily cap and the per-reply cap both mean
  silence when hit - no degraded answers. There is no token budget anywhere:
  the history window is counted in messages and evicts ten at a time to keep
  the prefix cache warm, and billing always uses the usage the API returns.
  Prices live in the backend classes as rate tables (peak/off-peak included);
  an unknown model bills at the priciest tier. `/top` attributes each reply's
  full cost to the member who triggered it.
- **The text models deliberate before answering.** Reasoning tokens bill as
  output and arrive in a separate field, so they never reach the group but do
  reach the invoice. Memory calls ask for terse answers (each backend's
  `_terse_body`, selected by `reasoning_effort: off`); replies keep their
  configured grade - that is what the quality is for.
- **Ops state has its own tables.** `group_state` holds what must survive a
  restart (the mute switch, extraction watermarks); `cost_ledger` is what the
  budget gate, `/stats`, `/top` and the daily report read. Neither carries
  memory semantics.

## Deploy

```bash
cp .env.example .env                      # then fill in the credentials it documents
cp config/settings.yaml.example config/settings.yaml
cp config/personas/default.yaml.example config/personas/default.yaml
$EDITOR config/settings.yaml              # owners, trigger.nicknames
$EDITOR config/personas/default.yaml      # persona, group knowledge
# Both copies stay untracked: they name real accounts and real groups, so only
# the .example templates live in version control. deploy.sh ships the working
# tree, untracked local config included.

docker compose up -d postgres napcat
docker compose build bot
docker compose run --rm bot python scripts/preflight.py
docker compose up -d bot
```

`sql/init.sql` only runs when `data/pg` is empty. If you change the schema after the
first start, apply it by hand and record the statement in
[sql/MIGRATIONS.md](sql/MIGRATIONS.md) - `ensure_schema` refuses to boot until the
live schema has caught up, but it checks names, not the ALTERs that get you there.

### Rollback

`deploy.sh` tags the previously running image `qbot-bot:rollback` before every build.
If a deploy verifies but misbehaves at runtime, from the server:

```bash
cd /opt/docker/qbot
docker tag qbot-bot:rollback qbot-bot:latest    # adjust if `docker compose images bot` names it differently
docker compose up -d --no-build bot
```

The tag holds exactly one step of history and is overwritten on every deploy: after
two bad deploys in a row it points at the *first* bad image, so roll back promptly
or not at all.

The server's source tree stays at the bad version - fix forward from the workstation
(git revert + deploy.sh) as soon as the fire is out, or the next `compose up --build`
rebuilds the bad code.

### Restore from a backup

The nightly `pg_dump -Fc` dumps land in `backups/`. To restore onto a fresh volume:

```bash
docker compose stop bot
docker compose up -d postgres
docker cp backups/qqbot-<date>.dump qbot-postgres-1:/tmp/r.dump
docker exec qbot-postgres-1 dropdb  -U qqbot --if-exists qqbot
docker exec qbot-postgres-1 createdb -U qqbot qqbot
docker exec qbot-postgres-1 pg_restore -U qqbot -d qqbot /tmp/r.dump
docker exec qbot-postgres-1 rm /tmp/r.dump
docker compose up -d bot          # ensure_schema verifies the result at boot
```

The dump carries the extensions and every table; `-Fc` archives are verified nightly
with `pg_restore --list` before old ones rotate out.

### NapCat login and the reverse WS

1. `docker compose logs -f napcat` and scan the QR code, or open the WebUI on
   `http://127.0.0.1:6099` (bound to loopback - use an SSH tunnel from elsewhere).
   The WebUI token is printed in the logs on first start.
2. After login, NapCat writes `data/napcat/config/onebot11_<QQ>.json`. Merge in the
   `websocketClients` entry from [napcat/onebot11.json.template](napcat/onebot11.json.template)
   so it dials `ws://bot:8080/onebot/v11/ws`, keep `messagePostFormat: "array"`, then
   `docker compose restart napcat`.
3. `docker compose logs bot` should show the adapter connecting.

## Ops

The console is the owner's, with two carve-outs. Any member may run `/who`,
`/note`, `/alias` and `/forget` against themselves - their own record, note and
names, at the same full trust as the owner's hand - plus `/agree` for
themselves by nature; and the read-only surfaces `/card`, `/stats`, `/top` and
`/groupstats` whole. `/help` lists each reader exactly what they may run;
everything else answers members with silence. A member can still just ask the
bot in conversation - the roster is already in its prompt - but the command
path answers for free.

Scheduled: memory extraction 02:30 (the nightly drain), memory decay 04:00, `pg_dump -Fc`
04:30 (keeps 14, into `backups/` - point that volume at a NAS mount; until then the dumps
share the disk they protect), NapCat media cleanup 05:00, daily report to the owners 09:00.

Routine intervention is meant to be one thing: read the daily report, change the config.

Model-behaviour tooling: `scripts/eval_replies.py` runs the deterministic eval set against
the real model from the workstation (needs the test DB and `.env`; ~CNY 0.02 a run) — run
it before and after any prompt or model change. `/debug N` captures the next N model
rounds' full requests and responses into `logs/debug/` for when a reply misbehaves and
you need to see what the model was actually shown. The daily report carries the output
stripper's hit counters: every hit is a marker the model wrote and the stripper caught.

## Local development

The container is the only supported runtime, but the pure logic runs without a
protocol side:

```bash
python -m venv .venv && .venv/bin/pip install pydantic pyyaml jieba asyncpg openai httpx
docker run -d --name qbot-pgtest -e POSTGRES_DB=qqbot -e POSTGRES_USER=qqbot \
  -e POSTGRES_PASSWORD=testpw -p 15432:5432 \
  -v "$PWD/sql/init.sql:/docker-entrypoint-initdb.d/init.sql:ro" \
  pgvector/pgvector:0.8.5-pg17
```
