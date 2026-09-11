# QQ Group-Chat AI Bot · Design Document

> Python · single personal machine · 2026-08-28 · v6 (describes the implemented system)
> 中文版（权威版本）：[QQ-AI-Bot-设计文档.md](QQ-AI-Bot-设计文档.md). Where the two disagree, the Chinese version wins.

An AI bot that lives in QQ group chats: it must answer when @-mentioned or called by name, and stays silent otherwise; it remembers the group's people and past events; it reads pictures and understands voice messages (receive-only). v5 was the pre-implementation spec; this version describes the system as it actually stands after the August 2026 rebuild.

Code comments cite the design doc as "design doc section N", "design goal N" and "(DN)"; two-digit section numbers (46, 52, 63, ...) follow the v5 spec's numbering, kept as stable anchors, and do not map to this version's chapters. Grep for those references before renumbering anything.

## Design goals

Cited by number from code comments; the numbering must not be reshuffled:

1. **Economy**: a prefix-cache hit costs ~1/30 of a miss, and every arrangement (prompt layering, chunked window eviction, batched extraction) serves the hit rate; one call reads a stretch of conversation, never one call per message.
2. **Measured, not assumed**: model, dimension and threshold choices follow measurements (embedding dimensions, the RAG design, cache hit rates were all measured before deciding).
3. **What can go undone, goes undone**: a mechanism whose upkeep exceeds what it saves is a liability (the three questions in appendix D8).
4. **Group isolation**: every table carrying a group_id indexes it first, and no retrieval path crosses groups; the one exception is a global alias an owner establishes by hand.
5. **Money is the only limit**: free things run without count; paid actions answer to money, never to call quotas — proxies that drift whenever prices move.
6. **A limit stops the spending**: the constraint is bounded consumption, not money already spent going to waste. The daily cap is checked before anything is spent and means silence; a limit tripping mid-reply (the per-reply cap, the monthly search allowance) stops everything that would still cost money and adds one tool-less wrap-up round answering from what was already fetched - an overshoot of exactly one bounded round.
7. **Structured output goes through function calls**: whenever the model writes into the system (memory candidates, aliases) it does so via tool-call schemas, never free-text JSON to parse.
8. **Text is the archival form**: the model reads original pixels; the archive stores text descriptions — search, extraction and restart-rebuild all consume text.

## Decision table

| Decision | One-line rationale |
|---|---|
| Protocol side: NapCat (OneBot v11, reverse WS) | The only actively maintained option; the third-party-client ban risk is accepted |
| Framework: NoneBot2 | The most mature Python ecosystem; its plugin system stays out of the way |
| Text and vision are both `deepseek-flash` (V4.1-Flash, multimodal, from 2026-09-10) | The reply model reads pictures itself, so originals reach the prompt and the read-it-for-me `inspect_image` is gone; only multimodal models are used from here, and the flag that asked whether the text model could see went with it; descriptions are still written for every picture, because search, extraction and restart-rebuild consume text alone and descriptions are written at arrival - including in groups the bot never answers in |
| Model config is organised by use, not only by capability: `llm.text` is the reply path, `llm.text.extract` is the same account read differently, `llm.vision` describes. Each use names its own model, `reasoning_effort` (off/low/high/max) and timeout | Thinking bills at output price, and the counterintuitive rules go with it: "off" consistently loses alias reconfirmation, so low is the floor; all uses currently run low. Endpoint, credential, concurrency and retries stay shared - they describe the account, not the task, and a second copy of an endpoint is how one goes stale |
| ASR: in-process sherpa-onnx (SenseVoice int8, CPU); embedding: Bailian `text-embedding-v4` (2048-dim, API) | SenseVoice beats the Whisper family on colloquial Chinese in real-world tests, and a small non-autoregressive model decodes in sub-second CPU time at zero rates; embedding stays API-side (D 5.2) |
| Search: Tavily free tier (1000/month), called directly (D5) | Zero marginal cost; never the model's built-in search (double billing, breaks the prefix cache) |
| Storage: Postgres 17 + pgvector on local disk | Twenty tables and one vector store; NFS fsync is unreliable, the NAS is for backups only |
| Trigger = must-answer on @/nickname/a quote of the bot's line, silence otherwise | Arbiter, interest vectors and adaptive intensity all deleted (appendix D1/D7); one rule, no knobs |
| Structured memory: entity/alias/fact/episode layers with evidence-driven confidence | Wilson lower bound + channel fusion + per-predicate decay; v5's prose profiles are dead (appendix D2) |
| Text-only output | Pictures and voice are understood inbound only; the output layer is just `clean_reply()` |
| No fallback, no automatic downgrade (D6) | On failure, stop; being addressed yet silent gets its own log line, so silent failure stays diagnosable |
| Credentials from `.env` | compose injects `${VAR:?}`; a missing entry fails `up`, not an API call hours later |
| Runtime traffic is direct; the one exception is the search client via an HTTP proxy on the host | The proxy hangs only on a path that is free and allowed to fail; the model hot path gains no extra single point |

---

## 1. Risk premise

Third-party protocol clients violate Tencent's ToS; the account can be banned. Accepted, and enforced in code:

- A secondary account, warmed up before going live; fixed IP.
- No reply rate cap: each addressed message gets exactly one reply attempt, and the pace is bounded naturally by the budget and the provider-layer concurrency semaphore (`max_concurrency: 3`).

## 2. Architecture

```
QQ ←→ napcat container ←OneBot v11 reverse WS→ bot container ←asyncpg→ postgres+pgvector container
```

### Message pipeline

```
message arrives
 ↓ dedup
 ↓ write raw_event                      async, unconditional; replays never land twice
 ↓ free resolution + pictures           @s, quotes, forwards; pictures download now →
                                        Files API upload → async description backfill
 ↓ trigger (§4) ──no──→ done            each message decides for itself; an addressed one
                                        cuts its context slice on the spot and spawns a
                                        concurrent reply task (quote, @ and billing all
                                        belong to this message's sender)
 ↓ in the task: daily-budget gate → block (withholds only the reply) → agreement gate
 ↓ retrieval: roster/cards → group knowledge (episodic memory is never pushed -
   the model pulls it with recall_events)
 ↓ prompt assembly (cache-friendly ordering §6.2, recent originals inline)
 ↓ money-bounded tool loop (§2, engine)
 ↓ strip_markdown → send (quoting the trigger message and @-ing its sender) → the bot's own reply is archived too
```

### The conversation engine (agent loop)

No round quota, no per-tool quota; the boundary is money:

```
with BUDGET.scope(per_reply_cny):
    loop (runaway fuse at 20 rounds, only against a backend billing zero):
        call the model (deliberation per config, every tool always offered)
        no tool calls → return the text
        budget gate: the per-reply cap is already spent → one tool-less wrap-up
            round answers from what is already fetched. The first round always runs.
        execute tools (an identical repeated call is answered in words, not re-run);
            monthly search allowance exhausted → the same wrap-up round
        results appended as tool messages, after the cache boundary
```

Five tools: `web_search` (Tavily, via proxy), `search_history` (boolean search over this group's L0 archive: Lucene syntax parsed by luqum - space=AND, OR, `-`exclusion, parentheses, quoted phrases, only the boolean subset accepted and the rest refused in words; the AST compiles into one parameterized ILIKE expression, member words entering SQL only as parameters; narrowable by speaker and days; each hit returns wrapped in `retrieval.history_context` lines of surrounding conversation each way - chat is fragments, and a matched line is routinely a bare answer to the line above it - with touching windows merged and blocks separated by an ellipsis line; the filters pick the hits, the context is whatever actually surrounds them; a matched message comes back whole and the result carries no length quota - long messages are 1% of the archive and carry most of what a search is for, so a fixed per-message width cut exactly them; the size is already settled by hits x context lines x max_msg_len, and past that by money, which a character budget could only duplicate more bluntly), `recall_events` (vector recall over this group's episodes; each recalled episode arrives framed by `retrieval.episode_context` neighbours either way in group time - an episode summarises one stretch of conversation, and its cause and consequence sit in the adjacent stretches - windows merged and blocks ellipsis-separated the same way, undated episodes standing alone), `read_url` (one page's readable text, off the same monthly search allowance), and `open_image` (one picture's original, by number, handed to the model as a file block). All five are free - a SQL query, an HTTP fetch, a file upload; the boundary is still money, because every round carrying them is a paid model call. Their division of labour is written into the tool descriptions and deliberately de-overlapped.

### Concurrency

A global `max_concurrency: 3` semaphore at the provider layer; one independent reply task per addressed message, running concurrently (each with the context slice cut at its arrival; budget attribution is contextvar task-local); background work (extraction, embeddings) rides a DB job queue (FOR UPDATE SKIP LOCKED + leases + a pending-dedup index).

### Output

Only `clean_reply` (strips the Markdown QQ cannot render - emphasis, headings, code fences, rule lines, link syntax - while leaving what plain text carries on its own: hyphen bullets survive and asterisk ones are folded into hyphens, so a reply that is genuinely a list reads as one; every system marker - line numbers, timestamps, provenance, trace lines, quote pointers, name tags, in both bracket generations - and text-form tool-call markup, with a final pass replacing any leftover reserved brackets); length truncation is applied by the engine before send. There is no other exit.

## 3. Scope

In: must-answer when addressed; group memory (people, events, group knowledge); picture understanding (original pixels for the model + archived descriptions); voice transcription; web search; archive search; a command surface that recognises only the bot's owners, never QQ group admins - with two carve-outs: /who /note /alias /forget are open to any member against themselves only (at the same full manual trust as the owner's hand; /agree and /terms are theirs by nature), and the read-only /card /stats /top /groupstats are open whole; /help filters its listing per reader, and anything beyond the boundary draws silence. A user-agreement gate holds replies from members who have not sent /agree: they get a one-line pointer instead, rate-limited, transcribed as a notice about the member rather than as the bot's own words (the model repeats what it reads as its own earlier answer) - /terms shows the full text, /agree accepts, and those two are the only commands that answer before consent; the version and the text file's path are mandatory config (the agreement block in settings.yaml; the file loads with the config, a missing or empty one fails the load, /reload applies both together), acceptance is recorded per group, account and agreement version, reading and archiving are untouched, owners exempt. A block withholds exactly the reply - the account's messages still arrive, archive and feed memory, keeping context coherent; blocks can carry a duration (30m/12h/3d), lifted lazily the next time the account addresses the bot, no scheduler. Group notices (joins, leaves and kicks, recalls, bans, pokes) are transcribed as one bracketed line into the window and the archive, and never draw a reply.
Out: speaking uninvited (the arbiter is gone); sending pictures or voice; cross-group memory; multiple accounts.

## 4. Trigger

One rule: an @, a nickname hit, or a quote of one of the bot's own lines still in the window (even with the reply button's auto-@ stripped by hand) → must answer; otherwise silence. Nicknames match as jieba **words**, not substrings (小夜曲 does not trigger 小夜). Muting (/mute) outranks being addressed.

v5's probability gate, Flash arbiter and adaptive p_max are deleted as a layer: once the bot only speaks when spoken to, the judgement they existed for no longer exists (appendix D1, D7).

## 5. Models, external services, and the budget

### 5.1 Service roster

| Capability | Backend | Model / endpoint | Credential |
|---|---|---|---|
| text | deepseek | `deepseek-flash` (multimodal, low thinking; `llm.text.extract` can retarget extraction's model, grade and timeout alone - model currently empty, so extraction runs the reply model) | `TEXT_API_KEY` |
| vision (describes only) | deepseek | `deepseek-flash` (the same model the reply path uses, still a separate capability: descriptions are written at arrival for every picture, including in groups the bot never answers in) | `TEXT_API_KEY` |
| asr | sherpa (in-process) | SenseVoice int8 (sherpa-onnx, CPU; weights on the `models/` mount, fetched once by `fetch_asr_model.sh`) | none - zero rates, still booked |
| embedding | dashscope | `text-embedding-v4`, 2048-dim | `MEDIA_API_KEY` |
| search | tavily | `/search`, basic depth | `SEARCH_API_KEY` |

Five capabilities, five independent config blocks (`llm.<capability>`), each carrying its own endpoint, model and credential name, never borrowing — embedding once borrowed vision's wiring and was silently dragged along when vision moved platforms, muting every reply (a real incident). The registry picks the implementation class by `backend`; platform quirks (cache-usage fields, the no-deliberation parameter) live only in those classes.

Uploading a picture belongs to the text capability, not vision: the file has to live on the account that will read it, so `TextModel.upload` returns whatever handle the platform issues and everything above the providers layer speaks one neutral block, `{"type": "image", "id": ...}`. Each backend translates that block on the way out (DeepSeek takes a flat `file_id` part and rejects the nesting OpenAI documents). The prompt builder never learns a vendor's spelling, which is what keeps a second platform a registry entry instead of an edit to `prompt.py`.

### 5.2 Retired (pitfalls)

- Local embedding (bge-small + the whole PyTorch/ONNX/NPU line): the API's 2048 dimensions measured better separation and need no upkeep; weights, HF mirrors and the fetch script are all gone.
- Zhipu search: no reason to exist once Tavily is free.
- Bailian qwen vision rendering: retired when DeepSeek vision shipped.
- The model's built-in `enable_search`: double billing, and its uncontrollable insertion point breaks the prefix cache (D5).

### 5.3 Pictures and voice

**Pictures are handled on arrival** (the CDN link is freshest then): download → an async one-line description backfills `plain_text`. The description is the archival form (design goal 8): search, extraction, restart-rebuild and every picture past the prompt's rail all consume text alone, and descriptions are written at arrival for every picture - including in groups the bot never answers in, where the reply path never runs. The reply model has been multimodal since 2026-09-10: the newest `prompt.max_images` pictures ride behind the messages that posted them as originals, and the model reads the pixels. A message brings all of its pictures or none - the blocks are paired with that message's markers by order alone, so a partial set is a pairing the model gets wrong rather than a partial one. The describing call is cached by image content (a repost costs nothing), rate-limited per minute, and behind the daily cap. The cached description expires after `llm.vision.description_ttl_days` (15): models improve, and a vendor can put a better one behind an unchanged id, so age is what tracks description quality - a model-id stamp would not see that. Refreshing is lazy by construction, because only a picture posted again is ever looked up: one nobody reposts is never paid for twice however old its description gets, and transcript lines already archived keep the wording they were written with. A picture the backend's content filter declined is cached as a marked placeholder on the same clock, so a later backend gets to look again - before this, that first refusal blinded the bot to that picture permanently. Anything past the rail is fetched by number through the reply loop's `open_image` tool - every picture marker in the transcript carries its number (⟦图片3:description⟧), a position in one render like the line numbers. Picture references (`image_refs`) ride each message for the window's lifetime, are re-parsed from the archived payload after a restart, and QQ's file id trades for a fresh link at any time. The old `inspect_image`, which put the question to a second model and returned a sentence, is gone: once the model can look for itself the middleman is both redundant and worse, since it could only ever answer the one question it was given.

**Voice now follows pictures: understood on arrival** (since 2026-09-07): clips are far rarer than pictures and bill cheaper by the second, and an untranscribed clip is a blind spot in extraction - which reads only text, so a clip nobody asked about never reached memory. Arrival fetches NapCat's transcoded WAV via `get_record` and backfills the transcript into `plain_text`; a failure stays unsettled and the reply path's settle retries. The old pay-at-reply discipline is retired outright (D11 revised). Since 2026-09-08 transcription runs in-process (the `sherpa` backend: sherpa-onnx + SenseVoice int8, two CPU threads, a ten-second clip decodes in under a second): zero rates, no credential, no endpoint. The daily cap only stands in front of backends that spend - free transcription passes straight through it - and the per-minute gate stays on as a CPU guard. The per-second API backends remain in the registry, one config line away.

### 5.4 The budget — money as the only limit

Two levels, both in `core/budget.py`:

- **The day**, `daily_cny_cap: 3.0`: at the ceiling the bot stops answering and workers stop the paid half of their work until the date rolls over in the configured timezone. The running total is restored from `cost_ledger`; a restart does not reset the day.
- **The reply**, `per_reply_cny: 0.30`: `with BUDGET.scope(cap)` wraps the tool loop, and every backend books its own self-priced spend into it. Cap spent → tools stop and the wrap-up round answers. The gate reads money already spent, never a forecast of the next round: a forecast needs a price for the model, and a model the price table does not know fails it forever - the tool loop then goes dark at zero spend, looking exactly like a model that will not search.

Attribution (a contextvar; it feeds the /top leaderboard only and never changes the shared budget): the bot only ever replies when spoken to, so a reply's entire spend - the transcribes, image looks and searches it forces included - is booked to its initiator, the sender of the last message that @-ed the bot or said its name; a picture's archival description is booked to whoever posted the picture; extraction and other communal spend stays unattributed.

A limit stops the spending (design goal 6): the daily cap, checked at entry, means silence; a mid-reply limit (the per-reply cap, the monthly search allowance) does not discard the reply - unexecuted tool requests are completed with a placeholder result and one tool-less wrap-up round answers from the material already fetched, with the model told the allowance is gone. The monthly search allowance (1000 credits) is metered off the ledger's calendar-month calls count — booked in the vendor's own unit, so an advanced-depth search books two, and `read_url` page reads debit the same pool. A transport failure is a different thing — the model is told "search failed" and carries on, because a broken network is an error, not a limit.

Price tables live inside the backend classes, including DeepSeek's peak/off-peak split (weekdays 9–12 / 14–18 Beijing, ×2) and the two-era table around the 2026-08-17 repricing; an unknown model bills at the priciest tier and warns once, so a rename trips the gate early rather than under-billing. Calls are booked under the model that actually served them, which is not always the one requested. All figures verified against vendor pages (last checked 2026-09-09: unchanged, no repricing announced).

## 6. Memory and context

### 6.1 Storage

Twenty tables, layered (L2 is deliberately absent: every reference the system receives is an @ or a quote where the platform states the account outright, so there is no judgement to record):

- **L0 `raw_event`**: append-only archive; `payload` is the platform's verbatim message and is never modified, `plain_text` is the updatable derived reading (picture descriptions and voice transcripts land there).
- **L1 `entity` / `identity_account` / `alias`(+evidence)**: person, account and name kept apart. Person-level operations reduce to two primitives: write-side **expansion** (`accounts_of_person` - /block acts on the person) and read-side **aggregation by entity** (join `identity_account`, group by entity_id - the /top leaderboard ranks people). A merge physically repoints the account rows, so read-side aggregation is correct even for merges declared after the fact; a new person-level feature picks one of the two instead of inventing its own. The account is the strong identity; names carry scope, an evidence trail and a status. Alias confidence = max within a channel, noisy-OR across channels; a platform name's first day only makes it a candidate (against rename games); manual evidence is authoritative, and a retired name stays dead against automatic evidence.
- **L3 `memory_fact`(+evidence) / `memory_candidate`**: temporal semantic facts, 29 predicates, multi-valued ones distinguished by `object_key`; "one current fact per subject and predicate" is enforced by a partial unique index in the database itself. Confidence is earned via a Wilson lower bound over **distinct source events** (1 ≈ 0.27, 3 ≈ 0.53, 8 ≈ 0.75); re-confirmation accumulates evidence in place instead of adding rows. Decay is per predicate class (stable 90d / fast 14d / default 30d), with more evidence meaning a longer half-life. A predicate is one entry in `config/predicates.yaml` carrying all of it - meaning, cardinality, decay class, memory type, the Chinese it renders as, and the line the extraction model is shown - so adding one is a config edit and cannot half-happen. The prompt must carry the slot that block renders into, or the config fails to load.
- **L4 `episode`(+participant/event)**: event summaries, vector-searched.
- **L6 `embedding_index` / `memory_job`**: vector projections decoupled from what they project; the job queue.

The conversation window (immediate context) lives in an in-memory deque and is rebuilt from L0 after a restart — the bot does not rejoin a conversation it was part of thirty seconds ago knowing nothing. The tool loop's trajectories (what each answer looked up, with result digests) live in their own table (`reply_trace`) — the single source of truth; the deque holds only conversation. Prompt assembly queries the table for the window's replies and seats each `⟦检索记录⟧` entry directly before the reply it fed: the bytes come from an immutable row and the position from the reply, so the rendering is stable turn over turn; a reply sliding out of the window simply stops being asked about, so eviction needs no bookkeeping and restarts need no special handling. They are the bot's working notes, not the group's memory — never in L0, invisible to search/extraction — and are kept forever like L0, by the owner's call: rows are small and disk is not the constraint. The extraction watermark (`group_state.last_extract_at`) records what was actually **read** and never moves backwards; the one sanctioned exception is an explicit /relearn reset.

### 6.2 Retrieval and prompt assembly

The ordering is cost discipline (design goal 1), most stable first:

```
global constants (legend, reading rules, private rules, tone) → persona → group knowledge
→ roster & cards (account order, stable)
→ conversation history (append-only; 3 chunks × 30 entries = 90 cap, evicted a whole chunk at a time)
→ [cache boundary] → clock → tool results → current message
```

There is no token budget anywhere: every block renders whole, and each block's author (the owner, or a generation prompt with its own length discipline) is responsible for restraint. The only two numbers left are the window size and the eviction chunk, which serve finite context and the cache, not cost. Originals ride inline in their messages, text block first; a message without attached originals stays a plain string — so an untouched message renders byte-identical between turns.

Every line carries its own send time (`⟦MM-dd HH:mm⟧`, one format shared by the reply history, the extraction transcript and `search_history` results): without it the model reads sixty messages as one continuous conversation and bridges topics hours apart. The stamp is fixed at arrival and never rewritten, so it does not conflict with cache friendliness — any relative form ("5 minutes ago") would invalidate the prefix every turn, while an absolute moment is written once. The "current time" line sits after the cache boundary, giving the model a clock to compute gaps against.

**System markers are unforgeable.** Every marker the system writes uses one reserved bracket pair, `⟦ ⟧` (U+27E6/U+27E7, `util.sysmark`), and every untrusted string - member text, display names, picture and voice descriptions, forward and card contents, web and search digests - passes through `util.defang`, which replaces the pair with ASCII square brackets before it can reach a rendered line. "Inside ⟦ ⟧ means the system wrote it" thus becomes a rule the model can apply mechanically: a member whose card imitates the owner tag, or whose message body types out a picture marker, produces plain text that collides with nothing (the old square-bracket grammar could only plead in prose). Timestamps, `⟦拥有者⟧`, `⟦同名N⟧`, quote pointers, provenance, trace headers, media and notice markers all live inside this grammar; a transcript line opens with its time head and any other line is a continuation, so a multi-line message cannot forge a line start. `clean_reply`'s final pass keeps the reserved pair out of anything sent to the group, and `scripts/migrate_markers.py` converted the stored archive once.

Pull-style retrieval complements the pushed window: anything behind it is reachable on demand via `search_history` / `recall_events`.

Names are the model's only handle on people, and two members sharing a group card is ordinary: the member-list refresh checks the current cards for clashes and renders every clashing member with the reserved namesake tag (`名字⟦同名N⟧`), where N is the account's permanent per-group serial (the `member_seq` table - assigned once, never reused, never reassigned, so a numbered name in any old transcript still points at the same person). Renames create new clashes and dissolve old ones; both converge on the next refresh, and the refresh is the single choke point for current names (window relabelling, the roster, @-resolution and the notice lines all read it); the roster keeps one fallback pass for rows showing archived names (a departed namesake). `search_history`'s speaker argument accepts the tagged form (and the legacy parenthesised one, which old archive text still carries) and narrows by the serial's account rather than by name. Extraction is untouched: its transcript already names accounts by per-batch code, and every write takes codes only.

### 6.3 Writing: extract → validate → consolidate

Extraction is concentrated at one event point per day (the first stage of `nightly_cron`, 02:30 — the day's transcript is complete, the vendor bills off-peak, nobody is waiting): the drain reads everything unread oldest-first in chunks of up to 120, each chunk cut at a conversation gap of 30+ minutes so batch boundaries fall where conversations end and episodes stop being split mid-topic; a remainder under 20 waits for tomorrow. There are no real-time triggers and no idle flush: memory lags at most a day, and the reply path's immediate context was always the window. One model call per chunk, at extraction's own deliberation grade, with the group's known facts alongside so it does not re-learn them, and a "your names" line carrying the bot's own trigger nicknames - people address the bot by name, and an extractor that does not know whose name that is files it onto whoever sits nearby (measured in production: the bot's name became a confirmed alias of another bot's account). The comprehension layer is shared with the reply path: the joke-discernment text (tone_rules) is one file read by both, each path appending its own consequence note (reply: how to play along; extract: what not to record), and the owner's hand-written group background (the persona's group_knowledge) heads the known block - what it takes to understand the conversation is no longer the reply path's private supply, and each group's extraction input is assembled from its own material. The bot's own lines (command answers included) render into the transcript too, marked `名字⟦你⟧`, codeless and off the roster: the extractor reads both halves of every conversation, while the marked lines stay comprehension-only - they never match a quote, so a candidate citing one dies in validation, and comprehension coexists with self-loop immunity. Output arrives by function call (design goal 7); candidates carry the batch anchor plus batch size, so validation replays the exact rows. /relearn pulls the watermark back exactly one window and forces past the drain floor. A **pure-code Validator** gates them: verbatim quotes must match inside a single message, the batch anchor must reproduce; failures reject the candidate. The Consolidator writes: newest wins within a fact family, superseded values get `valid_to` instead of deletion, Wilson recomputes confidence, and incoming confidence only acts as a floor. The model proposes; code decides.

### 6.4 Archiving and forgetting

L0 is never deleted. Facts decay by predicate class (the nightly pipeline's stage after extraction drains); decayed-past-threshold means superseded, not physically removed. Episodes are append-only. `pg_dump` is the stage after that, 14 kept.

## 7. Deployment

### 7.1 Three containers

Three compose services: postgres (pgvector image, bind mount `data/pg`), napcat (`NAPCAT_ACCOUNT` quick-login, so restarts skip the QR scan), bot. All credentials come from `.env`; `${VAR:?}` makes a missing entry refuse to start. All three containers cap json-file logs at 20m×3; the app log is a RotatingFileHandler at 5MB×3.

**Deploys go through `scripts/deploy.sh` only**: whole-directory replacement + `up -d --build` + a content-fingerprint comparison inside the container (`_fingerprint.py`), non-zero exit on mismatch. `docker compose restart` does not rebuild the image and silently runs old code — a lesson paid for twice. SSH uses the `server` alias from `~/.ssh/config`.

### 7.2 Host footprint

Generation is entirely in the cloud; the box runs only asyncpg and HTTP clients. Postgres carries SSD tuning (`checkpoint_completion_target` flattens write spikes). No CPU pinning, no monitoring dependency (D6).

### 7.3 Network

Runtime traffic is direct. The only two exceptions: the search client points at an HTTP proxy on the host (`llm.search.proxy`, `172.17.0.1:7890` from inside the container) — hung on a path that is free and allowed to fail; and `BUILD_PROXY`, which affects image builds only.

### 7.4 Scheduled jobs (APScheduler)

| cron | job |
|---|---|
| `nightly_cron` 02:30 | the nightly pipeline, stages in dependency order: memory extraction (the single event point; drains everything unread) → memory decay → pg_dump (14 kept) → NapCat media cache cleanup (7 days). Between stages the pipeline waits for the job queue to empty (deadlines: 3h for extraction, 30min for decay; on timeout it logs and moves on) - decay must not run while a group's extraction retry is still queued, and the dump should carry what the night learned, and a clock cannot enforce either (extraction drains through the job queue, whose retries back off up to an hour). Accepted cost: a restart mid-pipeline skips that night's remaining stages - decay catches up next night, a missed dump trips the report's backup-age alarm |
| `report_cron` 00:00 | owner daily report, sent the moment the ledger day closes (the same midnight boundary the budget resets on): spend breakdown, per-use cache hit rates (reply/extract/other), monthly search allowance, job-queue depth, new groups, error digest |
| resident | job-queue worker (incl. lease reclaim; extraction jobs hold a 30-min lease so a deploy overlap cannot double-run one) |

## 8. Configuration

`config/settings.yaml` (global) + `config/personas/group_<gid>.yaml` (persona and per-group overrides; only differences are written). The discipline: **every field must have a reader and must be a real choice** — single-value knobs and switches for dead mechanisms get deleted (`heavy_when`, the token-budget block and `enabled_groups` all went that way). Validation fails loudly at load (pydantic `extra="forbid"`). New groups need zero config: served from the first message, default persona, visible in the daily report.

**Prompts are data**: every model-facing instructional text lives as its own text file, and the files are the source of truth; the `prompts:` mapping in settings.yaml names each key's path (resolved relative to the config directory), code holds only the key manifest (`settings.PROMPT_KEYS`), the mapping and the manifest are checked against each other both ways, and a missing key, a stray key, or a missing or empty file refuses to boot; edits apply on `/reload` (the extraction prompt is composed at worker construction, so on restart). The incident notes behind each wording live in `config/prompts/README.md`. Marker formats, section headings and one-line mechanical notices stay in code — code both produces and parses them.

## 9. Storage

Local SSD; `data/pg` bind-mounted; the backup directory can point at the NAS. The DB is ~20MB today. 2048 dimensions exceeding pgvector's HNSW limit is a deliberate trade: retrieval filters by group first, leaving a few hundred rows, and an exact scan is both more accurate than an approximate one and fast enough (a measured conclusion, design goal 2).

## 10. Operations and roadmap

- Observability: `/stats` (budget, call counts, monthly search allowance, per-use cache hit rates), `/groupstats`, the 09:00 daily report (now carrying the output-strip counters - every hit is a near-miss leak, so source discipline regressing shows up in a report instead of an owner's eyeballs).
- Debugging: `/debug N` captures the next N model rounds' full request messages and raw responses into `logs/debug/` - see what the model was actually shown instead of inferring it; self-disarms when exhausted, and a restart disarms it too.
- Behavioural evals: `scripts/eval_replies.py`, run by the owner on the workstation against the real model - deterministic assertions (no transcript markers reproduced, no self-@, injected instructions stay inert, no prompt leakage) plus observe-only cases for human eyes. Three verdicts: PASS / GUARDED (the model wrote it and the output stripper caught it - the source is regressing) / FAIL. Costs real calls (~CNY 0.02 a run) and deliberately stays out of CI (design goal 5); run before and after any prompt or model change.
- The launch checklist is mechanised as `scripts/preflight.py`: one real minimal call per capability, verifying credentials, reachability and model availability in one pass.
- Known weaknesses (accepted deliberately): alerting has exactly one channel (the daily report); the bot container has no healthcheck (NapCat's reconnect is the backstop); the daily cap is checked at pipeline entry, so concurrent replies can overshoot it slightly; and once the monthly search allowance is gone, questions that want a search stay silent for the rest of the month — the direct consequence of limits-mean-silence, chosen knowingly by the owner.
- Testing: 12 suites (`tests/run_all.py`) against a real Postgres (the qbot-pgtest container); commands.py/tasks.py cannot be imported by tests (registration at import time), so AST-based guards (syntax, attribute resolution, permission-gate presence) cover them.

## Appendix: decision record

Kept for reference; ~~struck-through~~ entries were overturned by later practice.

**D1 ~~Adaptive trigger intensity~~ → the whole uninvited-speech path deleted**: adaptive p_max saved hand-tuning, but "should it butt in" stopped being a question once the bot only speaks when spoken to. What was deleted was not a parameter but the problem.

**D2 ~~Unstructured memory~~ → structured with empirical confidence**: prose profiles once invented a group member outright, and could not be corrected line by line. The current design's confidence is accumulated by evidence, overridable by command, and decays by class — none of which prose can do. Literature: Wilson lower bound; multi-channel evidence pooling ([arXiv 2608.11275](https://arxiv.org/abs/2608.11275)); uniform decay measured worse than none (ScrubJay).

**D3 NPU unused**: workload profile mismatch (as in v5). The local-embedding line was later deleted wholesale, dissolving the question.

**D4 No pHash**: ~1/3 false-positive rate on text memes, and a wrong description would be amplified through memory; md5 comes free from the protocol side and suffices (as in v5).

**D5 Search wired directly**: built-in search double-bills and its insertion point breaks the prefix cache. Still true in the Tavily era.

**D6 No fallback / no automatic downgrade**: failover is the least-tested code path. Being addressed yet silent is the one failure indistinguishable from breakage; a limit at its ceiling is the opposite case, where silence is the defined meaning (goal 6, revised 09-07 to stop-the-spending mid-reply). (The old refinement - per-turn episode recall degrading to a reply with less memory - was deleted along with the push itself, see D14.)

**D7 Interest-vector layer deleted**: its upkeep exceeded what it saved (as in v5).

**D8 Three questions for any mechanism**: does the problem it prevents actually occur? Does something simpler already cover it? Will it grow complexity of its own?

**D9 One vision model for everything** (2026-08-27): `vision-exp` matches V4-Flash text at the same price and reads images; switching replies off Pro cut ~3x and removed tier routing. Weaker text than Pro is the accepted price.

**D10 ~~Deliberation off everywhere~~ → graded** (2026-08-27, revised 08-29): one extraction pass once thought for 6k tokens to produce candidates worth a tenth of that - the schema and Validator carry the think-it-through duty. But fully-off made replies noticeably dumber and the owner reversed course: each of the three paths now carries its own config grade (`reasoning_effort`: off/low/high/max, mapping to the vendor's parameter of the same name; off selects each backend's no-thinking route), all currently at low; replies think little even at high, so it costs fen-level money.

**D11 ~~Pictures~~ Media understood on arrival** (2026-08-27; extended to voice 09-07): the link is freshest, each unique picture is paid for once (cached), and groups the bot never answers still get a readable archive. Voice followed: extraction reads only text, so pay-at-reply kept every unasked-about clip out of memory forever, and clips are rarer and cheaper than pictures - the pay-at-reply discipline is retired outright.

**D12 ~~Limits mean silence~~ → limits stop the spending** (2026-08-27, revised 09-07): the three caps were once unified on silence, money already spent discarded with the reply. The owner ruled the constraint is bounded consumption, not strict non-overshoot: a mid-reply limit now ends in one tool-less wrap-up round, so the money spent produces a reply and the overshoot is bounded at exactly one round; the daily cap sits before any spending and still means silence.

**D13 Token-budget machinery deleted** (2026-08-27): money is already the only limit; token caps were a second budget dressed as layout. The surviving message-count window serves context and cache, not cost.

**D14 Episodic memory pulls only, never pushes** (2026-09-07): a per-turn block of related events used to be pushed - participant-filtered by the turn's speaker and @-targets, vector-ranked, seated right above the incoming message. A real misfire: an elliptical question @-ing a member resolved against that member's recalled past instead of the conversation, and the pushed block - undated, unexplained, in the position most sensitive to reference resolution - was the structural lure. The push is deleted; the past reaches a reply only through the recall_events tool, and every reply saves an embedding call. What arrives unasked is limited to what every reply needs.

### Key references

- [NapCatQQ](https://github.com/NapNeko/NapCatQQ) · [OneBot 11 segments (NapCat)](https://www.napcat.wiki/onebot/sement)
- [DeepSeek API docs/pricing](https://api-docs.deepseek.com/zh-cn/quick_start/pricing) · [Vision guide](https://api-docs.deepseek.com/zh-cn/guides/vision) · [Files API](https://api-docs.deepseek.com/zh-cn/guides/files_api)
- [Tavily /search](https://docs.tavily.com/documentation/api-reference/endpoint/search) · [API credits](https://docs.tavily.com/documentation/api-credits)
- [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) · [SenseVoice](https://github.com/FunAudioLLM/SenseVoice) · [text-embedding-v4](https://help.aliyun.com/zh/model-studio/text-embedding-v4)
- Confidence & memory: Wilson score interval (one-sided z=1.6449) · multi-channel evidence pooling [arXiv 2608.11275](https://arxiv.org/abs/2608.11275) · empirical memory decay (ScrubJay: uniform decay worse than none)
- [pgvector](https://github.com/pgvector/pgvector) · [pHash false positives on memes](https://arxiv.org/html/2408.08126v1)
