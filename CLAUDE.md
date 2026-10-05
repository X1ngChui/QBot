# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

QBot is an AI member for QQ group chats: a Rust service (`qbot run`) that NapCat (OneBot v11)
connects to over a reverse WebSocket, backed by PostgreSQL 17 + pgvector. Read
`docs/architecture.md` before larger changes; it is the authoritative design doc, with
`docs/memory.md` for identity, episodes, extraction and recall. Develop on `dev`; `main` is the
released line. The earlier Python implementation is kept on the `py` branch for reference only.

## Commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # the full suite needs the database below
cargo test -p qbot-memory                    # one crate
cargo test -p qbot-commands --test commands name   # one test
cargo run -p qbot-eval -- --repeat 3         # reply-quality evaluation; calls the real model
```

Database tests (`qbot-store`, `qbot-app`'s end-to-end test and others) need a disposable pgvector
Postgres; without `QBOT_TEST_DATABASE_URL` they fail on purpose (`QBOT_SKIP_DB_TESTS=1` skips
them, which is not a full run). Each test creates and drops its own database, whose name must start
with `qbot_test`:

```bash
docker run -d --name qbot-pgtest \
  -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw -e POSTGRES_DB=qbot_test_admin \
  -p 127.0.0.1:15432:5432 --tmpfs /var/lib/postgresql/data \
  pgvector/pgvector:0.8.5-pg17 -c fsync=off -c synchronous_commit=off -c max_connections=400
export QBOT_TEST_DATABASE_URL=postgres://qbot_test:testpw@127.0.0.1:15432/qbot_test_admin
```

The recognizer tests run only when `QBOT_ASR_MODEL_DIR` points at the SenseVoice bundle
(`deploy/fetch_asr_model.sh` fetches it); otherwise they say SKIPPED. Tests ignored with "calls
paid provider APIs" are live tests run by hand with `-- --ignored`; nothing else talks to QQ or a
paid API. Build the image with `docker build -f deploy/Dockerfile .`; deployment is described in
`deploy/README.md`.

## Architecture

- **Composition root:** `crates/qbot-app/src/run.rs` builds every part from one loaded
  configuration: database and the single-instance Postgres lease (losing it shuts the process
  down), providers, tools, prompt context, supervisor, scheduler, media service, commands, the
  gateway pipeline and server. No globals: settings and ports are passed explicitly.
- **Message path:** `qbot-gateway` parses OneBot frames, renders a message to archive text with
  ASCII markers, archives it (`UNIQUE (group_id, message_id)` is the only dedup gate), then routes
  the bot's own line to the echo board, a command word to `qbot-commands`, or a trigger (@, a
  nickname as a whole jieba token, a quote of a bot line) to the supervisor.
- **Runs:** `qbot-agent`. Every trigger or due task is its own run with its own deadline over an
  append-only transcript (`qbot-context`). The model speaks only through `send_message`
  (`{text, end_turn?}` in the chat's marker syntax, waits for its echo) or ends with
  `stay_silent`; text without a tool call gets one note and a forced tool turn. Every tool call
  executes (no dedup). Tools live in `qbot-tools` (and `open_images` in `qbot-media`).
- **Providers:** `qbot-llm` is the only place vendors are named: text and vision through the
  Responses API (DeepSeek and OpenAI-style dialects, typed capabilities), embeddings, Tavily search.
  No local output-token caps; reasoning items never leave the adapter.
- **Memory:** `qbot-memory`. The archive is the source of truth. Episodes summarize fixed slices of
  whole batches, extracted with validated findings (names, facts, group knowledge) that
  consolidation applies; recall ranks by similarity decaying with age; embeddings are an index the
  nightly run keeps complete for the configured model. Notes written by hand are a separate store.
- **Durable work:** one `timer` table in `qbot-sched`: group tasks (at most once, interrupted on
  restart, never replayed) and jobs (leased, retried), plus cron schedules fired exactly once per
  occurrence. `qbot-ops` runs the nightly extraction, decay, backup and cleanup, and the report.
- **Layering:** `qbot-core`/`qbot-context` are pure; a port is defined by the crate that needs it
  and implemented by `qbot-store` (Postgres) or `qbot-gateway` (OneBot).

## Conventions that bite

- **Ids are newtypes** (`AccountId`, `GroupId`, `MessageId`, ...) parsed at the boundary; don't pass
  raw integers around. Closed sets are enums, and the schema repeats them as CHECK constraints.
- **No CJK in Rust code or comments.** It lives only in `locales/`, `prompts/`, persona files and
  test fixtures; tests build CJK input from `\u` escapes.
- **Wording is data.** Instruction templates are `prompts/*.md` (closed slot sets, tested); every
  short model-facing text is in `prompts/wording.toml` behind `qbot-wording`'s `Text` enum;
  member-facing text is a typed `Msg` in `qbot-i18n` with Fluent catalogs. Markers the code writes
  and parses stay in code. Run a changed prompt through `qbot-eval`.
- **Configuration holds only what a deployment may choose** (`crates/qbot-config/defaults.toml` is
  the whole schema; unknown keys are rejected). Internal tuning is the `Default` of the owning
  crate's settings. A new setting goes in `defaults.toml` with a comment, the example config if
  operators need it, and the conversion in `qbot-config`; credentials are never configuration.
- **Every local limit needs a concrete reason** and must not duplicate a provider-side or
  already-implied bound; `docs/architecture.md` section 14 lists them.
- **Schema:** `crates/qbot-store/migrations/`, forward-only: production runs it, so a change is a
  new numbered file, never an edit to an applied one. The in-memory store implementations
  define the semantics, and shared conformance suites run against both.
- **New command:** catalog in `qbot-commands/src/catalog.rs`, handler, `Msg` texts in both catalogs,
  a test in `crates/qbot-commands/tests/commands.rs`, and the table in `docs/architecture.md`.
- Comments describe the present, not history. Tests, docs and examples use invented names and
  group numbers.
- Commit messages: one line saying what changed (and why if not obvious), naming the crate,
  command or setting.
