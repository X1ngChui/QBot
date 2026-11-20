# QBot

QBot is an AI member for QQ group chats. It answers when it is @-mentioned, called by name or
quoted, and when one of its own scheduled tasks comes due; it remembers what each group talks
about and who its members are; it reads pictures and listens to voice messages; and otherwise it
stays out of the way.

- **Speaks when addressed.** An @, one of its nicknames in a message, a quote of one of its
  messages, or a due group task starts a reply; optionally it also joins in on its own now and then
  (`replies.spontaneous_chance`, off by default). The model decides whether to say anything, whom to mention
  and what to quote, and sends real QQ messages (mentions, quotes, faces, dice, contact cards).
- **Memory grounded in the chat.** The archive of every message is the source of truth. Once a
  night the bot divides each group's day into conversations, summarizes them, and learns the names
  people use for one another and what is said about members and the group, each from a verbatim
  quote checked by code and kept with who said it and when. A later statement replaces or ends a
  memory with its history kept, and what nobody repeats makes way as the conversation moves on.
  With one command, `/memory`, members see everything known about them and add, correct, end or
  forget any of it; a forgotten memory is never learned again.
- **Identity, not nicknames.** Accounts belong to people; linking needs confirmation; every member
  has a stable number in the group, so people sharing a name are never confused.
- **Pictures and voice.** Pictures are described once and archived as text, and the model can look
  at an original itself; voice clips are transcribed on the CPU, in the process.
- **Tools.** Search the group's history, recall earlier conversations by meaning,
  search the web and read pages, open pictures, and manage the group's scheduled tasks.
- **Operations built in.** A daily off-peak memory batch, verified nightly backups, a daily report to
  the owners, and a single-instance lease. Usage is counted; nothing refuses work on cost.

QBot connects to QQ through [NapCat](https://github.com/NapNeko/NapCatQQ) (OneBot v11, reverse
WebSocket) and stores everything in PostgreSQL 17 with pgvector. The language model, vision model,
embeddings and web search are providers behind one crate (DeepSeek's Responses API, an
OpenAI-compatible embeddings endpoint such as DashScope, Tavily).

## Running it

Deployment is a Docker Compose project: see [deploy/README.md](deploy/README.md) for the
configuration layers, secrets and NapCat settings.

```bash
qbot check-config   # layers, secrets, directories, required settings; exit 0 if valid
qbot run            # serve NapCat's connection until SIGINT/SIGTERM
```

`qbot run` takes the database lease first (a second instance on the same database refuses to
start), recovers what a previous process left open, then listens for NapCat's reverse WebSocket
on `gateway.listen`, path `/onebot/v11/ws`, with the token named by `gateway.access_token_secret`.

How it works: [docs/architecture.md](docs/architecture.md), and [docs/memory.md](docs/memory.md)
for memory.

## Development

Develop on `dev`; `main` is the released line. The earlier Python implementation is kept on the
`py` branch for reference.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**Database tests** need a disposable PostgreSQL server with pgvector. Each test creates its own
database and drops it afterwards; the database named in the URL must start with `qbot_test`.
Without `QBOT_TEST_DATABASE_URL` they fail on purpose (`QBOT_SKIP_DB_TESTS=1` skips them, which is
not a full run).

```bash
docker run -d --name qbot-pgtest \
  -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw -e POSTGRES_DB=qbot_test_admin \
  -p 127.0.0.1:15432:5432 --tmpfs /var/lib/postgresql/data \
  pgvector/pgvector:0.8.5-pg17 -c fsync=off -c synchronous_commit=off -c max_connections=400

export QBOT_TEST_DATABASE_URL=postgres://qbot_test:testpw@127.0.0.1:15432/qbot_test_admin
cargo test --workspace
```

**Speech model tests** run the real recognizer only when `QBOT_ASR_MODEL_DIR` points at the
SenseVoice bundle (`model.int8.onnx`, `tokens.txt`, `test_wavs/{en,zh}.wav`; fetched by
`deploy/fetch_asr_model.sh`); otherwise they say SKIPPED. The first build downloads sherpa-onnx's
native library, so it needs network access.

**Live tests** (ignored, "calls paid provider APIs") run against the real providers by hand with
`-- --ignored`. **Reply quality** is evaluated with `cargo run -p qbot-eval -- [--only NAME]
[--repeat N]`, scenarios in `eval/scenarios/`; it calls the real model.

The end-to-end test in `crates/qbot-app/tests/run_e2e.rs` runs the whole service against a real
Postgres, a mock Responses server and a simulated NapCat.

## Crates

| crate | what it is |
|---|---|
| `qbot-core` | ids, clock, the chat-batch grid, markers |
| `qbot-context` | the canonical transcript and its projection |
| `qbot-llm` | provider contract, Responses/DeepSeek adapter, embeddings, web search, fakes |
| `qbot-agent` | run loop, tool contract, supervisor |
| `qbot-sched` | timers: group tasks, background jobs, repeat rules |
| `qbot-tools` | the model's tools |
| `qbot-memory` | identity, names, episodes, the daily batch, memories, recall |
| `qbot-store` | Postgres adapters, the schema, the lease |
| `qbot-config` | layered, typed configuration and secrets |
| `qbot-i18n` | member-facing text: typed messages and validated catalogs (`locales/`) |
| `qbot-wording` | model-facing short texts (`prompts/wording.toml`) |
| `qbot-prompt` | instruction templates (`prompts/`), personas, chat rendering, run context |
| `qbot-media` | pictures and voice: fetch, describe, transcribe, fill the archived markers |
| `qbot-asr` | in-process speech recognition (SenseVoice via sherpa-onnx) |
| `qbot-ops` | the nightly run, verified backups, the daily report |
| `qbot-gateway` | OneBot v11: frames, rendering, triggers, delivery, the WebSocket server |
| `qbot-commands` | `/memory /link /unlink /stats /top /tasks /members /block /mute /admin /runs /logs /help` |
| `qbot-eval` | reply-quality evaluation against the real model |
| `qbot-app` | the `qbot` binary and the composition root |

## License

MIT, see [LICENSE](LICENSE).
