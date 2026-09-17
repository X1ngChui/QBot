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

| Capability | Used for | Default backend |
| --- | --- | --- |
| text | Replies, memory extraction, file upload for pictures | DeepSeek |
| vision | One-line picture descriptions for the archive | DeepSeek |
| asr | Voice transcription | sherpa-onnx with SenseVoice, in-process on the CPU |
| embedding | Vectors for episode recall | DashScope `text-embedding-v4` |
| search | Web search and page reading | Tavily |

Each backend is a subclass of the capability's abstract base class in
`qqbot/providers/base.py`. Platform differences (how cache hits are reported, how to
disable deliberation, how a picture is attached) live in the subclass, so the layers
above it stay vendor-neutral. `qqbot/providers/registry.py` maps the `backend` name in
configuration to the class. Adding a backend is one subclass plus one registry entry.

Price tables live in the backend classes. An unknown model bills at the most expensive
known tier and logs a warning, so a renamed model trips the budget early rather than
under-billing.

There is no automatic fallback between backends. A failed call is logged and the reply
is dropped; being addressed and staying silent gets its own log line so it can be told
apart from a message that was not addressed at all.

## Message pipeline

```text
message arrives
 |- dedup (platform message id, short TTL)
 |- archive to raw_event (always, including in groups the bot never answers in)
 |- resolve @-mentions, quotes and forwarded records; start picture and voice work
 |- command?  -> archived and windowed, then routed to the command handlers
 |- trigger?  -> no: done
 |             yes: cut the context slice now and spawn one reply task
 |
 |- in the task: daily budget -> block list -> mute -> user agreement
 |- retrieval: member roster (everyone who has appeared, with known facts), group knowledge
 |- prompt assembly
 |- tool loop until the model calls send_message
 |- clean the text, send it with the @s and reply it asked for, archive the bot's own line
```

Every message that addresses the bot gets its own task, running concurrently with any
others. Each task works on the window as it stood when its message arrived, answers that
message, and bills its whole cost to that sender.

Group notices (joins, leaves, kicks, recalls, bans, pokes) are transcribed as one marked
line into the window and the archive. They never trigger a reply.

### Trigger

One rule, no tuning: the bot answers when a message @-mentions it, contains one of its
nicknames as a whole word, or quotes one of the bot's own lines still in the window.
Nicknames are matched as jieba tokens with an ASCII boundary check, so a Latin-lettered
nickname cannot match inside a longer word. A muted group never answers.

### Gates

Before any money is spent the task checks, in order: the daily cap (silence when
reached), the group's block list (a blocked member is read and remembered as always
and only never answered), the mute switch, and the user agreement. A member who has
not accepted the agreement receives a short pointer to `/terms` at most once per
cooldown, transcribed into the window as a notice rather than as the bot's own words.
Owners are exempt from the agreement.

## Reply engine

The engine is an agent loop bounded by money rather than by a round count:

```text
with budget.scope(per_reply_cny):
    loop:
        call the model with every tool offered
        a send_message call -> return it (other calls in the same round are not run)
        no tool calls, only text -> tell the model it was not sent (once), else silence
        per-reply cap already spent -> one final round offered only send_message
        execute the tool calls (a repeated identical call is answered in words, not re-run)
        monthly search allowance exhausted -> the same final round
        append results as tool messages
```

The first round always runs. A cap that trips mid-reply does not discard the reply:
outstanding tool requests receive a placeholder result and one final round, offered only
`send_message`, answers from what was already fetched, so the overshoot is exactly one
round. A transport
failure is reported to the model as a failure and the loop continues; a broken network
is an error, not a limit. A configurable round cap exists only as a tripwire against a
backend that bills zero.

### Sending

A reply is a call to `send_message`. Its parameters follow the OneBot message segments
it becomes:

| Parameter | Segment | Meaning |
| --- | --- | --- |
| `text` | `text` | The body, plain text. Required. |
| `at` | `at` | Member numbers to @, placed before the body in order. Optional. |
| `reply` | `reply` | The line number of the message to reply to. Optional. |

With neither `at` nor `reply` the message goes out plain. Whether to @ anyone and
whether to reply to a line is the model's decision. A member number or line number the
prompt never showed is dropped with a log line and the rest is sent; a call with no text
is answered with a note and the loop continues, so the model can send again. At most
five accounts are @-ed per message. Bare text is never sent: a round that ends without
a tool call ends the reply in silence, with a warning in the log.

If a send with a reply segment is refused by the platform (the replied-to message may
have been recalled), it is retried once without the segment.

### Tools

All the query tools are free in themselves; what costs money is the model round that
carries them.

| Tool | What it does |
| --- | --- |
| `send_message` | Sends the reply and ends the loop (see above). |
| `web_search` | Web search through the configured search backend. Debits the monthly allowance. |
| `read_url` | The readable text of one page, bounded in characters. Debits the same allowance. |
| `search_history` | Boolean search over this group's archive. Lucene syntax parsed by luqum: space means AND, `OR`, `-` exclusion, parentheses, quoted phrases. Only the boolean subset is accepted; fields and ranges are refused in words. The query compiles to one parameterised `ILIKE` expression. Narrowable by speaker (a member number, or a display name for someone the prompt shows no number for) and by days. Each hit is returned with surrounding lines, touching windows merged, and the whole answer is bounded in characters with a note when cut. |
| `recall_events` | Vector search over this group's episodes. Each recalled episode is framed by its neighbours in group time. |
| `open_images` | Fetches picture originals by their number in the transcript, several per call, and hands them to the model as file blocks. |

The tools' descriptions are prompt files and are written to overlap as little as
possible: the outside world, the group's own words, past events by meaning, one page,
and pixels.

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

The prompt is ordered from most stable to least, so that a provider's prefix cache is
hit as often as possible:

```text
constant rules (how to speak, transcript legend, identity, credibility and retrieval, privacy, tone)
-> persona
-> group knowledge
-> member roster: everyone who has appeared, numbered, with confirmed facts (stable order)
-> conversation history (append-only chunks)
-> [cache boundary]
-> current time
-> tool results
-> the message being answered
```

There is no token budget. Every block renders whole; the author of each block (the
operator for persona and group knowledge, a prompt with its own length discipline for
generated text) is responsible for restraint. The history window is counted in
messages and is evicted a whole chunk at a time, so the prefix changes rarely.

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
| `名字⟦N⟧` | Member number: who this is, within this one prompt |
| `⟦拥有者⟧` | The speaker is one of the bot's owners |
| `⟦你⟧` | The bot's own line (extraction transcript and search results) |
| `⟦图片N:描述⟧`, `⟦语音:转写⟧` | Media, with the archived description or transcript |
| `⟦转发的聊天记录 N条⟧` | A forwarded record, rendered as an indented block |
| `⟦依据:…⟧`, `⟦检索记录⟧` | Provenance of a bot reply and the retrieval trace that fed it |

### Member numbers

Two members sharing a display name is ordinary, so every person a prompt shows wears a
member number behind their name (`qqbot/core/member_numbers.py`). The number belongs to
a person: accounts merged into one person share it. The numbers follow the roster. The
roster lists everyone who has appeared in the group, whether or not anything is known
about them, ordered by first appearance, and is numbered in that order, so a newcomer
joins at the end and nobody else's number moves. The roster is therefore also where the
model finds the number of someone who is not in the current conversation. Anybody a
prompt shows who is not in the roster yet (a first message still being archived) is
numbered after it. Because the numbers depend on the roster alone, the conversation
moving never renumbers anything above the history. The same numbering runs through the
roster, the window, `search_history` results and the tool arguments that name people,
so the model @-s and filters by the number it read.

Numbers are never stored. The archive, retrieval traces and provenance markers hold
names only. Members' own @-mentions stay in their text as names.

The bot's own messages are rendered in the history as the `send_message` calls that sent
them (text, @-ed member numbers and replied-to line number as arguments), each followed
by a tool result carrying the message's line number, send time and provenance marker.
What the model reads of its own output is the shape it is asked to produce. In the
archive, the bot's line reads as the group saw it, `@name` openings included, with the
@-ed accounts kept as `at` segments so a restart rebuilds the same window.

## Pictures and voice

**Pictures are understood on arrival**, when the download link is freshest. Each picture
is downloaded, uploaded to the text backend's file store, and described in one line by
the vision backend; the description is written into the archived line. Descriptions
are the archival form: search, extraction and window rebuild after a restart all read
text. The description is cached by image content, so a repost costs nothing, and it
expires by age so that a better model gets to look again; refreshing is lazy, because
only a picture posted again is ever looked up. A picture the backend's content filter
declined is cached as a marked placeholder on the same clock.

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
guard. The transcript lands in the archived line like a picture description. API-based
speech backends remain available in the registry.

## Memory

### Storage

Nineteen tables in `sql/init.sql`, layered:

| Layer | Tables | Holds |
| --- | --- | --- |
| Archive | `raw_event` | Every event, append-only. `payload` is the platform's verbatim message and is never modified; `plain_text` is the derived reading, updated with descriptions and transcripts. |
| Identity | `entity`, `identity_account`, `alias`, `alias_evidence` | Persons, their accounts, and their names with evidence and scope. |
| Facts | `memory_fact`, `memory_fact_evidence`, `memory_candidate` | Temporal facts about persons and groups, their evidence, and the model's proposals awaiting validation. |
| Episodes | `episode`, `episode_participant`, `episode_event` | Summaries of stretches of conversation, with participants. |
| Index and queue | `embedding_index`, `memory_job` | Vectors, kept apart from what they index; the background job queue. |
| Operations | `reply_trace`, `cost_ledger`, `group_state`, `group_blocklist`, `user_agreement`, `image_cache` | Retrieval traces, spending, per-group switches and watermarks, blocks, consent, picture descriptions and file ids. |

Every table that carries a `group_id` indexes it first, and no retrieval path crosses
groups. The identity graph (which accounts form one person) is the one deliberately
global structure.

### Identity

An account is the strong identity. A person is an entity that owns one or more
accounts; a name is an alias with a scope (group or global), an evidence trail and a
status. Alias confidence is the maximum within an evidence channel and a noisy-OR across
channels. A platform display name only becomes a candidate on its first day, so a
rename game cannot confirm itself. Manual evidence (`/alias`, `/note`) is authoritative,
and a retired name stays retired against automatic evidence.

`/merge` repoints an account's rows to another person; `/split` gives an account its own
person again. Person-level operations reduce to two primitives: expanding a person to
their accounts on the write side (`/block` blocks the person) and aggregating by entity
on the read side (`/top` ranks people).

### Facts

A fact is a subject, a predicate from a closed set, an object, a validity window, a
confidence and evidence. Predicates are defined in `config/predicates.yaml` with their
Chinese rendering, cardinality, decay class and the rule shown to the extraction
model. A single-valued predicate closes the previous value when a new one arrives, so
"they moved" is expressible; multi-valued ones stand side by side. "One current fact per
subject, predicate and object key" is enforced by a partial unique index in the
database.

Confidence is a Wilson lower bound over distinct source events, so one mention earns
about 0.27, three about 0.53 and eight about 0.75. Re-confirmation accumulates evidence
in place. Facts decay by predicate class with half-lives of 90, 30 or 14 days, one
half-life per supporting event, so what a group repeats stays and a passing remark
fades. A decayed fact is superseded, never physically deleted; the archive is never
deleted at all.

### Extraction

Extraction runs once a night as the first stage of the nightly pipeline. It drains
everything unread, oldest first, in chunks of up to `memory.extract_window` messages,
each chunk cut at a conversation gap of at least `memory.batch_gap_min` minutes so that
batches end where conversations end. A remainder below `memory.drain_floor` waits for
the next night. There are no real-time triggers, so memory lags at most a day.

One model call reads one chunk, with the group's known facts alongside so they are not
re-learned, the group's standing knowledge from the persona, and the bot's own names so
that people addressing the bot are not filed onto whoever sits nearby. The bot's own
lines are rendered too, marked and off the roster, so the extractor reads both halves
of every conversation; a candidate quoting one of them fails validation.

The model may only propose. Names, facts and episodes arrive through function calls,
never free-text JSON. A pure-code validator checks that every quote matches verbatim
inside a single message of the batch and that the batch anchor reproduces exactly;
failures reject the candidate. The consolidator writes: newest wins within a fact
family, superseded values get a `valid_to`, confidence is recomputed from evidence,
and an incoming confidence only acts as a floor.

The extraction watermark (`group_state.last_extract_at`) records what was actually
read and never moves backwards. `/relearn` pulls it back exactly one window and forces
past the drain floor.

### Retrieval traces

What each reply looked up, with result digests, is stored in `reply_trace`, one row
per reply. Prompt assembly seats each trace directly before the reply it fed, so the
model can see what its earlier answers rested on. Traces are the bot's working notes,
not the group's memory: they are never archived in `raw_event` and are invisible to
search and extraction.

## Budget

Two levels, both in `qqbot/core/budget.py`:

- **The day.** `budget.daily_cny_cap` is a total over every group against one shared
  ledger. At the cap the bot stops answering and workers stop the paid half of their
  work until the date rolls over in the configured timezone. The running total is
  restored from `cost_ledger` on restart.
- **The reply.** `budget.per_reply_cny` wraps the tool loop; every backend books its
  own priced spend into it. The gate reads money already spent, never a forecast, so a
  model the price table does not know cannot silently disable the tools.

The monthly search allowance (`llm.search.monthly_quota`) is metered from the ledger's
calendar-month call count in the vendor's own unit; an advanced-depth search books two,
and page reads debit the same pool. Both caps are global: a per-group override of either
is ignored with a warning.

Attribution is task-local and feeds `/top` only. A reply's entire spend, including the
transcriptions, picture looks and searches it forces, is booked to the member who
addressed the bot. A picture's archival description is booked to whoever posted it.
Extraction and other communal spend stays unattributed.

Free work is not gated by money: in-process transcription runs on an exhausted day and
is still booked to the ledger at zero.

## Concurrency

One reply task per addressed message. A global semaphore at the provider layer
(`llm.text.max_concurrency`) bounds concurrent model calls. Background work rides the
database job queue (`FOR UPDATE SKIP LOCKED`, leases, and a partial unique index that
keeps one pending job per type and group). Extraction jobs hold a lease long enough
that a deploy overlapping a drain cannot run it twice.

## Scheduled jobs

| Schedule | Job |
| --- | --- |
| `schedule.nightly_cron` (02:30) | One pipeline in dependency order: memory extraction, memory decay, `pg_dump`, NapCat media cache cleanup. Between stages it waits, with a deadline, for the job queue to empty. |
| `schedule.report_cron` (00:00) | The daily report to the owners: spend by kind and model, cache hit rates, picture cache, search allowance, job backlog, new and muted groups, backup age, error digest, output-stripper counters. |
| resident | The job-queue worker, including lease reclaim. |

A restart in the middle of the nightly pipeline skips that night's remaining stages;
decay catches up the next night, and a missed dump shows up in the report's backup-age
line.

## Configuration model

`config/settings.yaml` is global. `config/personas/default.yaml` is the default persona;
`config/personas/group_<id>.yaml` states only what differs for that group, including
overrides of any setting. Settings are validated with pydantic and unknown keys are
rejected. New groups need no configuration.

Prompts are data: every instruction text the model reads is its own file under
`config/prompts/`, keyed by name in a manifest in code. A missing or empty file fails
the load. Marker formats, section headings and one-line mechanical notices stay in
code, because code both produces and parses them.

## Design decisions

| Decision | Rationale |
| --- | --- |
| Speak only when spoken to | Removes a whole class of judgement (should the bot interject) and every knob that came with it. |
| Structured memory with evidence, no prose profiles | Prose cannot be corrected line by line, cannot carry confidence, and tends to invent. Evidence-based confidence and per-class decay can. |
| Money as the only limit | Call quotas and token budgets are proxies that drift whenever prices change. Free work runs unbounded; paid work answers to a cap. |
| A tripped cap ends in one tool-less round | Money already spent should produce a reply. The overshoot is bounded at one round. |
| Function calls for every structured write | Free-text JSON needs parsing and invites drift; a schema is checked by the platform. |
| Text as the archival form | Search, extraction and restart rebuild read text; the model fetches pixels only when it wants them. |
| Media understood on arrival | The link is freshest then, each unique picture is paid for once, and groups the bot never answers in still get a readable archive. |
| Episodes are pulled, never pushed | A block of related past events pushed next to the incoming message misled reference resolution. The past reaches a reply only through `recall_events`. |
| Prefix-cache-friendly prompt order | A cache hit costs a small fraction of a miss; ordering, chunked eviction and batched extraction all serve the hit rate. |
| No fallback, no automatic downgrade | Failover is the least-tested path. A failure stops and is logged. |
| Search called directly, not the model's built-in search | Built-in search bills twice and inserts content where it breaks the prefix cache. |
| Exact vector scan, no HNSW index | The embedding dimension exceeds pgvector's HNSW limit; retrieval filters by group first, so an exact scan over a few hundred rows is both accurate and fast. |
| Reserved bracket grammar for markers | Makes system markers unforgeable by construction instead of asking the model in prose not to be fooled. |
