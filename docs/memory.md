# Memory: identity, episodes, memories, recall

How QBot remembers: `qbot-memory` (domain, pipeline, ports), `qbot-store` (Postgres), the
`recall_episodes`, `read_episode` and `about_member` tools, and what every run is shown about the
people in its group. The archive of chat lines is the source of truth; everything here is derived
from it. Why active memory is built this way, and what was rejected, is in
[memory-design.md](memory-design.md).

## 1. Model

- **Archive.** Every group line has a dense per-group *ordinal* (1, 2, 3, ...), assigned under the
  group lock in the same transaction as the line. Duplicates consume no ordinal.
- **Batch grid.** History is organized in fixed *batches* of `history.batch_lines` consecutive
  ordinals (30 by default), and the prompt's tiers move a whole batch at a time so the prompt
  prefix changes rarely: `history.raw_batches` newest batches verbatim, `history.summary_batches`
  before them as episode summaries, older chat only through tools (architecture.md section 7).
- **Slice and episode.** A *slice* is `memory.slice_batches` whole batches. An *episode* is the
  summary of exactly one slice: it owns that exact ordinal range and nothing else. The ranges of
  one group tile the archive with no gap and no overlap; Postgres enforces non-overlap with a
  range-exclusion constraint, so two workers cannot both store the same slice.
- **Context is not source.** When extracting a slice the model also sees the batch before it and
  the batch after it. They are labelled
  "context only, do not summarize or quote", evidence quotes must come from the target, and
  they never extend the episode's range. The transcript stays authoritative.
- **Following context is bounded.** It is capped by what the retained raw window leaves after the
  slice (`retained_raw_batches - slice_batches`), and extraction uses as many *complete* following
  batches as exist at that moment, possibly none. It never waits for context. A partial batch is
  not used, so the context is deterministic.
- **Immutable summaries.** An episode has a title, a summary, 1 to 3 verbatim evidence quotes
  (validated as substrings of target lines), its participants, and the method and model that
  produced it. Overlap or repetition between adjacent episodes is acceptable.

## 2. Extraction

Every filled batch queues its group's extraction job; nothing else extracts. A job that fails
is retried with backoff, and whatever a job leaves undone the group's next one does. `EpisodeJobs::extract_group` runs one group at a time: it first embeds any episode the
current embedding index lacks, then loops: find the next complete slice after the group's newest
episode, read the previous, target and next lines, call the model, validate, embed
`title + summary`, store the episode and its vector in one transaction, and apply its findings. It
is idempotent, resumes at a failed slice, and refuses an archive with a gap in a slice rather than
guessing. A slice that can never be extracted (the provider refuses its content or finds it too
long, or no valid answer came back in any of the three attempts) is recorded in `slice_skip` and
counts as covered, so it does not hold back the group's later slices; its lines have no episode,
so they stay verbatim wherever history shows them, and remain searchable. Network, account and
provider-side failures are not permanent: the job fails and the slice is tried again.

**What the extractor reads.** No tools, only context, so everything a reader needs to understand
the slice and to tell what lasts from what passes is put in front of it (method `slice-v6`):
- Lines are `[msg:ID] MM-DD HH:MM member:N: text` in the bot's time zone: who said what,
  when, and which line a `[reply:ID]` quotes. The target part's heading gives its first and last
  moments in full, with the year and weekday (`2026-10-15 Thu 20:30`), so "tomorrow" or "on
  Friday" can be turned into dates; the summary writes plans and deadlines as dates.
- The target lines are numbered; one batch on either side is unnumbered context.
- "Earlier in this chat" gives the title and summary of the group's episode just before the
  slice, so a slice that begins mid-conversation is read in its context.
- "Quoted from earlier" shows older lines that the shown lines reply to. Context only.
- Ahead of the lines, "Who is who" (`SliceBackground`, implemented by the prompt layer from the
  same records replies use) lists every member the lines involve, as speaker or as `[at:N]`: the
  display names their lines in the slice were sent under (from the archive, so a name the member
  has since changed is shown as it was then), the names people use for them with how they were
  used and whom else each is used for, linked accounts, a block, and every current memory about
  them with its id (`m12`), the most worth first. A member only mentioned is resolved to their
  account here, so a line addressed to them can be about them.
- "About the group" says what the bot is called (never a member's name), the operator's group
  background, and every current group memory with its id.
- Notes are left out: they are what members wrote about each other.

The prompt says that names and memories listed help interpretation but are no evidence: a finding
needs a line of the target and a quote from it. It says what counts as lasting: a memory must
still be true and useful in a few weeks; a mood, a one-off event, a plan for tonight or a wish
voiced once belongs in the summary at most. The model's reasoning effort is
`providers.text.extraction_reasoning`.

The model answers with one tool call, `submit_episode`: a title and summary in the bot's writing
language, one to three evidence quotes, and two lists, each item tied to a target line and a
verbatim quote. Code validates everything against the line's structure:

- The episode needs a title, a summary and one to three quotes copied exactly from target lines.
  A rejected episode goes back to the model with every reason.
- **Names** (`names`): a name a line uses for a member, with its *basis*. `own`: the member wrote
  the line. `addressed`: someone else wrote it and it mentions the member as `[at:N]` or replies
  to one of their messages. `about`: someone else wrote it. A basis the line does not bear out is
  rejected, as is a name not in its quote, one of the bot's names, a name some other member's
  lines are sent under, and a name given to two members in one answer.
- **Memories** (`memories`): about `member:N` or `group`, with an action. `new` (text and
  durability), `same` as memory `id` (it gains support), `changed` (memory `id` is replaced by
  the new text, which is kept with its durability) or `ended` (memory `id` holds no longer). A
  memory about a member needs a line they wrote or one directed at them (`[at:N]`, or a reply to
  their message): talk about someone absent is no evidence about any account. An `id` must be
  one the background listed for that same subject. A term is the group's and must be written in
  the target. A text is one sentence of at most 200 characters. One stretch of chat says one thing
  about each memory: its last line ("I quit it" then "back on it again" leaves it current).
- Rejected findings go back once; if the correction is not valid, the episode is kept with the
  findings that held. An answer cut off at the provider's output limit, or whose arguments are
  not JSON, is lost rather than wrong: the same request is sent again. At most three model calls
  per slice.
There is no staging state machine: a crash between the model response and the insert repeats one
slice's model call, which is cheaper than the machinery to avoid it. The episode records the
extraction method and model; a later change of extraction model does not invalidate existing
episodes or what was learned from them.

## 3. Retrieval

- `recall_episodes(question)` embeds the question and scans the group's episode vectors exactly
  (retrieval filters by group first, so the scan is small; no approximate index). Candidates are
  the episodes within `memory.recall.max_distance` (cosine, below 1). They are ranked by one score,
  similarity times `0.5^(age / half_life)` with age counted from the episode's end
  (`memory.recall.half_life_days`, 180 by default; 0 ranks by similarity alone), then the more
  recent episode, then the higher id, and the best five are returned. There is no age cutoff: a
  strong old match still surfaces over a weak recent one (0.98 similarity three months old beats
  0.56 from today), while a somewhat less similar recent episode beats a very old one (0.7 from ten
  days ago beats 0.8 from a year ago).
- `read_episode(id)` returns the raw lines of that episode's range, bounded by the slice size.
- **Embeddings are a derived index.** An episode has one vector, from the configured embedding
  model and width. When either changes, recall finds an episode again once its group's next extraction
  (queued by its next filled batch) has embedded it anew from its stored title and summary;
  nothing is extracted again.

## 4. Identity

Terms. A member's *group nickname* is the name they set for one group (OneBot `card`). Their
*account nickname* is the account's global QQ name (OneBot `nickname`). Their *group display name*
is what the group shows: the group nickname, else the account nickname.

- *Account* (a platform login, the QQ number) is recorded in the same transaction as its first
  line. It is the one global identity record and is never shown; everything about an account is
  kept per group ([architecture.md](architecture.md) section 8, "Groups are separate").
- *Member number*: an account's number in one group (`member:N` to the model), assigned densely
  the first time the account appears there and never changed, reused or removed (a trigger on
  `member_number` rejects every update, delete and truncate). Account ids never reach the model;
  two accounts of one person keep two numbers.
- *Linking* records a link between two accounts in the group where it was made
  (`account_link`). A *person* in a group is the accounts linked there, directly or through
  others; a link made in another group plays no part. *Invitations* to link: ten minutes, one
  pending per account per group, only the target confirms, an expired or stale one is closed.
- *Display names are recorded.* Every member line keeps the display name it was sent under
  (`chat_line.sender_name`, from the event's `sender.card`, else `sender.nickname`). From the
  archive the bot knows what each member is shown as now and was shown as before: members keep
  calling each other by old and shortened display names. The current name is also read live from
  NapCat for the members of a run's conversation (`qbot_agent::Directory`), for the trigger note
  and command replies, because a name changes apart from any line.
- *Names people use are uses, not beliefs* (`qbot_memory::names`, `name_use`). Each use records
  that, in one episode, a line used a name for a member, with its basis: `own`, `addressed`,
  `about`, or `set` for a name a person gave with `/name` (vouched for, not inferred). One
  episode counts once per member and name, with its strongest basis. Whom a name refers to is
  read from all its uses together (`KnownName`: per member and name, how many episodes used it
  each way and when last), so a name used for one member in eight conversations and for another
  once reads exactly so, and a name used for several members (or that is another member's display
  name) is marked as shared. Nothing resolves a name automatically: the model reads the uses and
  the conversation, and asks when they do not tell.
- *A name others use needs an anchor before it is shown.* A use with basis `addressed` or
  `about` is the extractor's reading of a line, and a line that mentions or answers someone can
  still be naming a third person. So such a name is shown to a model (the roster, the
  extractor's "Who is who", `about_member`) only when something ties it to the member: a person
  set it; the member gave it themselves; the member answered the line that used it (a later line
  of theirs in the slice replies to it, recorded as `name_use.anchored`); or it is at least two
  characters long and part of a display name their lines were sent under. Without an anchor it
  is kept, and shown once `CORROBORATED_USES` (2) conversations used it for that member and no
  other person has a claim to it (an anchor of their own, as many conversations, or it being
  their current display name); two people with unanchored claims to one name are both left
  out. `/who` and `/name` list every recorded name, shown or not, so a member can remove one.
  The rule is applied on read (`qbot_prompt::people::NameIndex`): nothing is deleted, and a name
  appears as soon as it is corroborated.
- A name a person removed (`/name remove`) loses its uses and is suppressed for that member
  (`name_removed`): chat using it again is not recorded until a person sets it again. A set name
  is not set for a member outside those given while it is set for another (`NameTaken`); learned
  names may be shared.
- *Member number versus name*: in what the bot writes, people are called by their current group
  display name, addressed with `[at:N]`, or described; a number appears only when someone asks
  about the numbers.

## 5. Compaction alignment

An episode covers whole batches and its summary never changes, so it is exactly what can stand in
for older chat when context must shrink. `compose(lines, episodes, keep_tail)` shows this: lines
wholly inside an episode and before the raw tail become one recap; partly covered episodes and the
tail stay raw; every line appears exactly once. Because the replacement is whole-batch and
immutable, the composed view is stable between requests and keeps the provider's prefix cache.
This is how the summary tier is shown: episodes ending in it replace their lines from the start
of every run (as `Summary` items over exactly those chat items), and episodes inside the verbatim
tier are the fallback when a provider still reports the context too long. Each filled batch queues
the extraction, so episodes normally exist before their lines leave the verbatim tier.

## 6. Why slicing is fixed

Before settling on fixed slices we measured segmentation strategies on synthetic chats with known
conversation boundaries (silence, speaker changes, reply edges, embedding-similarity dips; ground
truth by construction; the experiment code has since been removed). Findings, mean boundary F1
over five scenarios:

| Method | Mean F1 |
| --- | --- |
| silence gaps only | 0.58 |
| gaps + speaker change + reply edges | 0.58 |
| embedding-similarity dips only | 0.70 |
| all four signals combined (noisy-OR) | 0.82 |

Other results: no single signal wins everywhere (gaps are perfect when conversations are
separated by silence and near 0.03 when only the topic changes); each structural signal helps
*inside* the combination (replies lift interleaved threads from 0.64 to 0.75) though not alone;
averaging and noisy-OR combination perform the same once each is tuned (0.805 vs 0.816); the cut
threshold is the sensitive part (a 0.05 shift costs 0.05 to 0.10 F1), so any cutoff would be
brittle on real data; a generous candidate set reaches about 0.98 recall at roughly 5 candidates
per true boundary.

Caveat: the generator plants exactly the signals the scorers read, so this shows the mechanisms
and their trade-offs, not real-chat quality. Decision (project owner): precise boundaries are not
needed for this use case, so episodes are fixed, batch-aligned slices; an LLM boundary detector or
a segmentation state machine is only worth building if fixed slicing proves clearly insufficient.
The design above would then still hold: the slicing function is the only part that changes.

## 7. Memories: provenance, history, rank and room

Memory holds two kinds of thing. *Records* say what happened: archive lines, episodes, notes
members wrote. *Memories* (`qbot_memory::memories`, `memory` and `memory_evidence`) are what the
bot keeps knowing about a member or the group: one short statement in the bot's words ("lives in
Hangzhou", a group term and what it means here, what the group is for), each with every line
that supported it (episode, message, author, quote, time).

**Provenance, not probability.** What a reader sees of a memory is who said it (the member
themselves, others to them, or both; for the group, its members), in how many conversations, and
when last. No confidence number is computed or shown: almost everything is said once, and a
probability that is the same everywhere carries no information. The reply prompt says how to
weigh what it sees: said by the member, recently or more than once, is a good guide; said once,
long ago, or only by others is what was said then.

**History, not overwriting.** A later statement either supports a memory (`same`, or a `new`
statement equal to a current memory by `memory_key`: the term and text compatibility-normalized
and lower-cased), replaces it (`changed`: a new memory, the old one kept as `replaced` with the
time and its replacement), or ends it (`ended`, with the time). A replaced or ended memory is no
longer shown in replies; `about_member` shows it as what no longer holds. Said again after it
ended, a statement is remembered anew. The model makes the semantic call (it sees the current
memories with their ids); code checks that every id names a current memory of that subject in
that group, so a statement can never touch another member's memories.

**Durability and rank.** A memory is `lasting` (where someone lives, comes from, works or studies,
family, a lasting trait or boundary), `ongoing` (tastes, hobbies, what they keep doing) or
`current` (what they are playing or working towards these weeks). Durability sets how fast it
loses rank, never whether it is true. Rank (`qbot_memory::worth`, never shown) is
`n / (n + 1) * 0.5^(k / h)`: `n` distinct supporting episodes, `k` the episodes its subject took
part in since the last support (for the group, every episode of the group), `h` the durability's
`memory.rank.half_life_episodes` (lasting 200, ongoing 40, current 10). The clock is the
conversation, not the calendar: a quiet member's or group's memories keep their place, and a
member who says nothing has not passed anything over. Rank is computed as of the start of the
UTC day in what replies are shown, so the cached prefix changes by ageing at most once a day.

**What each run is shown** (`qbot-prompt`):
- In the cached prefix, **People in this group**: the roster (`PgArchive::people`). A person is
  listed when one of their accounts spoke within `memory.roster_days` (30), is linked or blocked,
  or has a name set with `/name`; at most `memory.roster_people` (80), the most recently active
  first. For each: what they are shown as and were shown as before (up to four display names per
  account), the names people use for them with how each was used and whom else it is used for,
  linked accounts, blocks. Activity and display names count lines up to the start of the UTC day.
- In the cached prefix, **Learned about this group**: the `memory.group_memories` (40) group
  memories of most rank, listed by term and text.
- In the run's opening note, for the people of the run's conversation (who wrote the newest 50
  lines of the window, whom those lines mention, whom a due task's intent names): notes members
  wrote and the `memory.person_memories` (8) memories of most rank for each. Memories of people
  not in the conversation are not in the prompt at all: relevance, and less to pass on unasked.
- On demand, **`about_member(member)`**: everything kept about one member, with what no longer
  holds. It also makes them mentionable in the run.

**Room.** Each member keeps at most `memory.capacity.per_member` (30) memories in every state and
the group `memory.capacity.group` (120). Applying an episode, a subject it touched that is over
its room gives up replaced and ended memories first (the longest closed first), then current ones
of least rank, weighed as of the episode's end; never one the episode itself touched, so new
information always finds room and applying an episode again evicts nothing more. Episodes keep
their findings and the archive its lines: something said again is learned again.

**Consolidation** applies each episode's findings right after it is stored and, for any left over
by a crash, at the start of the group's next extraction: name uses to the name index, statements
as changes in the order the extractor listed them (one per memory: its last line), then room. Every step is idempotent: an episode records a use and
supports a memory at most once.

**Erasure.** `/who` lists a member's current memories and, apart, those a later statement
replaced or ended (still kept), each with its id. `/forget ID` deletes one memory of either kind
and its evidence, `/forget all` every memory about a
member, `/forget group ID` (owners) a group memory; `/name remove` a name. Deletion is real: the
row and its evidence go. An episode already applied is never applied again, so a forgotten memory
comes back only if someone says it again. The episode's own findings and the archive's lines are
records of what was said and stay (section 9).

**Cases** (`qbot-memory/tests/pipeline.rs`, `qbot-prompt/tests/prompt.rs`, `eval/scenarios`):
- *A shared name*: "Boss" used for one member in eight conversations and for another once is
  shown for the first only: one unanchored use is a guess (`shared_name`). Anchored for both
  (each answered to it, or set it), it is listed for both and marked as also used for the other,
  and the reply model goes by the conversation or asks.
- *An old display name*: a member asked about by the name they were shown as before is found by
  the roster's earlier display names (`former_display_name`).
- *Moved*: "lives in Hangzhou" replaced by "lives in Ningbo"; replies see Ningbo, `about_member`
  shows Hangzhou as no longer holding (`moved_member`, `absent_member`).
- *Quit*: an ended memory leaves replies and takes room first.
- *Someone absent*: what one member says about another who is neither writing nor addressed is
  not recorded about anyone.

## 8. Manual notes are not memory

Two kinds of information about people and the group are kept apart (architecture.md section 18):

| | Manual notes | Memories |
| --- | --- | --- |
| Written by | a person, with `/note` | extraction, from chat lines |
| Store | `note` | `memory` / `memory_evidence` (names: `name_use`) |
| Removed by | `/note remove` / `/note clear` | `/forget ID`, `/forget all`, `/forget group ID` |
| Standing | a record of what someone wrote | who said it, how often, when last |
| Model sees it as | a note written by a member | a memory with its provenance |

Neither path writes into the other: extraction reads chat lines, never notes; `/forget` never
touches notes and `/note` never touches memories. Help texts name the store each command acts on.

## 9. Retention

Episodes, memories and their evidence are small text. Memories are bounded per subject (section
7); episodes and the archive are kept.

A recalled message's line keeps its place in the archive but says only `[recalled]`
([architecture.md](architecture.md) section 4), so a slice extracted after the recall learns
nothing from it. An episode or memory made from it before the recall stays: episodes are never
rewritten, and a message recalled within minutes is rarely in an extracted slice yet. A member
removes what was remembered from it with `/forget`.
