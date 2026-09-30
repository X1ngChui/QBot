# Architecture

This document describes how QBot is built and why. It covers the runtime components,
the path a message takes, the reply engine and its tools, how the prompt is assembled,
how pictures and voice are handled, the memory model, the budget, and the scheduled
jobs. Configuration keys are described in [configuration.md](configuration.md); the
operator commands in [commands.md](commands.md).

## Components

```text
QQ  <->  NapCat (OneBot v11)  <-- reverse WebSocket -->  bot  <-- asyncpg -->  PostgreSQL 17 + pgvector
```

| Component | Role |
| --- | --- |
| NapCat | QQ protocol client. Speaks OneBot v11 and connects to the bot over a reverse WebSocket, so the bot is the server and NapCat reconnects on its own. |
| bot | A NoneBot2 application (FastAPI driver). Everything in this document except storage. |
| PostgreSQL | The archive, the memory model, the vector index, the job queue, the cost ledger and the per-group operational state. pgvector provides the vector type. |

All three run as Docker Compose services. Credentials are injected from `.env`; a
missing one fails `docker compose up` rather than the first API call.

### Providers

The code deals in five capabilities and never names a vendor outside the providers
package:

| Capability | Used for | Default provider |
| --- | --- | --- |
| text | Replies, memory extraction, attachment storage for pictures | DeepSeek |
| vision | One-line picture descriptions for the archive | DeepSeek |
| asr | Voice transcription | sherpa-onnx with SenseVoice, fixed in-process CPU service |
| embedding | Vectors for episode recall | DashScope `text-embedding-v4` |
| search | Web search | Tavily |
| page reader | Optional readable-page extraction | Tavily, sharing the search runtime |

Lifecycle-bearing capabilities implement the abstract contracts in
`qqbot/providers/base.py`; narrow collaborators such as attachment storage and page
reading are protocols. Upper layers receive the `Providers` composition root and never
see vendor wire objects, response ids or SDK exceptions.

Text and vision use Responses end to end. One task-local `TextSession` owns the ordered
replay for one addressed message; function results carry exact `call_id` values, and the
session cannot branch or continue after completion. DeepSeek is stateless, so its codec
replays the complete sequence locally. Prompt chunks remain independently reconstructible:
response lineage is never shared across messages, persisted, or attached to a group.
Reasoning items needed for same-session replay remain inside the provider adapter and are
excluded from debug files, archives, evidence and structured memory.

Responses transport, provider codec, pricing and task-local session strategy are composed
rather than inherited through a vendor class tree. Configuration chooses a closed
`provider` value; adapters own role mapping, reasoning parameters, cache accounting and
attachment representation. There is no Chat Completions fallback and no hosted ASR path.

Each Responses call emits privacy-safe cache telemetry containing only a random run id,
provider/model, initial-or-continuation phase, replay strategy, stable-prefix hash,
history count, token counts, estimated flag, charge, latency and status. It never logs
prompt text, query text, tool output, reasoning content, response id or credential.

Price tables live in the backend classes. An unknown model bills at the most expensive
known tier and logs a warning, so a renamed model trips the budget early rather than
under-billing.

There is no automatic fallback between backends. A failed call is logged and the reply
is dropped; being addressed and staying silent gets its own log line so it can be told
apart from a message that was not addressed at all.

### Platform identifiers

`AccountId`, `GroupId` and `MessageId` are distinct nominal value types throughout
core records, services, cache keys and repository read models. Ingress and SQL reads
construct them at the boundary; only protocol and SQL encoding turn them into wire
values. Identity-account keys and raw-event keys remain UUIDs, not platform identifiers.
The NoneBot adapter supplies a small `OneBotClient` that wraps the framework bot account
once and forwards protocol I/O; the core `BotApi` contract never exposes an untyped
`self_id`. Pyright checks these contracts, with regressions rejecting erased or
interchanged platform identifiers.

### Runtime ownership

`qqbot/runtime.py` is the process composition root. The plugin loads one immutable
`ConfigBundle`, then passes it to `Runtime.build()`. The Runtime owns its `Clock`,
`Database`, budget ledger, providers, member directory, roster cache, registry,
`GroupDelivery`, media services, command router, one shared `ReplyScheduler`, gateway
and workers. Settings, prompt catalogs and predicate tables are passed explicitly;
production code has no `config()` or `prompt_catalog()` service locator. Group policy,
media cache, reply evidence, archive and identity each have a dedicated repository.
Repository operations use the owning database's pool getter, not a process-global pool.

The Runtime-local clock controls display timestamps and accounting day boundaries.
PostgreSQL remains authoritative for job leases, fencing and durable due times. A
second constructed Runtime cannot change the first one's timezone or close its pool.
The diagnostic error ring also belongs to the Runtime and uses its clock. It attaches
to application loggers during startup and detaches after teardown, including partial
startup failure. Closing an unstarted Runtime cannot detach another Runtime's handler.

`Runtime.start()` acquires the exclusive PostgreSQL session lease before opening the
pool, checking schema, starting ASR and launching memory work. Loss of the lease
revokes new side effects and begins shutdown. `Runtime.aclose()` first quiesces
admission, then cancels and joins owned work. Before closing the database it
attempts to persist timer receipts; a failed flush is logged and does not prevent
later resources from closing. Uncertain running tasks are marked interrupted at
the next exclusive startup rather than replayed. Teardown also closes media,
caches, providers, the lease and pool, including after partial startup or a
cancelled startup/shutdown caller. A close failure does not skip later resources.

`plugin.py` adapts NoneBot lifecycle/events to this Runtime. Framework-independent
scheduled job bodies live in `qqbot/operations/scheduled.py`. CLI entry points load
their own bundles and clocks, and own their database/provider lifetimes through exit
stacks. Importing a CLI module does not read deployment configuration or credentials,
start a database connection, or issue a model request.

Reply roster reads use five bulk queries within one read-only repeatable-read
transaction. Exact-account extraction identity reads use three such queries, rather
than querying once for every speaker or mention. Extraction does not collapse linked
accounts into a reply-holder code, and ambiguous or unconfirmed names do not become
line-local identity targets. Runtime-owned roster caching is separately bounded by
entry count, row count, text weight and TTL; a roster too large to cache is returned
whole rather than silently truncated. The separate provider projection bound rejects
the whole request if that roster makes the model payload too large.

## Message pipeline

```text
message or supported notice arrives
 |- normalize to one InboundEvent
 |- parse and load the existing window
 |- append raw_event and required identity writes in one transaction
 |   `ON CONFLICT DO NOTHING RETURNING id` is the sole admission gate
 |- duplicate or archive failure -> stop with no window, media, command or reply effect
 |- admitted -> append to the live window; start picture and voice work
 |- self message or notice? -> done
 |- command?  -> exact first token of the original typed text routes through the
 |                importable command registry and GroupDelivery
 |- trigger?  -> no: done
 |             yes: cut the context slice now and submit one bounded reply request
 |                  to the shared inbox
 |
 |- in the session: mute -> block list -> budget -> bounded media wait
 |- retrieval: member roster (everyone who has appeared, with known facts), group knowledge
 |- prompt assembly
 |- tool loop: send_message sends one message and waits for its admitted self event
 |- model sees the observed result, may send again, then calls finish_reply
 |- self events are archived without dispatching another reply
```

Every addressed message creates an independent request with its arrival-time window
and an absolute deadline. One `ReplyScheduler` bounds active plus pending addressed
replies and due timers, including outstanding claim reservations, under the same
capacity; waiting requests do not each own an asyncio task. Queueing counts against the same reply deadline. Full or expired requests
end silently. The timer worker reserves a slot *before* claiming a due database row;
without capacity it leaves that row pending. Both paths run the same `ReplySession`
through `ReplyExecutor`, with separate initiator attribution and no message merging.

Group notices (joins, leaves, kicks, recalls, bans, pokes) are transcribed as one marked
line into the window and the archive. They never trigger a reply.

### Trigger

One rule, no tuning: the bot answers when a message @-mentions it, contains one of its
nicknames as a whole word, or quotes one of the bot's own lines still in the window.
Nicknames are matched as jieba tokens with an ASCII boundary check, so a Latin-lettered
nickname cannot match inside a longer word. A muted group never answers an addressed message. One-shot task wakeups are a separate
cause for a reply: they still obey the mute and budget gates, but do not invent an
incoming QQ message or enter the archive.

### Gates

Before reply-model or tool spending, an addressed task checks the daily cap and its
initiator's current group block status. A blocked account cannot independently trigger
an ordinary reply, but its messages remain in context; another triggered session may
refer or respond to them. There is no blocked-recipient send filter or guarantee of
semantic silence. Group task wakeups have no member initiator or member block gate.
The mute switch and budget still apply. Other members can use the bot immediately,
without an acceptance step.

## Reply engine

Each addressed message or due timer owns an independent model session and stop-loss scope.
Addressed work retains its causing AccountId; group timers have no single-account attribution. A session begins with the arrival-time window, then
extends its own stable line, member and picture numbers only when it resumes after tools.
New member messages still trigger their own concurrent tasks. They are additional
observations in the existing task, not a replacement for its original request.

Every admitted tool call executes, including repeated calls with identical arguments;
there is no session-level tool deduplication. A repeat query can observe changed state,
and a repeat mutation has its own effect. The model retains the original request and
the complete ordered call/result replay so it can track its goal and completed steps.
Parameter and permission validation, money gates, fuel, payload bounds and delivery
uncertainty rules still apply; an oversized replay fails rather than silently dropping
prior progress. Independent sessions can still interleave task mutations, so a final
listing is a current observation rather than an atomic replacement guarantee.

A model sends **one QQ message per `send_message(content=[...])` call**. The model must
call `finish_reply({})` to end deliberately; a tool-free model turn ends silently.
Outbound actions are exclusive in their tool round: if either is requested alongside
retrieval, scheduling or another send, every call gets a refusal and no side effect.
The send schema and runtime parser share the same strict Pydantic model. It accepts
`text`, `at`, `reply`, `face`, `dice`, `rps`, `contact_member` and `contact_group`.
Numbers resolve against this session's stable prompt-local member and line maps.
`dice`, `rps` and either contact card must occupy the entire message alone. Historical
rich cards are readable but not present in the current sending schema. Invalid
arguments send nothing; bare model text is never sent to the group.

The engine cleans and bounds the message before sending it. `GroupDelivery.deliver_one`
locks only the OneBot API call within the group, including the specific reply-segment
refusal fallback; it releases that lock before waiting. A confirmed `message_id` is
matched against the bot's own `message_sent` event by bot account, group and message ID.
The gateway first admits that event to PostgreSQL and projects it into the live group
window; only then can the observer release the model session. The observer caches a
bounded number of recent projections, so an event that arrives **before** the API ACK
still matches. No fabricated echo or success is inserted by the send path. `get_msg`
can supplement a missing random result only after that observation and only if it
confirms the same sender, group and message ID. Its verified reading backfills the
archive's derived text (not the original event payload). A missing ACK, uncertain transport
failure or echo timeout stops the session without a blind resend; late events still
archive normally. Once observed, the actual displayed text and any random result
enter the tool result; newly arrived group messages are added after that result in a
separate lower-trust observation. The bot's own send is numbered but not duplicated
in that supplemental transcript.

Reply preferences are `conversation.max_messages_per_reply` and
`conversation.reply_deadline_sec`. Session fuel owns finite model/tool counts, retained
tool results and attachments; delivery owns the bounded echo wait. The absolute reply
deadline can end that wait sooner. If retrieval allowance is exhausted before sending,
a final round can send or end without searching. After an already confirmed send,
exhaustion ends immediately; it never retracts a visible message. Retrieval evidence
is attached to the first platform-acknowledged message ID. Timers use the same send
loop with group mute, budget, deadline and fuel checks. Member blocking gates addressed
message causes, not group tasks; context remains available to every private session.

### Model payload safety

The provider-neutral projection bound covers a complete next request and its replay:
up to 2 MiB of weighted text and structure, with 32 bytes charged per traversed node,
and at most 40,000 nodes. It checks the entire input, tools and returned output as one
projection, without truncating a roster that made the request too large. Inline image
bytes are counted separately and remain under the existing media and image-count
bounds, not the 2 MiB textual bound. These are local payload safety limits, not a
model-token or context-window limit, nor a guarantee about heap use or SDK parsing.

A request that exceeds the bound before dispatch is rejected without a provider call.
If a provider has already produced an oversized result, the backend books its actual
usage when available, otherwise one conservative estimate, before reporting a
nonretryable overflow failure. It does not drop a potentially charged call from the
ledger or retry it as if no charge occurred.

### Tools

All the query tools are free in themselves; what costs money is the model round that
carries them.

| Tool | What it does |
| --- | --- |
| `send_message`, `finish_reply` | Sends one QQ message with an observed result, or ends the session without another send. |
| `schedule_task`, `list_scheduled_tasks`, `get_scheduled_task`, `update_scheduled_task`, `cancel_scheduled_task` | Group-scoped persistent work. The model autonomously creates, inspects, edits or cancels tasks; scope is supplied by the runtime, never by model arguments. Active lists include pending and running, with explicit paging. Editing and cancellation are pending-only conditional SQL mutations; edit preserves ID and chain. Task operations and sending occupy separate model rounds. |
| `web_search` | Web search through the configured search backend. Debits the monthly allowance. |
| `read_url` | The readable text of one page, bounded in characters. Debits the same allowance. |
| `search_history` | Boolean search over this group's archive. Lucene syntax parsed by luqum: space means AND, `OR`, `-` exclusion, parentheses, quoted phrases. Only the boolean subset is accepted; fields and ranges are refused in words. The query compiles to one parameterised `ILIKE` expression. Narrowable by speaker (a member number, or a display name for someone the prompt shows no number for) and by days. Each hit is returned with surrounding lines, touching windows merged, and the whole answer is bounded in characters with a note when cut. |
| `recall_events` | Vector search over this group's episodes. Each recalled episode is framed by its neighbours in group time. |
| `open_images` | Fetches picture originals by their number in the transcript, several per call, and hands them to the model as file blocks. |

All model-facing wording lives in one `config/prompts/prompts.yaml` bundle. Tool
descriptions remain separate logical templates because each maps one-to-one to a
code-owned schema, and they are generated, validated, loaded and reviewed with the
rest of the family as one atomic document.

### Output

The text of every send passes `clean_reply`. It strips Markdown that QQ cannot render (emphasis,
headings, code fences, rules, link syntax) while keeping plain-text lists readable;
removes every system marker (line numbers, timestamps, provenance, trace lines, quote
pointers, owner and self tags, member numbers) and any text-form tool-call markup; and finally replaces any
reserved bracket left over. An `@name` opening the text for an account the message
already @-s is removed, so the address is not sent twice. The engine truncates to the configured message length
before sending. The stripper counts what it removed, and the daily report shows the
counters: every hit is a marker the model wrote and the stripper caught.

## Prompt assembly

Prompt wording is a single versioned YAML bundle with a closed code-owned template
contract (`qqbot/prompting/templates.py`). The only syntax is a declared, one-pass
`{{ascii_slot}}`; unknown, missing, duplicate and malformed slots fail the entire load.
Inserted values are never evaluated as templates. Two explicit partials—the transcript
legend and conversational pragmatics—are shared between reply and extraction. Their declared
slot sources are injected by `PromptCatalog`; callers provide only dynamic values and cannot
override a code-owned partial. Every other rule belongs to one complete role template.

The prompt is ordered from most stable to least, so that a provider's prefix cache is
hit as often as possible:

```text
constant rules (how to speak, transcript legend, identity, credibility and retrieval, privacy, tone)
-> persona
-> group knowledge
-> member roster: numbered identity and manual notes, then scored unconfirmed hints (stable order)
-> conversation history (append-only chunks)
-> [cache boundary]
-> current time
-> tool results
-> the message being answered
```

There is no prompt-token trimming budget. Each block renders whole; if the resulting
request exceeds the local payload bound, the whole request is rejected instead of
silently cutting the roster. The author of each block (the operator for persona and
group knowledge, a prompt with its own length discipline for generated text) remains
responsible for restraint. The history window is counted in messages and evicted a
whole chunk at a time, so the prefix changes rarely.

Every transcript line carries its send time in a fixed format. The stamp is written
once at arrival and never rewritten, so it is cache-stable; the current time sits after
the cache boundary and gives the model a clock to measure gaps against.

### System markers

Every marker the system writes uses one reserved bracket pair, `⟦ ⟧` (U+27E6 and
U+27E7). Every untrusted string (member text, display names, picture and voice
descriptions, forwarded and card contents, web and search digests) passes through a
function that replaces that pair with ASCII square brackets before it can reach a
rendered line. "Inside `⟦ ⟧` means the system wrote it" is therefore a rule the model
can apply mechanically: a member whose card imitates the owner tag, or whose message
types out a picture marker, produces plain text that collides with nothing.

A transcript line opens with its line number and time; any other line is a continuation
of the previous message, so a multi-line message cannot forge a line start. The same
grammar is used in the reply history, the extraction transcript and search results.

Markers in use:

| Marker | Meaning |
| --- | --- |
| `#N ⟦MM-dd HH:mm⟧` | Line number and send time |
| `⟦回复 #N⟧` | This message quotes line N |
| `名字⟦0⟧` | The bot's reserved display identity; never a legal tool target |
| `名字⟦N⟧`, N > 0 | Prompt-local member/account identity |
| `⟦拥有者⟧` | The speaker is one of the bot's owners |
| `⟦图片N:描述⟧`, `⟦语音:转写⟧` | Media, with the archived description or transcript |
| `⟦转发的聊天记录 N条⟧` | A forwarded record, rendered as an indented block |
| `⟦检索记录⟧` | Bounded, expiring retrieval context for the historical send immediately following it |

### Member numbers

Two members sharing a display name is ordinary, so every person a reply prompt shows wears
a member number behind their name (`qqbot/conversation/member_numbers.py`). Zero is reserved for
the bot's display identity; humans receive positive numbers, while `None` is the only
absence/unknown sentinel. Zero never resolves through member-targeted tools. In reply
prompts a positive number belongs to a current account holder, so linked accounts share
it. Extraction uses a separate exact-account projection: each account receives its own
batch-local code and every candidate is validated against that precise account. The reply
roster lists everyone who has appeared in the group, whether or not anything is known
about them, ordered by first appearance, and is numbered in that order, so a newcomer
joins at the end and nobody else's number moves. The roster is therefore also where the
model finds the number of someone who is not in the current conversation. Anybody a
prompt shows who is not in the roster yet (a first message still being archived) is
numbered after it. Because the numbers depend on the roster alone, the conversation
moving never renumbers anything above the history. The same numbering runs through the
roster, the window, `search_history` results and the tool arguments that name people,
so the model @-s and filters by the number it read.

Numbers are never stored. The archive and structured evidence hold stable account data or
names only. A real platform mention of the bot renders as `@name⟦0⟧`; member-typed
`@我` remains ordinary text. Members' other @-mentions keep their account identity and
receive the target's prompt-local positive number during projection.

The bot's own messages are rendered in the history as the `send_message` calls that
requested them (ordered text and control segments). Each is followed by a tool result
carrying the message's line number and send time; when QQ transformed the request, that
result also states the platform-visible reading. Unexpired structured evidence, when
present, is rendered as a separate assistant block immediately before that call. NapCat's
reported self event is the only source for the live window and archive, so random dice and
RPS results and other platform transformations are preserved without send-side lookups.
In the archive, the bot's line reads exactly as the group saw it, `@name` openings
included, with the @-ed accounts kept as `at` segments so a restart rebuilds the same
window. Permanent-looking evidence tails are not an archive feature and carry no
special authority if untrusted text imitates one.

## Pictures and voice

**Pictures are understood on arrival**, when the download link is freshest. Each picture
is downloaded, uploaded to the text backend's file store, and described in one line by
the vision backend; the description is written into the archived line. Descriptions
are the archival form: search, extraction and window rebuild after a restart all read
text. The description is cached by image content, so a repost costs nothing, and it
expires by age so that a better model gets to look again; refreshing is lazy, because
only a picture posted again is ever looked up. A picture the backend's content filter
declined is cached as a marked placeholder on the same clock.

`MediaCoordinator` owns asynchronous work by admitted raw-event ID. A `MediaTicket`
tracks `pending`, `retryable` or `final`, the shared task, parsed references, bounded
reply waits, live-window patching and archive backfill. `ChatMsg` contains only stable
raw-event identity and typed image references; it owns no tasks. Multiple waiters share
one task, waiter cancellation does not cancel that task, a timeout leaves it running to
patch later, and a transient verdict can retry on a later reply. Restart restores image
references from canonical archive segments but does not create a durable media queue.

No picture is pushed into the prompt. Every picture appears as a numbered description
line, and the reply model fetches the originals it wants with `open_images`. The
history therefore stays byte-identical between turns. Uploaded file ids are stamped and
re-uploaded when older than the configured age.

Forwarded chat records, nested ones included, render as an indented block under the
message that carries them, bounded in lines, depth and characters. Their pictures are
numbered with the rest and can be opened, but are not described on their own account.

**Voice is transcribed on arrival** by the in-process backend (sherpa-onnx with
SenseVoice, two CPU threads, sub-second for a ten-second clip). It costs nothing, so it
runs even on a day whose budget is exhausted; a per-minute gate stands in as a CPU
guard. The transcript lands in the archived line like a picture description. There is no
hosted or API ASR path in the provider registry.

## Memory

### Storage

Twenty-one application tables form the canonical schema:

| Layer | Tables | Holds |
| --- | --- | --- |
| Archive | `raw_event` | Every event, append-once. Versioned `payload` is the canonical code-owned envelope; its segments preserve the detached platform data. `plain_text` is the derived reading, updated with descriptions and transcripts. |
| Identity | `entity`, `identity_account`, `alias`, `alias_evidence`, `account_link_challenge` | Account equivalence classes, exact accounts, scoped names with evidence, and durable two-account confirmation. |
| Facts | `memory_fact`, `memory_fact_evidence`, `memory_candidate` | Temporal exact-account, holder and group facts; evidence; and audited model proposals. |
| Extraction | `memory_extraction`, `memory_extraction_event` | Durable extraction state, exact ordered event membership, and the immutable staged validation snapshot. |
| Episodes | `episode`, `episode_event` | Group-scoped summaries backed by every source event and their extraction batch. |
| Index and queue | `embedding_index`, `memory_job` | Derived vectors and the background job queue. |
| Operations | `reply_trace`, `cost_ledger`, `group_state`, `group_blocklist`, `image_cache` | Expiring reply evidence, spending, group switches, dynamic block rules, descriptions and file IDs. |

`sql/init.sql` is the sole schema definition and describes a fresh database at the current
code contract. Existing installations are updated manually under a verified backup.
Startup and deployment perform a read-only structural check over required tables, columns,
constraints, retired-table absence and vector width; they never execute DDL.

Every table that carries a `group_id` indexes it first, and no retrieval path crosses
groups. The identity graph (which accounts form one person) is the one deliberately
global structure.

### Identity

An account is the strong identity. `entity` supplies the small account-equivalence class;
`identity_account.entity_id` is its current membership and `merged_into` preserves root
history. Automatic person aliases and facts are written against exact account foreign
keys. Explicit `--all` edits write holder-scoped rows, and aggregate views combine those
rows with the exact-account rows of the holder's current set.

`/merge` and confirmed `/link` use one deterministic union primitive. One transaction-level
advisory lock serializes this deliberately tiny identity topology; each operation then locks
its exact accounts and current roots before changing membership. Link challenges snapshot
both root IDs and entity revisions;
union and detach increment the affected live root revision, so any intervening membership
change invalidates every stale confirmation even when the same root survives.
`/split @account` and `/unlink` use one detach primitive that
moves only that exact account to a fresh holder; every other account stays linked.
Account-scoped rows therefore follow their account without rewrites, while holder-scoped
rows remain with their holder. Exact-account and holder block rules are also evaluated
dynamically, so a later link or split cannot leave expanded block copies behind.

Aliases retain group/global scope, evidence and status. Confidence is the maximum within
one evidence channel and a noisy-OR across channels. A platform display name begins as a
candidate, manual `/alias` evidence is authoritative, and a retired name stays retired
against automatic evidence.

### Memory visibility and identity authority

Current learned facts and candidate names share one visibility rule: both appear as
individually typed, scored hints, without a minimum-confidence display gate. Retired,
closed, retracted, superseded and expired records are excluded. Current platform display
names, confirmed names and manual notes remain separate reliable context; the current
live platform display name is not repeated as a candidate hint in the reply roster.
When the live name cannot be read, a stored candidate display name remains a scored hint;
only a confirmed platform name may label the roster as a fallback.

A score measures evidence strength within its own type, not calibrated probability.
Fact scores and alias scores use different formulas; they are not compared against one
universal cutoff. Even a high-scoring learned fact is still an automatic observation,
not a manually confirmed statement. Group facts use the same scored hint renderer.

Candidate names are visible for understanding and text search, not for resolving identity
or addressing a member. Only confirmed live aliases enter lookup and the extraction
snapshot's name-based account targets. In extraction, candidate names appear in known
context rather than the account roster; holder hints are labelled as shared, and another
linked account's exact aliases are not copied onto the current account. Neither reading
a hint nor repeating it as the bot creates evidence. A fresh member-authored reuse of
an exact-account candidate can add evidence across batches only when that same line
independently identifies the exact account; stored candidate context is not evidence.
Tool argument validation checks the
selected numeric target, not the model's reason for selecting it; reply prompts enforce
the distinction, while extraction also enforces line-local source/target validation.

### Facts

A fact has exactly one subject: an exact account or an entity (a holder or the group).
It also has a predicate from a closed set, object, validity window, confidence and exact
evidence events. Predicates are defined in `config/predicates.yaml` with their Chinese
rendering, cardinality, decay class and the rule shown to the extraction model. A
single-valued predicate closes the previous value within the same subject scope when a
new one arrives; multi-valued values stand side by side. Partial unique indexes enforce
one current row per subject, predicate and object key.

Confidence is a Wilson lower bound over distinct source events, so one mention earns
about 0.27, three about 0.53 and eight about 0.75. Re-confirmation accumulates evidence
in place. Facts decay by predicate class with half-lives of 90, 30 or 14 days, one
half-life per supporting event, so what a group repeats stays and a passing remark
fades. A decayed fact is expired, never physically deleted; the archive is never
deleted at all.

Episodes remain available to `recall_events` for `memory.episode_ttl_days` from the
conversation time. Nightly decay marks them expired while retaining extraction and exact
source-event provenance. Their embedding rows are derived projections rather than
evidence, so decay deletes them across model versions; an expired episode cannot be
returned or re-embedded.

### Extraction

Nightly maintenance queues extraction. Each claimed job processes one exact batch,
oldest first, with a code-owned limit of 120 events and a 30-minute gap policy.
Successful work queues a continuation when unconsumed events remain; small final
batches are not held behind a configurable drain floor. Failed and budget-deferred
work retains durable state, so there is no promise that all memory is at most a day old.
Each exact batch has six durable model-attempt reservations across replacement jobs.
Cancelled or crashed attempts are not refunded. Exhausted batches retain their event
links and become terminal rather than repackaging the same events into a fresh budget.

Before any provider call, a short transaction reserves the exact ordered raw-event IDs
in `memory_extraction_event`. A per-group session advisory lock allows only one worker to
pay for an extracting batch at a time without keeping a transaction open across the
provider call. Events that arrive after reservation belong to a later batch, even when
the timestamps tie. If a worker fails, the next worker opens the same durable event set.

One model call reads one chunk, known structured memory for deduplication, standing group
knowledge, and the bot's names. Every rendered line receives a batch-local source ordinal.
The immutable version-2 snapshot binds that ordinal to the raw event, timestamp, exact
author account, eligible member-authored span and line-local account resolutions.

Only top-level member text, structured mentions and top-level voice transcripts are
eligible evidence. Bot output, notices, forwarded text, cards and generated media
descriptions remain visible as context but cannot validate a quote. A person target must
be the source author, a structured mention, or an account uniquely resolved by a
confirmed name or alias literally present on that line. This permits facts about a
clearly named non-speaker without exposing the complete group directory to the model.

The model may only propose through structured calls. Every candidate supplies an explicit
source ordinal and verbatim quote; an episode supplies one or more independent
`source + quote` pairs and no model-generated identity list. The pure validator binds each
quote to the eligible span of its exact source event and binds each person candidate to
an allowed exact account before any projection begins. Known memory, notes and standing
knowledge aid interpretation and deduplication but can never become fresh evidence. The
worker checkpoints the snapshot and every candidate, including an empty set, before
projection begins.

An extraction moves through `extracting`, `staged`, and `applied`. Applying a staged
checkpoint locks it and commits candidate audit, aliases, facts and evidence, group
facts, episodes and provenance, required EMBED jobs, and the final `applied` state in one
transaction. A database failure rolls the entire projection back; restart retries the
staged checkpoint without another extraction call. The only unavoidable duplicate-call
window is a process failure after the provider returns but before the staged checkpoint
commits, because the provider offers no idempotency key.

### Reply evidence

Each reply's retrieval work becomes a versioned `EvidenceMemo`: a closed source kind,
a bounded sanitized request summary, verified/unconfirmed outcome, bounded defanged
digest, and explicit creation/expiry times. It contains no provider response, reasoning,
response id, prompt-local member number or full page. `reply_trace` stores only the
structured memo and mandatory expiry; there is no prose or compatibility payload.

Prompt assembly renders unexpired evidence immediately before the send call it supported.
The memo is working context for nearby follow-ups, not group memory: it is absent from
`raw_event`, search and extraction, and the nightly pipeline deletes expired rows.

## Budget

Two monetary levels, both in `qqbot/services/budget.py`:

- **The day.** `budget.daily_cny_cap` is a total over every group against one shared
  ledger. Paid admission checks already-accounted spend before each provider attempt;
  at the cap replies end and workers defer paid work until the
  date rolls over in the configured timezone. The running total is restored from
  `cost_ledger` on restart.
- **The reply.** `budget.per_reply_cny` covers work started by the reply, including
  media understanding and the tool loop; each backend books its priced spend into
  that scope. The check reads money already spent rather than demanding
  strict prepayment for a predicted call. A completed call can therefore cross a cap;
  reaching it prevents another paid request, not an extra paid closing round.

The monthly search allowance (`backends.search.monthly_quota`) is metered from the ledger's
calendar-month call count in the vendor's own unit; an advanced-depth search books two,
and page reads debit the same pool. Both caps are global settings.

Attribution is task-local and feeds `/top` only. A reply's entire spend, including the
transcriptions, picture looks and searches it forces, is booked to the member who
addressed the bot. A picture's archival description is booked to whoever posted it.
Extraction and other communal spend stays unattributed.

In-process speech transcription bypasses the paid-spend gate: it can run on an
exhausted day and is booked to the ledger at zero. It is not unbounded: CPU and media
admission limits still apply. Reply sessions also have finite fuel, deadlines and
payload bounds, including when their model backend is free.

## Concurrency

Every addressed message owns an independent work item. One finite inbox bounds
active plus waiting addressed replies *and* due timers using
`runtime.reply_capacity`; queued work does not create a task per waiter, and timers
reserve capacity before claiming a row. Full or expired requests end silently. A
provider semaphore (`backends.text.max_concurrency`) bounds concurrent model calls.
A per-group delivery lock covers each protocol send but never the wait for its self
echo, so other replies can proceed while one observes its own result.
Self-observation archives messages independently when NapCat reports them. Background
work rides the database job queue (`FOR UPDATE SKIP LOCKED`, leases, and a partial
unique index that keeps one pending job per type and group). Exact extraction
membership and the per-group provider slot prevent twin workers from paying for one
batch together.

## Scheduled jobs

| Schedule | Job |
| --- | --- |
| `maintenance.nightly_cron` (02:30) | One pipeline in dependency order: memory extraction, memory decay, `pg_dump`, NapCat media cache cleanup. Each wait tracks only that stage's jobs; independent embedding backlog does not delay decay or backup. |
| `maintenance.report_cron` (00:00) | The daily report to the owners: spend by kind and model, cache hit rates, picture cache, search allowance, job backlog, new and muted groups, backup age, error digest, output-stripper counters. |
| resident | The job-queue worker, including lease reclaim. |
| resident | The model-created timer worker polls due PostgreSQL rows only while OneBot is connected. It reserves the shared reply inbox first, then atomically claims a due task once and runs the common `ReplySession` against a fresh group window. |

A restart in the middle of the nightly pipeline skips that night's remaining stages;
decay catches up the next night, and a missed dump shows up in the report's backup-age
line.

Model-created timers persist across restarts in a separate `scheduled_task` table;
`memory_job` cannot hold them because it deduplicates pending work by type and group.
A due timer loads the latest archived messages even after a restart and rechecks the
current group mute and shared daily budget before generation. Its original short intent
is lower-trust historical input, not a new group message; the model decides anew whether
to speak, retrieve facts, or create a bounded follow-up. Pending tasks wait for the
OneBot connection. The worker must reserve inbox capacity before claiming a task;
without that reservation the task stays pending. Claims are single-attempt: if a process dies after
claiming a task, it is marked interrupted on restart rather than risking a duplicate
unsolicited QQ send. Group/day execution counts and chain depth bound automatic
rechecks. Ordinary replies retain their frozen arrival-time context and the same
observable single-send contract.

## Configuration model

`config/settings.yaml` is global. `config/personas/default.yaml` is the default persona;
`config/personas/group_<id>.yaml` may vary only the bot's name, persona prompt and standing
group context. Persona fields are inherited from the default, settings are not. Both schemas
are validated with pydantic and unknown keys are rejected. New groups need no configuration.

Prompts are data: every runtime prompt template lives in one versioned
`config/prompts/prompts.yaml` bundle. A closed contract in
`qqbot/prompting/templates.py` owns the template keys, roles and exact `{{slot}}` sets.
The whole catalog is validated as one immutable startup snapshot and rejects missing,
extra, duplicate or malformed slots before any template becomes active. Marker formats
that application code produces remain code-owned; their shared explanation has one
source in the bundle.

## Validation

`python -m pytest` is the supported test entry point; `python -m ruff check .`
runs separately. Unit tests need no database. DB-backed tests require an explicit
`QBOT_TEST_DATABASE_URL` and skip when it is absent. Before any mutation, their
fixture checks the distinct disposable test role, test database and guard marker,
then owns the pool for that test. Do not run DB-mutating cases concurrently against
the same database. Neither tests nor imports read private deployment configuration
or call a paid API; manual evaluation scripts are separate.

## Design decisions

| Decision | Rationale |
| --- | --- |
| Speak only when addressed or an explicit timer fires | Incoming chatter alone never causes an unsolicited reply; a previously requested wakeup is a separate, bounded cause. |
| Structured memory with evidence, no prose profiles | Prose cannot be corrected line by line, cannot carry confidence, and tends to invent. Evidence-based confidence and per-class decay can. |
| Accounted spend plus finite safety bounds | Monetary caps govern paid admission using spend already booked; session fuel, deadlines, media limits and the model projection bound separately constrain work and payload size. Free work is not unlimited. |
| A reached cap stops another paid attempt | A completed call may cross the cap because there is no strict price prepayment. The gate checks the ledger before the next provider attempt rather than purchasing a closing round. |
| Reject an oversized projection whole | A large roster is not silently trimmed. Text and structure have a local safety ceiling distinct from model tokens, with inline image bytes governed by media admission. |
| Function calls for every structured write | Free-text JSON needs parsing and invites drift; a schema is checked by the platform. |
| Text as the archival form | Search, extraction and restart rebuild read text; the model fetches pixels only when it wants them. |
| Media understood on arrival | The link is freshest then, each unique picture is paid for once, and groups the bot never answers in still get a readable archive. |
| Episodes are pulled, never pushed | A block of related past events pushed next to the incoming message misled reference resolution. The past reaches a reply only through `recall_events`. |
| Prefix-cache-friendly prompt order | A cache hit costs a small fraction of a miss; ordering, chunked eviction and batched extraction all serve the hit rate. |
| No fallback, no automatic downgrade | Failover is the least-tested path. A failure stops and is logged. |
| Search called directly, not the model's built-in search | Built-in search bills twice and inserts content where it breaks the prefix cache. |
| Exact vector scan, no HNSW index | The embedding dimension exceeds pgvector's HNSW limit; retrieval filters by group first, so an exact scan over a few hundred rows is both accurate and fast. |
| Reserved bracket grammar for markers | Makes system markers unforgeable by construction instead of asking the model in prose not to be fooled. |
