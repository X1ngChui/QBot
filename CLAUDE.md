# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

QBot is an AI member for QQ group chats: a NoneBot2 app (`bot.py` → `qqbot/plugin.py`) that
NapCat (OneBot v11) connects to over a reverse WebSocket, backed by PostgreSQL 17 + pgvector.
Python 3.12. Read `docs/architecture.md` before larger changes; it is the authoritative design doc.

## Commands

```bash
python -m venv .venv && .venv/bin/pip install -r requirements.txt -r requirements-dev.txt

python -m pytest                              # the only supported test entry point
python -m pytest tests/test_pipeline.py       # one suite
python -m pytest tests/test_commands.py -k name   # one test
python -m ruff check .                        # lint (import sorting deliberately off)
python -m pyright                             # types: bot.py, qqbot/, scripts/ only (not tests); CI runs this

python scripts/lint_prompts.py                # validate config/prompts/prompts.yaml offline
python scripts/config_reference.py [--write]  # check / regenerate docs/configuration-reference.md and migration map
```

Run pytest, Ruff and Pyright as independent checks. Database tests (`tests/integration/`,
`test_schema.py`, `test_scheduled.py`, …, marker `database`) **skip** unless
`QBOT_TEST_DATABASE_URL` is set; a skipped DB case is not a pass. Start the disposable DB:

```bash
docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qbot_test -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw \
  -p 127.0.0.1:15432:5432 \
  -v "$PWD/sql/init.sql:/docker-entrypoint-initdb.d/01-init.sql:ro" \
  -v "$PWD/tests/fixtures/test_db_marker.sql:/docker-entrypoint-initdb.d/02-test-marker.sql:ro" \
  pgvector/pgvector:0.8.5-pg17
export QBOT_TEST_DATABASE_URL=postgresql://qbot_test@127.0.0.1:15432/qbot_test
export QBOT_TEST_DATABASE_PASSWORD=testpw
```

The fixture verifies the test role/database/`qbot_test_guard` marker before mutating. Never
point it at production, and don't run DB-mutating tests concurrently against one database.
Nothing in the suite talks to QQ or a paid API. `scripts/eval_replies.py`, `eval_extract.py`,
`eval_tasks.py`, `generate_prompts.py`, `review_prompts.py` and `preflight.py` call real models
and are run by hand only.

Deploy with `scripts/deploy.sh` (always rebuilds; `docker compose restart` does not pick up code).

## Architecture

- **Composition root:** `qqbot/runtime.py`. `plugin.py` loads one immutable `ConfigBundle` and
  calls `Runtime.build()`; the Runtime owns clock, database, budget ledger, providers, caches,
  `GroupDelivery`, command router, the shared `ReplyScheduler`, gateway and workers. There is no
  `config()`/service-locator global — pass settings, prompt catalogs and predicates explicitly.
  Startup takes an exclusive PostgreSQL session lease; losing it shuts the process down.
- **Message path:** `gateway/` normalizes OneBot events to one `InboundEvent`, archives it
  (`ON CONFLICT DO NOTHING` is the sole dedup gate), then routes commands (`commands/`) or, if
  triggered (@, whole-word jieba nickname match, or quoting the bot — `gateway/trigger.py`),
  submits a bounded reply request to the shared inbox.
- **Reply engine:** `conversation/`. Each addressed message or due timer gets an independent
  `ReplySession` (no merging) with its own deadline and fuel. Gates: mute → block → budget →
  media wait. The model replies only via tools: `send_message` (one QQ message per call, waits
  for its self-echo) and `finish_reply`; a tool-free turn ends silently. Every tool call executes
  (no dedup). Tools live in `conversation/tools.py` / `tool_registry.py`.
- **Providers:** `qqbot/providers/` is the only place vendors are named. Five capabilities (text,
  vision, asr, embedding, search) behind contracts in `providers/base.py`; text/vision use the
  Responses API with task-local replay (`TextSession`). No fallback between backends; unknown
  models bill at the most expensive tier. Reasoning items never leave the provider adapter.
- **Memory:** `domain/memory/`, `services/memory_extractor.py`, `workers/memory.py`. Facts are
  extracted nightly by function call, validated against verbatim quotes, stored with evidence,
  confidence and expiry. Identity (`domain/identity/`) separates accounts, display names and
  people; prompts refer to members by member number. Episodes reach a reply only via the
  `recall_events` tool.
- **Durable work:** `memory_job` queue (`FOR UPDATE SKIP LOCKED` + leases) and a separate
  `scheduled_task` table for group tasks (`services/scheduled_tasks.py`, `workers/scheduled.py`);
  claims are single-attempt and are marked interrupted on restart, never replayed.
- **Layering:** `domain/` (pure model) → `repositories/` (SQL, one per aggregate) →
  `services/` → `conversation/`, `gateway/`, `commands/`, `operations/`.

## Conventions that bite

- **Nominal IDs:** `AccountId`, `GroupId`, `MessageId` (`qqbot/domain/ids.py`) must not be
  interchanged with `str`; construct at ingress/SQL boundaries. Don't suppress Pyright wholesale.
- **Language:** comments, docstrings, logs and SQL are English; Chinese only in model-facing
  prompts/personas/predicates and member-facing command replies. `tests/test_logic.py` enforces this.
- **Prompts are data:** all prompt wording lives in `config/prompts/prompts.yaml`; template keys,
  roles and exact `{{slot}}` sets are a closed contract in `qqbot/prompting/templates.py`
  (see `config/prompts/README.md`). Markers the code writes and parses stay in code. A changed
  prompt should be run through the matching eval script.
- **Schema:** `sql/init.sql` is the sole canonical schema; also update the structural contract in
  `qqbot/db/repo.py`. No runtime fallback reads or migration chains.
- **Config:** settings are pydantic-validated, unknown keys rejected. A new key goes in
  `config/settings.yaml.example` and `docs/configuration.md` (then regenerate the reference).
  Every setting needs a reader and must be a real choice. Real `settings.yaml`/personas are gitignored.
- **New command:** add to `commands/catalog.py`, handle in `commands/router.py`, document in
  `docs/commands.md`, test in `tests/test_commands.py`.
- Comments describe the present, not history. Tests/docs/examples use invented names and group numbers.
- Commit messages: one line saying what changed (and why if not obvious), naming the module/command/setting.

## Rust rewrite (in progress)

`rust/` holds the Rust implementation that replaces the Python bot at the cutover; the Python code
stays until then. Start with `docs/rust-rewrite/design.md` (architecture, configuration, every limit
and its reason), `docs/rust-rewrite/memory.md` (identity, episodes, extraction, recall),
`docs/rust-rewrite/cutover.md` (the remaining steps and the transition tooling deleted afterwards),
`rust/README.md` (commands; the database tests need a disposable pgvector Postgres) and
`rust/deploy/README.md` (configuration layers, Docker layout, secrets).
Workspace crates: `qbot-core` (ids, batch grid, markers), `qbot-context` (canonical transcript),
`qbot-llm` (provider contract, Responses/DeepSeek adapter, embeddings, web search, fakes),
`qbot-agent` (run loop, tools, supervisor), `qbot-sched` (timers, tasks, recurring schedules),
`qbot-tools`, `qbot-memory` (identity, episodes, facts, recall, notes), `qbot-store` (Postgres),
`qbot-config` (typed layered configuration), `qbot-i18n` (member-facing text), `qbot-wording`
(model-facing short texts), `qbot-prompt` (instruction templates, personas, chat rendering),
`qbot-gateway` (OneBot), `qbot-media` (pictures and voice), `qbot-asr` (speech recognition),
`qbot-ops` (nightly run, backups, report), `qbot-commands` (chat commands), `qbot-app` (the `qbot`
binary), `qbot-eval` (reply-quality evaluation against the real model; run by hand, see `rust/eval/`).
Conventions: no CJK in Rust code or comments (it lives only in locale catalogs, prompts and
fixtures); configuration holds only what a deployment may choose, and internal tuning is the
`Default` of the owning crate's settings; every local limit needs a concrete reason (design.md 14)
and must not duplicate a provider-side or already-implied bound (no local output-token caps);
credentials are never config; the schema is one migration file until the Rust schema is in
production; model-facing wording lives in `rust/prompts/`.
