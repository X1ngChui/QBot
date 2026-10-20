# QBot architecture

QBot is an AI member of QQ group chats. NapCat (OneBot v11) connects to it over a reverse
WebSocket; it archives every group message, answers when it is addressed or when one of its own
scheduled tasks comes due, may join in on its own now and then, remembers what the group talks
about, and keeps out of the way otherwise. This document describes how it works and why. Memory (identity,
episodes, recall) has its own document, [memory.md](memory.md).

## 1. What the product does

R1. Join QQ groups through NapCat (OneBot v11, reverse WebSocket, the bot is the server).
Receive group messages, the bot's own echoed messages and notices (recall, join, leave, mute,
poke). Send group messages: text, mentions, a quote, QQ faces, dice, rock-paper-scissors, and a
member's contact card. The bot knows only the group it is in and never sends or reveals another
group's card.
R2. Archive every event exactly once. A unique-key insert is the only dedup gate; a duplicate
stops all further effects. The bot's own lines come only from the platform's echo.
R3. Speak only when caused: an @ of the bot, one of its nicknames in a message, a quote of one of
the bot's lines, a due group task, or (at `replies.spontaneous_chance`) a look at the chat on its
own. A muted group gets nothing, commands included except a bot owner's and its delegated
admins' `/mute` and `/block`; a blocked member cannot start a reply or get a command answered,
but stays in context.
R4. Reply through tools: the model decides whether to speak, whom to mention and what to quote.
R5. Tools: send, stay silent, archive search, episode recall and reading, web search and page
reading, opening pictures, and the group's scheduled tasks.
R6. Group-scoped memory learned from chat: episodes with embeddings, names, facts about members
and group knowledge, each grounded in verbatim quotes, with confidence that fades. Notes written
by hand are a separate store with their own commands.
R7. Identity: accounts are linked in a group, with a confirmation that lapses if either side's
links there changed; stable member numbers per group.
R8. Media: pictures described once by content and archived as text, originals opened on demand;
voice transcribed on the CPU in the process.
R9. Commands without model calls for members and owners (section 12).
R10. Group tasks: durable timers owned by the group, bounded, waking the agent with fresh
context; one-shot, or repeating by a rule the scheduler keeps (never by the model), each
occurrence run at most once.
R11. Operations: nightly extraction, embedding, fact decay, verified backups, cleanup, a daily
report to the owners, and single-instance exclusivity.
R12. Observability: every provider call (model turns of replies, extraction and picture
description, embeddings, web searches and page reads) is counted with its requests, tokens, cache
hits, credits and latency, by the work it served; a run that fails logs why (`/logs`). Nothing
refuses work on cost.
R13. Localization: member-facing text comes from message catalogs.
R14. Groups are separate: nothing learned or stored in one group is shown in, used by or acted on
from another; the one exception is the owners' `/stats global` totals (section 8).

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
- **Every read of group data names its group.** A query, tool, command or prompt block that
  shows what a group said or what the bot keeps about it takes the group and filters by it
  first (section 8).

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
`EpisodeStore`, `TimerStore`) is defined by the crate that needs it, so the agent runtime, the
scheduler and memory depend on no database or platform. Its Postgres adapter lives in
`qbot-store` when the port belongs to one of those core crates; a port of an outer crate
(`qbot-prompt`, `qbot-gateway`, `qbot-media`, `qbot-commands`, `qbot-ops`) is implemented over
`qbot-store`'s types next to the port, in that crate. Platform adapters are in `qbot-gateway`.

## 4. The message path

- **Parsing** (`qbot-gateway::wire`): frames become typed events (`GroupMessage`, `GroupNotice`,
  `ActionResponse`); anything unusable is `Frame::Ignored` with a reason.
- **Rendering** (`render`): a message becomes archive text with ASCII markers, one per segment
  (the table below). What members typed is kept as typed; only a marker-shaped token in it is
  escaped, so a marker cannot be forged (section 7, Trust). Forwarded records show at most `FORWARD_MAX_LINES` (30) of their messages: a
  record can hold hundreds, and every prompt showing the line would carry them all.
- **Segments, received and sent.** Receiving aims to understand everything NapCat (v4.18)
  reports in a group message; sending is a separate, deliberately small set. A marker the model
  can read is not therefore one it can write: a receive-only marker in `send_messages` text is
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
- **A recall is honoured.** A recall notice names the message and its author; that author's
  line with that id (the newest, as for a quote; the bot's own lines too, which an admin may
  recall) keeps its ordinal, author and time, but its text becomes `[recalled]` and its media
  references are deleted, in one transaction (`PgArchive::recall`). History, `search_history`,
  later extraction and `open_images` then see nothing of it. Runs already under way keep what
  they were shown, run transcripts are not rewritten (they are the operational record, kept
  `maintenance.records_keep_days`), and memory already extracted from the line stays (`/forget`
  removes a fact). A picture description that finishes after the recall finds no marker to fill.
- **A received card is context, never a capability.** No account or group id a card carries is
  shown, numbered or stored: only mentions get member numbers, and `send_messages` addresses only
  members the run's context names (below). A card for another
  group gives the bot no handle on that group.
- **Archiving** (`pipeline::archive_form`, `PgArchive::append_line`): one transaction per line
  assigns the line its dense per-group ordinal, gives every mentioned account a stable member
  number (rewriting `[at:ACCOUNT]` to `[at:N]`) and records each media item's platform
  reference. An insert with `ON CONFLICT DO NOTHING` against the line's platform identity is
  the only dedup gate. A group message's identity is `real_seq`, the sequence QQ's server gives
  each message of a group (`UNIQUE (group_id, platform_seq)`): the same on every redelivery,
  never shared by two messages of a group. NapCat's `message_id` is a 31-bit hash of QQ's own
  id (measured: about half of consecutive ids in a group decrease): never ordered, and in a
  group of n lines two collide with a chance of about n²/2³² (about 17% at 28,000 lines), so
  two lines may share one. A line without a sequence is unique by its message id
  (`UNIQUE (group_id, message_id) WHERE platform_seq IS NULL`): notices, archived under a
  synthetic id derived from their content (at and above 2^52), and lines archived before
  sequences were kept. Nothing compares ids for order; order is the archive ordinal. A lookup
  by message id (a quote, a picture the model opens) takes the newest line with that id, and a
  filled media description addresses its line by ordinal. At the boundary of the change
  (migration 0008) a message archived just before it, without a sequence, and redelivered just
  after, with one, would be stored twice; NapCat does not replay history on reconnect, so that
  takes a redelivery within the seconds of the restart, and it is accepted.
- **Routing**: the bot's own line goes to the echo board; a recognised command word runs the
  command on its own task; otherwise a trigger (an @ of the bot, a nickname, or a quote of a bot
  line) submits a run. A nickname counts in Chinese text wherever it appears (jieba always cuts
  it out), in Latin text as a whole word; so a member who talks *about* the bot by a nickname
  that is an ordinary word starts a run too. Mute and block are decided by the supervisor for
  runs and by the command router for commands, not here. A filled batch queues the group's
  extraction job.
- **Delivery**: `send_group_msg`, correlated by `echo` token, then a wait for the platform's
  report of the bot's own message (`message_sent`), which is how dice and rock-paper-scissors
  results become known. A failed send is returned to the model as an error of one of two kinds:
  certainly not sent (the platform refused it, or there was no connection to send it on), or
  unconfirmed (no answer in time, the connection ended while waiting, or no echo), when the
  message may be in the group and the model is told not to send it again. Only a message that
  may be in the group counts against `replies.max_messages` and makes the run one that spoke.
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
  - `Spontaneous`: the bot looked at the conversation on its own. Every member message that did
    not trigger the bot rolls `replies.spontaneous_chance` (default 0, off). "Did not trigger"
    means no @ of the bot, no nickname, no quote of a bot line and not a command; a message to
    another member (`@B ...`) counts. On a hit, a run starts after the group's media in flight
    is described. It has no initiator and no triggering message, and its note says so: nobody
    is waiting for an answer, and `stay_silent` is the usual outcome. The supervisor admits at
    most one spontaneous run per group at a time, so a lively chat cannot start several that
    talk over each other. That rule is for spontaneous runs only: addressed and scheduled runs
    start and overlap as always, also while a spontaneous run is under way.
- **Supervisor.** Every trigger becomes its own run with its own deadline; runs of one group may
  overlap. The deadline and a shutdown bound every step, opening the context included (it asks
  the platform for display names). Admission gates, in order: muted group, then a blocked member (the initiator of an
  addressed run; for a spontaneous run, the author of the line that rolled it, since a blocked
  member's message never starts a run of any kind), then for a spontaneous run another one of
  the group still under way, then capacity. All admission policy is here; the gateway only
  decides that a message triggers or rolls a run. One bounded queue serves all
  runs; `replies.concurrency` runs call the model at once; queue time counts against
  `replies.deadline_secs`.
- **A member's runs take turns.** Addressed runs one member starts in one group never overlap:
  a message of a member whose run there is under way is admitted (run record, deadline,
  capacity) but waits for that run to end before it waits for a model slot and opens its
  context, so it sees the first run's reply. While one waits, a further message of the member
  folds into it (`Rejected::Folded`: no run of its own); the waiting run then answers the newest
  message, with a whole deadline from it, and its run record keeps the message that admitted it.
  The member's first run takes its turn at admission, so runs take turns in admission order; when
  a waiting run's turn comes, mute and block are checked again (a run they now refuse ends
  `cancelled` without a model call). The pipeline hands each group's addressed triggers to one
  queue that submits them in arrival order, each after the group's media in flight is worked on,
  so a member's quick messages reach the supervisor as they were sent. So a member holds at most one
  run under way and one waiting per group. Measured before this rule: 62 times in 30 days a
  member's second message (58 within five seconds) started a run beside their first, and 40 of
  those pairs both replied. Other members' runs, scheduled and spontaneous runs, and commands
  are not affected. The order is held in memory only: a restart has no runs to order. When an
  addressed run's chat shows a reply of the bot after its message, the trigger note says so and
  to stay silent if that reply already answers it.
- **Run loop.** Project, call the model, append, execute the calls, append the results, then the
  chat that arrived. A turn with no tool call ends the run, except as below. The run also ends on
  a tool that ends it (`send_messages` by default, `stay_silent`), the deadline, `max_turns`, or a
  model error. A run that ends on a model or environment error (the archive, the run log, an
  opened context) logs the cause as a warning; its record keeps the end and the error class.
  A model turn whose tool call cannot be read (arguments that are not JSON, typically a quotation
  mark left unescaped) is asked for again once: nothing ran, so the same request is safe to
  repeat. Every run logs one INFO line when it ends (trigger, end, duration, turns, sends,
  tokens).
- **Speaking.** Only `send_messages` reaches the group: `{messages: [text, ..], end_turn?}`, each
  item one QQ message in the chat's own marker syntax (`[reply:ID]` first, `[at:N]`,
  `[face:N]`; `[dice]`, `[rps]`, `[contact:N]` alone, never a game result), parsed strictly with
  an explanation for every mistake. The model decides how many messages a reply is and where it
  splits; nothing is split by length. Every item is checked before any is sent, and a batch
  that does not fit what is left of `replies.max_messages` is refused whole, so a mistake never
  leaves half a reply in the group. The messages then go out in order, each waiting for its
  echo, and the call returns only when all have their outcome; its result shows each message as
  delivered. The first failure stops the batch: what was sent is listed, the failing message is
  reported as not sent or unconfirmed, the rest as not sent, and nothing is retried. The limit
  counts messages, not calls, and a successful call ends the run unless `end_turn` is false.
  `stay_silent` makes silence explicit. `[at:N]` and `[contact:N]` may name only a member the
  run's context names (`OpenedContext::members`): someone who speaks in the visible chat, is
  mentioned in it, is named in a task's intent as `member:N`, or has an entry in the people
  block. So a reminder reaches someone who has been quiet, while the model cannot address a
  number nothing showed it.
- **Tool choice per turn.** Every turn of a run must call a tool (a turn without one ends the
  run), but a turn asks for `ToolChoice::Auto`, not a forced choice: the reply keeps its
  reasoning, and DeepSeek's Responses API refuses any forced `tool_choice` (`required` or a
  named function) whenever thinking is on, which it is by default (re-measured 2026-10-14, and
  2026-10-21 on `deepseek-flash` as DeepSeek-V4.1-Flash-1017 through both the Responses and the
  Chat Completions API, efforts low, high and max: "Thinking mode does not support this
  tool_choice"; with `reasoning.effort = none` a forced choice is accepted; the live test
  `deepseek_forced_tool_choice_with_thinking_on_the_real_api` measures it). In thinking mode the model tends to answer in plain text when the
  request ends on chat rather than on an instruction (6 of 6 replays of a real reply request,
  against 0 of 6 without thinking), so every trigger note ends by saying to reply through
  `send_messages` or `stay_silent`; with that line, 11 of 12 replays called a tool directly.
- **Undelivered text, the fallback.** A turn that still ends with text but no tool call wrote
  something nobody saw. The run logs it, appends one note (`prompts/undelivered_note.md`) and
  asks for one more turn with a forced tool choice, so a written reply is never lost and
  narration is never delivered. `qbot-eval` reports how many runs needed it.
- **Forced tool choice is a provider capability** (`ForcedToolChoice`). DeepSeek refuses a
  forced choice with reasoning on, and refuses a reasoning turn after one without it (both
  measured on the live API, pinned by an ignored live test), so on DeepSeek the forced turn and
  the rest of that run go without reasoning. OpenAI-style providers declare `Always`; a
  provider that declares `Never` gets `Auto` and the note alone.
- **Executor.** Reads run concurrently, then writes (sends included) one at a time in call order; a refused call gets
  a typed refusal; every call executes (no deduplication), so the prompt teaches the model not to
  repeat a write. A call with invalid arguments is marked as not run (nothing happened, correct
  it and call again), unlike a failure, whose effect is not confirmed.
- **Tools** (`Tool` trait): the argument schema is generated from the Rust type (schemars, nested
  types inlined); descriptions of the tool and its parameters come from `wording.toml` by field
  path. The tool set is fixed for the whole run, so the cached prefix does not change. Tools show
  chat lines exactly as the chat does and times in the bot's zone with their offset, through the
  run's renderer (`ToolCx::renderer`), so the model never converts zones.

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
  A summary names its episode (`episode 12, ...`), so `read_episode` can open its messages; before
  it did, 12 of 20 `read_episode` calls in a month were refused ids the model had guessed.
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

**Groups are separate.** Except the owners' `/stats global` (totals of runs, provider calls and
tokens over all groups), nothing learned or stored in one group becomes visible in, or changes
what happens in, another: not in a prompt, a tool result, a command reply or `/logs`. Every
record is one of these:

| Scope | Records | Notes |
| --- | --- | --- |
| Global, never shown | `account` (that a login exists, first seen) | the key other records point at; a member's identity reaches a group only through that group's records |
| Global, by content | picture descriptions and voice transcripts (`media_cache`) | keyed by a digest of the bytes and made from the bytes alone (the describer and recognizer see nothing else), so a hit gives a group only what it could get from the same picture or clip it received itself; nothing learned from any group's conversation enters them. A forwarded picture is described only on a hit, which says only that identical bytes were seen before |
| Global, owners | `usage_event`, `run` totals for `/stats global` and the daily report | counts and tokens, no text; the report goes to the owners privately |
| Process, owners | warnings and errors for `/logs` | a line about a group (its `group` field) is shown only in that group; a line about no group is shown everywhere, so it keeps only its fixed message and numeric fields, never free text such as an error or a model's answer (the full line stays in the process log) |
| Group | archive and `media_ref`, member numbers, blocks, mute, delegated admins (`group_admin`), links (`account_link`), names, notes, facts and group knowledge, episodes and their embeddings, tasks and recurring schedules, runs and their transcripts, invitations | read only with the group's id |

Nothing else crosses: no name, fact, note, relationship (link), chat text, memory, task context
or anything else derived from a group's conversation is shown or used in another group.
Linking is per group: a person in a group is the accounts linked to each other there
(`IdentityStore::linked`), and a link made elsewhere does not join their numbers, names, notes,
facts, blocks, `/top` rows or command authority. Names, notes and facts about an account are
kept per group and read only from the group's own rows.

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
  failed call, a shutdown or restart) stays a bare marker, and `open_images` still reaches it.
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
  idempotent: claimed with a lease renewed while it runs (a failed renewal is tried again at the
  next beat, a third of the lease later), retried with backoff, failed for good after five
  attempts, each failure logged. A job that panics counts as a failed attempt. A job running at
  shutdown is dropped with its claim in place, and startup queues it again.
- **Group tasks** are created by the model or an owner, owned by the group, and bounded where a
  model could otherwise loop: at least five minutes ahead, at most 50 pending per group, follow-up
  chains at most 24 deep. Update and cancel apply to pending tasks only, except that a repeating
  task can be cancelled while it runs.
- **Repeating tasks** are one `wake` timer with a rule (`repeat_*` columns): an interval of whole
  minutes (at least an hour: each occurrence is a model run, and the series never ends by
  itself), every day at a time, or chosen weekdays at a time. A time of day is read in the zone
  stored with the rule, the bot's `bot.timezone` when the rule was set. The scheduler owns the
  series: when an occurrence ends (completed, failed, deadline, muted, rejected) one
  compare-and-set on `(id, due_ms, state = claimed)` records its outcome as the task's last run
  and returns the same timer to pending at the next occurrence after now, so the model never
  schedules it again and the wake note tells it not to. Occurrences come from the rule, not from
  when a run ended: an interval keeps its phase, a daily or weekly time stays at its wall-clock
  time. On a day the clock jumps over that time the occurrence moves forward by the jump; on a
  day the time happens twice only the first counts. Guarantees: one timer means one occurrence
  in flight at most, never overlapping; each occurrence runs at most once, like a one-shot task
  (one that was running when the process died is recorded as interrupted and not replayed;
  recovery moves the task to its next occurrence after the restart); a failed or skipped
  occurrence is not retried, and the next one comes as usual. Occurrences missed while the bot
  was down (or blocked) are made up by a single run as soon as it can start, after which the
  series continues from the first occurrence after that run, on its original phase. A store
  error while recording an occurrence is retried every 30 s until it answers, since a repeating
  task left claimed would not come due again before a restart. Cancelling a running repeating
  task ends the series at once; the running occurrence is not cut off and its outcome is still
  recorded. Only a run started by a member's message (or an owner's `/tasks`) may create, change or
  cancel a repeating task: a task's own run or a spontaneous one can neither start an endless
  series nor rewrite or end one a member asked for. A repeating task counts against the pending
  bound like any other, also while its occurrence runs.
- **The task tools do one thing each**: `schedule_task` makes a one-shot task (`intent`, `run_at`
  or `delay_seconds`); `schedule_repeating_task` makes a repeating one (`intent`, `rule`,
  optional `start_at`), the rule written in the grammar `/tasks --every` uses (`6h`, `90m`,
  `daily@08:00`, `mon,thu@08:00`) and parsed by the same `Rule::from_str`; `update_task`
  changes a pending task's intent or time, never its rule (a new schedule is cancel and
  schedule again; the owner's `/tasks edit --every` can still replace one); `list_tasks`,
  `get_task` and `cancel_task` as named. Each refuses an argument that belongs to another (a
  `rule` given to `schedule_task`), so a mistaken call is explained rather than half done.
- **Recurring schedules** (`maintenance.nightly_cron`, `report_cron`, croner with the bot's time
  zone) turn each occurrence into a job through one atomic store step, so an occurrence fires once
  however often the process restarts; one missed while down fires once on startup. The loop fires
  the occurrence it slept toward even when the timer wakes a moment early.
- **The scheduler loop** takes one clock reading per tick for claiming and for deciding what comes
  due next, so a timer due between two readings is not mistaken for a blocked one. A store error
  (claiming, reading when the next timer comes due, recording an outcome) is logged, and the
  read is retried; scheduled work never stops silently.

## 11. Providers (`qbot-llm`)

- One `Provider` trait with one method, `respond`. A request carries the complete logical
  conversation (lowered from the transcript view through a renderer), the tools, the tool choice,
  the reasoning effort and an optional continuation; the answer is one whole turn. Nothing is
  streamed: every consumer (sending a message, running tools, storing an episode, a picture's
  description) needs the complete turn, and no person watches it being written.
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
  deadline; other calls by a per-call deadline (ten minutes for text, two for a picture). Each
  HTTP attempt has its own bound inside that deadline (200 s text, 40 s picture, 10 s embedding
  and search), so a request that stalls (accepted, then silent) is tried again rather than
  spending the whole deadline: before this bound, 11% of picture calls were lost that way.
- **Errors** are one taxonomy (`Auth`, `RateLimited`, `Unavailable`, `Timeout`, `ContextTooLong`,
  `QuotaExhausted`, `Protocol`, `MalformedCall`, ...). Retries live in the adapter: bounded, transient failures
  only (a connection error, a status that says to try again), honoring Retry-After. A used-up plan (HTTP 402, Tavily 432/433)
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
- **Accounting** (`qbot_llm::meter`): each adapter method that reaches a provider (`respond`,
  `embed`, `search`, `read`) is one call, recorded once on the adapter's `Meter` when it ends,
  failed or cut off by its caller included. What it consumed is gathered where each fact is
  known: the shared retry helper counts every HTTP request (retries, embedding batches and image
  uploads each count), and tokens or credits are added where a response body is parsed, once
  per response. So a retried call is one record with all its requests, and no response is
  counted twice. The work a call serves is declared around it, not passed to the adapter:
  `attributed(Attribution, ..)` wraps a reply run's model turns and tool turns (purpose `reply`,
  the run and the turn), a group's extraction job (`extraction`) and a picture description
  (`picture`). A call made outside any such scope is still recorded, as unattributed, with a
  warning. Search providers report plan credits, not tokens; a provider that does not report
  its cache use is `CacheUsage::NotReported`, stored as NULL, never as zero.
- **Fakes**: `FakeProvider` implements the whole contract offline (validation, prefix-cache
  simulation, recorded requests); `SimServer` is a Responses server for adapter tests.

## 12. Commands (`qbot-commands`)

Commands run without a model call. Each acts on one kind of information, so no command touches
two stores.

Who may do what, in the group where the command is sent. *Self* is the sender and the accounts
linked to them in this group; an *ordinary member* is a person of this group none of whose
accounts linked here is a bot owner's or a delegated admin's.

| Command | Member | Delegated admin (its group only) | Bot owner |
| --- | --- | --- | --- |
| `/who [--linked] [@m]`: names, notes, learned facts on record | self | self, ordinary members | anyone |
| `/note` `add` `edit N` `remove N` `clear` | self | self, ordinary members, except notes a bot owner wrote | anyone |
| `/name` `add` `remove` | self | self, ordinary members | anyone |
| `/forget [--linked] [@m] N`: a learned fact | self | self, ordinary members | anyone |
| `/forget group N`: group knowledge | no | no | yes |
| `/group`, `/stats`, `/top`, `/help` | yes | yes | yes |
| `/stats global` | no | no | yes |
| `/link @m`, `/link confirm`, `/link cancel`, `/unlink` | self | self | self |
| `/link @a @b`, `/unlink @m` | no | no | yes |
| `/mute`, `/block [--linked]` | no | ordinary members; never a bot owner's block | yes; the bot and bot owners cannot be blocked |
| `/members`: the roster | no | yes | yes |
| `/admin`, `/admin add @m`, `/admin remove @m` | no | no | yes |
| `/tasks` (`add`/`edit` take `--every RULE` for a repeating task), `/runs [N]`, `/logs [n]` | no | no | yes |

Replies quote the command, mention the sender, and are split at QQ's 2,000-character message
limit. Mute and block hold for commands as for runs: in a muted group only bot owners are
answered, and delegated admins for `/mute` and `/block` (so `/mute off` works), and a blocked
member gets no answer and so cannot write notes either.

**Authority** has three levels and is not taken from QQ's own group roles: bot owner > delegated
admin > member. *Bot owners* (`owners` in the configuration) may do everything everywhere. A
*delegated admin* is an account a bot owner made one with `/admin add` in one group
(`group_admin`); there, and only there, it may manage the group's ordinary members as the table
says. It never gains what is global or about how the bot runs (`/stats global`, `/tasks`,
`/runs`, `/logs`, identity links of others), never changes the admin list (so delegated
authority never passes itself on), and never acts on a bot owner or another delegated admin,
counting the accounts linked to them in the group. Lower authority never undoes higher: a
delegated admin can neither change nor lift a bot owner's block (`group_block.by_owner`) nor
change or remove a note a bot owner wrote, while a bot owner can change anything. The grant
belongs to that account alone: a linked account, in the group or elsewhere, gets nothing from
it. Whether the sender is a delegated admin is read from the database, and only for the
commands it can matter to; when it cannot be read, nothing is granted. A member's name in a
reply is their current group display name, read live from the platform.

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
| Repeating task interval | 1 h | every occurrence is a model run in the group, and a series never ends by itself |
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
- **Single instance**: the process takes a Postgres advisory-lock session lease before it migrates
  or recovers anything, checks it every ten seconds, and shuts down if it loses it. A check that does not answer within the interval
  counts as lost: on a hung connection only an answer proves the lock is still held.
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
  startup. A partial file left by a backup stopped with the process is removed by the next one.
- **The daily report** to the owners is a set of typed messages, so it follows the locale.
- **Stopping** (SIGTERM, as `docker stop` sends) ends input first, then cancels the runs in
  flight (recorded as cancelled, open calls interrupted) before anything that might wait on one,
  lets commands finish, drops media work (its markers stay bare) and running jobs (queued again
  at the next start), writes the usage records, and releases the lease. Nothing waits out a
  model call, so the container stops within its grace period. The process also stops, with an
  error, if the gateway server, the scheduler or the recurring schedules end by themselves or the
  lease is lost, so the container restarts instead of running deaf. While no platform connection
  is attached, a WARN is logged after ten minutes and then hourly: an expired QQ session is
  otherwise silent. Startup closes a stored run it cannot read as interrupted rather than refusing
  to start. `check-config` runs every check `run` makes before connecting (secrets, directories,
  locale, personas, voice model files, backup tools).
- **Usage**: one `usage_event` table, written off the hot path by one writer: a reply run's end and
  each of its tool calls (`UsageSink`), and every provider call of any work (`Meter`): its
  purpose, kind (`model`, `embedding`, `search`, `extract`), provider and model, requests,
  input, cached and output tokens, credits, status and latency. Daily views
  (`usage_provider_daily`, `usage_tool_daily`) roll them up. `/stats` and the daily report show
  the same lines from one aggregation: one per work and kind of call, then the total; `/top`
  ranks who started reply runs. Nothing reads them to refuse work.

## 17. Evaluation (`qbot-eval`)

Scenarios in `eval/scenarios/*.toml` (an English description and rubric, chat in any
language, a trigger, optional notes, facts, group knowledge and canned web results) run through
the production prompt layer, tools and run loop against the real text model over a simulated
group. Deterministic checks (sent or silent, how many messages and send calls, tools required or
forbidden, members not to mention, a normal ending) are followed by an English LLM judge per rubric item; `--repeat N` because model
output varies. It is run by hand and spends the text model's account. A changed prompt is run
through it.

## 18. Decisions that shape the design

1. Every trigger starts its own run; runs of one group may overlap, except that one member's
   addressed runs in a group take turns (section 6). A blocked member's message is archived and
   shown as marked context but never starts a run.
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
- An uploaded picture stays at DeepSeek for the Files API's longest lifetime (30 days), though
  each upload is used only for one description or one run's turns; the Files API's shortest
  lifetime would do, once it is checked against the live API.
- `open_images` cannot open pictures inside forwarded records whose content was not delivered.
- History search scans the group's lines; a `pg_trgm` index is the step if archives grow large.
- OpenAI-style providers are covered by the simulated server, not by a live test.
- Runs of different members overlap (decision 1), and nothing tells a run what another is
  answering until that one has replied: two members asking the same thing within the same
  seconds can both be answered (13 such pairs in 30 days before the turn-taking rule).
- Memory already extracted from a recalled message stays until it fades or is forgotten.
- `/forget` removes one learned fact; a later episode can learn it again.
- Every run sees every recorded member's names, notes and facts of its group; only the
  commands limit what a member may look up.
- A slice the provider refuses with an error that is not classed as permanent is tried again on
  every filled batch and every night, and holds back that group's later slices.
- Messages shown by `search_history` and `read_episode` cannot be quoted, opened or used to
  address their authors: the run's view knows only what its context showed.
- The people block has no size bound (about 15 KB in the largest group today).
