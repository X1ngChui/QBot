# QBot Rust rewrite: requirements, diagnosis and target architecture

Status: approved with the decisions in section 11; implementation started. Derived from a
read-only survey of the Python implementation; file references are to that code.

## 1. What the product must do (requirements)

R1. Join QQ groups through NapCat (OneBot v11, reverse WebSocket, bot is the server). Receive
group messages, the bot's own echoed messages, and notices (recall, join, leave, kick, ban,
poke). Send group messages (text, at, reply, face, dice, rps, a member's contact card). The bot
never sends or reveals another group's card: it knows only the group it is in (decision 7).
R2. Archive every event exactly once. A unique-key insert is the only dedup gate; a duplicate
stops all further effects. Identity writes for the speaker and mentioned accounts happen in the
same transaction. The bot's own lines come only from the echo.
R3. Speak only when caused: @bot, whole-word nickname, quote of a bot line, or a due group task.
Mute stops all of it. A blocked member cannot trigger a reply but stays in context.
R4. Reply through tools: the model decides whom to @, what to quote, and whether to send at all.
R5. Tools: web search, page read, boolean archive search, semantic episode recall, open pictures,
group task create/list/get/update/cancel, send.
R6. Group-scoped memory: aliases, facts (closed predicate set, evidence, confidence, expiry),
episodes with embeddings. Extraction with verbatim-quote validation, staged then applied in one
transaction. Decay marks rows expired, never deletes. Manual notes written by people are a
separate store with their own commands, never mixed with extracted memory (decision 6).
R7. Identity: accounts, holders (persons), exact-account vs linked scope, link/unlink/merge/split
with revision-checked confirmation. Stable member numbers inside a group.
R8. Media: every picture described once and archived as text (cached), the model may open
originals; voice transcribed locally on CPU on arrival.
R9. Commands without model calls: help, who, note, name, forget, group, link, unlink, stats, top
(member); tasks, members, block, mute, runs, logs (owner only). Owner-only forms of member
commands: `/link @a @b`, `/unlink @a`, `/forget group N`. Names follow what a command is about,
not the Python version (4.22).
R10. Group tasks: durable one-shot timers, group-owned, bounded in count and horizon, never run
twice, wake the agent with fresh context. Manageable by the model and by owner commands.
R11. Operations: nightly extraction, decay, verified pg_dump with rotation, media cache cleanup,
daily owner report, single-instance exclusivity.
R12. Observability: token, call, latency and cache statistics. No money enforcement of any kind.
R13. Localization: all user-facing text comes from message catalogs; no CJK in the repository.

## 2. What is accidental in the Python version

Spending enforcement (budget.py, per-reply scope, search allowance, price tables, the
fail-closed ledger latch, `/top` by spend) is removed entirely. Only counters survive.

Agent loop and context, ranked by cost:
1. Two transcripts: a neutral `PromptItem` list plus raw provider dicts (reasoning, assistant
   items) kept inside the provider session; a third partial copy for debug. Nothing can
   reproduce the exact request.
2. Tool results are bare strings; failure is a `str` subclass lost on serialization. Quota,
   overflow and exclusivity notes are indistinguishable from content.
3. History across sessions is a lossy projection: only sends survive (re-synthesized from the
   archive with invented call ids); searches, recalls, silent endings and decisions vanish.
4. Hidden mutable numbering (line, member, picture numbers) lives outside the transcript.
5. Token waste: an extra model call after almost every send just to emit `finish_reply`; a
   forced finish round at the message cap; the tools array swapped mid-session (breaks the
   prefix cache); refused rounds (exclusivity, bad finish args); an unbounded roster inside
   the cached prefix whose hint scores change nightly; positional `#N` line numbers that shift
   on every chunk eviction; one independent full session per addressed message, so concurrent
   sessions in one group overlap and answer twice; retries re-bill whole prompts; per-character
   Python payload measurement run several times per round.

Structure: `services/` thin wrappers over `repositories/`; the same confidence and decay rules
stated in domain code and in SQL; scattered limit dataclasses; bespoke bounded caches and
single-flight code in four places; duplicated validation and double deadlines/scopes; two
schedulers (APScheduler for cron, a 15 s poller for timers); a job queue heavier than its use.

Fallbacks: most of 46 `except Exception` sites log and continue; legacy schema and prompt-lint
guards; compatibility aliases; dead isinstance/None guards behind strict models. Real ones are
listed in section 7.

## 3. Reference designs studied (from general knowledge of these systems)

- Coding-agent loops (Claude Code, Codex CLI whose core is itself Rust): one loop, "call model;
  if it requested tools run them and append results; else stop". One turn owner per session;
  new user input while running is queued/steered into the same run, not a parallel run.
- Codex-style canonical history: a typed item list (message, function call, function output,
  reasoning) with `call_id` pairing, persisted as an append-only log, with provider formats
  produced at the edge. Resume replays the log.
- Anthropic/OpenAI wire rules: every tool request needs exactly one result, adjacent and in
  order. This is an invariant the core must guarantee, not something a provider adapter repairs.
- Durable-state frameworks (LangGraph checkpoints, OpenAI Agents SDK sessions): state per thread
  is stored; resume continues from a checkpoint; interrupted tool calls are resolved explicitly.
- Context management: keep a stable cached prefix and append only; summarize only at turn
  boundaries and when the window actually requires it, never split a call/result pair, keep
  originals in storage.

We copy the principles (single loop, canonical append-only log, adapter edge, explicit
compaction), not an implementation.

## 4. Target architecture

### 4.1 Workspace and dependency direction

A Cargo workspace under `rust/`. Arrows point from dependent to dependency; no cycles.

```text
qbot-app (binary: config, wiring, signals)
  |- qbot-gateway   OneBot server, event normalization, delivery, echo correlation
  |- qbot-agent     run loop, tool trait + executor, turn policy
  |- qbot-tools     concrete tools (send, search, recall, tasks, ...)
  |- qbot-memory    extraction, consolidation, decay, retrieval, identity services
  |- qbot-media     picture description, voice transcription
  |- qbot-sched     unified durable scheduler (tasks, jobs, cron)
  |- qbot-commands  command catalog and handlers
  |- qbot-llm       Provider trait + adapters (Responses/DeepSeek, embeddings, search, ASR)
  |- qbot-store     sqlx repositories, schema, transactions
  |- qbot-context   canonical transcript, projection, compaction   (pure, no IO)
  |- qbot-i18n      catalog loading, typed message keys
  |- qbot-metrics   usage events and counters
  |- qbot-core      ids, events, identity and memory domain types, errors (pure)
```

Rules: `core` and `context` do no IO and depend on nothing but std/serde. Only `store` talks to
Postgres; only `llm` talks to providers; only `gateway` talks to OneBot. Higher crates depend on
traits defined in the lower crate that needs them (`context` defines nothing about providers).

### 4.2 Core types (qbot-core)

Newtypes with private constructors: `GroupId`, `AccountId`, `MessageId`, `HolderId`,
`RunId`, `CallId`, `TaskId`, `ItemSeq`. Platform ids are parsed at the gateway boundary and
never exist as raw integers elsewhere. Enums for closed sets: `Predicate`, `DecayClass`,
`FactStatus`, `TaskStatus`, `RunEnd`, `Access`. Invalid states are unrepresentable where cheap:
`Subject::{Account, Holder, Group}` instead of two nullable columns; `Scope::{Exact, Holder}`;
`TaskStatus` carries its own data (`Running { started_at }`, `Done { outcome }`).

### 4.3 Conversation model (qbot-context) - the central redesign

One canonical append-only transcript per run. Everything the model sees in that run is derived
from it. Runs of the same group may overlap (section 4.4); each owns its transcript.

```rust
enum Item {
    Chat(ChatBatch),            // group lines: the window at trigger time, then later arrivals
    Assistant(AssistantTurn),   // ordered parts: reasoning (opaque), text, tool calls
    ToolResult(ToolResult),     // result of exactly one ToolCall
    Instruction(Instruction),   // system/developer text, versioned by template hash
    Meta(Meta),                 // run start/end, interruption, compaction, never sent raw
    Summary(Summary),           // compaction product, references the items it replaces
}
struct ToolResult { call_id: CallId, outcome: Outcome, content: Vec<Part> }
enum Outcome { Ok, Error(ErrorKind), Refused(Reason), Interrupted, Truncated { kept: usize } }
```

Invariants enforced by the type and by the only mutation API, `Transcript::append`:
- A `ToolResult` appends only if its call is pending; each call gets exactly one result.
- While calls are pending, only their results may be appended (type-state `Open`/`Quiescent`).
- Items are append-only with a dense `ItemSeq`; nothing is edited in place. The invariants are
  checked at runtime in the single mutation path (state is persisted data, so a compile-time
  type-state would not survive a reload); loading replays the stored items through the same path.
- Blocking never removes context. A `ChatBatch` holds every archived line, including lines from
  blocked members; each member line carries a `MemberStanding` (`Normal` or `Blocked`) that the
  prompt renders as a marker, with a standing instruction not to reply to or address blocked
  members. Blocking is enforced structurally at admission (section 4.4), not by this marker.
- New chat arrives as further `Chat` items appended between turns (never while calls are
  pending). The first `Chat` item is the window cut at trigger time. Echoes of the bot's own
  messages and platform-generated results (dice, rock-paper-scissors) arrive the same way, which
  is how the model observes the real outcome of a nondeterministic send.
- Chat lines carry stable `MessageId`/archive sequence numbers; member numbers are persisted
  per group on first appearance. Nothing is positional, so eviction cannot renumber, and no
  hidden numbering state exists outside the log.

Persistence: `run_item(run_id, seq, kind, payload jsonb)` append-only, plus `run(run_id, group,
trigger, started, ended, end_reason)`. Resume loads the log, and any pending calls are closed with `Outcome::Interrupted` before anything else
(deterministic, recorded as `Meta::Resumed`). Tool-effect idempotence is by `CallId`.

Projection: a pure function `project(&Transcript, &ProjectionPolicy, Provider) -> Wire`
implemented as two steps: policy picks the visible items (compaction view), then the provider
adapter serializes. Each model request stores `request_log(run, turn, item_range, view_hash,
tools_hash, usage)`. "The exact logical conversation the model saw" is therefore
`(run items 0..n, policy version)`, reproducible and inspectable by an owner `/runs`
command that renders the view, never a separately built copy.

Context management (`CompactionPolicy`), applied only at turn boundaries and in this order:
1. Stable prefix: instructions, persona, tools. Never changes inside a run; tools array is fixed
   for the whole run (use tool_choice, not a swapped tool list).
2. History is never rewritten in place: the view of an earlier turn is byte-identical on every
   later request, so the provider's prefix cache keeps hitting. There is no sliding retention
   window and no result elision.
3. Replace an older range with a `Summary` produced by a model call, only when the projected
   size crosses a threshold. Summaries are chunk-aligned so the prefix stays cache-stable
   between compactions. Originals stay in storage; a Summary names its source range.
4. Never compact: an open run, pending calls, any call/result pair half-inside the range, and
   items flagged `pinned` (task creations, send acknowledgements within a retention window).
A property test asserts that for every policy and every transcript the projection has paired,
ordered calls and results.

Cross-run history: the group chat archive is the shared stream; a run's `ChatBatch` is the
visible tail of it at the trigger time. Earlier runs are stored in full (calls, results, end
reason) and are inspectable, but later runs see them only through `HistoryTrace`: `SendsOnly`
(default; the bot's own lines appear as the real stored send call/result pairs, not ids
invented from message ids) or `Full`. Silent endings are recorded in `run`, not rendered.

Group volatile data is not in the prefix. The roster is not a block: the reply instruction
carries only members present in the visible window (numbers, display names), and a
`lookup_member` read tool returns facts and notes on demand. Hint scores are never rendered into
the cached prefix.

### 4.4 Agent runtime (qbot-agent)

Each eligible trigger - addressed message, quote, nickname, task wakeup - creates its own
independent run with its own transcript, deadline and stop-loss scope, exactly as today. Runs of
one group may overlap; a run starts from the window cut at its trigger time and takes in newly
archived group lines between turns. Admission is a single `RunSupervisor` with one bounded inbox shared by addressed
replies and due timers (capacity and deadline as today, queue wait counts against the deadline),
and one global concurrency semaphore. Gates run in order at admission: mute, then block
(addressed triggers only; task wakeups have no member initiator). A blocked member's own message
is rejected before a run, transcript or model call exists, so it can never start a run. A blocked
member may still speak during someone else's run and their line then enters that run as marked
context; the marker and instruction above are a safeguard, accepted as sufficient because
incidental replies to them are unlikely. Repeated context between overlapping runs relies on the provider prefix cache,
so the stable prefix (instructions, persona, tools) is byte-identical across runs.

Loop (a single function; no wrap-up states, no fuel object):

```text
loop {
    view = project(transcript)                  // same tools, same prefix
    response = llm.respond(view, tools)         // typed error -> RunEnd::ModelError
    transcript.append(response)
    if response.tool_calls.is_empty() { end(Completed) }
    results = executor.run(response.tool_calls) // rules below
    transcript.append(results)                  // always one result per call
    if results.ended_run { end(Delivered) }     // see send_message
    if turn >= MAX_TURNS or elapsed > deadline { end(StepLimit | Deadline) }
    transcript.append(archive.arrivals_since(cursor))  // new lines, echoes, dice results
}
```

Savings versus Python:
- `finish_reply` is deleted. A turn with no tool calls ends the run. Assistant text is never
  delivered; only `send_message` delivers.
- `send_message` has `end_turn` (default true). Sending then costs no extra model round in the
  common case. The model sets `end_turn=false` only to keep working after a send.
- No refusal rounds. Executor ordering is deterministic and never rejects a batch: read tools
  run concurrently, then write/send tools run sequentially in call order. A violated limit
  yields `Outcome::Refused` on that call only, with a machine-readable reason.
- No forced wrap-up or finish rounds. On step or message limit the run ends with a recorded
  `RunEnd`; a run that never sent is a visible, counted outcome, not a hidden extra model call.
- Retries live in the provider adapter only: bounded, on transport errors and 429/5xx, never
  re-sending after a stream that already produced billed output unless the adapter can prove it
  is safe. Token estimates for failed attempts are not invented.
- One local loop bound: `max_turns`, a runaway guard. Call counts, output sizes and context
  size are not capped locally (see section 4.13).

Tools:

```rust
trait Tool: Send + Sync {
    const NAME: &'static str;
    type Args: DeserializeOwned + JsonSchema;     // schema generated from the type
    fn effect(&self) -> Effect;                   // Read | Write | Send
    async fn call(&self, cx: &ToolCx, args: Self::Args) -> Result<ToolOutput, ToolError>;
}
```

The JSON schema comes from the argument type, so schema, parser and validation cannot drift;
the typed `ToolError` maps to `Outcome::Error(kind)` with a localized, model-facing message.
Tool descriptions are English templates in the repo. Tool set per group is chosen once at run
start and fixed.

### 4.5 Scheduler (qbot-sched)

One scheduler, one concept: a durable `timer` row that fires a typed payload at a time.

```text
timer(id, due_at, kind, payload, state, attempts, lease, created_by, chain_id, depth)
kind: AgentWake{group, intent} | Extract{group} | Embed{group} | Decay{group} | Backup | Report | Cleanup
state: Pending -> Claimed{lease_until, token} -> Succeeded | Failed{reason} | Cancelled | Interrupted
```

- Delivery class is a property of the kind: `AtMostOnce` (AgentWake): one claim, never replayed,
  startup marks stale claims `Interrupted`. `Idempotent` (the rest): lease with fencing token,
  heartbeat, bounded exponential retry, dead after N.
- Recurring work is a `schedule` row (cron expression) that materializes the next `timer`
  idempotently; the nightly pipeline is a chain of timers with dependencies, not a polled wait.
  This removes APScheduler and the 15 s poll: one loop sleeps until `min(due_at)` and is woken
  by LISTEN/NOTIFY.
- `AgentWake` is simply another trigger into the `RunSupervisor`; it starts an independent run
  with the same loop and the mute gate (no member block gate, no initiator). Its intent enters the
  run as an `Instruction` item flagged lower-trust.
- Limits (min delay, horizon, pending per group, chain depth) live in one `TaskLimits` const
  and are enforced once, at creation, with a typed refusal the model/owner sees. The silent
  "claimed then marked done as daily_limit" path is deleted; a per-group daily execution cap, if
  kept, is also enforced at creation time. Decision for milestone 4: the per-group daily execution
  cap is dropped; chain depth, pending count and the minimum delay already bound execution.
- Update and cancel apply to Pending only and return the typed current state.
- Single-instance exclusivity: Postgres advisory-lock session lease, loss triggers shutdown.
  Retained as is.

### 4.6 Persistence (qbot-store)

Postgres 17 + pgvector via sqlx, compile-time checked queries, one repository module per
aggregate with the SQL for its invariants (confidence/decay rules stated once, in Rust types,
applied through typed parameters, not duplicated in SQL). Forward-only `sqlx` migrations from a
clean new baseline. The schema keeps no compatibility with the Python one; old chat history may
be imported once by replaying it through the gateway, and everything else is rebuilt or discarded
(decision 10).

Kept: archive gate, partial unique indexes for "one current fact", extraction events consumed
once, advisory-lock topology changes, 2048-dim exact vector scan, group isolation (every query
takes `GroupId`). New: `run`, `run_item`, `request_log`, `timer`/`schedule`, `usage_event`.
Dropped: `cost_ledger.cny`, retired-schema checks, the pending-job-twin merge logic,
`scheduled_task` as a separate table.

### 4.7 Usage and observability (qbot-metrics)

Informational only. `usage_event(ts, group, run, turn, kind, model, input_tokens,
cached_input_tokens, output_tokens, reasoning_tokens, latency_ms, status, tool)` appended per
model call, embedding, search, ASR, tool call and timer execution. Views give per-day, per-group,
per-model and per-account rollups, cache hit rate, tool call counts and latency percentiles.
`tracing` spans carry run/turn/call ids; counters are exportable. `/stats`, `/top` (ranked by
calls or tokens) and the daily report read these. Nothing reads them to refuse work.

### 4.8 Localization (qbot-i18n)

- Typed message keys generated at build time from `locales/en.ftl` (Project Fluent): a missing
  key is a compile error; placeholders are typed.
- Catalogs live in `rust/locales/<tag>.ftl`; `en` is the source of truth and `zh-CN` is
  committed alongside it. Startup validates each locale against `en`: missing keys and
  mismatched placeholders fail startup.
- Three text classes: member-facing (command replies, report) uses the catalog; model-facing
  instructions are English templates (askama/minijinja, typed context structs) plus a
  "reply language" parameter; developer logs and errors are English constants.
- Markers shown to the model are ASCII tokens (for example `[img:12]`, `[member:3]`,
  `[voice]`); the stripper and parsers key on these tokens. The QQ face table is numeric ids;
  display names come from the catalog.

### 4.9 Configuration

Typed serde structs with `deny_unknown_fields`, one `Config` loaded and validated at startup,
passed explicitly (no globals). TOML files plus env secrets. Only real choices are keys; the
budget section, task chain depth and pending counts, and other constants become code constants.
Persona and group knowledge stay deployment files.

### 4.10 Gateway, delivery, media, memory

- `qbot-gateway`: axum WebSocket server, `OneBot` frames parsed into a closed `InboundEvent`
  enum; unknown or malformed frames return typed errors, are counted and dropped at one place.
  Delivery serializes sends per group, waits for the echo with a timeout, and retries once without
  the reply segment on ActionFailed (a real protocol need).
- `qbot-media`: one bounded single-flight cache type and one sliding-window type shared by
  picture and voice paths. ASR is the main Rust risk (section 9).
- `qbot-memory`: identity and memory services merged with their repositories (no thin service
  layer). Extraction keeps the reserve -> stage -> apply state machine unchanged.

### 4.11 Provider contract (qbot-llm)

One trait, `Provider`, with `respond` and `stream`. The runtime never branches on vendor.

- **Input is the complete logical conversation**, lowered from the transcript view by
  `Conversation::lower` through a `Renderer` (model-facing wording stays in the prompt layer).
  The request also carries tools, `ToolChoice`, sampling params and an optional `Continuation`.
- **Continuation is opaque and uniform.** A response returns one; the caller passes it back with
  the next, longer conversation. A stateful provider uses it to send only the new tail
  (`plan_replay` verifies the covered prefix is byte-identical, else it replays in full, so
  compaction of old items is always safe). A stateless provider ignores the handle and
  replays the whole conversation, rebuilt deterministically. The caller cannot tell which.
- **Capabilities are typed.** Required features (streaming, tool calls, parallel calls,
  continuation, developer role) are `Realization::{Native, Emulated}`; optional ones are
  `Option`/`bool` (image input, cache metrics, forced tool choice, temperature). Unsupported
  requests fail with `LlmError::Unsupported`, never a silent downgrade.
- **Reasoning.** Parts stay normalized (`Reasoning`, `Text`, `Call`) in order. Each turn also
  keeps the provider's raw output (`NativeReplay`), echoed verbatim to the same provider (needed
  for reasoning/message pairing) and ignored by any other, which rebuilds from parts.
- **Usage and cache** are `Usage { input, output, cache: Reported{hit} | NotReported,
  reasoning, reported }`; absence is typed, not zero-filled.
- **Errors** are one taxonomy (`Auth`, `RateLimited`, `Unavailable`, `Timeout`, `ContextTooLong`,
  `ContentFiltered`, `StreamInterrupted`, `Protocol`, `Unsupported`, ...). Retries live in the
  adapter: bounded, transient failures only, before any body arrives, honoring Retry-After.
- **Streaming** deltas are previews; the final `Completed(Response)` is authoritative and last.
- **Adapters:** `ResponsesProvider` (Responses wire format; `DeepSeek` dialect: no developer role,
  its own effort strings, stateless; `Standard` dialect: stateless or server-state). `FakeProvider`
  implements the whole contract offline (validation, prefix-cache simulation, optional server
  state, recorded requests) and drives milestone 4. `SimServer` simulates a Responses server for
  adapter tests. One conformance suite runs against all five variants.
- The old 2 MiB / 40,000-node request-size cap is removed outright, with no replacement.

### 4.13 Limits policy

Existing limits are things to justify, not requirements. A value that is policy or varies by
deployment is configuration (see 4.15b); a value that is internal tuning stays a constant. A limit stays only for a concrete
product, safety, resource-isolation or correctness reason; anything the provider already rejects
with a clear typed error is left to that error.

Kept, with the reason:
- Transcript pairing and call-id uniqueness: our own invariant, enforced the same on every provider.
- Image input only where the adapter can carry it: a capability, not a size.
- `max_turns` (20): stops a runaway tool loop that would otherwise run for the whole deadline.
- Run deadline, supervisor capacity (active plus waiting) and model concurrency: reply relevance
  expires, floods must not fan out into unbounded model calls, providers rate-limit.
- Task minimum delay (5 min), chain depth (24), pending tasks per group (50): stop the model from
  scheduling itself in a loop or filling the table.
- Sends per run (a tool-level product rule against spam), at most one reply segment, and
  exclusive dice/rps segments (QQ protocol semantics).
- Connection timeout on the HTTP client: a hung connect is invisible to the provider.

Removed: the local output-token ceiling check, unknown-tool and forced-choice pre-checks, the
per-turn and per-run tool-call caps, the tool-result byte cap (a tool bounds its own output when
its content needs it), the task horizon cap, the task intent length cap, the 2 MiB request cap,
and the segment-count and at-count caps on outbound messages. A provider `ContextTooLong` error
is propagated as typed. How much history a run sees is a batch-count policy (`[history]`, 4.19),
not a token budget, and a provider's context limit is not that policy: a model that accepts
hundreds of thousands of tokens is still shown only the configured tiers. `ContextTooLong` is the
exceptional fallback: the run then summarizes the episodes inside its verbatim tier as well,
once, and retries.

### 4.14 Milestone 4 as built

- `qbot-agent`: the run loop (`run.rs`), the typed `Tool` trait with schema generated from the
  argument type, the executor (reads concurrent, then writes and sends in call order; every call
  gets its own result), the `Supervisor` (gates before a run exists, one capacity shared by
  addressed runs and timers, a concurrency permit, one deadline that queue time counts against,
  `Reservation` for timers) and the ports in `env.rs`. `sim.rs` is a simulated group chat.
- `qbot-sched`: one timer model (`Wake` tasks at-most-once, `Job`s idempotent with lease and
  backoff), `TaskService` (the single place task limits live), the tick loop (reserve capacity,
  then claim; waits for a notification rather than polling when work is blocked), startup recovery.
- `qbot-tools`: `send_message`, the five task tools, `search_history`. `send_message` validates
  only references that must exist (member numbers, message ids) and protocol shape. It does not
  police blocked members: a mention or quote of one can be innocent, so blocking stays an
  admission rule with the prompt marker as the in-generation safeguard.
- Dropped from the Python design: the per-group daily execution cap, `finish_reply`, forced
  wrap-up rounds, every local size cap listed in 4.13.
- Tool-facing text is plain English in the tools for now; it moves to the catalog with the
  renderer in the localization milestone.

### 4.15 Milestone 2 (store) as built

- `qbot-store` adapts the ports the runtime defines to Postgres: `PgArchive` (archive, stable
  member numbers), `PgGroupPolicy` (mute, block with expiry), `PgRunLog` (run record, transcript
  load, crash recovery), `PgUsageSink` (batched writer), `PgTimerStore`, `RuntimeLease`. It
  depends on the crates that define those ports, not the other way round.
- Schema v2 is `migrations/0001_init.sql`, forward-only, with CHECK constraints for every
  closed set and cross-column rule. No Python-schema compatibility; the one-time history import
of decision 10 writes through the normal archive path.
- The archive gate is `UNIQUE (group_id, message_id)` with `ON CONFLICT DO NOTHING`: a duplicate
  stores nothing and consumes no member number. Member numbers are assigned densely under a
  per-group advisory lock, so concurrent first appearances cannot collide or leave gaps.
- A line's blocked marker is its author's *current* standing (block list with expiry), computed
  at read time; blocking never removes or rewrites lines.
- Run ids are allocated by the database in `RunLog::begin`, called at admission, so ids are
  unique across restarts and a run that never starts is still recorded and finished.
- `run_item` is append-only with dense `seq`; loading rebuilds the transcript through the same
  invariants and rejects gaps. Startup recovery (only under the lease) closes open runs by
  interrupting unresolved calls and ending them `interrupted`, keeping an already-recorded end.
- Timers: claims use `FOR UPDATE SKIP LOCKED`; the per-group pending bound is checked under an
  advisory lock in the insert transaction. `qbot_sched::conformance` is the single contract both
  the in-memory and Postgres stores must pass.
- Usage rows store NULL, not zero, for unreported cache usage; `usage_model_daily` and
  `usage_tool_daily` are plain views. Usage writes never block or fail a run.
- Database tests need a disposable server in `QBOT_TEST_DATABASE_URL` (database name must start
  with `qbot_test`); each test creates and drops its own database. They fail loudly when the
  variable is missing, unless `QBOT_SKIP_DB_TESTS=1`.

### 4.15b Memory and configuration as built

- Memory (identity, fixed batch-aligned episodes, recall, compaction alignment) is specified in
  [memory.md](memory.md). The store gained migration `0002_memory.sql` (pgvector, a range
  exclusion constraint on episodes) and the archive gained a dense per-group ordinal.
- Configuration is a typed schema loaded in layers (baked-in `defaults.toml`, then
  `/etc/qbot/config.toml`, `conf.d/*.toml`, `QBOT__SECTION__KEY` environment variables), validated
  at startup with every problem reported together; credentials are named by config keys and read
  from `NAME_FILE`, `NAME` or `/run/secrets/<name>` only. Policy that earlier appeared as
  constants (supervisor capacity and deadline, task guards, scheduler tuning, search limits,
  retry and timeout values, history and memory sizes, identity thresholds) is now configuration;
  a test pins each default to the consuming crate's own default so they cannot drift. The Docker
  layout and precedence are documented in `rust/deploy/README.md`. Internal tuning (evidence
  weights, name length bound, usage batch size) stays in code on purpose.

### 4.16 Gateway, prompts, commands and the composition root as built

- **`qbot-gateway`** speaks OneBot v11 over NapCat's reverse WebSocket (axum). Frames parse into
  typed events (`wire`); a message becomes archive text with ASCII markers (`render`):
  `[at:N]`, `[at:bot]`, `[reply:ID]`, `[image]`, `[face:N]`, `[dice:N]`, `[notice:..]`. Member-typed
  ASCII brackets are replaced by their fullwidth forms, so a marker cannot be forged. The archive
  gives every mentioned account a member number and rewrites `[at:ACCOUNT]` to `[at:N]`.
  Notices (recall, join/leave, mute, poke) are archived as marker lines under a synthetic message
  id (>= 2^52, derived from content so redelivery deduplicates).
- The **pipeline** archives first (the only dedup gate), then: bot's own line -> publish to the echo
  board; exact lower-case command word -> run the command on its own task; otherwise trigger
  (an @, a nickname as a whole jieba token, or a quote of a bot line, looked up in the archive)
  -> `Supervisor::submit`. Blocked members and muted groups are refused by the supervisor, not here.
- **Delivery** sends `send_group_msg`, correlates the response by `echo` token, then waits for the
  platform's report of the bot's own message (`message_sent`), which is how dice results are
  observed. A rejected send is returned to the model as an error; there is no silent retry without
  the quote segment. A lost connection fails pending calls immediately.
- **`qbot-prompt`**: wording lives in `rust/prompts/*.md` (English, rendered with minijinja in strict
  mode; the engine reports the slots each template references and a test holds them equal to the
  declared set); persona and group background are TOML files in `paths.personas_dir`
  (`default.toml`, `group_<id>.toml` replacing it whole). Instructions are identical across runs of
  a group (cache-friendly); what varies (time, trigger, task intent) is a separate trigger note.
  The window comes from `PgArchive::window`: whole batches ending at the batch being filled, so the
  start moves one batch at a time. Times are shown in `bot.timezone`.
- **`qbot-i18n`**: member-facing text is a typed `Msg` rendered through Fluent catalogs
  (`fluent-bundle`: `locales/en.ftl`, `zh-CN.ftl`, or `<locales_dir>/<tag>.ftl`), so plural forms and
  other language-dependent wording live in the catalog. At startup each catalog is checked against
  the schema: syntax errors, missing or unknown messages, and any message that cannot render with
  the arguments the code supplies (an invented variable, say) are all reported together.
- **`qbot-commands`**: the command set of 4.22. Replies quote the command and mention the sender,
  split at `gateway.max_message_chars - 2`. Owners come from `bot.owners`.
- **`qbot run`** (`qbot-app`): config and secrets, locale and personas, database and migrations,
  the single-instance lease, recovery of open runs, providers, tools, supervisor, scheduler,
  commands, pipeline, server; graceful shutdown in dependency order. The end-to-end test runs this
  against real Postgres, a mock Responses server over HTTP and a simulated NapCat.

Differences from the Python commands, each deliberate:

- `/alias confidence` is gone: confidence is derived from evidence (a name added by hand is
  confirmed), so a command that sets it would contradict the model.
- Money is gone from `/stats` and `/top`: they report runs, calls and tokens; `/top` ranks by
  replies started this month.
- `/block add --linked` (the flag that includes a person's linked accounts, `--all` in Python) blocks the accounts linked to the person at that moment; accounts linked
  later are not blocked (blocks are per account).
- Task ids are integers, not UUIDs; there is no 500-character cap on task text.
- Member names in replies are the current group display name, from the platform on demand
  (`get_group_member_info`: the group nickname, else the account nickname); when the platform cannot say,
  the reply says "member N" (their number in this group), and "account N" only for an account
  with no record in the group.

Known gaps: media markers (`[image]`, `[voice]`) carry no content until the media milestone;
background jobs have no scheduler producers yet, so the job runner fails any job it is given
rather than pretending to run it; the window shows only members who spoke, so `at` can only name
them (a mentioned but silent member has a number in the text but is not addressable yet).

### 4.16b Library choices

Established crates are preferred to hand-written parsers: Fluent for catalogs, minijinja for prompt
templates, humantime for command durations, hound for WAV, eventsource-stream for SSE, jiff for
time zones and RFC 3339, jieba-rs for nickname matching, sherpa-onnx for recognition, and Figment
for configuration layering (defaults, files, drop-ins, `QBOT__` environment). The configuration
loader is deliberately thin: the typed schema (`deny_unknown_fields`) plus a validation pass that
reports every semantic problem together; secrets stay outside it (a `*_secret` key names a secret
read from the environment or a mounted file). Figment reports the first schema-level error it
finds, and types environment values by its own parsing rules rather than by the default they
override; both were accepted so that the configuration surface can grow on a standard foundation.
A dedicated configuration review (what is configurable at all, grouping, naming, defaults) is
planned for after the rewrite is functionally complete. The one bespoke piece left is the marker
scan in `qbot-core::marker` (domain syntax no library knows).

### 4.17 Media as built

- A message with pictures or clips is archived at once with bare markers (`[image]`, `[voice]`);
  `qbot-media` then fills them in place (`[image:a red bicycle ...]`, `[voice:see you at eight]`,
  `[voice:unclear]`) by rewriting the stored line under a row lock. The k-th marker of a kind is
  addressed by position, counting filled markers too, so slots survive out-of-order completion.
  Bytes exist only in memory; what is kept is text plus a cache (`media_cache`, migration 0003)
  keyed by platform file id and by content hash, so the same picture is never described twice.
- Pictures: cache by id, size cap, per-group per-minute window, then a single-flight fetch (message
  link, then a fresh link from `get_image`, optionally a file in the shared QQ data directory),
  content-hash cache, vision model. A declined or unreadable picture is held for a while rather than
  retried on every repost. The vision model is the standard Responses dialect through the same
  provider layer (`providers.vision`, off by default).
- Voice: always `get_record` with a WAV conversion (the stored file and the CDN link hold native
  SILK audio that a recogniser answers with silence), inline base64 first, then a shared file, then
  a link; transcribed in the process by `qbot-asr` (SenseVoice via sherpa-onnx on CPU, `workers`
  recognitions at once on blocking threads, model loaded and checked at startup).
- A reply triggered by a message with media waits up to `media.wait_secs` for it, off the reader
  task, then goes ahead with what is there. Capacity and rate windows bound the work; beyond them
  media simply stays a bare marker.
- Stickers (marketplace `mface`) are described like pictures and cached under the sticker id
  (`p:sticker:<id>`), fetched from the sticker CDN first. QQ faces stay `[face:N]`.
- Forwarded records delivered inline render as `[forward:N]` with indented `  | name: text` lines,
  at most `media.forward_max_lines` (then `[forward_more:K]`). Mentions inside render as
  `@someone`, replies are dropped, nested forwards are counted only. Pictures inside a shown node
  take marker positions like top-level ones but are filled only from the cache (no model call
  for forwarded content); nested voices are never transcribed.
- `open_images {pictures: [{message, position, sticker}]}` lets the model look at an archived
  picture itself. The archive records each media item's file id and link (`media_ref`, migration
  0003); bytes are fetched again on demand and kept in a small in-process cache, so nothing
  binary is stored. The tool is registered only when the text provider takes images.
- DeepSeek image input: images are uploaded once through its Files API (multipart,
  `purpose=user_data`, 30-day expiry) and referenced by `file_id`; the id is cached in process
  for slightly less than its expiry. OpenAI-style providers take `image_url` data URLs.
- Cached descriptions expire after `maintenance.description_ttl_days` in the nightly decay stage.
- Not done: `open_images` for pictures in forwarded records whose content was not delivered
  inline; live verification of the DeepSeek Files upload.

### 4.18 Operations as built

- **Recurring schedules** (`qbot-sched::Recurring`): cron expressions (`croner`, with a time zone)
  turn each occurrence into a timer job through one atomic store step that advances a per-schedule
  `recurrence` row and inserts the job, so an occurrence is never fired twice across restarts. A new
  schedule starts from now; a missed one fires once, for its latest occurrence, on startup.
- **Job leases are renewed** while a job runs (`TimerStore::extend_lease`, a heartbeat at a third of
  the lease). Without it a job longer than its lease would have been claimed a second time.
- **`qbot-ops`**: `Operations` is the `JobRunner`. `Nightly` runs extraction (all groups, errors
  collected per group), decay (`expire_candidates`, description expiry), backup, cleanup; every stage
  runs, and the job fails if any did. Backups shell out to `pg_dump`/`pg_restore` (a mature tool
  beats a bespoke dumper): password via environment, deadline with kill-on-drop, a partial file that
  is verified by listing (it must contain `public chat_line`) before an atomic rename, rotation after
  success, tools checked at startup. The report is a typed `Msg` set, so it follows the locale.
- Not done: NapCat media cache cleanup (NapCat's own setting is the right place); episode expiry
  (episodes are immutable summaries the compaction path depends on). Fact decay runs in the
  nightly decay stage (4.19).

### 4.19 Facts, group knowledge and compaction as built

- **Findings.** The extraction call that writes an episode also returns names, facts about
  members and group knowledge (terms, the group topic), each tied to a line and a verbatim quote.
  They are validated in code: the quote must be in that line of the target part, the subject must
  be the line's author or be mentioned in it, bot lines and notices are never sources, a predicate
  must be in the table (`rust/prompts/predicates.toml`: kind, cardinality, decay class, optional
  opposite). Invalid findings are dropped and logged; they never fail the episode. Findings are
  stored on the episode (migration 0002), so nothing found is lost between extraction and
  applying it.
- **Consolidation.** Episodes are applied in order (`consolidated_ms`), right after insert and on
  startup for any left over. Names become extracted identity evidence; facts and knowledge become
  observations in `fact` / `fact_evidence` (migration 0004). A single-valued predicate supersedes
  its previous value; a multi-valued one keys on the normalized object; an opposite predicate
  (likes / dislikes) supersedes the other. One active fact per key is enforced by a partial
  unique index, with a per-group advisory lock around each observation.
- **Confidence and decay.** Confidence is the Wilson lower bound of the supporting episodes times
  `0.5^(age / half-life)`, with the half-life chosen by the predicate's decay class
  (`memory.facts.half_life_days`); a fact below `memory.facts.forget_below` is expired by the
  nightly decay stage. Computed on read, so it is deterministic for a given time.
- **Where facts reach the model.** Member facts only through the `lookup_member` tool (names
  and facts of all linked accounts, with confidence and age); group knowledge (topic first, then
  terms in key order) as an instruction after the persona, so it is stable between runs.
  Members see their own facts with `/who` and remove them with `/forget`.
- **Display names.** A member's current group display name (their group nickname, else their
  account nickname, as QQ shows it in the group) is authoritative and read live from NapCat when needed; it is never
  stored as name evidence (memory.md 4). Stored names are only those set by hand or learned from
  chat.
- **History tiers.** A run's view of the chat is set in whole batches counted back from the
  batch being filled (`history.batch_lines`, `history.raw_batches`, `history.summary_batches`):
  - the newest `raw_batches` verbatim;
  - the `summary_batches` before them through the summaries of the episodes that end there (an
    episode beginning earlier is still shown whole; lines no episode covers yet, because
    extraction is behind or failed, stay verbatim, so nothing is silently lost);
  - nothing older: that chat is reached only through `recall_episodes`, `read_episode` and
    `search_history`.
  The boundaries move one batch at a time, so the prompt changes only when a batch fills.
- **How summaries enter a run.** The prompt layer loads the summary and raw tiers and decides
  with `compose` which lines each episode stands in for; it never covers the trigger message or
  anything after it. The run gives each such range its own chat item and, for summary-tier
  episodes, appends a `Summary` over it at once: the originals stay in the transcript, the model
  sees the summary. Episodes inside the verbatim tier are kept as the `ContextTooLong` fallback
  only (applied once, then the call is retried).
- **Episodes keep up with the tiers.** Every filled batch queues a durable `extract` job for its
  group (and the scheduler is woken), so a slice's episode is normally written within a batch of
  its completion, before its lines leave the verbatim tier. Extraction runs one at a time, so a
  filled batch and the nightly run never write the same slice twice; the nightly run extracts
  whatever was missed.

### 4.20 Web search as built

- `qbot-llm::search` is the capability contract (`WebSearch`: a query in, normalized
  `SearchHit {title, url, content, published}` out, plus credits and request count); Tavily
  (`POST /search`, depth and result count from `[providers.search]`) is the only adapter, and the
  only place the vendor is named. It shares the HTTP transport and retry rules of the model
  adapters; a search-only HTTP proxy is optional.
- The `web_search` tool (`qbot-tools`) calls it directly rather than a model's built-in search:
  results are an ordinary tool result in the transcript, billed once, and marked as outside text
  the model must not take instructions from. Registered only when search is enabled.
- Errors are typed: a wrong key is `Auth`, a used-up plan (Tavily 432/433) is the new
  non-retried `LlmError::QuotaExhausted`, which the tool reports to the model as unavailable
  search. The Python version's local monthly quota counter is not carried over: it duplicated
  the provider's own refusal (4.13).
- `read_url {url, question?}` reads one page through the same provider (`PageReader`, Tavily
  `/extract`), as Markdown so headings, lists and tables survive. With a question only the most
  relevant passages come back (`providers.search.chunks_per_source`); without one, the whole page,
  cut at `tools.read_url.max_chars` with a note saying how much was left out. Only absolute
  http(s) URLs without credentials are accepted. A page the provider cannot read, or one with no
  text, is an ordinary tool answer; only provider failures (key, plan, network) are tool errors.
  Whole pages include site chrome (navigation, image links), which is why the question form is
  the better default for the model.

### 4.21 History search queries as built

- `search_history {query, speaker?, limit?}` takes the Python version's query language: words
  separated by spaces must all occur (`AND` may be written), `OR` between parts (looser than
  AND), `-word` or `NOT word` excludes, parentheses group, `"quoted words"` are one phrase.
  Operators are recognized only in capitals, so ordinary words never turn into operators.
  `speaker` limits the search to one member number.
- Parsing uses `winnow` 1.0 (stable; `chumsky`'s current line is still a pre-release). A malformed
  query is an `InvalidArguments` tool error that says what was expected and at which character,
  and a query that only excludes is refused, since it would match nearly everything.
- The parsed tree (`qbot_agent::TextQuery`, part of the archive port) is compiled into nested SQL
  in `qbot-store` from fixed fragments and numbered placeholders only; every term and the
  speaker are bind parameters. Each term matches as a case-insensitive substring with
  `position(lower($n) in lower(text))`: not `LIKE` (so `%` and `_` are literal), and not
  PostgreSQL full-text search, whose default parser does not split Chinese into words. The
  search is a scan of the group's lines; a `pg_trgm` index is the step if archives grow large.

### 4.22 Commands, notes and the NapCat cache as built (phase A)

- **Command names say what a command is about**, and kinds of information that are kept apart
  get separate commands, so no command acts on two stores:

  | Command | Who | Acts on |
  | --- | --- | --- |
  | `/who [--linked] [@m]` | member | everything on record, in separately numbered sections: names, notes written by people, facts learned from chat |
  | `/note` `add` `edit N` `remove N` `clear` | member | notes only (the `note` table, migration 0005) |
  | `/name` `add` `remove` | member | names (was `/alias`) |
  | `/forget N`, `/forget group N` | member / owner | learned memory only: a member's fact, or group knowledge |
  | `/group` | member | what the bot learned about the group (was Python's `/card`) |
  | `/link`, `/unlink` | member | linking; owner forms `/link @a @b` and `/unlink @a` replace `/merge` and `/split` |
  | `/stats`, `/top`, `/help` | member | unchanged |
  | `/tasks`, `/members`, `/block`, `/mute` | owner | unchanged |
  | `/runs [N]` | owner | a group's latest runs, or one run's steps (calls, results, sent text); replaces `/debug` |
  | `/logs [n]` | owner | recent warnings and errors from an in-process buffer (`runtime.log_buffer_lines`); replaces `/log` |

- **Notes** (decision 6) attach to one account in one group and are shown across linked accounts
  with `--linked`; Python's separate "shared person" note scope is gone. Members manage notes about
  their own (linked) accounts, owners about anyone, the same rule as every profile command. The
  author and times are kept; text is stored exactly as typed. `commands.notes_per_account` bounds
  how much one member's notes add to a lookup. `/forget` never touches notes, `/note` never
  touches facts, and a test pins both directions.
- **Tools** were renamed for the same reason: `recall_episodes` (pairs with `read_episode`;
  was `recall_events`), and `list_tasks`, `get_task`, `update_task`, `cancel_task` (were
  `*_scheduled_task`). `lookup_member` lists notes ("their words, not checked") and learned facts
  ("inferred automatically", with confidence) as separate sections.
- **Contact cards** (decision 7): `send_message` takes `{"type": "contact", "member": N}`, sent
  alone as NapCat's `contact` segment for a QQ account. N must be a member seen in this chat, so
  a card can never point outside the group; there is no contact card for a group.
- **NapCat's file cache**: NapCat has no retention setting (checked against its configuration and
  source); its supported `clean_cache` action wipes its temp directory and its picture, voice,
  video, file and log caches. The nightly cleanup calls it over the OneBot connection
  (`maintenance.napcat_clean_cache`), best effort: a failure is logged and retried the next
  night instead of failing the whole pipeline. QBot no longer reads NapCat's files at all:
  `media.napcat_data_dir` and the data-directory mount are gone, and picture and voice bytes come
  inline from `get_image` / `get_record`, which requires NapCat's own `enableLocalFile2Url`
  setting (rust/deploy/README.md). Production currently has it off, so it must be switched on
  before the Rust bot handles voice.

### 4.23 Reply reliability, evaluation and the configuration pass (step 1)

- **Evaluation harness** (`qbot-eval`, decision 9): scenarios in `rust/eval/scenarios/*.toml`
  (English description and rubric, chat in any language, trigger, optional notes, facts, group
  knowledge, canned web results) run through the production prompt layer, tool set and run loop
  against the real text model, over a simulated group. Deterministic checks (sent or silent,
  tools required or forbidden, members not to mention, normal ending) plus an English LLM judge
  (`eval/judge.md`) per rubric item; `--repeat N` because model output varies. Reports go to
  `rust/eval/results/` (git-ignored). Run by hand; it spends the text model's account.
- **What it found and what changed.** The first baseline passed 6 of 27 runs:
  - Half the runs died on invalid JSON: the model wrote the chat's own markers (`[reply:102]`)
    into the old segment-array `send_message` arguments. `send_message` now takes
    `{text, end_turn?}` with the same markers the chat uses (`[reply:ID]` first, `[at:N]`,
    `[face:N]`, `[dice]` / `[rps]` / `[contact:N]` alone), parsed strictly with an explanation
    for every mistake; other bracketed text stays text.
  - The model often wrote its finished reply as plain text, which is never delivered. A turn
    that ends with text and no tool call now gets one note (`prompts/undelivered_note.md`) and
    one more turn that must call a tool; `stay_silent` is the explicit way to say nothing. Narration
    is therefore never delivered and a written reply is never lost.
  - Task intents may name people by member number, which is stable in a group (the old advice
    against it came from Python's positional numbers).
  After these, 25 of 27 runs passed. After the wording and extraction changes below, the full
  run (`--repeat 3`, 2026-10-05) passed 27 of 27, every check and rubric item.
- **Forced tool choice is a per-adapter capability** (`ForcedToolChoice`), measured on the real
  API rather than assumed. DeepSeek's Responses API (2026-10-05, deepseek-flash and
  deepseek-v4-pro, every reasoning effort, `required` and named) refuses a forced tool choice
  with thinking on, and refuses a thinking turn after a turn made without it. So the forced turn
  runs without reasoning and the rest of that run stays without it. An ignored live test pins
  both behaviors. OpenAI-style Responses providers declare `Always` (not verified live: no key).
- **Errors**: HTTP 402 (no balance) is `QuotaExhausted`, not retried.
- **Model-facing wording is data.** Long instructions are templates in `rust/prompts/*.md`;
  every short text a model reads (tool and parameter descriptions, tool results and error
  explanations, outcome notes, recap and knowledge lines, the extraction's headers and
  correction messages) is in `rust/prompts/wording.toml`, read through `qbot-wording`. Its
  `Text` enum fixes each key's slots, and tests keep the file and the code in step and the texts
  free of CJK. Parameter descriptions are attached to the generated schema by field path
  (`qbot_llm::schema::tool_schema`), so argument types carry no prose. Markers the code writes
  and parses (`[summary of earlier conversation]`, `[msg:ID]`, ...) stay in code. Log-only texts
  stay in code.
- **Extraction prompt** (`rust/prompts/extract.md`, method `slice-v3`), rewritten for the system
  as built: members as `member:N`, `[bot]` lines as context that is never a subject or a source,
  picture descriptions and transcripts as content (transcripts may be wrong), forwarded records
  as not said in the group, chat text as material rather than instructions, title and summary
  in the configured language, fact objects in the quote's own words. The predicate table moved
  beside it (`rust/prompts/predicates.toml`). Rejected findings are now explained to the model
  once, like a rejected episode, and the corrected answer replaces them; an episode that was
  valid is never lost to the correction round. A term must be written in the target part, and
  its quote shows what it means (the defining line often does not repeat it). The ignored live
  test passes repeatedly on the first attempt.
- **Configuration pass**: removed settings nothing read (`providers.text.context_window_tokens`,
  `providers.text.max_output_tokens`, `providers.search.kind`) and the informational `Limits`
  type; `providers.vision` defaults to DeepSeek. Every remaining setting has a reader.

### 4.24 Chat-history import and the migration rehearsal (step 2)

- **What production holds** (inspected read-only, 2026-10-05): `raw_event.payload` is not the
  raw OneBot frame but the Python bot's normalized record of it. It keeps the platform's
  `{type, data}` segments as received (only NUL removed), the sender, `message_id`, `reply_to`,
  `to_me` and `self_id`; the time is the `occurred_at` column. About 35,000 messages in 11 groups
  from 2026-08-10, no missing days, including the bot's own sent messages and inline forwarded
  records (NapCat sends their content). That is enough to rebuild the frames.
- **What the Python adapter took out** (NoneBot onebot adapter 2.4.6, read from its source): a
  quote segment moves into `reply_to` (with an @ of the quoted author right after it); a leading
  (up to two) or trailing @ of the bot is removed and sets `to_me`, as does quoting the bot. From
  2026-09-18 the Python bot put the @ back before archiving; before that it is missing. Notices
  were stored only as the Python bot's own text.
- **The importer** (`qbot import-history FILE [--dry-run]`, `qbot-app/src/import.rs`) reads an
  export made by `rust/deploy/export_python_history.sql` (read-only, one JSON object per line),
  rebuilds each message's OneBot frame and runs it through the gateway's own parser and the same
  `archive_form` the live pipeline uses, then the archive's normal append (whose message-id dedup
  makes reruns no-ops). It restores the quote from `reply_to`; restores a leading `[at:bot]` only
  where the export proves one was stripped (`to_me`, no @bot, and the quoted message, if any, is a
  member's); leaves the 16 undecidable cases (quoting a message not in the export) as stored;
  fills picture markers with the Python bot's descriptions by file hash (they cannot be rebuilt:
  nothing is re-fetched); skips notices. The whole export is checked before anything is written;
  a group that already has lines not in the export is refused (older history would land after
  them); the bot's database lease is taken, so the import cannot run beside the bot. The quoted
  author's @ is not restored: nothing records whether it was there.
- **Rehearsal** against a full copy (2026-10-05): 34,883 messages (5,664 by the bot) in 11 groups,
  148 notices skipped, 7,561 quotes and 2,552 bot mentions restored, 2,943 of 3,734 pictures
  described, in about two minutes; a rerun writes nothing; ordinals are dense and in time order,
  every mention resolved to a member number. The nightly extraction on one imported group (450
  lines) produced its 5 episodes, facts and a name in Chinese on the first attempt each.
- **Defects the rehearsal found and fixed:**
  - Extraction on real chat failed every slice: the model's reasoning used the whole 2048-token
    output limit, the call was cut off, and the model was told it "did not call" the tool.
    `memory.extraction.max_output_tokens` is now 8192. A cut-off answer, or one whose
    arguments are not JSON, is lost rather than wrong, so the same request is sent again within
    `max_attempts`. The full rebuild showed that both happen now and then. When every attempt
    is cut off, the error names the setting.
  - The recurring loop could skip a nightly run: a sleep that ended milliseconds before the
    occurrence fired nothing, and the next reading, just past it, waited for the next day. The
    occurrence slept toward is now fired regardless.
  - The scheduler could strand a timer that came due between its two clock readings (it looked
    due but blocked and waited for a notification). One reading now serves both.
  - Scheduled work stopped silently on a store error until restart, and failed job attempts were
    not logged. Both now log and retry.
- **Findings not acted on** (none blocks the cutover):
  - NapCat sends marketplace stickers as `image` segments with `emoji_id` (61 in production), not
    `mface`; the live gateway renders and describes them as pictures, which works.
  - A contact card the bot sent echoes back as `[unsupported:contact]` (5 in production).
  - Extraction and description model calls are not recorded in `usage_event` (only reply runs
    are).
  - Python's one active per-account block is not imported; re-apply it with `/block` after the
    cutover. No group was muted.
  - The full rebuild after the cutover is about 390 extraction calls (one per 90-line slice) on
    the shared DeepSeek account, a few hours of nightly work.

### 4.25 Recency in long-term memory retrieval

When more memory matches than can be offered, newer matches come first, because among similarly
relevant memories the newer one more likely reflects how things are now. Relevance still decides
what is a candidate. Every order is total, so it never depends on insertion or scan order.

- `recall_episodes`: the store returns every episode within `memory.recall.max_distance` (below
  1, so every candidate is similar). `qbot_memory::rank` orders them by one score,
  `similarity * 0.5^(age / half_life)`, which is `(1 - cosine distance) * exp(-ln 2 / half_life *
  age)`. Age is counted from the episode's last line, and `memory.recall.half_life_days`
  defaults to 180; 0 means similarity alone. Then the list is cut to `limit`. There is no age
  cutoff: an old episode weighs less, never nothing. With the default, 0.7 similarity from ten
  days ago beats 0.8 from a year ago, but 0.98 from three months ago beats 0.56 from today.
  Equal scores go to the more recent episode, then the higher id.
- Group knowledge in the instructions: the topic, plus at most `memory.knowledge.max_terms`
  terms (default 40). The terms kept are the most recently confirmed. They are listed in key
  order, so confirming a term that is already shown does not change the cached prompt prefix.
  Before this, every active term entered every prompt, which a long-lived group outgrows.
- Member facts (`lookup_member`) are not cut. Within a predicate they are listed most recently
  confirmed first, then the newest fact (the stores and the tool's merge across linked accounts
  agree).
- Unchanged: `search_history` already returns the newest matches first. Names are ordered by
  confidence. Notes, at most `notes_per_account` of them, are all shown in the order they were
  written, which the note commands' numbering follows.

### 4.12 Follow-up work (recorded, not blocking)

- Live verification against OpenAI-style endpoints (DeepSeek, DashScope and Tavily are verified
  by the ignored live tests in `qbot-llm`, `qbot-memory` and `qbot-tools`).
- Outbound `contact_member` segments in `send_message` (no `contact_group`: decision 7).
- Arrivals folded into a run between turns are deliberately not capped (decision 8); revisit only
  if real usage shows a problem.

## 5. Preserve / redesign / simplify / remove

Preserve: R2 archive gate; echo as the only source of bot lines; exact vs holder scope and
revision-checked link confirmation; alias evidence rules and the 0.75 line; fact uniqueness;
extraction state machine; decay-without-delete; group isolation; trigger/mute/block semantics;
owner = `bot.owners`; output stripping; process lease; vector width and exact scan.

Redesign: per-run conversation model, agent loop, tool result typing, scheduler, usage accounting,
localization, module boundaries, member and line numbering, roster delivery.

Simplify: limits as constants; single deadline and single timeout owner; one cache and one
rate-window implementation; one validation point per limit; merge thin services into repos.

Remove: budget system and spend commands, `finish_reply`, wrap-up/forced-finish rounds,
mid-run tool swaps, APScheduler and the poller, legacy-schema guards, dormant evidence types
(explicit-at, self-claim, reply-context) and the placeholder `importance`, compatibility
aliases, dead defensive branches, CJK literals in implementation code (they move to locale catalogs).

## 6. Behavior changes

1. `/top` ranks by calls or tokens, not spend. `/stats` drops cost lines.
2. Task daily-execution cap, if kept, rejects at creation instead of silently consuming a task.
3. Positional `#N` replaced by stable message references.
4. A run starts from the window cut at its trigger time and takes in newly archived lines between
   turns, including echoes of its own sends and platform results such as dice and
   rock-paper-scissors. `send_message` waits for the echo before the next step, as today.
5. Group knowledge and roster delivered as a window-local member list plus a lookup tool.
6. Blocked members stay fully visible as context, marked as blocked, with an instruction not to
   reply to them. A block's only hard effect is that their own message cannot start a run. This
   is fixed behavior, not a policy option.

## 7. Fallbacks that stay

Send retry without the reply segment; bounded transport retries on 429/5xx/network; process
lease and job fencing; strict parsing of untrusted OneBot input (reject and count, no repair);
log-and-continue only for non-critical post-effect persistence (evidence memo, dice result)
with a metric. Everything else is a typed `Result` ending the run with a recorded `RunEnd`.

## 8. Language separation rule

CJK text does not appear in implementation code, comments, logs or identifiers. It lives only in
locale catalogs (`rust/locales/`), persona and prompt resource files, and test fixtures. This is
a plain convention followed while writing the code; there is no lint tool for it. Tests that
need CJK input in code build it from `\u` escapes.

## 9. Risks

- ASR: start with in-process sherpa-onnx Rust bindings; fall back to a small sidecar process
  speaking a local socket if the bindings prove insufficient.
- `search_history` boolean queries: resolved with a winnow parser (4.21).
- Word segmentation for nickname matching needs a Rust jieba port; its dictionary is crate data.
- Provider Responses/DeepSeek reasoning-replay details must be captured in recorded fixtures
  before the adapter is trusted.
- Docker is not available in the current WSL environment, so database-backed tests cannot run
  until it is installed.

## 10. Milestones

M0 workspace in `rust/`, CI, error and id conventions.
M1 `core` + `context`: transcript, invariants, projection, compaction, property tests (no IO).
M2 `store`: new schema, run persistence, resume, usage events (done; see 4.15).
M3 `llm`: Provider trait, Responses/DeepSeek adapter, replay fixtures, fake provider.
M4 `agent` + `tools` + `sched`: loop, executor, group actor, tasks/timers, tested with a fake
   model; end-to-end run in-memory and on Postgres.
M5 `gateway` + `commands` + `i18n` + `prompt` + `qbot run`: OneBot, delivery, echo, member commands,
   catalogs, prompts, composition root (done; see 4.16).
M6 `memory`: identity, extraction, consolidation, decay, retrieval.
M7 `media`: pictures, voice (done; see 4.17).
M8 operations (nightly pipeline, backup, report, recurring schedules; done, see 4.18), cutover: move `rust/` to the repo root, delete Python. What remains before and including the cutover is listed in section 12.

## 11. Decisions (from the project owner)

1. Every eligible trigger starts its own independent run; runs of one group may overlap. A run
   starts from the window at trigger time and takes in new group lines between turns, which
   includes echoes and platform results needed to observe dice and rock-paper-scissors. A blocked
   member's message is archived and shown as marked context, but never starts a run; the block is
   enforced at admission, and a blocked member speaking during another run is an accepted risk.
2. New database schema. Old data: chat history only, if it imports cleanly (decision 10).
3. Rust workspace in `rust/` inside this repository; Python stays until cutover, then Rust moves
   to the root and Python is deleted.
4. Speech recognition: in-process sherpa-onnx bindings first, sidecar as fallback.
5. CJK is not banned repository-wide; it is kept out of implementation code and confined to
   locale catalogs, resources and fixtures (section 8).
6. Manual notes and extracted memory stay separate in storage, commands, help text and what the
   model is told. `/note` writes and deletes notes people wrote by hand; `/forget` removes facts
   and knowledge that extraction produced. Each deletes only from its own store: forgetting an
   extracted fact never removes a note, and a note never becomes an extracted fact (extraction
   reads chat lines, not notes). The model sees notes labelled as written by members and facts
   labelled as learned from chat, with their confidence.
7. No `contact_group`. The model knows only the group it is in and cannot send or reveal another
   group's contact card. `contact_member` (a member's card) is kept.
8. No cap on chat lines folded into a run between turns for now; revisit only if real usage shows
   a practical problem.
9. Reply-quality evaluation is written in English: harness, rubrics, expected-behavior
   descriptions and evaluation prompts. Test conversations may be in Chinese.
10. Migration carries chat history only. First establish whether the Python archive imports
    cleanly into the Rust archive with the information that matters (who said what, when, in
    which group, the bot's own lines, message ids for replies, media as markers). If it does,
    the imported history is the source of truth and every derived structure (episodes, facts,
    group knowledge, embeddings, identity evidence) is rebuilt from it by the Rust code; old
    derived data is not carried over. If even the chat history needs awkward compatibility work,
    the old database is discarded and the Rust bot starts fresh; losing that data is acceptable.
11. Prompts are rewritten near the end for the system as built (its tools, memory model, history
    tiers and run semantics), not ported from the Python wording.
12. No separate QQ account or test group. The Rust bot replaces the production image directly
    after offline verification, a migration rehearsal on a copy of production data and a prepared
    rollback; the first real NapCat and QQ checks happen during the controlled cutover, which is
    rolled back at once if a critical path fails (section 12).

## 12. Remaining work, in order

There is no separate QQ account or test group (decision 12): the Rust bot replaces the production
image directly. So everything that can be checked without real QQ traffic is checked first, the
deployment is prepared and rehearsed beside production without the live NapCat connection, and
the first real NapCat and QQ verification is the controlled cutover itself, with an immediate
rollback if a critical path fails.

### 1. Finish everything that needs no production traffic

- Phase A (features and commands): done, see 4.22.
- Reply-quality evaluation harness (decision 9): done, see 4.23. More scenarios as gaps show.
- Configuration pass: done, see 4.23.
- Prompt rewrite (decision 11): done, see 4.23 (reply prompts, wording out of code, extraction
  prompt), checked with the evaluation harness and the live extraction test.
- First CI run: done (2026-10-05, commit 9784e36: fmt, clippy and the full test suite with the
  database tests).
- Optional: WAV pad-byte handling for odd-sized chunks.

### 2. Investigate and prepare the migration (decision 10)

Done (4.24): the history imports cleanly, so there is no fresh start. Items 1 to 3 below are
complete; the importer and its rehearsal on a production copy are in place.

1. Inspect the production database read-only: whether `raw_event.payload` holds the original
   OneBot event for every group message, including the bot's own sent messages, and how media,
   replies and forwards appear in it.
2. If the chat history imports cleanly: build the importer, which replays the stored events
   oldest first through the Rust gateway's own parser and renderer into the Rust archive (fresh
   member numbers in order of first appearance, media as bare markers, nothing fetched, no
   replies, no runs), and test it against a copy of the production database.
3. Derived data (episodes, facts, group knowledge, embeddings) is rebuilt from the imported
   archive by the normal extraction; old episodes and facts are not carried over.
4. If the history cannot be imported without awkward compatibility code, choose a fresh start
   explicitly and record it here.

### 3. Prepare the production deployment beside the current one

In progress (2026-10-05): `/opt/docker/qbot-rust` on the production host is the compose project
`qbot-rust` (renamed from `qbot`, which is the Python bot's project on the same host), with its
own Postgres and the image loaded from a local build (`qbot-rust:latest`). The production
history is imported and `qbot rebuild-memory` built memory from it ahead of the cutover; the
cutover import adds only what arrived since. Not connected to NapCat. Still to do: persona,
voice model, vision and search settings, backups, and NapCat's network path to the Rust bot.

Without connecting it to the live NapCat account:

- the final Docker image;
- its own compose project and Postgres volume (`/opt/docker/qbot-rs` or similar), Rust config,
  personas converted to the Rust format, secret files, ASR model, search proxy
  (`providers.search.proxy`), backup paths and operational settings;
- `qbot check-config` and every startup check passing;
- rollback assets: the Python image (`qbot-bot:latest`, with `qbot-bot:rollback`), its compose
  project, database and `.env` untouched, so rolling back is stopping the Rust bot, pointing
  NapCat back and starting the Python container; and whether the Python bot tolerates NapCat's
  `enableLocalFile2Url = true`, or the setting must be reverted on rollback.

### 4. Rehearse the cutover without production message traffic

- Import a copy of the production data into the Rust database (or initialize it fresh).
- Start the Rust stack without the live NapCat connection, and verify: migrations, the memory
  rebuild from the imported archive (batch extraction, embeddings, consolidation), scheduled jobs
  and the nightly pipeline, a verified backup and a restore from it, startup and graceful
  shutdown, recovery after an unclean stop (lease, open runs, interrupted tasks).

### 5. Controlled production cutover

1. Back up the production database and its configuration.
2. Keep the Python image and container available as the rollback target.
3. Stop the Python bot.
4. Apply NapCat settings: `enableLocalFile2Url = true` (rust/deploy/README.md), and the reverse
   WebSocket URL and access token of the Rust bot.
5. Import into, or initialize, the Rust database.
6. Start the Rust bot and verify the critical paths on the real account at once: trigger and
   reply, echo handling, mentions, picture fetching, voice (`get_record`), stickers and forwards,
   scheduled tasks, web search and page reading, reconnect after a NapCat restart.

### 6. Roll back at once if a critical path fails

Stop the Rust bot, restore NapCat's previous settings, start the Python container; debug from
logs and the Rust database afterwards, not in production.

### 7. Repository cutover, once the Rust bot is stable in production

Move `rust/` to the repository root, delete the Python implementation, update CI, CLAUDE.md, the
deploy script and the documentation, and remove the obsolete production configuration and old
secret layout.

### Optional and accepted

- Optional: `open_images` for forwarded pictures without inline content; trimming site chrome
  from whole-page `read_url`; `pg_trgm` index for history search on large archives; admin
  subcommands (`backup-now`, `restore`); OpenAI-style providers verified live when a key exists;
  refresh of old episodes when the extraction method changes (not needed if migration rebuilds
  everything with the current method).
- Accepted: no spending enforcement (section 2); no task horizon cap and no local search quota
  (4.13); no cap on arrivals between turns (decision 8); fixed batch-aligned episodes and
  unbounded episode retention (memory.md 6, 9); history search by scanning (4.21).
