# Active memory: design record

Why active memory (what QBot learns from group chat and brings into its own context unasked) is
built the way [memory.md](memory.md) describes. Episodes and the archive are not the subject
here; they are inputs. This record covers the requirements, what was measured, the alternatives
weighed and the migration.

## 1. Evidence from the deployment

Read-only aggregates over the production database (2026-11-03, 9 groups, 636 episodes):

- **Names.** 77 stored names: 2 set by a person, 75 learned from chat. 60 of the 75 rest on one
  episode, 8 on two, 7 on three or more. In the busiest group 5 name texts point at two or three
  accounts at once, every one a learned lead (evidence counts such as 1/8/12 or 1/2/2), and 15
  accounts have more than one name (up to 8). Each was shown as an independent belief "about 67%
  likely", so a name used twelve times for one member and once for another looked the same.
- **Where names came from.** Of 122 name findings, 68 were taken from a line the named member
  wrote and 54 from a line that @-mentions them; there was no check that the line actually uses
  the name *for* that member (writing a name in your own line usually names someone else).
- **Display names were never recorded.** Every message event carries the sender's group
  nickname and account nickname, but only the live name was asked of the platform. The extractor
  saw today's display name for lines written months ago, and nothing knew that a member was
  shown as something else before, though members keep calling each other by old and shortened
  display names.
- **Facts.** 506 active member facts by 28 fixed predicates; 94% single-support, so almost
  every fact showed the same "about 67% likely". The largest classes were `wants` and `uses`
  (92 each), mostly passing wishes. 573 of 575 fact evidence lines were written by the subject.
- **What every reply carried.** The people block listed everyone with a record (up to 29 people
  in one group, 16 facts each), whoever was talking, in every run.

## 2. Requirements

Derived from what an AI member of a shared group chat needs, not from the existing design.

1. **Reference.** When someone writes a name, the bot must be able to tell whom it means, and
   when a name could mean several members, see that it could. Remembered things are always about
   one member account (or the group), never about a name.
2. **Grounding.** Every remembered item traces to the lines it came from: who said it, when, the
   quote. Derived text never stands without its source.
3. **Time.** Things change: a new statement must be able to replace an old one without erasing
   that the old one was said; the bot must see how old something is.
4. **Honest standing.** The model must see *why* it believes something (they said it
   themselves, someone else said it, how often, how recently), not a number that looks precise.
5. **Relevance.** A reply needs what matters to the people in the conversation, not everything
   about everyone; anything else must be reachable on demand.
6. **Correction and erasure.** Members see what is kept about them and remove it; removal deletes
   the item and its evidence; a name a person removed stays removed.
7. **Isolation.** Nothing learned in one group appears in, or influences, another.
8. **Untrusted content.** Memory carries what members said: it is material, never instruction,
   and never forms a marker.
9. **Bounded.** Storage per member and per group, and the tokens memory adds to a run, are
   bounded; what makes way follows the group's own conversation, not the calendar.
10. **Cost.** No model call beyond the one extraction call per episode that already exists; the
    cached prompt prefix stays stable within a day.

## 3. Research

- **Multi-party memory is its own problem.** GroupMemBench (2026) finds the best memory systems
  at 46% on multi-party conversations, worst on knowledge updates (27%) and term ambiguity
  (38%), and a BM25 baseline matching most of them: fact extraction "strips away the identity of
  the speaker and the threading structure". SpeakerMem-R1 (2026) keeps speaker-labelled verbatim
  messages beside derived per-person and per-group state and combines them at query time; its
  ablations show the two tracks are complementary.
- **Updates as history, not deletion.** Zep / Graphiti (Rasmussen et al. 2025) is bi-temporal:
  a contradicted fact gets an invalidation time and is kept. Mem0 (Chhikara et al. 2025) began
  with model-chosen ADD / UPDATE / DELETE / NOOP and has since moved to ADD-only with temporal
  context, because UPDATE and DELETE destroyed history. MemoryAgentBench (ICLR 2026) shows
  conflict resolution is where current methods fail most.
- **Memory the agent maintains and the agent looks up.** MemGPT / Letta separate a small core
  memory always in context from archival memory searched by tools; A-MEM links notes and lets new
  ones revise old ones. Generative Agents rank records by recency, importance and relevance.
- **Names in chat.** IRC studies (Ecker) find nicknames are mostly addressing markers, members
  use several variants of one another's names, and which part of a name survives shortening is
  unpredictable: names are matched against the current user list. Dickey (1997) separates forms
  of address from terms of reference: a name used *to* someone and a name used *about* someone
  are different evidence.
- **Product practice.** ChatGPT and Claude both expose memory as items a person can see and
  delete, separate from chat history; Claude scopes memory per project, which is the analogue of
  per-group memory here.

## 4. Decisions

### 4.1 Identity: what the platform shows is evidence; names are a reference index

The platform's display name is the most reliable name evidence there is, so it is recorded: each
member line keeps the display name it was sent under (`chat_line.sender_name`). From the archive
the bot knows what each member is shown as now and was shown as before.

Names people use in conversation are not beliefs attached to an account but **uses**: "in this
episode, this line used name X for member N, as their own name / addressing them / talking about
them". A name is resolved by looking at all its uses together: `X` used in eight conversations
for member 3 and once for member 7 is shown exactly so, and a name used for several members is
marked as shared instead of being a confident claim about each. The basis of a use is checked in
code against the line's structure: *self* only on the member's own line, *addressed* only on a line
that @-mentions the member or replies to their message, *about* only on someone else's line. A name a
person set (`/name`) is a use with basis *set*; a name a person removed is suppressed for that
member until a person sets it again.

### 4.2 Memories: items with provenance and history

A memory is one short statement about one member (or the group) in the bot's own words, with its
evidence: each supporting line, its author and quote. The extractor sees the subject's current
memories with their ids and says for each statement whether it is *new*, *the same* as memory
`m12` (support), *changes* `m12` (a new memory replaces it; the old is kept as superseded, with
the time), or *ends* `m12` (no longer holds). Code checks every id belongs to that subject in that
group and is current. The model makes the semantic call it already sees the material for; nothing
is computed from string normalisation or a predicate table.

A memory has a durability (*lasting*: where someone lives, works or studies, family, names;
*ongoing*: tastes, hobbies, long-running interests; *current*: what someone is playing, reading
or working towards these weeks), which sets how fast it loses rank, never how true it is.

### 4.3 Standing is shown as provenance, not probability

What the model reads for each memory: who said it (they themselves, or another member), in how
many conversations, and when last. A replaced memory is not shown in replies; `about_member`
shows recent history ("previously: ..."). There is no confidence percentage: with one statement
behind almost everything, a computed probability was the same number everywhere and implied a
precision nobody had.

### 4.4 Ranking and capacity follow the conversation

Rank (never shown) is `n / (n + 1) * 0.5^(k / h)`: `n` distinct supporting episodes, `k` episodes
the subject has taken part in since the last support (every group episode for group memories),
`h` the durability's half-life in episodes. A quiet member's or group's memories keep their rank.
Each subject keeps at most `memory.capacity` memories; after an episode is applied, a subject over
its room loses replaced and ended memories first, then the lowest ranked, never what that episode
touched.

### 4.5 Injection: identity always, memories for the people present, the rest on demand

- **Cached prefix (stable within a day):** "People in this group": for each member active recently
  or with a record, what they are shown as (now and before), the names others use for them with
  their counts and sharing, names set by a person, linked accounts, blocks. Then "Learned about
  this group": the group's terms and group memories, ranked.
- **Each run's opening note:** for the members in the visible chat (speakers, mentioned members,
  the sender), notes members wrote and the best-ranked memories, with provenance.
- **On demand:** `about_member(member)` returns everything kept about one member, for anyone not
  in view.

### 4.6 Erasure and isolation

`/forget N` deletes memory N and its evidence; `/forget all` deletes every memory about a member
(the sender's own accounts, or a member an admin or owner may manage, as every profile command);
`/name remove` deletes a name's uses and suppresses it. Every table is keyed by group and every query filters by it; a memory's evidence
must be a line of that group. Episodes keep their findings as records of the extraction; a deleted
memory is not re-learned from an episode already applied.

## 5. Alternatives rejected

| Alternative | Why not |
| --- | --- |
| Keep per-(name, account) beliefs, tune thresholds | the failure is structural: independent per-account confidence cannot express that one name is shared or that one referent dominates |
| Names only from `/name` and display names (no learning) | members call each other by nicknames no display name contains; those would be unresolvable |
| Model resolves names freely without stored evidence | the same name resolved differently on every run, with nothing to show members or correct |
| Fixed predicate triples (the previous design) | a closed list forces a fit (`wants`, `uses` became noise), rewording needed exact-value matching, and conflicts needed a slot table |
| Per-member profile text rewritten by the model each episode (Letta-style blocks) | drift and compounding errors; no per-item provenance; a removed item can survive in rewritten prose |
| No derived memory, only search at answer time | identity must be known before the model can search; the model would rediscover the same facts every run at tool cost |
| Model-chosen DELETE on contradiction | destroys the record that the old value was said (Mem0 moved away from it) |
| Confidence probability per item (the previous design) | 94% of items had one statement; a number the same everywhere carries no information |
| Every person's memories in the cached prefix | relevance and privacy: what is not about the people talking is not needed and invites mentioning it |

## 6. Migration

One forward migration:

- `chat_line.sender_name` is added; older lines have none (unknown), new lines are filled by the
  gateway from the event.
- Names: names set by a person become *set* uses, removed ones become suppressions; names learned
  from chat are dropped (their evidence never said how the name was used, 60 of 75 rested on one
  episode, and they caused the ambiguity). Learned names come back as episodes are extracted.
- Facts: each active member fact becomes a memory `predicate: value` with its evidence (author
  from the archive line); of competing claims for one single-valued attribute the most recently
  supported stays current and the others are superseded by it; facts said to have ended become
  ended memories; forgotten facts are dropped. Group terms and topics become group memories.
  Durability follows the old decay class (stable, default, fast).
- The old `alias`, `alias_evidence`, `fact` and `fact_evidence` tables are dropped. Configuration
  keys `memory.person_facts`, `memory.group_terms`, `memory.facts.*` and `memory.capacity.*` are
  replaced (no deployment overrides them).
- Episodes, the archive, notes, links and blocks are untouched.
