# QBot (Rust rewrite)

Design: [`docs/rust-rewrite/design.md`](../docs/rust-rewrite/design.md). The Python
implementation in the repository root stays in place until cutover.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Database tests

The `qbot-store` tests need a disposable PostgreSQL server with the pgvector and btree_gist extensions
(the `pgvector/pgvector:pg17` image). Each test creates its own database
and drops it afterwards. The database named in the URL must start with `qbot_test`; the tests
refuse anything else.

```bash
docker run -d --name qbot-pgtest \
  -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw -e POSTGRES_DB=qbot_test_admin \
  -p 127.0.0.1:15432:5432 --tmpfs /var/lib/postgresql/data \
  pgvector/pgvector:pg17 -c fsync=off -c synchronous_commit=off -c max_connections=400

export QBOT_TEST_DATABASE_URL=postgres://qbot_test:testpw@127.0.0.1:15432/qbot_test_admin
cargo test --workspace
```

Without `QBOT_TEST_DATABASE_URL` the database tests fail on purpose. Set `QBOT_SKIP_DB_TESTS=1`
to skip them; a run with skipped database tests is not the full suite.

## Speech model tests

`qbot-asr` and the voice part of the end-to-end test run the real recognizer only when
`QBOT_ASR_MODEL_DIR` points at a directory with `model.int8.onnx`, `tokens.txt` and
`test_wavs/{en,zh}.wav` (the published SenseVoice bundle layout); without it they say SKIPPED and
pass. The first build downloads sherpa-onnx's native library, so it needs network access.

## Configuration and deployment

Settings are layered (baked-in defaults, `/etc/qbot/config.toml`, `conf.d/*.toml`, then
`QBOT__SECTION__KEY` environment variables) and validated at startup; credentials come only from
the environment or mounted secret files. See [deploy/README.md](deploy/README.md) for the Docker
layout, precedence rules and secret handling. Check a deployment with `qbot check-config`.

## Running

```bash
qbot check-config   # layers, secrets, directories, required settings; exit 0 if valid
qbot run            # serve the platform connection until SIGINT/SIGTERM
```

`qbot run` takes the database lease first (a second instance on the same database refuses to
start), recovers what the previous process left open, then listens for NapCat's reverse WebSocket
(`gateway.listen`, `gateway.path`, bearer `gateway.access_token_secret`). It needs `bot.account`.
The end-to-end test in `crates/qbot-app/tests/run_e2e.rs` runs all of it against a real Postgres,
a mock Responses server and a simulated NapCat.

## Crates

| crate | what it is |
|---|---|
| `qbot-core` | ids, clock, the chat-batch grid |
| `qbot-context` | the canonical transcript and its projection |
| `qbot-llm` | provider contract, Responses/DeepSeek adapter, embeddings, fakes |
| `qbot-agent` | run loop, tool contract, supervisor |
| `qbot-sched` | timers: group tasks and background jobs |
| `qbot-tools` | the model's tools |
| `qbot-memory` | identity and episodic memory |
| `qbot-store` | Postgres (archive, run log, usage, timers, identity, episodes, admin reads) |
| `qbot-config` | layered, typed configuration and secrets |
| `qbot-i18n` | member-facing text: typed messages and validated catalogs (`locales/`) |
| `qbot-prompt` | instruction templates (`prompts/`), personas, chat rendering, run context |
| `qbot-media` | pictures and voice: fetch, describe, transcribe, fill the archived markers |
| `qbot-ops` | the nightly pipeline, verified backups, the daily report |
| `qbot-asr` | in-process speech recognition (SenseVoice via sherpa-onnx) |
| `qbot-gateway` | OneBot v11: frames, rendering, triggers, delivery, the WebSocket server |
| `qbot-commands` | `/who /note /name /forget /group /link /unlink /stats /top /tasks /members /block /mute /runs /logs /help` |
| `qbot-eval` | reply-quality evaluation: `cargo run -p qbot-eval -- [--only NAME] [--repeat N]` (calls the real model; scenarios in `eval/scenarios/`) |
| `qbot-app` | the `qbot` binary and the composition root |
