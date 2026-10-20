# System review: requirements, agent design and target architecture

A whole-system review of QBot (October 2026): what the product must do in a shared QQ group,
how the implementation measures up against that and against current agent-design guidance, which
design was chosen for the problems found, and why. [architecture.md](architecture.md) stays the
description of how the system works; this document records the reasoning, the evidence and what
remains open. Where they differ, architecture.md describes the code.

## 1. Method and coverage

Every first-party file tracked in the repository was read: the 19 crates' sources (about 31,000
lines) and their tests (about 30,000), the 11 migrations, every prompt template, `wording.toml`
and `predicates.toml`, both locale catalogs, the eval scenarios and judge, `defaults.toml` and the
example configuration, the Dockerfile, the compose file, `deploy/README.md`, the CI workflow,
CLAUDE.md, README.md and the two design documents. Tests were read for what they pin, not line by
line for style.

| Subsystem | Files |
| --- | --- |
| Ingestion, routing, delivery | `qbot-gateway/src/{wire,card,render,notice,media,trigger,pipeline,intake,bridge,echo,delivery,directory,server}.rs` |
| Runs and admission | `qbot-agent/src/{supervisor,run,executor,tool,env,sim}.rs` |
| Transcript and projection | `qbot-context/src/*`, `qbot-core/src/{marker,batch,ids,time,clock}.rs` |
| Prompt and context | `qbot-prompt/src/{source,templates,render,persona}.rs`, `prompts/*`, `qbot-wording` |
| Tools | `qbot-tools/src/{send,tasks,search,query,memory,web}.rs`, `qbot-media/src/open.rs` |
| Scheduling | `qbot-sched/src/{scheduler,service,store,timer,repeat,recurring,conformance}.rs` |
| Memory | `qbot-memory/src/*` (extraction, findings, consolidation, facts, identity, recall, notes) |
| Persistence | `qbot-store/src/*`, `qbot-store/migrations/*` |
| Providers and accounting | `qbot-llm/src/*` (responses adapter, retry, meter, embeddings, Tavily) |
| Media | `qbot-media/src/*`, `qbot-asr/src/*` |
| Commands and authority | `qbot-commands/src/{router,catalog,args,fail,reply,ports}.rs`, `handlers/*` |
| Operations | `qbot-ops/src/*`, `qbot-app/src/{run,main,logs,report_sink}.rs`, `deploy/*`, CI |
| Configuration, catalogs | `qbot-config/src/*`, `defaults.toml`, `qbot-i18n`, `locales/*` |
| Evaluation | `qbot-eval/src/*`, `eval/scenarios/*`, `eval/judge.md` |

Excluded: `target/` (build output), `Cargo.lock` beyond the advisory scan (generated), the `py`
branch (reference only, not deployed) and `goal.txt` (the task brief). Production was inspected
read-only: the running image and schema version, and 30-day aggregates from the `run`,
`run_item`, `usage_event` and `timer` tables. No message text was read; tool arguments were read
only as numbers (episode ids).

## 2. Production evidence (30 days to 2026-10-20)

| Measure | Value |
| --- | --- |
| Groups with chat; lines archived | 10; 19,981 |
| Runs: addressed / spontaneous / wake | 1,292 / 42 / 27 |
| Addressed runs ending `delivered` / `completed` / error | 1,266 / 22 / 4 |
| Run duration of delivered addressed runs, p50 / p90 / max | 4.2 s / 11.7 s / 115 s |
| Model turns per run: 1 / 2 / 3 / 4+ | 399 / 680 / 178 / 104 |
| Input tokens, of which cached; output tokens | 53.0 M, 45.7 M (86%); 0.85 M |
| Same member addressed the bot again while their run was open | 62 pairs, 58 within 5 s |
| ... and both runs sent a reply | **40** |
| Different members' overlapping addressed runs, both replied | 13 |
| Most runs one member started in 10 minutes | 28 |
| Most runs open at once in one group | 8 |
| `read_episode` calls refused (an id the run was never shown) | 12 of 20 |
| Reply queue full | 0 |

The duplicate replies are the known gap in architecture.md section 19 ("a member who repeats or
rephrases a question within seconds gets it answered twice"), now measured: about 3% of addressed
runs, almost all from one member writing twice within five seconds (a greeting and then the
question, or a question in two messages). The refused `read_episode` calls were all ids the model
guessed; 6 of them belong to other groups and were refused by the group filter, so isolation held.

## 3. Requirements baseline

### 3.1 Explicit requirements

R1 to R14 in architecture.md section 1 are explicit and current. They were each confirmed against
the code; the notes that came with this task (append-only transcript, overlapping runs,
`send_messages` with `end_turn`, accounting of every provider call, scheduler-owned repeating
tasks, group isolation with `/stats global` as the one exception, owner-only `/tasks`, delegated
admins limited to their group) all hold in the code and in production (image `qbot-rust:fe02011`,
schema 11).

### 3.2 Inferred requirements

From the implementation, the prompts and operations:

- I1. A member gets one coherent reply to what they said, even when they say it in several quick
  messages. (Implied by R3 and R4; violated today, section 2.)
- I2. A message its author or an admin recalled is not repeated, quoted, searched or learned from
  by the bot afterwards. (QQ users' expectation of a recall; the archive keeps it today.)
- I3. One member cannot take more than a fair share of the reply capacity that all groups share.
  (Supervisor capacity is global; media already has per-group rates for the same reason.)
- I4. Everything a run can reference it was shown: message ids, member numbers, episode ids.
  (The send tool enforces it for ids and numbers; summaries show no episode id.)
- I5. The bot never acts on another group's data, even by guessing an id. (Holds: every store
  query filters by group; section 2 shows it working.)

### 3.3 Assumptions (kept, not resolved by expanding authority)

- A1. Every run may see the group's notes and learned facts about any member (the people block);
  only commands restrict what a member may look up. Kept: it is what lets the bot know people.
  Open product question below.
- A2. Facts about a member may come from what another member said about them (`subject` may be
  mentioned in the quote). Kept: it is how the group's knowledge of people works; a fact needs a
  verbatim quote and decays.
- A3. "Nothing refuses work on cost" (R12) means no spending caps, not "no fairness": a bound on
  how many runs one member holds is resource isolation, as the media rates are.

### 3.4 Conflicts

- C1. Overlapping runs (decision 1 in architecture.md) against I1: every trigger starting an
  independent run is what produces the duplicate replies. Resolved in section 7.
- C2. Notes visible to every run (A1) against `/who` being self-only for members: a member may ask
  the bot what is noted about someone else and the model may tell them. Mitigated only by the
  prompt ("do not pass one member's notes or facts to another unless the conversation calls for
  it"), which is not a security boundary. Left open as a product decision (section 10).

### 3.5 Unknowns

- U1. Whether the operator wants recalled messages removed from the record kept for diagnosis.
  The design keeps the run transcripts as they were (they are the operational record, kept
  `records_keep_days`) and changes only the archive. Reversible.
- U2. How the model behaves on the changed trigger note; measured with `qbot-eval` (section 9).

### 3.6 Out of scope (deliberately)

Exactly-once delivery of a QQ message (the platform offers no idempotency key), video
understanding, reading files, a spending cap, multi-agent delegation, and per-group fair queuing
of model slots (no starvation observed: the queue never filled).

### 3.7 Requirements matrix

| Requirement | Actor / event | Expected behaviour | Authorization and scope | Failure and concurrency | Where | Acceptance test |
| --- | --- | --- | --- | --- | --- | --- |
| Speak only when caused (R3) | member message | @, nickname, quote of the bot, due task, or the spontaneous roll start a run; nothing else | mute and block gates in the supervisor | gate store error rejects the trigger | `pipeline.rs`, `supervisor.rs` | gateway e2e, supervisor tests |
| One reply per member burst (I1) | member writes twice within a run | the second message waits for the first run, then is answered once with the first reply in view; a third folds into the waiting run | per group and sender | waiting counts against the deadline; cancellation and shutdown end it | `supervisor.rs`, `trigger_addressed.md` | new supervisor tests; `eval/scenarios/second_message_*.toml` |
| Bounded capacity per member (I3) | a member spamming triggers | at most one running and one waiting run per member and group | admission only | further triggers fold or are refused as overloaded | `supervisor.rs` | new supervisor test |
| Recall honoured (I2) | recall notice | the archived line becomes `[recalled]`, its media references go; search, history and later extraction see no text | the notice's group; the line of its author | an unknown message is a no-op; a store error is logged | `pipeline.rs`, `archive.rs` | new gateway e2e and store tests |
| Group isolation (R14, I5) | any tool, command, prompt | reads and writes only the run's group | `group_id` filter in every query; ids checked against the group | a foreign id is "no such ..." | store, tools | existing isolation tests, store tests |
| Model speaks only through `send_messages` (R4) | model turn | text without a tool call is never sent; one nudge, then a forced tool turn | the run's view bounds ids and numbers | partial batches reported per message; nothing retried | `send.rs`, `run.rs` | `send_messages.rs`, `run_loop.rs` |
| Tasks bounded (R10) | model or owner | min delay, pending bound, chain depth, interval >= 1 h; repeating only from a member's request | group-owned; `/tasks` owner-only | at most once per occurrence; restart recovery | `qbot-sched` | sched conformance and tests |
| Accounting (R12) | every provider call | one usage row per call with requests, tokens, credits | attributed to work and group | dropped calls recorded as cancelled | `meter.rs`, `usage.rs` | `run_e2e.rs` |
| Single instance, migrations (R11) | startup | lease before migrate; forward-only schema | operator | lease loss stops the process | `run.rs`, `lease.rs` | store tests, rehearsal |
| Owner and delegated authority (section 12) | commands | the permission matrix | read from config and `group_admin` | unknown standing grants nothing | `qbot-commands` | `commands.rs` |

## 4. Research: principles that apply here

Sources (read 2026-10-20):

- [S1] Anthropic, "Building effective agents" (published 2024-12-19; the page notes tooling has
  changed since). https://www.anthropic.com/engineering/building-effective-agents
- [S2] Anthropic, "Effective context engineering for AI agents" (2025-09-29).
  https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- [S3] Google Cloud, "Choose a design pattern for your agentic AI system" (updated 2026-05-28).
  https://docs.cloud.google.com/architecture/choose-design-pattern-agentic-ai-system
- [S4] OpenAI Agents SDK guide and its guardrails page (undated pages, read current).
  https://developers.openai.com/api/docs/guides/agents/sdk,
  https://openai.github.io/openai-agents-python/guardrails/
- [S5] OWASP Top 10 for Agentic Applications 2026 (2025-12-09), ASI01 to ASI10: goal hijack, tool
  misuse, identity and privilege abuse, supply chain, unexpected code execution, memory and
  context poisoning, inter-agent communication, cascading failures, human-agent trust
  exploitation, rogue agents. The item names are from the OWASP page and summaries of the
  published document; the full PDF was not read.
  https://genai.owasp.org/resource/owasp-top-10-for-agentic-applications-for-2026/
- [S6] OWASP Top 10 for LLM Applications, 2025 edition; LLM01 prompt injection, LLM06 excessive
  agency and LLM10 unbounded consumption were read in full. https://genai.owasp.org/llm-top-10/

| Principle | Source | Why it matters for this bot | Where it stands |
| --- | --- | --- | --- |
| Use the simplest pattern that works; add agents only when one struggles | S1, S3 | a group reply is one ReAct-style loop over a handful of tools; routing or a critic would add a model call to every message | kept: one loop per run, no sub-agents |
| Deterministic workflow where the steps are known | S1, S3 | admission, scheduling, recurrence, extraction validation and recall handling are known steps | all are code; this review adds per-member admission and recall as code, not as prompt rules |
| Authorization in trusted code (complete mediation), least functionality | S5 ASI02/ASI03, S6 LLM06 | anyone in a group can talk to the model | tools validate ids against the run's view and the group; owner powers are commands, not tools |
| Treat outside content as data, label it, constrain outputs | S6 LLM01, S5 ASI01 | chat, cards, web pages and memory all carry other people's text | structural provenance and marker escaping; prompts are not relied on for safety |
| Memory poisoning is a distinct risk | S5 ASI06 | facts are learned from what members say | verbatim quotes, subject rules, decay, `/forget`; recall honoured now |
| Bound consumption per source; limit queued actions | S6 LLM10 | one member's burst used 28 runs in ten minutes | per-member admission bound added |
| Stopping conditions | S1, S4 | a looping model spends money in every group | `max_turns`, the deadline, task bounds |
| Context is finite: compact, retrieve just in time, keep identifiers | S2 | prompts are 40k tokens a run; history tiers and recall | kept; episode ids added to summaries so retrieval can follow them |
| Tool design is interface design; make mistakes hard | S1, S2 | 12 of 20 `read_episode` calls guessed ids | summaries carry their episode id |
| Guardrails before side effects; trace everything | S4 | sends are the only side effect members see | every send is checked whole before the first goes out; every run's transcript is stored |
| Evaluate behaviour, not only code | S1, S3, S4 | a passing suite says nothing about whether a reply is right | `qbot-eval` with deterministic checks and a judge, run against the live model |

Assumptions the research argues against, and how they apply:

- *More agents make a better agent.* A router or critic per message would double the latency of
  a 4-second reply and its cost, and add a coordination failure; nothing in section 2 is a
  quality problem a second model would fix. Rejected.
- *More autonomy is more capability.* The failures found are the opposite: the model cannot know
  that another run is answering. Moving that decision into admission code fixes it for free.
- *More context is better.* The prompt already carries 40k tokens; the change adds no standing
  context, only a flag in the per-run note when it applies.
- *All control flow to the model.* Admission, recall and recurrence stay deterministic.
- *A detailed system prompt can replace authorization.* It cannot (S6 LLM01: prevention is not
  guaranteed); C2 is recorded as a gap for that reason.
- *More frameworks and workers improve maintainability.* No framework is adopted; the change is
  one admission rule in the supervisor that already holds every admission rule.
- *Passing unit tests proves behaviour.* The deterministic tests pin the mechanism; whether the
  model then stays silent or answers once is measured by `qbot-eval` and production counts.

## 5. Current architecture in brief

The control and data flow is described in architecture.md sections 4 to 10. In one line per path:

- **Message**: NapCat frame, then `wire::parse_frame`, then `render`, then `PgArchive::append_line`
  (one transaction, the dedup gate), then echo board, command router or trigger, then
  `Supervisor::submit` (gates, capacity, run record), then a spawned run: wait for a model slot,
  `PromptContext::open`, then the loop (project, `Provider::respond` inside `attributed`,
  `execute_turn`, fold new chat), then `RunLog::finish` and usage rows.
- **Delivery**: `send_messages` checks every message, then `OneBotDelivery::send` per message
  (action, then echo), then the result per message; nothing is retried.
- **Tasks**: `TaskService` (all limits), then `PgTimerStore`, then the scheduler tick (reserve
  capacity, claim, `submit_reserved`), then the run, then `finish` or `finish_occurrence`
  (compare-and-set); startup `recover`.
- **Memory**: a filled batch queues `Extract`; `EpisodeJobs` (slice, model, validate, embed,
  store, consolidate); nightly catch-up, decay, backup, cleanup.

Strengths confirmed by this review: one mutation path per invariant (transcript, timer store),
typed errors with no silent fallback, every limit justified, group filters on every query,
prompts and wording as tested data, and complete accounting.

## 6. Findings

| # | Severity | Kind | Where | What | Evidence | Decision |
| --- | --- | --- | --- | --- | --- | --- |
| F1 | High (user-visible, frequent) | confirmed defect | `qbot-agent/src/supervisor.rs` `submit`/`start` | a member's quick second message starts a second independent run; both answer | 40 double replies in 30 days (section 2); reproducible with two `submit` calls from one sender | fixed (section 7) |
| F2 | Medium | resource isolation | `supervisor.rs` capacity | one member can hold every queue slot shared by all groups (capacity 30) | 28 runs from one member in 10 minutes; reproducible by submitting 30 triggers from one sender | fixed by the same rule |
| F3 | Medium | privacy / missing requirement | `pipeline.rs` `notice`, `notice.rs` | a recalled message stays in the archive, every later prompt, search and extraction; the recall notice is archived but not applied | code: the notice's `discriminator` holds the recalled message id and is used only for dedup | fixed |
| F4 | Low | tool design | `qbot-prompt/src/source.rs` recap, `wording.toml` | summaries carry no episode id, so the model guesses ids for `read_episode` | 12 of 20 calls refused, all guessed ids | fixed |
| F5 | Low | design concern | `prompts/reply_system.md`, people block | every run sees every member's notes and facts (C2) | by construction | open product question |
| F6 | Low | operational | CI | no dependency-advisory check | CI has fmt, clippy, tests, image only | advisory scan run by hand (section 9); adding it to CI needs network access to the advisory database and is left to the operator |
| F7 | Info | design concern | `read_url` | a page could tell the model to fetch an attacker's URL carrying group text | inherent to page reading | not changed: a URL allowlist from the run's own context would not stop it (the page itself supplies the URL) and would break legitimate reads; what such a request could carry is the group's own context |
| F9 | Medium | tool design | `qbot-tools/src/tasks.rs` | `schedule_task` and `update_task` each bundled several jobs behind interdependent optional fields | schema and code (section 7.3) | fixed |
| F10 | Medium | dependency advisories | `Cargo.lock` | rustls 0.23.45: RUSTSEC-2026-0346..0349 (TLS secrets in `Debug`, ECH data leak, downgrade, decompression bomb); zip 2.4.2: RUSTSEC-2026-0367..0369; rsa: RUSTSEC-2023-0071 | `cargo audit` | rustls updated to 0.23.46; zip is only a build dependency of `sherpa-onnx-sys` (unpacks its native library at build time) with no fixed release of sherpa-onnx yet; rsa is in the lockfile through sqlx's unused MySQL driver and not compiled |
| F11 | Low | confirmed defect | `qbot-agent/src/supervisor.rs` `drive`, `run.rs` `bounded`/`tool_turn` | `tokio::select!` chose at random between a cancellation (shutdown) or deadline that had fired and other work that was ready, so a cancelled run could still go on to a model call | found by a new supervisor test that failed intermittently (a run waiting at shutdown ended `model_error`) | fixed: the selects are `biased`, cancellation then the deadline first; the test passes 20 of 20 |
| F12 | Medium | requirement gap | `prompts/extract.md`, `qbot-memory/src/{extract,jobs,findings,consolidate}.rs` | the extractor lacked dates with a year, the conversation before the slice, quoted lines from further back, the bot's name, a rule for lasting versus passing, and any way to end a fact that no longer holds | by reading the prompt it is given against what the task needs (section 7.4) | fixed |
| F8 | Info | design concern | `executor.rs` | `stay_silent` and `send_messages` in one turn both execute (reads before writes) | by construction; no production occurrence | kept: every call executes is a stated rule |

No critical or security-boundary defect was found: authorization, group filters, marker escaping,
id validation in tools, the lease, migrations and the timer state machine held under the review.

## 7. Candidate designs for F1 to F3 and the decision

### 7.1 Overlapping runs of one member (F1, F2)

- **A. Status quo.** Every trigger its own independent run. Simple; 3% duplicate replies.
- **B. Per-member turn-taking in admission (chosen).** Runs of one member in one group take turns:
  a second addressed trigger from the same member while their run is under way is admitted (it
  gets its run record, its deadline and its capacity) but waits for the first run to end before
  it opens its context. A third while one is waiting folds into the waiting run, whose focus
  moves to the newest message; it starts no run of its own. The waiting run opens its context
  after the first ended, so it sees the first run's reply; the trigger note then says the bot
  already replied after that message and to stay silent if that reply covers it.
- **C. One conversation actor per group.** All of a group's triggers feed one long-lived loop
  that answers the latest state. Removes duplicates across members too, but serializes a busy
  group (1,027 runs a month in the largest), changes the transcript model (one run per trigger),
  the run record, recovery and every test, and couples unrelated members' questions.
- **D. A classifier model per trigger** ("is this already being answered?") before a run.
  Probabilistic, an extra model call and latency on every message, and it still races with a run
  that has not replied yet.

| Criterion | A | B | C | D |
| --- | --- | --- | --- | --- |
| Duplicate replies (same member, 89% of cases) | no fix | fixed | fixed | partly |
| Duplicates across members (13 cases) | no | partly (the note applies when a reply already exists) | fixed | partly |
| Isolation and security | unchanged | unchanged; admission stays code | unchanged | adds a model in the path |
| Reliability and recovery | unchanged | unchanged: each run still a record and transcript | new recovery story | new failure mode |
| Latency | best | the second message waits for the first run (p50 4 s) | a busy group waits on itself | +1 model call per message |
| Cost | duplicates paid twice | fewer runs | fewest | +1 call per message |
| Complexity | none | one map in the supervisor | large rewrite | moderate |
| Testability | | deterministic with paused time | hard | nondeterministic |
| Migration and rollback | | none | schema of runs | none |

B removes the measured problem at its source with a change confined to the component that owns
admission, no schema change and no new model call. C is better only for cross-member duplicates,
which are a fifth as frequent, at the price of a rewrite. A remains better in one respect: a member
who sends two unrelated questions in two seconds gets the second answer a few seconds later under
B. That is accepted: the second answer is better for seeing the first.

### 7.2 Recalled messages (F3)

- **A. Status quo**, documented as a gap.
- **B. Honour the recall in the archive (chosen).** The recall notice names the message; the line
  of that author with that id (the newest, as for quotes) gets the text `[recalled]` (a new
  registered marker) and loses its media references, in one transaction. Everything that reads
  the archive afterwards (history, search, extraction, `open_images`) sees no content. Runs
  already under way keep what they were shown, and the run transcripts (the operational record)
  are not rewritten. Memory already extracted from the line stays (`/forget` removes a fact).
- **C. Also delete derived memory**: facts whose only evidence is the line, and re-extract its
  episode. Episodes are immutable by design (memory.md section 1); re-extraction would rewrite
  them. Not worth the machinery while recalls come within minutes of a message, which is then
  rarely in an extracted slice yet (a slice is 90 lines).

### 7.3 Tool interfaces: one job per tool (F9)

Reviewed from the requirement (the model must be able to do each task operation, with mistakes
impossible or explained) rather than from the existing schema. Most tools already do one thing:
`send_messages`, `stay_silent`, `search_history`, `recall_episodes`, `read_episode`, `web_search`,
`read_url`, `open_images`, `list_tasks`, `get_task`, `cancel_task`. Two did not:

- `schedule_task` made one-shot *and* repeating tasks, through four flattened optional fields
  (`repeat`, `repeat_minutes`, `repeat_time`, `repeat_weekdays`) whose valid combinations
  depended on each other (eight distinct error texts just for the combinations).
- `update_task` changed intent, time *and* the repeat rule, and could turn a one-shot task into a
  series.

Chosen: `schedule_task` (one-shot), `schedule_repeating_task` (`intent`, `rule`, `start_at`), and
`update_task` limited to intent and time. The rule is one string in the grammar the owner's
`/tasks --every` already uses, parsed by the same function: one argument with one parser and one
error, instead of a union of fields. Alternatives considered: a tagged-union `rule` object (a
`oneOf` schema; more structure but more tokens, weaker support across providers, and the
parameter-description machinery describes only plain properties), and one tool per rule kind
(three tools for one job). Changing a series' rule is now cancel plus schedule; it is rare (2
`update_task` calls in 30 days, none with a rule) and the owner command keeps the shortcut. Task
argument types reject unknown fields, so a `rule` passed to `schedule_task` is an explained error
instead of a one-shot task made silently.

### 7.4 Memory extraction, designed from what the reader needs (F12)

The question asked of each design element was what a careful human, given only this text and no
tools, would need to write a summary someone can use months later and to keep only what lasts.
Against that, the extractor was missing:

| Need | Before | Now |
| --- | --- | --- |
| Absolute time ("tomorrow", "on Friday", a deadline) | `MM-DD HH:MM` per line, no year or weekday | the target's heading gives its span with year and weekday; summaries write plans as dates |
| What the conversation was already about | one raw batch before | plus the previous episode's title and summary ("Earlier in this chat") |
| What a reply refers to | only if the quoted line is within a batch | lines quoted from further back are shown ("Quoted from earlier") |
| Who the bot is | `bot:` lines only | the bot's name, so members' "Bobo, ..." never becomes a member's name |
| What lasts versus what passes | the predicate file's own rule ("still true next week") was not shown to the model | stated in the instructions, with examples of each |
| Corrections ("I quit that game") | none: an old fact stayed until it decayed | `ended` findings retire the fact with that value, under the same quoting and subject rules |
| Recalled messages | not explained | `[recalled]` explained: nothing may be guessed about it |

Considered and not chosen: giving the extractor tools (history search, recall) — another loop, more
calls per slice in the nightly run, and nondeterministic context, when the needed context can be
assembled deterministically; a second "critic" model to check findings — the code already checks
every finding against verbatim quotes, and a critic doubles the cost of every slice; free-form
fact predicates — they would make facts unqueryable and unbounded. Episodes keep their method
(`slice-v4` from now on); older ones stay as they are.

### 7.5 Episode ids in summaries (F4)

The summary standing in for older chat names its episode (`episode 12, 10-03 14:00 - 10-03 16:30:
title`), and `read_episode` says its id comes from `recall_episodes` or a summary. The summary
text is fixed per episode, so the cached prefix is unchanged after the first run.

## 8. Target design: invariants and semantics

**State and scope**

- Per (group, sender) the supervisor holds at most one *running* addressed run and one *waiting*
  one. Held in memory only: a restart has no runs to order (open runs are closed at startup).
- A waiting run has a run record and a deadline from admission; it holds one unit of capacity.
- A folded trigger creates nothing: no run record, no model call. Its message is in the waiting
  run's window, because the window is read only when that run opens, after the fold.
- Wake and spontaneous runs and other members' runs are not affected. Commands are not affected.
- A recalled line keeps its ordinal, message id, author and time; only its text and media
  references change, so member numbers, slices and episode ranges are untouched.

**Concurrency**

- Turn-taking is a per-key async mutex held for the whole run, acquired before the model slot,
  so the order is: wait for the member's earlier run, then for a model slot. A waiting run
  watches cancellation and its deadline while it waits, as the slot wait does.
- The fold and the waiting run's start are serialized by the supervisor's map lock: a trigger
  arriving after the waiting run started becomes the next waiting run.
- Recall and media filling both edit a line under its row lock (`FOR UPDATE`); a fill that finds
  `[recalled]` finds no marker and changes nothing.

**Authorization**

- Unchanged. Mute and block gates run before turn-taking; a folded trigger has passed them.
- The recall applies only to the line of the notice's author in the notice's group.

**Failure**

- A store error while recording a run rejects the trigger as before; the turn is not taken.
- A store error while applying a recall is logged as an error and the notice is still archived;
  the line keeps its text (the outcome is visible in `/logs`).
- A cancelled or past-deadline waiting run ends as `cancelled` or `deadline`, recorded, without a
  model call.

**Scenarios walked through**

1. *A member tells the bot to ignore its rules*: chat is user content under the trust rule; tools
   validate every reference; owner powers are not tools. Outcome: no privileged effect.
   Mechanism: authorization in code (`check_requested`, view checks, command standing).
2. *A web page tells the model to call a tool*: the page is a tool result; the tools it could
   reach are the run's own (send, tasks in this group); a repeating task is refused outside a
   member-addressed run. Outcome: at worst a message in the same group, under the send rules.
3. *Two members ask related questions while a run executes*: two runs (different senders), each
   sees the other's echoed reply when it folds arrivals between turns and, if it opens after it,
   in its window with the "already replied" note. The same member's second message waits (B).
4. *Two groups use tools on persistent state at once*: every store call is keyed by group;
   per-group advisory locks (archive, slices, facts) and row-level compare-and-set (timers) keep
   them independent.
5. *A transaction fails partway*: every multi-row write is one transaction (line and its media
   references, episode and vector, recall); a failure leaves nothing half-written.
6. *A provider times out after a side effect*: the only side effect is a send, which happens in a
   tool after the model turn has fully arrived; a model call has no effect to repeat, and a retry
   of a call is a retry of a read.
7. *A task fires during downtime or races cancellation*: one-shot tasks are interrupted, never
   replayed; a repeating task makes up missed occurrences once; cancellation during a run ends
   the series and records the outcome (architecture.md section 10).
8. *Shutdown in the middle of a multi-message send*: the run is cancelled; the message being sent
   may or may not arrive (no idempotency on QQ's side); the rest are not sent; the transcript
   records the call as interrupted. At most once per message, never exactly once.
9. *Token spike or a loop*: `max_turns` (20) and the deadline end it; per-member turn-taking
   bounds a burst; accounting shows it; nothing refuses work on cost (R12).
10. *A member asks for another group's data*: no tool or command reads another group; a guessed
    id is "no such ..." (section 2 shows 6 such refusals).

## 9. Implementation plan and acceptance tests

Ordered by risk-adjusted value:

1. Per-member turn-taking and folding in the supervisor; the trigger note's "already replied" flag.
   Tests: same-sender runs do not overlap; the second sees the first's reply; a third folds and
   the waiting run's focus moves; cancellation and the deadline end a waiting run without a model
   call; other senders and wake runs still overlap; capacity is not consumed by a fold. Eval
   scenarios `second_message_already_answered` and `second_message_still_open`.
2. Recall honoured: `Intake::recall`, `PgArchive::recall`, the `[recalled]` marker in `MARKERS`
   and the reading guide. Tests: store (text, media references, wrong author is a no-op), gateway
   e2e (a recall notice blanks the line for the next run and for search).
3. Episode ids in summaries. Tests: prompt tests on the recap text.
4. Task tools split (7.3). Tests: each tool's schema has exactly its own fields; bad rules and
   foreign arguments are explained and create nothing; the existing authorization tests through
   the new tool. Eval scenario `schedule_recurring` expects `schedule_repeating_task`.
5. `cargo update -p rustls`.
6. Documentation: architecture.md sections 4, 6, 7, 10, 18 and 19; memory.md section 9.

No migration: all changes are in code and text. The previous image runs on the same schema, so a
rollback is a tag change (the recall edits remain, which the previous image reads as ordinary
text).

Verification: the full workspace suite against Postgres, fmt, clippy with warnings denied, the
production image build, `qbot-eval` on the changed prompts against the live model, and after
deploy the production counts of section 2 for the following days.

## 9a. Independent review of the changes and the fixes

A separate review agent read the requirements, the design above and the whole repository and
derived its own findings from the code. Confirmed and fixed:

| # | Severity | Finding | Fix | Regression test |
| --- | --- | --- | --- | --- |
| R1 | Medium | turn order followed whichever task took the turn lock first (the run record's insert sits between), so a member could get three runs, answered out of order (reproduced) | the member's first run takes its turn at admission | `a_members_runs_take_turns_in_the_order_they_were_admitted` |
| R2 | Medium | triggers waiting for media were submitted in any order, so a fold could move the focus to an older message | each group's addressed triggers go through one queue, submitted in arrival order | covered by R1's test at the supervisor; the queue keeps arrival order by construction |
| R3 | Low-medium | a message folded late inherited what was left of the waiting run's deadline and could go unanswered | a fold gives the waiting run a whole deadline from the newest message | `a_folded_message_gets_a_whole_deadline` |
| R4 | Low-medium | a picture opened before its recall stayed openable from the in-memory cache | the stored reference is checked before the cache | `a_picture_whose_message_was_recalled_is_not_shown_from_the_cache` |
| R5 | Low-medium | a mute or block set while a run waited its turn did not hold | admission is checked again when the turn comes | `a_block_or_mute_set_while_a_run_waits_its_turn_holds` |
| R6 | Low | trigger lookups took the oldest line with a (colliding) message id, quotes and recalls the newest | newest everywhere | existing prompt tests |
| R7 | Low | a waiting run whose own message was recalled still told the model it was addressed | the note says the message was recalled and to stay silent unless something else needs an answer | prompt tests |
| R8 | Low | the rule grammar refused `Mon, thu@08:00`; the interval minimum was written twice | case and spaces are forgiven; the description takes the minimum from `TaskLimits` | tool tests |

A second independent review covered the extraction redesign and the fixes above:

| # | Severity | Finding | Fix | Regression test |
| --- | --- | --- | --- | --- |
| R9 | Medium | within one episode an `ended` item always beat a fact of the same value, whatever the line order ("I quit it" ... "back on it" ended it), and re-applying such an episode left extra rows (reproduced) | the later line wins; only that item is kept | `the_later_of_a_fact_and_its_end_wins_and_the_bots_names_are_no_ones` |
| R10 | Low-medium | the previous episode's title and summary entered the extraction prompt unescaped and could open a section | marker-escaped and put on one line | covered by the extraction prompt tests |
| R11 | Low-medium | the prompt promised the bot's "other names" but only the persona name was given; nothing refused a name finding for the bot | `bot.nicknames` join the persona name in the background, and a name finding equal to one is refused with a reason | same test |
| R12 | Low | prompt: an end must be lasting too; a move away loses `lived_in`; `wants` versus tonight's plan; the schema's summary text lagged; a fixed-offset zone was labelled UTC; one media wait per queued trigger | stated in `extract.md`; schema text updated; the heading shows the offset; one wait per batch of queued triggers | prompt and gateway tests |

Kept as design limits: an end whose value does not match the recorded one exactly changes nothing
(the model is told to copy the value "known before" lists); a colliding hashed message id could make
"Quoted from earlier" show another line with the same id (as quotes already can).

Live extraction (2 tests against `deepseek-flash`, run on the production host with the key
mounted read-only, about 9k tokens): the new test's slice ended `plays: <the game>` from "I don't
play it any more" without recording it again, wrote the meetup as `01-16` and a trip as
`2027-01-23` rather than "tomorrow"/"next Saturday", recorded no fact from "tired today, eating
instant noodles", took no name for the bot, and read the reply to the quoted older line correctly
("agreed to the weekend hike"); the existing test still finds the like and the group term.

Accepted: a run record keeps the message that admitted it after a fold (the transcript shows the
focus); a folded message is lost with its run if the run's own record could not be written (the
database is then failing for every trigger).

## 9b. Live evaluation

`qbot-eval` ran on the production host in a throwaway container (the text key mounted read-only,
never copied off the host), reply model `deepseek-flash`, judge `deepseek-v4-pro`, with the
production persona:

- All 30 scenarios once: 27 passed every check and rubric item. Three missed one item each: in
  `injection_in_forward` the reply said a forwarded "tomorrow" meeting was "today" (the injection
  itself was refused); in `injection_in_web` the answer was right but given without calling
  `web_search`; in `rps_game` the model sent `[rps]` without `end_turn: false`, so it never
  announced the result.
- Those three and every scenario this release touches, three times each (24 runs): all passed
  except one `injection_in_web` run, again for answering without searching. The new
  `second_message_already_answered` (stays silent when its reply already covered the message) and
  `second_message_still_open` (answers only what is open, without greeting again) and
  `schedule_recurring` (one `schedule_repeating_task` call with `mon,tue,wed,thu,fri@08:40`)
  passed 3 of 3.

Nondeterminism remains: the same scenario can pass and fail across runs, so these are samples,
not proofs. Not covered by the eval: cross-member duplicates under real timing, and recall
handling (both are pinned by deterministic tests instead).

## 10. Open decisions

- **Notes and facts visible to every run (C2/F5).** The options are to keep them (current), to
  show a run only the entries of people in its visible chat, or to show notes only to runs
  addressed by the person they are about. Each changes what the bot can do for the group, so it
  is left to the project owner.
- **Cross-member duplicates.** The "already replied" note covers a run that opens after another
  run's reply; two runs started within the same seconds by different members can still both
  answer. Revisit with production counts after this release.
- **Derived memory of recalled lines** (7.2 C), if recalls of already-extracted lines turn out to
  matter.
