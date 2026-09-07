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
6. **A limit reached means silence**: any cap hitting its ceiling (daily budget, per-reply budget, monthly search allowance) drops the reply outright; there is no degraded answer.
7. **Structured output goes through function calls**: whenever the model writes into the system (memory candidates, aliases) it does so via tool-call schemas, never free-text JSON to parse.
8. **Text is the archival form**: the model reads original pixels; the archive stores text descriptions — search, extraction and restart-rebuild all consume text.

## Decision table

| Decision | One-line rationale |
|---|---|
| Protocol side: NapCat (OneBot v11, reverse WS) | The only actively maintained option; the third-party-client ban risk is accepted |
| Framework: NoneBot2 | The most mature Python ecosystem; its plugin system stays out of the way |
| Text: `deepseek-v4-pro` (replies + extraction); vision: `vision-exp` describes only | Lived experience judged vision-exp's language too weak - pro is a necessity; pictures return to description form, with the inline-originals machinery kept behind `reads_images` for a future vision-capable pro-grade model |
| Deliberation graded per path: replies, extraction and describing each carry a `reasoning_effort` config (off/low/high/max) | Thinking bills at output price; all three paths currently run low - the schema and Validator carry the bulk, and high once billed ~6k thought tokens an extraction pass |
| ASR: Bailian `qwen3-asr-flash`; embedding: Bailian `text-embedding-v4` (2048-dim, API) | DeepSeek has neither; the two share one credential |
| Search: Tavily free tier (1000/month), called directly (D5) | Zero marginal cost; never the model's built-in search (double billing, breaks the prefix cache) |
| Storage: Postgres 17 + pgvector on local disk | Eighteen tables and one vector store; NFS fsync is unreliable, the NAS is for backups only |
| Trigger = must-answer on @/nickname, silence otherwise | Arbiter, interest vectors and adaptive intensity all deleted (appendix D1/D7); one rule, no knobs |
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
 ↓ voice transcribed only now, paid (§5.3)
 ↓ retrieval: roster/cards → group knowledge → episode recall (sequential;
   recall failure degrades to a reply with less memory, never to silence)
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
        affordability gate: cannot pay to read another round's results → drop the
            reply (silence). The first round always runs.
        execute tools (an identical repeated call is answered in words, not re-run);
            monthly search allowance exhausted → drop the reply
        results appended as tool messages, after the cache boundary
```

Five tools: `web_search` (Tavily, via proxy), `search_history` (SQL AND-search over this group's L0 archive, narrowable by speaker and days), `recall_events` (vector recall over this group's episodes), `read_url` (one page's readable text, off the same monthly search allowance), and `inspect_image` (a second look at a picture in the window, with a specific question - the archival description is one sentence, and detail questions reopen the bot's eyes). The first four are free; `inspect_image` is the one paid tool, its vision call booking itself into the reply's budget scope. Their division of labour is written into the tool descriptions and deliberately de-overlapped.

### Concurrency

A global `max_concurrency: 3` semaphore at the provider layer; one independent reply task per addressed message, running concurrently (each with the context slice cut at its arrival; budget attribution is contextvar task-local); background work (extraction, embeddings) rides a DB job queue (FOR UPDATE SKIP LOCKED + leases + a pending-dedup index).

### Output

Only `clean_reply` (strips Markdown, every system marker - line numbers, timestamps, provenance, trace lines, quote pointers - and text-form tool-call markup); length truncation is applied by the engine before send. There is no other exit.

## 3. Scope

In: must-answer when addressed; group memory (people, events, group knowledge); picture understanding (original pixels for the model + archived descriptions); voice transcription; web search; archive search; a command surface that recognises only the bot's owners, never QQ group admins - with two carve-outs: /who /note /alias /forget are open to any member against themselves only (at the same full manual trust as the owner's hand; /agree and /terms are theirs by nature), and the read-only /card /stats /top /groupstats are open whole; /help filters its listing per reader, and anything beyond the boundary draws silence. A user-agreement gate holds replies from members who have not sent /agree: they get a one-line pointer instead, rate-limited - /terms shows the full text, /agree accepts, and those two are the only commands that answer before consent; the version and the text file's path are mandatory config (the agreement block in settings.yaml; the file loads with the config, a missing or empty one fails the load, /reload applies both together), acceptance is recorded per group, account and agreement version, reading and archiving are untouched, owners exempt. A block withholds exactly the reply - the account's messages still arrive, archive and feed memory, keeping context coherent; blocks can carry a duration (30m/12h/3d), lifted lazily the next time the account addresses the bot, no scheduler. Group notices (joins, leaves and kicks, recalls, bans, pokes) are transcribed as one bracketed line into the window and the archive, and never draw a reply.
Out: speaking uninvited (the arbiter is gone); sending pictures or voice; cross-group memory; multiple accounts.

## 4. Trigger

One rule: an @ or a nickname hit → must answer; otherwise silence. Nicknames match as jieba **words**, not substrings (小夜曲 does not trigger 小夜). Muting (/mute) outranks being addressed.

v5's probability gate, Flash arbiter and adaptive p_max are deleted as a layer: once the bot only speaks when spoken to, the judgement they existed for no longer exists (appendix D1, D7).

## 5. Models, external services, and the budget

### 5.1 Service roster

| Capability | Backend | Model / endpoint | Credential |
|---|---|---|---|
| text | deepseek | `deepseek-v4-pro` (low thinking) | `TEXT_API_KEY` |
| vision (describes only) | deepseek | `deepseek-v4-flash-vision-exp` | `TEXT_API_KEY` |
| asr | dashscope | `qwen3-asr-flash` | `MEDIA_API_KEY` |
| embedding | dashscope | `text-embedding-v4`, 2048-dim | `MEDIA_API_KEY` |
| search | tavily | `/search`, basic depth | `SEARCH_API_KEY` |

Five capabilities, five independent config blocks (`llm.<capability>`), each carrying its own endpoint, model and credential name, never borrowing — embedding once borrowed vision's wiring and was silently dragged along when vision moved platforms, muting every reply (a real incident). The registry picks the implementation class by `backend`; platform quirks (cache-usage fields, the no-deliberation parameter) live only in those classes.

### 5.2 Retired (pitfalls)

- Local embedding (bge-small + the whole PyTorch/ONNX/NPU line): the API's 2048 dimensions measured better separation and need no upkeep; weights, HF mirrors and the fetch script are all gone.
- Zhipu search: no reason to exist once Tavily is free.
- Bailian qwen vision rendering: retired when DeepSeek vision shipped.
- The model's built-in `enable_search`: double billing, and its uncontrollable insertion point breaks the prefix cache (D5).

### 5.3 Pictures and voice

**Pictures are handled on arrival** (the CDN link is freshest then): download → an async one-line description backfills `plain_text`. The description is the archival form (design goal 8) and, with pro as the text model, the reply form too - pro cannot read file blocks. The inline-originals machinery (Files API upload + file blocks) stays in code behind `llm.text.reads_images`: off means no uploads and no attachments, one flag flips it back when a vision-capable pro-grade model exists. The describing call is cached (the same picture is never paid for twice), rate-limited per minute, and behind the daily cap. Detail questions beyond the one-line description go through the reply loop's `inspect_image` tool: picture references (`image_refs`) ride each message for the window's lifetime, are re-parsed from the archived payload after a restart, and QQ's file id trades for a fresh link at any time.

**Voice is handled at reply time**: billed per second, never repeated, and only ever discussed right after being sent — arrival leaves a marker; deciding to speak pays for the transcript. An expired link is recovered through `get_record`.

### 5.4 The budget — money as the only limit

Two levels, both in `core/budget.py`:

- **The day**, `daily_cny_cap: 3.0`: at the ceiling the bot stops answering and workers stop the paid half of their work until the date rolls over in the configured timezone. The running total is restored from `cost_ledger`; a restart does not reset the day.
- **The reply**, `per_reply_cny: 0.30`: `with BUDGET.scope(cap)` wraps the tool loop, and every backend books its own self-priced spend into it. Cannot afford the next round → the reply is dropped.

Attribution (a contextvar; it feeds the /top leaderboard only and never changes the shared budget): the bot only ever replies when spoken to, so a reply's entire spend - the transcribes, image looks and searches it forces included - is booked to its initiator, the sender of the last message that @-ed the bot or said its name; a picture's archival description is booked to whoever posted the picture; extraction and other communal spend stays unattributed.

A limit reached means silence (design goal 6): no closing round that wraps up with what it has, no "the allowance is gone, answer from what you know". The monthly search allowance (1000 credits) is metered off the ledger's calendar-month calls count — booked in the vendor's own unit, so an advanced-depth search books two, and `read_url` page reads debit the same pool; past it, `QuotaExhausted` propagates and ends the whole reply. A transport failure is a different thing — the model is told "search failed" and carries on, because a broken network is an error, not a limit.

Price tables live inside the backend classes, including DeepSeek's peak/off-peak split (weekdays 9–12 / 14–18 Beijing, ×2) and the two-era table around the 2026-08-17 repricing; an unknown model bills at the priciest tier, so a rename trips the gate early rather than under-billing. All figures verified against vendor pages (2026-08-27).

## 6. Memory and context

### 6.1 Storage

Eighteen tables, layered (L2 is deliberately absent: every reference the system receives is an @ or a quote where the platform states the account outright, so there is no judgement to record):

- **L0 `raw_event`**: append-only archive; `payload` is the platform's verbatim message and is never modified, `plain_text` is the updatable derived reading (picture descriptions and voice transcripts land there).
- **L1 `entity` / `identity_account` / `alias`(+evidence)**: person, account and name kept apart. Person-level operations reduce to two primitives: write-side **expansion** (`accounts_of_person` - /block acts on the person) and read-side **aggregation by entity** (join `identity_account`, group by entity_id - the /top leaderboard ranks people). A merge physically repoints the account rows, so read-side aggregation is correct even for merges declared after the fact; a new person-level feature picks one of the two instead of inventing its own. The account is the strong identity; names carry scope, an evidence trail and a status. Alias confidence = max within a channel, noisy-OR across channels; a platform name's first day only makes it a candidate (against rename games); manual evidence is authoritative, and a retired name stays dead against automatic evidence.
- **L3 `memory_fact`(+evidence) / `memory_candidate`**: temporal semantic facts, 25 predicates, multi-valued ones distinguished by `object_key`; "one current fact per subject and predicate" is enforced by a partial unique index in the database itself. Confidence is earned via a Wilson lower bound over **distinct source events** (1 ≈ 0.27, 3 ≈ 0.53, 8 ≈ 0.75); re-confirmation accumulates evidence in place instead of adding rows. Decay is per predicate class (stable 90d / fast 14d / default 30d), with more evidence meaning a longer half-life.
- **L4 `episode`(+participant/event)**: event summaries, vector-searched.
- **L6 `embedding_index` / `memory_job`**: vector projections decoupled from what they project; the job queue.

The conversation window (immediate context) lives in an in-memory deque and is rebuilt from L0 after a restart — the bot does not rejoin a conversation it was part of thirty seconds ago knowing nothing. The tool loop's trajectories (what each answer looked up, with result digests) live in their own table (`reply_trace`) — the single source of truth; the deque holds only conversation. Prompt assembly queries the table for the window's replies and seats each `[检索记录]` entry directly before the reply it fed: the bytes come from an immutable row and the position from the reply, so the rendering is stable turn over turn; a reply sliding out of the window simply stops being asked about, so eviction needs no bookkeeping and restarts need no special handling. They are the bot's working notes, not the group's memory — never in L0, invisible to search/extraction — and are kept forever like L0, by the owner's call: rows are small and disk is not the constraint. The extraction watermark (`group_state.last_extract_at`) records what was actually **read** and never moves backwards; the one sanctioned exception is an explicit /relearn reset.

### 6.2 Retrieval and prompt assembly

The ordering is cost discipline (design goal 1), most stable first:

```
global constants (legend, reading rules, private rules, tone) → persona → group knowledge
→ roster & cards (account order, stable)
→ conversation history (append-only; 3 chunks × 30 entries = 90 cap, evicted a whole chunk at a time)
→ [cache boundary] → clock → episode recall → tool results → current message
```

There is no token budget anywhere: every block renders whole, and each block's author (the owner, or a generation prompt with its own length discipline) is responsible for restraint. The only two numbers left are the window size and the eviction chunk, which serve finite context and the cache, not cost. Originals ride inline in their messages, text block first; a message without attached originals stays a plain string — so an untouched message renders byte-identical between turns.

Every line carries its own send time (`[MM-dd HH:mm]`, one format shared by the reply history, the extraction transcript and `search_history` results): without it the model reads sixty messages as one continuous conversation and bridges topics hours apart. The stamp is fixed at arrival and never rewritten, so it does not conflict with cache friendliness — any relative form ("5 minutes ago") would invalidate the prefix every turn, while an absolute moment is written once. The "current time" line sits after the cache boundary, giving the model a clock to compute gaps against.

Pull-style retrieval complements the pushed window: anything behind it is reachable on demand via `search_history` / `recall_events`.

Names are the model's only handle on people, and two members sharing a group card is ordinary: the member-list refresh checks the current cards for clashes and renders every clashing member as name(N), where N is the account's permanent per-group serial (the `member_seq` table - assigned once, never reused, never reassigned, so a numbered name in any old transcript still points at the same person). Renames create new clashes and dissolve old ones; both converge on the next refresh, and the refresh is the single choke point for current names (window relabelling, the roster, @-resolution and the notice lines all read it), so the numbering happens exactly once. `search_history`'s speaker argument accepts the numbered form and narrows by the serial's account rather than by name. Extraction is untouched: its transcript already names accounts by per-batch code, and every write takes codes only.

### 6.3 Writing: extract → validate → consolidate

Extraction is concentrated at one event point per day (`extract_cron`, 02:30 — the day's transcript is complete, the vendor bills off-peak, nobody is waiting): the drain reads everything unread oldest-first in chunks of up to 120, each chunk cut at a conversation gap of 30+ minutes so batch boundaries fall where conversations end and episodes stop being split mid-topic; a remainder under 20 waits for tomorrow. The real-time triggers and the idle flush are deleted whole (the owner's trade: memory lags at most a day, and the reply path's immediate context was always the window). One model call per chunk, at extraction's own deliberation grade, with the group's known facts alongside so it does not re-learn them; candidates carry the batch anchor plus batch size, so validation replays the exact rows. /relearn pulls the watermark back exactly one window and forces past the drain floor. A **pure-code Validator** gates them: verbatim quotes must match inside a single message, the batch anchor must reproduce; failures reject the candidate. The Consolidator writes: newest wins within a fact family, superseded values get `valid_to` instead of deletion, Wilson recomputes confidence, and incoming confidence only acts as a floor. The model proposes; code decides.

### 6.4 Archiving and forgetting

L0 is never deleted. Facts decay by predicate class (the small-hours forget_cron); decayed-past-threshold means superseded, not physically removed. Episodes are append-only. `pg_dump` daily at 04:30, 14 kept.

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
| `extract_cron` 02:30 | the day's memory extraction (the single event point; drains everything unread) |
| `forget_cron` 04:00 | memory decay |
| `backup_cron` 04:30 | pg_dump, 14 kept |
| `cache_clean_cron` 05:00 | NapCat media cache cleanup (7 days) |
| `report_cron` 09:00 | owner daily report: spend breakdown, per-use cache hit rates (reply/extract/other), monthly search allowance, job-queue depth, new groups, error digest |
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

**D6 No fallback / no automatic downgrade**: failover is the least-tested code path. The one refinement: auxiliary retrieval (episode recall) failing degrades to a reply with less memory rather than silence — being addressed yet silent is the one failure indistinguishable from breakage; a limit at its ceiling is the opposite case, where silence is the defined meaning (goal 6).

**D7 Interest-vector layer deleted**: its upkeep exceeded what it saved (as in v5).

**D8 Three questions for any mechanism**: does the problem it prevents actually occur? Does something simpler already cover it? Will it grow complexity of its own?

**D9 One vision model for everything** (2026-08-27): `vision-exp` matches V4-Flash text at the same price and reads images; switching replies off Pro cut ~3x and removed tier routing. Weaker text than Pro is the accepted price.

**D10 ~~Deliberation off everywhere~~ → graded** (2026-08-27, revised 08-29): one extraction pass once thought for 6k tokens to produce candidates worth a tenth of that - the schema and Validator carry the think-it-through duty. But fully-off made replies noticeably dumber and the owner reversed course: each of the three paths now carries its own config grade (`reasoning_effort`: off/low/high/max, mapping to the vendor's parameter of the same name; off selects each backend's no-thinking route), all currently at low; replies think little even at high, so it costs fen-level money.

**D11 Pictures understood on arrival** (2026-08-27): the link is freshest, each unique picture is paid for once (cached), groups the bot never answers still get a readable archive, and replies stop waiting on rendering. The old pay-at-reply discipline survives only for voice.

**D12 Limits mean silence** (2026-08-27): all three caps now behave identically. Money already spent mid-reply being discarded is the price of limits that mean what they say.

**D13 Token-budget machinery deleted** (2026-08-27): money is already the only limit; token caps were a second budget dressed as layout. The surviving message-count window serves context and cache, not cost.

### Key references

- [NapCatQQ](https://github.com/NapNeko/NapCatQQ) · [OneBot 11 segments (NapCat)](https://www.napcat.wiki/onebot/sement)
- [DeepSeek API docs/pricing](https://api-docs.deepseek.com/zh-cn/quick_start/pricing) · [Vision guide](https://api-docs.deepseek.com/zh-cn/guides/vision) · [Files API](https://api-docs.deepseek.com/zh-cn/guides/files_api)
- [Tavily /search](https://docs.tavily.com/documentation/api-reference/endpoint/search) · [API credits](https://docs.tavily.com/documentation/api-credits)
- [Bailian ASR](https://help.aliyun.com/zh/model-studio/asr-model) · [text-embedding-v4](https://help.aliyun.com/zh/model-studio/text-embedding-v4)
- Confidence & memory: Wilson score interval (one-sided z=1.6449) · multi-channel evidence pooling [arXiv 2608.11275](https://arxiv.org/abs/2608.11275) · empirical memory decay (ScrubJay: uniform decay worse than none)
- [pgvector](https://github.com/pgvector/pgvector) · [pHash false positives on memes](https://arxiv.org/html/2408.08126v1)
