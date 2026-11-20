# Memory: design record

Why memory (what QBot learns from group chat, and what people enter or remove by hand) is built
the way [memory.md](memory.md) describes: the evidence, the requirements, the research, the
decisions and what was rejected. Episodes and the archive are inputs here.

## 1. Evidence from the deployment

Read-only aggregates over the production database (counts only, no member text).

- **Before the rework (2026-11-03, 9 groups, 636 episodes).** 77 stored names, 75 learned, 60 of
  them on one episode; names shown as independent beliefs "about 67% likely". 506 member facts in
  28 fixed predicates, 94% single-support; the largest classes were passing wishes. Display names
  were never recorded, so nothing knew a member had been shown as something else before.
- **Reference ledger (2026-11).** 3,900 references to members, 758 distinct (member, name) pairs
  over 91 members, 38 names used for two or more members: names are a large share of what is
  learned about people, and shared names are ordinary. Many learned names were wrong.
- **Relations.** 3 of 766 current member memories, and 21 of 1,028 statements, named another
  member; 129 of 452 group memories named a member, so names and group vocabulary overlapped.
- **How the chat looks.** Lines per group-day: median 30, p90 388, max 1,521. 89% of replies
  quote a line within 5 lines, 99% within 30, 0.6% one on an earlier day. 67% of member lines have
  three or more speakers in the ten minutes before. Command lines 0.9%, bot lines 18%; three
  group-days in the whole archive had chat on both sides of 04:00.
- **Forgetting left no trace.** 11 past `/forget`s deleted rows without recording what was
  forgotten, so nothing could keep it from being learned again.

## 2. Requirements

1. **Durable over months.** Near-duplicates converge; what changes is replaced with history kept;
   extraction mistakes are checked and correctable; growth is bounded.
2. **Never half-applied.** A failed or repeated batch leaves the same state as one clean run.
3. **People are authoritative.** What a person enters, corrects or forgets wins over anything
   read from chat, now and later, in any wording.
4. **Reference.** When someone writes a name, the bot can tell whom it means from the
   conversation; what is remembered is about a member account or the group, never about a name.
5. **Grounding and provenance.** Every learned item cites the lines it came from; readers see
   where it comes from, not a probability.
6. **Context.** Every model call sees the whole day it reads and what came before.
7. **Relevance and isolation.** A reply carries what matters to the people talking; the rest is
   on demand; nothing crosses groups.
8. **Untrusted content.** Memory carries what members said: material, never instruction.
9. **Simple and cheap.** As few mechanisms as hold up; a few extra calls a night on a cached
   prefix at most.

## 3. Research

- **Updates as history.** Zep / Graphiti keeps contradicted facts with an invalidation time. Mem0
  moved from model-chosen ADD / UPDATE / DELETE to add-only with temporal context, because
  UPDATE and DELETE destroyed history. MemoryAgentBench finds conflict resolution is where memory
  methods fail most, and a last-write resolver recovers much of it.
- **Propose, verify, commit.** EGMemory separates proposing memory changes from verifying them
  against evidence and committing them; HaluMem shows extraction and update errors compound
  across stages, so each stage is checked before anything is written.
- **Multi-party chat.** GroupMemBench finds memory systems at 46% on multi-party conversations
  and a BM25 baseline matching most of them: fact extraction strips speaker identity and threading.
  SpeakerMem-R1 keeps speaker-labelled messages beside per-person state. Tan et al. find models
  lose track of who said what in multi-party chat; whole-context reading with a focused task
  beats isolated slices (Lost in the Middle limits how much one call should be asked at once).
- **Segmentation.** SeCom and Nemori segment conversations into topical units with every turn in
  one unit; LightMem compresses inside units rather than dropping turns.
- **Offline consolidation.** Sleep-time Compute moves consolidation off the request path; a
  nightly batch is that.
- **Graphs.** Graphiti resolves entities by name similarity (exact, MinHash, embedding) before a
  model decides, with no notion of an alias; GraphRAG-Bench finds graphs help multi-hop reasoning
  and underperform plain retrieval on simple lookup; Mem0's graph variant gained about 2% on
  LoCoMo; mention-to-character linking in multi-party dialogue (SemEval-2018 Task 4) remains hard.
- **Forgetting.** Deployment-time memorization and Agentic Unlearning show deleted content returns
  through re-extraction and backflow (memory written into chat read back into memory) unless the
  deletion is recorded and checked. Product memory (OpenAI) exposes items people can see and
  delete. MINJA shows memory can be poisoned through ordinary interaction: what is remembered
  stays material, never instruction.
- **Sarcasm and jokes** are hard to detect (SarcasmBench); a decision may reject a statement as a
  joke, and code does not try.

## 4. Decisions

### 4.1 One store, as a deliberately small temporal graph

Names, statements, group vocabulary and what people enter by hand are one table, `memory`:
subject (a member or the group), an optional second member, kind (`name` or `statement`),
origin (`chat`, `ruling`, `hand`), durability and validity (current, replaced by, ended). Nodes
are members and the group only: identity comes from QQ accounts and `/link`, so people never
need entity resolution. Edges are memories with a validity period and their evidence. There are
no topic, event or outsider nodes and no predicate ontology: topics and events are episodes,
which are time-bounded, summarized, found by similarity and never need merging.

A full knowledge graph was rejected. Our hard problem is whom a name means, which a graph's
similarity-based entity resolution does not solve (a QQ nickname rarely resembles an account
name); our reads are per person, not multi-hop; and everything relevant to one conversation fits
in the prompt (a group has at most about a hundred people, each with at most 30 memories).

### 4.2 Observations are added; decisions are made once a night, per subject

Each episode's reading only lists observations: a name use or a statement, its line, author and
quote. It decides nothing about existing memories. After all episodes are read, one call per
subject sees its memories with ids (current and closed), what was forgotten, and every statement
about it from the day in chat order, and decides each once: support, new, replace, end, reject
with a reason, or merge. The latest sincere statement wins; what it changed is kept as history.
Code verifies and never judges: ids, one decision each, and the authority rule (hand entries are
never closed by chat; only a member's own word closes what they said about themselves or ends
their name).

Names are formed by code, keyed by their normalized text, with no model merging. A name nobody
could place is held under the name until a ruling places it, then its uses and statements are
filed with the member.

### 4.3 The whole day in every call; one commit per group

Every call of a group's batch starts with the same cached prefix: instructions, group state,
the eight newest summaries and the lines before, and the whole day. Each call then asks one
focused thing. All calls come first; then one transaction writes episodes, observations, memory
changes, room and the batch time. A failure writes nothing; a rerun after the commit finds
nothing new. This replaced the "stored but not applied" recovery and re-reading old episodes.

The model divides the day into contiguous episodes with every line once: the check guards against
truncated or lazy output, and extraction does not depend on the boundaries. Skipping lines and
holding back an unfinished conversation were rejected: real noise is rare (0.8% sticker-only
lines) and conversations almost never cross 04:00. A backlog is read a day at a time; a day too
long is split at its longest silence. Command traffic is left out by code: `/memory` replies print
what is known and must not be read back into it.

### 4.4 People manage memory with one command

Before, `/who`, `/name`, `/note`, `/forget` and `/group` each touched a different slice of what is
now one store. `/memory` views, adds (a statement or a name), corrects, ends and forgets any
entry by its id. Notes fold in as statements entered by hand: they are what a person registered,
so they are shown with who entered them, ranked first, and never changed by chat. `/link` and
`/block` stay separate: identity and admission, not memory.

### 4.5 Forgetting is a recorded suppression

`/memory forget` deletes the memory and its observations and records a suppression that keeps
the forgotten text, hidden from everyone. Code drops observations citing the lines it rested on
and refuses the same normalized text; the nightly decision sees the forgotten texts and rejects
restatements in other words. A batch that read before the forget leaves that subject alone until
its next run. Keeping the text is what makes a paraphrase recognizable; it never appears in a
reply, a command reply or any prompt but that subject's decision. Entering it again by hand lifts
the suppression.

### 4.6 Duplicate names are ordinary

Several members may go by the same name. The roster lists each member's names on their own
evidence; whom a line means is read from the conversation by the model, which needs no marking
of shared names (marking them made it hesitate where context was clear), and a person may enter a
name another member already has.

### 4.7 Standing as provenance; rank and room follow the conversation

Kept from the earlier design: what a reader sees is where an item comes from, not a probability;
rank decays with the subject's own conversation, not the calendar; each subject keeps a bounded
room, and eviction gives up closed memories first, then the lowest ranked, never what the batch
touched. Replies carry the roster and group knowledge in the cached prefix, the memories of the
people talking in the opening note, and the rest through `about_member` and `about_group`.

## 5. Alternatives rejected

| Alternative | Why not |
| --- | --- |
| Full knowledge graph (Graphiti-style) | similarity-based entity resolution does not find aliases; reads are per person; topic and event nodes need merging that compounds errors |
| Separate stores for names, notes and memories | five commands each touching a slice; `/forget` could not cover what it did not know about |
| Per-episode new / same / changed / ended with ids | each episode decided alone, so one night's statements could contradict each other; replaced by one decision per subject over the whole day |
| Model-chosen DELETE on contradiction | destroys the record that the old value was said |
| Per-member profile text rewritten nightly (Letta-style blocks) | drift and compounding errors; no per-item provenance; a forgotten item can survive in rewritten prose |
| Deletion without a record | what was forgotten is learned again from the archive or a paraphrase |
| Marking shared names | duplicates are ordinary; the conversation shows whom a name means |
| A write tool for the reply model | memory changes are a person's to make, with the command's permissions; a chat request is read nightly |

## 6. Migration

Migration `0020_one_store.sql` drops the old memory tables (`memory`, `memory_evidence`,
`name_reference`, `name_set`, `name_ruling`, `held_claim`, `name_removed`, `note`) and the
episodes' stored findings, and creates `memory`, `observation` and `suppression`. Learned and
hand-set names, notes and memories start from scratch: many learned names were wrong, and past
forgets left no record, so nothing is rebuilt from the archive. Episodes, the archive, links and
blocks are untouched; the bot relearns as people talk, from the next batch on.

## Sources

- Memory: [Mem0](https://arxiv.org/abs/2504.19413) and [its add-only algorithm](https://mem0.ai/blog/mem0-the-token-efficient-memory-algorithm), [Zep / Graphiti](https://arxiv.org/abs/2501.13956), [EGMemory](https://arxiv.org/abs/2609.23465), [HaluMem](https://arxiv.org/abs/2511.03506), [MemoryAgentBench](https://arxiv.org/abs/2507.05257) and [a last-write resolver](https://arxiv.org/abs/2610.09193), [LongMemEval](https://arxiv.org/abs/2410.10813), [Sleep-time Compute](https://arxiv.org/abs/2504.13171), [a memory survey](https://arxiv.org/abs/2505.00675), [Chain-of-Verification](https://arxiv.org/abs/2309.11495), [SarcasmBench](https://arxiv.org/abs/2408.11319), [MINJA](https://arxiv.org/abs/2503.03704)
- Multi-party chat and segmentation: [GroupMemBench](https://arxiv.org/abs/2605.14498), [SpeakerMem-R1](https://arxiv.org/abs/2609.26780), [Tan et al.](https://arxiv.org/abs/2310.16301), [Lost in the Middle](https://aclanthology.org/2024.tacl-1.9/), [SeCom](https://arxiv.org/abs/2502.05589), [Nemori](https://arxiv.org/abs/2508.03341), [LightMem](https://arxiv.org/abs/2510.18866), [EverMemBench](https://arxiv.org/abs/2602.01313), [T-Mem](https://arxiv.org/abs/2610.12874)
- Graphs: [Graphiti](https://github.com/getzep/graphiti), [GraphRAG-Bench](https://arxiv.org/abs/2506.05690), [SemEval-2018 Task 4](https://aclanthology.org/S18-1007/), [graph vs. vector memory](https://arxiv.org/abs/2601.07978)
- Forgetting: [deployment-time memorization](https://arxiv.org/abs/2606.10062), [Agentic Unlearning](https://arxiv.org/abs/2602.17692), [OpenAI memory FAQ](https://help.openai.com/en/articles/8590148-memory-faq)
- Tool design: [Anthropic, Writing effective tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents), [Claude docs, Define tools](https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools), [OpenAI, Function calling](https://developers.openai.com/api/docs/guides/function-calling)
