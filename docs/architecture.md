# QBot architecture

QBot is an AI member of QQ group chats. NapCat (OneBot v11) connects to it over a reverse
WebSocket; it archives every group message, answers when it is addressed or when one of its own
scheduled tasks comes due, remembers what the group talks about, and keeps out of the way
otherwise. This document describes how it works and why. Memory (identity,
episodes, recall) has its own document, [memory.md](memory.md).

## 1. What the product does

R1. Join QQ groups through NapCat (OneBot v11, reverse WebSocket, the bot is the server).
Receive group messages, the bot's own echoed messages and notices (recall, join, leave, mute,
poke). Send group messages: text, mentions, a quote, QQ faces, dice, rock-paper-scissors, and a
member's contact card. The bot knows only the group it is in and never sends or reveals another
group's card.
R2. Archive every event exactly once. A unique-key insert is the only dedup gate; a duplicate
stops all further effects. The bot's own lines come only from the platform's echo.
R3. Speak only when caused: an @ of the bot, a nickname as a whole word, a quote of one of the
bot's lines, or a due group task. A muted group gets nothing; a blocked member cannot start a
reply but stays in context.
R4. Reply through tools: the model decides whether to speak, whom to mention and what to quote.
R5. Tools: send, stay silent, archive search, episode recall and reading, member lookup, web
search and page reading, opening pictures, and the group's scheduled tasks.
R6. Group-scoped memory learned from chat: episodes with embeddings, names, facts about members
and group knowledge, each grounded in verbatim quotes, with confidence that fades. Notes written
by hand are a separate store with their own commands.
R7. Identity: accounts belong to people; linking and splitting with revision-checked
confirmation; stable member numbers per group.
R8. Media: pictures described once by content and archived as text, originals opened on demand;
voice transcribed on the CPU in the process.
R9. Commands without model calls for members and owners (section 12).
R10. Group tasks: durable one-shot timers owned by the group, bounded, never run twice, waking
the agent with fresh context.
R11. Operations: nightly extraction, embedding, fact decay, verified backups, cleanup, a daily
report to the owners, and single-instance exclusivity.
R12. Observability: calls, tokens, cache hits and latency are counted. Nothing refuses work on
cost.
R13. Localization: member-facing text comes from message catalogs.

## 2. Principles

- **One loop per run, one canonical log.** A run is "call the model; run the tools it asked for;
  append everything; repeat". Everything the model saw is an append-only, typed transcript, from
  which every request is derived and which is stored as it grows.
- **Vendors at the edge.** Only `qbot-llm` names model providers, only `qbot-gateway` speaks
  OneBot, only `qbot-store` speaks SQL. The runtime never branches on a vendor; differences are
  typed capabilities.
- **Invariants in types and in one mutation path.** Platform ids are newtypes parsed at the
  boundary; closed sets are enums; the transcript has a single append function that enforces
  call/result pairing; the schema repeats every closed set and cross-column rule as a CHECK.
- **No silent fallback.** Errors are typed and end a run with a recorded reason. A retry happens
  only where the failure is transient and the retry cannot duplicate an effect.
- **Every limit has a reason** (section 14). Policy a deployment may choose is configuration;
  everything else is a documented constant in the crate it governs (section 13).
- **Model-facing wording is data.** Instructions are templates in `prompts/`; short texts are in
  `prompts/wording.toml`; member-facing text is in `locales/`.
- **No CJK in Rust code or comments.** It lives only in catalogs, prompts, personas and fixtures;
  tests build CJK input from `\u` escapes.

## 3. Workspace

A Cargo workspace. Arrows point from dependent to dependency; there are no cycles.

```text
qbot-app       the `qbot` binary: configuration, wiring, signals
qbot-commands  chat commands
qbot-gateway   OneBot server, rendering, the message pipeline, delivery and echoes
qbot-ops       nightly run, backups, report, NapCat cache
qbot-tools     the model's tools
qbot-media     picture description and voice transcription service
qbot-asr       in-process speech recognition (sherpa-onnx, SenseVoice)
qbot-prompt    instructions, personas, history tiers, chat rendering
qbot-sched     timers, group tasks, recurring schedules, the scheduler loop
qbot-agent     supervisor, run loop, tool trait and executor, ports, a simulated group
qbot-memory    identity, episodes, extraction, consolidation, facts, recall, notes
qbot-store     Postgres adapters for every port, the schema, the lease
qbot-llm       provider contract, Responses/DeepSeek adapter, embeddings, web search, fakes
qbot-config    typed, layered configuration and secrets
qbot-i18n      member-facing message catalogs (Fluent)
qbot-wording   model-facing short texts (`prompts/wording.toml`)
qbot-context   the canonical transcript and its projection (pure)
qbot-core      ids, time, the batch grid, markers (pure)
qbot-eval      reply-quality evaluation against the real model (run by hand)
```

`qbot-core` and `qbot-context` do no IO. A port (a trait such as `Archive`, `Delivery`,
`EpisodeStore`, `TimerStore`) is defined by the crate that needs it and implemented by
`qbot-store` or `qbot-gateway`, so dependencies point at the domain, not at Postgres.

## 4. The message path

- **Parsing** (`qbot-gateway::wire`): frames become typed events (`GroupMessage`, `GroupNotice`,
  `ActionResponse`); anything unusable is `Frame::Ignored` with a reason.
- **Rendering** (`render`): a message becomes archive text with ASCII markers, one per segment
  (the table below). What members typed is kept as typed; only a marker-shaped token in it is
  escaped, so a marker cannot be forged (section 7, Trust). Forwarded records show at most `FORWARD_MAX_LINES` (30) of their messages: a
  record can hold hundreds, and every prompt showing the line would carry them all.
- **Segments, received and sent.** Receiving aims to understand everything NapCat (v4.18)
  reports in a group message; sending is a separate, deliberately small set. A marker the model
  can read is not therefore one it can write: a receive-only marker in `send_message` text is
  sent as the literal text it is. Rich cards arrive as one `json` segment holding QQ's "ark"
  document; `qbot-gateway::card` reads the few fields that say what the card is about.

  | NapCat segment (received) | Archived as | Receive | Send |
  | --- | --- | --- | --- |
  | `text`, `markdown` | the text | understood | text: supported |
  | `at` | `[at:N]`, `[at:bot]`, `[at:all]` | understood | `[at:N]`; `@all` refused |
  | `reply` (NapCat drops one it cannot resolve) | `[reply:ID]` | understood | `[reply:ID]`, first |
  | `face` | `[face:N]`, `[face:N:NAME]` (QQ's `faceText`) | understood | `[face:N]` |
  | `image` | `[image]`, then `[image:DESCRIPTION]` | understood (described; `open_images`) | unsupported |
  | `image` with `emoji_id`, `mface` (marketplace sticker) | `[sticker:NAME]`, then described | understood | unsupported |
  | `record` | `[voice]`, then `[voice:TRANSCRIPT]` | understood (transcribed) | unsupported |
  | `video` | `[video]` | marker only | unsupported |
  | `file`, `onlinefile` | `[file:NAME (SIZE)]`, `[folder:NAME]` | marker and metadata | unsupported |
  | `flashtransfer` | `[file transfer]` | marker only | unsupported |
  | `forward` (inline content) | `[forward:N]` and indented lines | understood (30 lines) | unsupported |
  | `dice`, `rps` | `[dice result:N]`, `[rps result:HAND]` | understood | `[dice]`, `[rps]`, no value |
  | `poke` (the poke face) | `[poke]` | marker only | unsupported |
  | `contact` (as the bot's own card echoes) | `[contact card]`, `[group card]` | marker only | `[contact:N]`, members seen speaking here |
  | `json` ark: contact, group | `[contact card:NAME]`, `[group card:NAME]` | marker and metadata | unsupported |
  | `json` ark: location | `[location:name=..; address=..]` | marker and metadata | unsupported |
  | `json` ark: music | `[music:title=..; artist=..; url=..]` | marker and metadata | unsupported |
  | `json` ark: news, link, mini-app; `share`, `miniapp` | `[link:title=..; text=..; source=..; url=..]` | marker and metadata | unsupported |
  | `json` ark, other; `xml` | `[card:SUMMARY]` (`prompt`, `brief`), else `[card]` | marker only | unsupported |
  | `location`, `music` (OneBot form) | as the ark cards | marker and metadata | unsupported |
  | anything else | `[unsupported:KIND]` | marker only | unsupported |

  NapCat reports group events (recalls, joins, leaves, mutes, pokes) as notices, never as
  segments; they are archived as `[notice:..]` lines. A card's fields are cut at 120 characters,
  and only an http(s) link of at most 400 characters is kept.
- **A received card is context, never a capability.** No account or group id a card carries is
  shown, numbered or stored: only mentions get member numbers, and `send_message` addresses only
  members the run's context names (below). A card for another
  group gives the bot no handle on that group.
- **Archiving** (`pipeline::archive_form`, `PgArchive::append_line`): one transaction per line
  assigns the line its dense per-group ordinal, gives every mentioned account a stable member
  number (rewriting `[at:ACCOUNT]` to `[at:N]`) and records each media item's platform
  reference. `UNIQUE (group_id, message_id)` with `ON CONFLICT DO NOTHING` is the only dedup gate.
  Notices are archived under a synthetic message id derived from their content (at and above
  2^52), so redelivery deduplicates them too.
- **Routing**: the bot's own line goes to the echo board; a recognised command word runs the
  command on its own task; otherwise a trigger (an @ of the bot, a nickname as a whole jieba
  token, or a quote of a bot line) submits a run. Mute and block are decided by the supervisor,
  not here. A filled batch queues the group's extraction job.
- **Delivery**: `send_group_msg`, correlated by `echo` token, then a wait for the platform's
  report of the bot's own message (`message_sent`), which is how dice and rock-paper-scissors
  results become known. A rejected send is returned to the model as an error.
- **Platform games.** A dice or rock-paper-scissors send is a request with no result
  (`{"type":"dice","data":{}}`); the platform decides the outcome and reports it in the echo.
  Rendering normalizes it on the incoming side only (`qbot_core::GameResult`): a face 1 to 6 as
  `[dice result:N]`, QQ's hand number (1 paper, 2 scissors, 3 rock) as `[rps result:HAND]`, and
  an unreadable one as the bare `[dice]` / `[rps]`. The send tool refuses any dice or
  rock-paper-scissors marker that carries a value, so the model can neither choose an outcome nor
  post text that imitates one; it reacts to the result in the tool result, with `end_turn: false`.

## 5. The conversation model (`qbot-context`)

One append-only transcript per run:

```rust
enum Item { Chat(ChatBatch), Assistant(AssistantTurn), ToolResult(ToolResult),
            Instruction(Instruction), Meta(Meta), Summary(Summary) }
struct ToolResult { call_id: CallId, outcome: Outcome, content: Vec<Part> }
enum Outcome { Ok, Error(ErrorKind), Refused(RefusalReason), Interrupted }
```

- A tool result appends only if its call is pending; each call gets exactly one result; while
  calls are pending only their results may be appended. The invariants are checked in the single
  append path, which loading a stored run replays too.
- Chat arrives as `Chat` items between turns: the window at trigger time first, then lines that
  arrived since, including echoes of the run's own sends. Lines carry stable message ids and
  member numbers; nothing is positional.
- Blocking never removes context: a blocked member's lines are shown as anyone's, the block is
  enforced at admission, and the instructions list who is blocked.
- Projection is pure: the visible view of the transcript, lowered by the provider adapter. A
  `Summary` stands in for a range of chat items (the summary history tier, and the
  `ContextTooLong` fallback); the originals stay in the transcript. An earlier turn's view never
  changes, so the provider's prefix cache keeps hitting.
- Runs are stored as `run` and `run_item` rows as they grow. Startup closes runs left open by a
  crashed process: pending calls get `Interrupted`, the run ends `interrupted`.

## 6. The agent runtime (`qbot-agent`)

- **Triggers.** Three kinds, each its own `Trigger` variant and trigger note:
  - `Addressed`: a member addressed the bot (an @, a nickname, a quote of a bot line). The
    member and their message are the run's initiator and focus.
  - `Wake`: a scheduled task came due, with its stored intent.
  - `Spontaneous`: the bot looked at the conversation on its own. A member's line that
    addresses no one rolls `replies.spontaneous_chance` (default 0, off). On a hit, a run starts
    after the group's media in flight is described. It has no initiator and no triggering
    message, and its note says so: nobody is waiting for an answer, and `stay_silent` is the
    usual outcome. A group has at most one spontaneous run at a time, so a lively chat cannot
    start several that talk over each other.
- **Supervisor.** Every trigger becomes its own run with its own deadline; runs of one group may
  overlap. Admission gates, in order: muted group, then a blocked member (the initiator of an
  addressed run; for a spontaneous run, the author of the line that rolled it, since a blocked
  member's message never starts a run of any kind), then capacity. One bounded queue serves all
  runs; `replies.concurrency` runs call the model at once; queue time counts against
  `replies.deadline_secs`.
- **Run loop.** Project, call the model, append, execute the calls, append the results, then the
  chat that arrived. A turn with no tool call ends the run, except as below. The run also ends on
  a tool that ends it (`send_message` by default, `stay_silent`), the deadline, `max_turns`, or a
  model error.
- **Speaking.** Only `send_message` reaches the group: `{text, end_turn?}` in the chat's own
  marker syntax (`[reply:ID]` first, `[at:N]`, `[face:N]`; `[dice]`, `[rps]`, `[contact:N]`
  alone, never a game result), parsed strictly with an explanation for every mistake. It waits for the echo, so the
  result shows the message as delivered. Sends per run are bounded by `replies.max_messages`.
  `stay_silent` makes silence explicit. `[at:N]` and `[contact:N]` may name only a member the
  run's context names (`OpenedContext::members`): someone who speaks in the visible chat, is
  mentioned in it, is named in a task's intent as `member:N`, or has an entry in the people
  block. So a reminder reaches someone who has been quiet, while the model cannot address a
  number nothing showed it.
- **Undelivered text.** A turn that ends with text but no tool call wrote something nobody saw.
  The run appends one note (`prompts/undelivered_note.md`) and asks for one more turn that must
  call a tool, so a written reply is never lost and narration is never delivered.
- **Forced tool choice is a provider capability** (`ForcedToolChoice`). DeepSeek's Responses API
  refuses a forced choice with reasoning on, and refuses a reasoning turn after one without it
  (measured on the live API, pinned by an ignored live test), so on DeepSeek the forced turn and
  the rest of that run go without reasoning. OpenAI-style providers declare `Always`.
- **Executor.** Reads run concurrently, then writes (sends included) one at a time in call order; a refused call gets
  a typed refusal; every call executes (no deduplication), so the prompt teaches the model not to
  repeat a write.
- **Tools** (`Tool` trait): the argument schema is generated from the Rust type (schemars, nested
  types inlined); descriptions of the tool and its parameters come from `wording.toml` by field
  path. The tool set is fixed for the whole run, so the cached prefix does not change.

## 7. The prompt (`qbot-prompt`, `qbot-wording`, `qbot-i18n`)

- **Instructions** are minijinja templates in `prompts/*.md` with a closed set of slots per
  template (a test holds them equal): the reply rules, the guide to reading chat markers, the
  persona block, the people block, the learned group knowledge, the three trigger notes, picture
  description, extraction.
- **Order is stable to volatile**, so runs of a group share the longest prefix the provider can
  cache:
  1. the reply rules, the legend, and the persona with its group background (fixed per
     deployment);
  2. the people block and learned knowledge, which change only when a command or an extraction
     changes a record;
  3. the chat, which grows by whole batches;
  4. last the trigger note (the time, the trigger, a task's intent, and the current group
     display names of the members in the chat and in the people block, read live through
     `with_directory`), which is different for every run.
- **People block** (`PgArchive::people`, one read model): everyone the bot keeps a record about,
  one entry per person, in member order. An entry has:
  - the person's numbered accounts here (two or more are linked accounts), and which are
    blocked now;
  - the names they go by, confirmed ones and leads from chat;
  - notes members wrote (each with its date);
  - facts learned from chat (each with its episode count and last confirmation date).

  Dates are absolute and nothing depends on the current hour, so the block is byte-identical
  until a record changes. Member-written text is escaped like chat and kept on its own line.
  Live display names are not part of it: they change apart from every record and sit in the
  trigger note. The model reads who someone is here, with no tool call; there is no lookup tool.
- **Personas** are TOML files: `default.toml`, or `group_<id>.toml` replacing it for one group.
- **History tiers.** A run sees whole batches counted back from the batch being filled: the newest
  `history.raw_batches` verbatim, the `history.summary_batches` before them as the summaries of
  the episodes that cover them (a batch no episode covers yet stays verbatim), nothing older; that
  is reached through `search_history` and `recall_episodes`. The tiers move a batch at a time.
- **Group knowledge** enters as an instruction: the group's topic, and at most
  `memory.group_terms` (40) terms, the most recently confirmed, listed in key order so that
  confirming a term again does not change the prompt.
- **Wording** (`qbot-wording`): every short model-facing text (tool and parameter descriptions,
  results, error explanations, outcome notes, extraction headers and corrections) is a `Text`
  variant with fixed slots, rendered from `wording.toml`; tests keep the file and the enum in
  step. Markers the code writes and parses stay in code.
- **Member-facing text** (`qbot-i18n`) is a typed `Msg` rendered through Fluent catalogs
  (`locales/en.ftl`, `zh-CN.ftl`, or `<config>/locales/<tag>.ftl`), checked against the schema at
  startup. The catalog also names the language the bot writes its memory and picture
  descriptions in (`writing-language`), so one setting, `bot.locale`, decides both.

- **Trust and provenance.** One stable rule in `reply_system.md` says the model's instructions
  are its rules, the reading guide and its persona, and that everything else is material, never
  instructions. Everything else carries its provenance in its structure rather than in repeated
  warnings:
  - The rules, reading guide, persona and operator group background are system messages. Notes
    for the model (the undelivered-text note) are developer messages.
  - Chat lines, episode summaries, the trigger note (names, a task's intent), the people block
    and learned group knowledge are user content (`InstructionRole::Trigger`,
    `InstructionRole::Reference`). The people block and knowledge are assembled by the system,
    but their names, notes and facts are what members wrote or said, so they get the chat's
    standing, never a system message's.
  - Web pages, search results and history come back as tool results under a one-line header
    naming their source; their text is passed on unchanged.
- **Escaping is narrow and tied to the protocol** (`qbot_core::marker`). `MARKERS` is the closed
  set of marker names the system writes into model-visible text (`[msg:`, `[at:`, `[image:`, the
  card kinds, `[notice:`, the summary heading). Outside text is changed only where it could
  collide with that protocol:
  - `escape_markers`, for text standing among markers (member text, forwarded messages and
    their sender names, display names, summaries and knowledge written from chat): an ASCII
    `[` opening a marker-shaped token (a registered name in any case or spacing, closed by `]`
    or opening a value with `:`) becomes fullwidth. `[at:1]` typed by a member shows as
    fullwidth-bracket `at:1]`, while `arr[0]`, `[1]`, `[sic]`, Markdown links, code and
    Chinese punctuation are untouched. Consecutive text segments are escaped as one run, so two
    pieces cannot join into a marker.
  - `marker_value`, for outside text inside a marker's value (picture descriptions,
    transcripts, card fields, file, face and sticker names): both brackets become fullwidth,
    because a value ends at the first `]`, and the value is put on one line.
  - NUL is dropped (the database rejects it). Nothing else is rewritten: no Markdown stripping,
    no punctuation changes.
  - Tool results are not escaped. Their place in the conversation already says what they are.
  - A test checks that every marker the renderer writes is registered; a new marker kind goes
    into `MARKERS` first.

## 8. Memory

Described in [memory.md](memory.md). In short: the archive is the source of truth; an episode
summarizes one fixed slice of whole batches and is never rewritten; extraction returns the
episode and its findings in one validated tool call; consolidation turns findings into names,
facts and group knowledge; recall ranks episodes by similarity decaying with age; embeddings are
a derived index the nightly run keeps complete for the configured embedding model.

## 9. Media (`qbot-media`, `qbot-asr`)

Three kinds of state are kept apart. The **archived line** is durable conversational state: a
picture's words are filled in once and are then part of the record, never revisited. The
**description cache** is a derived lookup that only saves model calls. **Transport state** (a
platform link, a provider's uploaded-file id, fetched bytes) is temporary and lives in memory or in
the `media_ref` row the platform reference came in.

- **Archiving.** A message with media is archived at once with bare markers, with each item's
  platform reference (`media_ref`: kind, position, file id or sticker id, link, size). The media
  service then fills the markers in place (`[image:a red bicycle]`, `[voice:see you at eight]`,
  `[voice:unclear]`) by rewriting the stored line. The k-th marker of a kind is addressed by
  position, counting filled markers, so slots survive out-of-order completion. Each message is
  worked on once, when it is first archived: the archive's dedup gate admits no second pass.
  Work is in memory only; media the service could not take (overload, the per-group rate, a
  failed call, a restart) stays a bare marker, and `open_images` still reaches it.
- **Pictures.** On arrival the bytes are always fetched (the message's link, then a fresh copy
  from `get_image`), within a size bound (untrusted input). The description is looked up by
  `fingerprint:sha256(bytes)`, where the fingerprint names the describer (provider, model and
  instructions, which carry the writing language). A platform id says where a picture is, not
  what it shows: the same picture under another id or in another group is recognised, a reused
  id with other pixels is described anew, and a changed model or locale writes new descriptions
  without touching the archived ones. Only a cache miss spends the group's rate
  (`media.images_per_minute`, governor) and one call to the vision model (`providers.vision`),
  single-flight per content key so concurrent reposts share the call. A source no route could
  read is held for ten minutes by its platform reference, a picture the model declined by its
  content key (a moka TTL cache); a failed call is not held. Cached descriptions are dropped after
  15 days: they serve reposts, which come within days, and must not grow with the archive.
- **Stickers** (marketplace) are fetched as their still PNG and described like pictures.
- **Forwarded records.** Pictures inside one are fetched and filled only when their content was
  described before: no model call for content not posted in the group. They are worked on after
  the message's own media and hold no reply up (each must be downloaded just to find out). Clips
  inside one are not transcribed.
- **Voice**: always `get_record` with a WAV conversion (the stored file holds SILK audio a
  recognizer answers with silence), transcribed by `qbot-asr` (SenseVoice through sherpa-onnx on
  the CPU, model loaded and checked at startup); `media.clips_per_minute` per group. Transcripts
  are not cached: a clip cannot be reposted as such.
- **`open_images`** lets the model look at an archived picture itself, addressed by message and
  position as the chat shows it. The bytes are fetched again from the platform through the stored
  reference and kept in a 64 MiB in-memory cache for the runs in flight; this works while QQ still
  serves the picture (NapCat's own file cache is cleaned nightly). It is offered only when the
  text model takes images. OpenAI-style providers take data URLs. DeepSeek takes images through
  its Files API: the adapter keys uploads by `sha256(bytes)` (a request's image keys name pictures
  only within its own media store), uploads each picture once per process for its 30-day lifetime,
  and shares one upload between concurrent requests; the cache belongs to one provider instance,
  so file ids never cross accounts or providers. The `media_cache` table holds only
  `fingerprint:sha256` keys.
- **Waiting.** A reply waits up to `media.reply_wait_secs` (25) for every picture and clip posted in its group that is
  still being worked on, not only its trigger's (the picture a question is about is often the message before
  it, or the quoted one), then goes ahead with what there is.

## 10. Scheduling (`qbot-sched`)

- One `timer` table. A `wake` is a group task: at most once, never replayed (a claim open at
  startup is marked interrupted). A `job` (extract, nightly, decay, backup, cleanup, report) is
  idempotent: claimed with a lease renewed while it runs, retried with backoff, failed for good
  after five attempts, each failure logged.
- **Group tasks** are created by the model or an owner, owned by the group, and bounded where a
  model could otherwise loop: at least five minutes ahead, at most 50 pending per group, follow-up
  chains at most 24 deep. Update and cancel apply to pending tasks only.
- **Recurring schedules** (`maintenance.nightly_cron`, `report_cron`, croner with the bot's time
  zone) turn each occurrence into a job through one atomic store step, so an occurrence fires once
  however often the process restarts; one missed while down fires once on startup. The loop fires
  the occurrence it slept toward even when the timer wakes a moment early.
- **The scheduler loop** takes one clock reading per tick for claiming and for deciding what comes
  due next, so a timer due between two readings is not mistaken for a blocked one. A store error
  is logged and retried; scheduled work never stops silently.

## 11. Providers (`qbot-llm`)

- One `Provider` trait (`respond`, `stream`). A request carries the complete logical conversation
  (lowered from the transcript view through a renderer), the tools, the tool choice, the
  reasoning effort and an optional continuation.
- **Continuation is opaque.** A stateful provider sends only the new tail after verifying the
  covered prefix is unchanged; a stateless one replays everything. DeepSeek is stateless; an
  OpenAI-style provider continues from its stored previous response.
- **Capabilities are typed** (`Realization::{Native, Emulated}`, forced tool choice, image input,
  cache metrics); an unsupported request fails with `LlmError::Unsupported`.
- **Reasoning items never leave the adapter**; a turn keeps the provider's raw output to echo back
  to the same provider.
- **The output length is the provider's.** No request carries a local output cap: the provider
  determines the usable range (DeepSeek returned a 44,000-token answer unprompted), and a local
  cap only adds a way for a long but valid answer to be cut off. A reply is bounded by its
  deadline; other calls by a per-call deadline (ten minutes for text, two for a picture).
- **Errors** are one taxonomy (`Auth`, `RateLimited`, `Unavailable`, `Timeout`, `ContextTooLong`,
  `QuotaExhausted`, `Protocol`, ...). Retries live in the adapter: bounded, transient failures
  only, before any body arrives, honoring Retry-After. A used-up plan (HTTP 402, Tavily 432/433)
  is `QuotaExhausted` and never retried.
- **Embeddings**: an OpenAI-compatible endpoint, split into requests of `max_batch` inputs,
  vectors normalized and checked for width.
- **Web search and page reading** (Tavily, the only search vendor): five results a search; a page
  read as Markdown, either the passages relevant to a question or the whole page cut at 8,000
  characters, marked as outside text.
- **Network**: every outbound client (the providers, web search, picture downloads) is built by
  `qbot_llm::net`, on a route the configuration decides: directly, or through the one proxy
  (`network.proxy`) for the services listed in `network.proxy_for`. Proxy variables in the
  environment are ignored, so nothing reroutes traffic behind the configuration's back.
- **Fakes**: `FakeProvider` implements the whole contract offline (validation, prefix-cache
  simulation, recorded requests); `SimServer` is a Responses server for adapter tests.

## 12. Commands (`qbot-commands`)

Commands run without a model call. Each acts on one kind of information, so no command touches
two stores.

| Command | Who | Acts on |
| --- | --- | --- |
| `/who [--linked] [@m]` | member | everything on record: names, notes written by people, facts learned from chat |
| `/note` `add` `edit N` `remove N` `clear` | member | notes only |
| `/name` `add` `remove` | member | names |
| `/forget N`, `/forget group N` | member / owner | learned memory only: a member's fact, group knowledge |
| `/group` | member | what the bot learned about the group |
| `/link`, `/unlink`; `/link @a @b`, `/unlink @a` | member; owner | linking accounts |
| `/stats`, `/top`, `/help` | member | runs, calls and tokens; replies started |
| `/tasks`, `/members`, `/block [--linked]`, `/mute` | owner | the group's tasks, roster, blocks, mute |
| `/runs [N]` | owner | the latest runs, or one run's steps |
| `/logs [n]` | owner | recent warnings and errors (the last 200) |

Replies quote the command, mention the sender, and are split at QQ's 2,000-character message
limit. Members act on their own (linked) accounts, owners on anyone. A member's name in a reply is
their current group display name, read live from the platform.

## 13. Configuration

Typed serde structs with `deny_unknown_fields`, loaded in layers by Figment: the baked-in
`defaults.toml`, `/etc/qbot/config.toml`, `conf.d/*.toml` in name order, then `QBOT__SECTION__KEY`
environment variables. Every semantic problem is reported together at startup. Credentials are
never configuration: a `*_secret` key names a secret read from `NAME_FILE`, `NAME` or
`/run/secrets/<name>`. `deploy/README.md` documents the layers and the Docker layout.

The schema holds only what a deployment may choose:

| Section | Settings |
| --- | --- |
| `bot` | account, owners, nicknames, timezone, locale (also the language the bot writes in) |
| `gateway` | listen address, access token secret |
| `database` | host, port, name, user, ssl mode, password secret |
| `replies` | concurrency, deadline, messages per reply, the chance of a spontaneous run |
| `history` | batch lines, raw and summary batches |
| `memory` | slice batches, group terms shown; recall's max distance and half-life; fact half-lives per decay class |
| `media` | voice transcription on or off; pictures and clips per minute per group; how long a reply waits for them |
| `network` | an outbound proxy and the services that use it |
| `providers.*` | text (kind, endpoint, model, reasoning of replies and of extraction), vision, embedding (model, width, batch), search (depths) |
| `maintenance` | nightly and report schedules, backups kept, client tools directory, record retention, NapCat cache cleanup |

Settings that depend on each other are checked together at startup:
- `memory.slice_batches` is at most `history.raw_batches`, so an episode exists before its chat
  leaves the verbatim tier;
- `media.reply_wait_secs` is less than `replies.deadline_secs`, since the wait counts against
  the deadline;
- `replies.spontaneous_chance` is from 0 to 1.

Derived rather than configured:
- the reply queue: ten waiting runs per model slot of `replies.concurrency`;
- the extraction's context: one batch on either side of a slice, within the verbatim tier;
- extraction cadence: every filled batch queues its group's extraction, which extracts each
  completed slice, and the nightly run catches up;
- how long an unused name lead lives: one default fact half-life
  (`memory.facts.half_life_days.default`), since a lead from chat fades like a fact;
- record retention: one `maintenance.records_keep_days` for finished runs and finished
  scheduled tasks, the two operational records;
- the provider's state mode (from its kind), the writing language (from the locale), and the
  directory layout (fixed under the configuration and data directories).

Everything else is a constant in the crate it governs, the `Default` of that crate's settings
struct, because it is a safety bound, a resource bound on untrusted input, or algorithm tuning
nobody should need. Section 14 lists the bounds and why each exists.

## 14. Limits

A limit stays only for a concrete reason; anything a provider already rejects with a clear typed
error is left to that error. The local limits and why they exist:

| Limit | Value | Reason |
| --- | --- | --- |
| Reply deadline, queue, concurrency | configured; 10 per slot | a late reply is worthless; a flood must not fan out into unbounded model calls |
| `max_turns` | 20 | a model looping on fast tool calls would spend dozens of growing-context calls within the deadline |
| Messages per reply | configured | a product rule against flooding the group |
| Task minimum delay, pending, chain depth | 5 min, 50, 24 | stop the model scheduling itself in a loop or filling the table |
| Extraction attempts | 3 | bounds the calls on a slice the model cannot summarize; after them the slice is skipped, so it cannot stall the group |
| Media per minute per group | configured | resource isolation between groups |
| Media workers, queue, picture size, clip length | 4, 32, 8 MiB, 300 s | CPU and memory isolation; pictures and clips are untrusted input of any size |
| Opened-picture cache | 64 MiB | memory bound for bytes fetched on demand |
| Forwarded lines shown | 30 | an unbounded external input enters every later prompt |
| Card field, file or face name; card link | 120 characters; 400 or left out | external text that enters every later prompt; a cut link leads nowhere |
| `read_url` page | 8,000 characters | a web page is the one tool input with no size of its own |
| Search results, passages | 5, 3 | enough to answer from, little enough to read |
| Group terms in the prompt | configured (40) | the block enters every reply's instructions |
| Notes per account | 20 | bounds what one member's notes add to every reply's people block |
| Name length | 64 characters | a name is a name, not a paragraph |
| Per-call deadlines | 10 min text, 2 min picture, 30 s embedding, 20 s search | a hung connection must not stall the nightly run |
| Connect timeout | 10 s | a hung connect is invisible to the provider |

Not limited, deliberately: model output length, context size (history is a batch-count policy;
`ContextTooLong` is the fallback that summarizes the verbatim tier once), tool calls per turn,
tool result size beyond the inputs above, chat folded into a run between turns, task horizon, and
spend.

## 15. Persistence (`qbot-store`)

- Postgres 17 with pgvector, through sqlx; forward-only migrations in `migrations/` (`0001_schema.sql`
  is the base schema, later files change it), with CHECK constraints for every closed set and cross-column rule. Instants are bigint milliseconds;
  every group table carries `group_id` and every query filters by it.
- One adapter per port. The in-memory implementations in the domain crates define the semantics;
  shared conformance suites (`qbot_memory::conformance`, `qbot_sched::conformance`, ...) run
  against both.
- Member numbers are assigned densely under a per-group advisory lock and are immutable: a
  trigger rejects any update, delete or truncate of `member_number`. A group's slices cannot
  overlap, whether episodes or skipped (`slice_skip`): range exclusion constraints, and a check
  across the two tables under the group's slice lock. One active fact per subject, predicate and key (a partial
  unique index).
- **Single instance**: the process holds a Postgres advisory-lock session lease and shuts down if
  it loses it.
- Database tests need a disposable server in `QBOT_TEST_DATABASE_URL`; each test creates and
  drops its own database.

## 16. Operations (`qbot-ops`)

- **Nightly**: extraction (each group's episodes, after embedding any the current index lacks),
  decay (name candidates nothing supported for one default fact half-life, cached picture descriptions older than 15 days,
  faded facts), a verified backup, cleanup (finished tasks and finished runs after
  `maintenance.records_keep_days`, NapCat's file cache through its `clean_cache` action). Every stage
  runs. The job fails, and is retried, if decay, the backup or cleanup failed; a failed extraction
  is logged and left to the group's next filled batch or the next night, since a retry of the
  whole job would also take another backup and rotate an older one out.
- **Backups** shell out to `pg_dump`/`pg_restore`: password through the environment, a partial file
  verified by listing before an atomic rename, rotation after success, the tools checked at
  startup.
- **The daily report** to the owners is a set of typed messages, so it follows the locale.
- **Usage**: `usage_event` rows per model and tool call of a reply run, with daily views; `/stats`,
  `/top` and the report read them. Nothing reads them to refuse work.

## 17. Evaluation (`qbot-eval`)

Scenarios in `eval/scenarios/*.toml` (an English description and rubric, chat in any
language, a trigger, optional notes, facts, group knowledge and canned web results) run through
the production prompt layer, tools and run loop against the real text model over a simulated
group. Deterministic checks (sent or silent, tools required or forbidden, members not to mention,
a normal ending) are followed by an English LLM judge per rubric item; `--repeat N` because model
output varies. It is run by hand and spends the text model's account. A changed prompt is run
through it.

## 18. Decisions that shape the design

1. Every trigger starts its own run; runs of one group may overlap. A blocked member's message is
   archived and shown as marked context but never starts a run.
2. Manual notes and learned memory stay separate in storage, commands, help and what the model is
   told: `/note` acts on notes, `/forget` on learned memory, and neither path writes the other.
3. No contact card for a group; a member's card only for members seen in this group.
4. No cap on chat folded into a run between turns until real use shows a problem.
5. Evaluation is written in English; test conversations may be in Chinese.
6. Speech recognition runs in the process.
7. The archive is the source of truth. Derived memory survives a change of extraction model; only
   an incompatible embedding model or width calls for re-embedding, which the nightly run does.

## 19. Known gaps

- Videos are only a `[video]` marker: nothing describes them.
- A file's contents and a card's target page are never fetched by themselves; `read_url` reads a
  card's link only if the model asks.
- Extraction, picture-description and embedding calls are not recorded in `usage_event`, so
  `/stats` and the report count reply runs only.
- An uploaded picture stays at DeepSeek for the Files API's longest lifetime (30 days), though
  each upload is used only for one description or one run's turns; the Files API's shortest
  lifetime would do, once it is checked against the live API.
- Why an extraction answer needs a second attempt (about three in ten on real chat, mostly
  findings sent back for correction) is not logged per attempt.
- `open_images` cannot open pictures inside forwarded records whose content was not delivered.
- History search scans the group's lines; a `pg_trgm` index is the step if archives grow large.
- OpenAI-style providers are covered by the simulated server, not by a live test.
