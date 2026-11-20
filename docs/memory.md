# Memory: identity, episodes, memories, recall

How QBot remembers: `qbot-memory` (domain, pipeline, ports), `qbot-store` (Postgres), the
`recall_episodes`, `read_episode`, `about_member` and `about_group` tools, the `/memory` command,
and what every run is shown about the people in its group. The archive of chat lines is the
source of truth; everything learned here is derived from it, and what people enter or remove by
hand is authoritative over it. Why memory is built this way, and what was rejected, is in
[memory-design.md](memory-design.md).

## 1. Model

- **Archive.** Every group line has a dense per-group *ordinal* (1, 2, 3, ...), assigned under the
  group lock in the same transaction as the line. Duplicates consume no ordinal.
- **Batch.** Once a day (`memory.batch`, `daily@04:00` by default: an off-peak hour of the
  provider) one job reads each group in turn: everything archived since the group's previous
  batch. Replies show history as of that batch (architecture.md section 7): the summaries it
  left, then the raw lines from just before what it summarized, so the prompt prefix changes once
  a day.
- **Episode.** An *episode* summarizes an exact range of ordinals, whose boundaries the model
  chooses where one conversation ends and the next begins; code checks that the episodes of a
  stretch hold every line read exactly once, in order. The ranges of one group tile the archive
  with no gap and no overlap; Postgres enforces non-overlap with a range-exclusion constraint.
  An episode has a title, a summary, one to three verbatim evidence quotes, its participants, and
  the method and model that produced it. It never changes.
- **Observation** (`observation`). What one line told: a *use* of a name for a person, or a
  *statement* about a member, the group, or a person known only by a name nobody has placed yet.
  Each keeps its episode, message, author, quote and time. Observations are added, never edited;
  one that supports a memory is its evidence, one about a name nobody placed waits (*pending*).
- **Memory** (`memory`). What the bot keeps knowing: one entry about a member (or two members),
  or about the group. Two kinds: a *name* the member goes by, and a *statement* (one short
  sentence in the bot's words; a group statement may be a *term* and what it means here). Each has
  an origin (`chat`, `ruling`, or `hand` with who entered it), a durability, and a state:
  current, replaced (by which memory, when) or ended (when). Its support is its observations.
- **Suppression** (`suppression`). What a person asked to be forgotten: subject, kind, text,
  normalized key, the messages it rested on, who and when. Never shown to anyone; it keeps the
  forgotten thing from being learned again (section 8).

## 2. The batch

Nothing else reads chat into memory. A job that fails is retried with backoff; whatever a batch
leaves undone the next one does. `EpisodeJobs::run` takes the groups one after another, so a
group's calls follow each other while the provider still caches their shared prefix; a group that
fails does not stop the others. `EpisodeJobs::batch_group`, under the group's lock:

1. embeds any episode the current embedding index lacks;
2. takes the lines after the last covered ordinal, refusing an archive with a gap rather than
   guessing, and reads them a local day at a time, oldest first (a backlog after downtime is
   several days; each day's commit is the state the next is read against);
3. leaves command traffic out (a member line whose first word is a command, and the bot's reply
   quoting it): it is interaction with the bot, and `/memory` replies print what is known, which
   must not be read back into memory. Its lines are still covered by the episodes around them;
4. builds one shared prefix: the instructions (`prompts/batch.md`) with all four tools, the group
   state ("Who is who", "About the group"), the group's eight newest episode summaries and the 30
   lines before the day ("Earlier in this chat"), older lines the day quotes, then the day's
   lines numbered from `#1`;
5. asks, each call that prefix plus a short task: the day's episodes (`submit_episodes`); each
   episode's findings (`submit_findings`), with the whole day in view; up to six rulings on names
   nobody could place (`submit_ruling`); and one decision per subject with statements
   (`submit_decisions`);
6. only when every call has answered, embeds the episodes and commits everything in **one
   transaction**: episodes, observations, memory changes, room, and the batch's time
   (`history_window`), which replies are shown as of. A failure before that writes nothing, and
   the next attempt reads the same lines again; a rerun after it finds nothing new.

A day the provider refuses as too long is cut at its longest silence and read in parts. Lines
whose episodes can never be read (the provider refuses their content, or no valid division came
back in any of the three attempts) are recorded in `slice_skip` and count as covered, so they do
not hold back the group's later chat. Network, account and provider-side failures are not
permanent: the job fails and the lines are read again.

**What the batch reads.** No tools, only context (method `batch-v2`):
- Lines are `[msg:ID] MM-DD HH:MM member:N: text` in the bot's time zone. The heading of the
  lines gives their first and last moments in full, with the year and weekday, so "tomorrow" can
  be written as a date.
- "Who is who" (`SliceBackground`, implemented by the prompt layer from the same records replies
  use) lists every member the lines involve: the display names their lines were sent under (from
  the archive, as they were then), the names people use for them (section 4), linked accounts, a
  block, and every current memory about them with its id (`m12`). Below them, the rest of the
  roster with their display names and names, and up to twelve names the group uses for someone
  nobody could place yet.
- "About the group" says what the bot is called (never a member's name), the operator's group
  background, and every current group memory with its id.

What the background lists helps reading; it is no evidence. A finding needs a line and a quote.

**Findings** (`submit_findings`), each tied to one line of the episode and a verbatim quote, and
checked by code against the line's structure:
- **References**: every place a member's words name a person: the expression, whom it means
  (`member:N`, `bot`, `outside`, or `unknown`), how the line uses it (`address`, `about`, `self`)
  and what shows who is meant (`marker`, `answer` with the answering line, `content`, `self`,
  `listed`, `none`). Kept only if the expression is in what the member typed on that line, a
  `marker` really replies to or mentions that member, an `answer` is a line that member wrote,
  and the expression is not how another member is shown. A use of one of the bot's names, or of
  someone outside the group, is not kept.
- **Statements**: about `member:N` (optionally `with` a second member), `group` (optionally a
  `term`), or `name:EXPRESSION` (someone recorded as `unknown`), with a text of at most 200
  characters and a durability. A statement about a member needs a line they wrote, one directed
  at them, or a reference in the same answer that names them.

Rejected findings go back once with every reason; an answer cut off at the provider's output
limit, or whose arguments are not JSON, is asked for again. At most three model calls per task.

**Names** are formed by code, not by the model: each use of a name for a member supports that
member's name memory (created on its first use, keyed by the normalized name).

**Decisions** (`submit_decisions`, `qbot_memory::decide`). One call per subject: the group first,
then each member. It sees the subject's memories with ids and where each comes from, what no
longer holds, what was forgotten at a member's request, and every statement about them read today
or left pending, in chat order (`o1`, `o2`, ...). Each statement gets one decision: `support` (a
current memory, or one created earlier in the same answer), `new`, `replace` (a current memory
no longer holds; the new text holds instead), `end`, or `reject` with a reason (`joke`,
`hypothetical`, `not_about`, `passing`, `forgotten`, `contradicts`, `unsupported`); two current
memories saying the same thing may be merged. Code checks, and sends back with every problem:
- every statement is decided once, and every id names a current memory of that subject;
- what was entered by hand is never replaced or ended by chat;
- what a member said about themselves is replaced or ended only by their own word (either
  member's, for a memory about two);
- a name ends only by its member's own word ("don't call me that any more");
- a statement that restates something forgotten, with the same normalized text, is rejected
  whatever was decided.

Undecided statements (the provider refused the case) stay pending for the subject's next batch.
The rule the model applies is the latest sincere statement wins, and what it changed is kept.

**Rulings** (`submit_ruling`). An expression with at least three `unknown` uses from at least two
episodes is *open*. The batch puts the six most used open names to a ruling with up to eight of
their uses, one per conversation, each with six lines before and four after. It rules `member:N`
(only a member the lines involve, citing lines that were shown), `outside` or `general` (with
what it means in the group), or `unknown` (asked again when more uses gather). A ruling for a
member files every use of the name, now and later, and every statement held under it, with that
member, and their statements are decided in the same batch. `outside` and `general` keep the
name as a group term with its meaning (origin `ruling`) and drop its uses.

## 3. Retrieval

- `recall_episodes(question)` embeds the question and scans the group's episode vectors exactly
  (retrieval filters by group first, so the scan is small). Candidates are within
  `memory.recall.max_distance`; they are ranked by similarity times `0.5^(age / half_life)`, age
  counted from the episode's end (`memory.recall.half_life_days`, 180), and the best five are
  returned. A strong old match still surfaces over a weak recent one.
- `read_episode(id)` returns the raw lines of that episode's range.
- `about_member(member)` and `about_group()` return everything kept about one member or about the
  group, with what no longer holds (section 7).
- **Embeddings are a derived index.** When the embedding model or width changes, recall finds an
  episode again once its group's next batch has embedded it anew from its title and summary.

## 4. Identity and names

Terms. A member's *group nickname* is the name they set for one group (OneBot `card`); their
*account nickname* is the account's global QQ name; their *group display name* is what the group
shows: the group nickname, else the account nickname.

- *Account* (the QQ number) is recorded with its first line. It is the one global identity record
  and is never shown; everything about an account is kept per group ([architecture.md](architecture.md)
  section 8, "Groups are separate").
- *Member number*: an account's number in one group (`member:N` to the model), assigned densely
  the first time the account appears there and never changed, reused or removed. Account ids never
  reach the model; two accounts of one person keep two numbers.
- *Linking* records a link between two accounts in the group where it was made (`account_link`).
  A *person* in a group is the accounts linked there.
- *Display names are recorded.* Every member line keeps the display name it was sent under
  (`chat_line.sender_name`). From the archive the bot knows what each member is shown as now and
  was shown as before: members keep calling each other by old and shortened display names.
- *Names are memories.* A name memory is supported by its uses; `KnownName` reads them together:
  per member and name, how many episodes used it for themselves, to them and about them, how many
  were anchored (a line said to them or answered by them) or grounded (shown by a marker, an
  answer or their own word), and when last. Several members may go by the same name; which one a
  line means is the conversation's to show, and the model reads it there.
- *A name others use needs evidence before it is shown.* It is shown to a model (the roster, the
  batch's background, `about_member`) when a person entered it, a ruling placed it, the member
  used it themselves, a line said to them or answered by them used it, or it is at least two
  characters long and part of a display name of theirs; otherwise once two conversations used it
  for that member. Unanchored uses are still recorded. `/memory` lists every name, shown or not.
  The rule is applied on read (`qbot_prompt::people::NameIndex`).
- *Member number versus name*: in what the bot writes, people are called by their current display
  name, addressed with `[at:N]`, or described; a number appears only when someone asks for it.

## 5. Compaction alignment

An episode covers an exact range and its summary never changes, so it is exactly what can stand
in for older chat. `compose(lines, episodes, keep_tail)` shows this: lines wholly inside an
episode and before the raw tail become one recap; partly covered episodes and the tail stay raw;
every line appears exactly once. The history window (architecture.md section 7) passes it the
newest `history.summaries` episodes before the raw part and only their lines, so every run
between two batches starts from the same summaries and the provider's prefix cache holds for the
day. A run refused as too long leaves out the older part of the raw lines (a `Trim` item), never
a summary.

## 6. Why the model chooses boundaries

Before the daily batch, episodes were fixed slices of whole batches. We had measured segmentation
strategies on synthetic chats with known conversation boundaries (silence, speaker changes, reply
edges, embedding-similarity dips; ground truth by construction; the experiment code has since
been removed). Mean boundary F1 over five scenarios:

| Method | Mean F1 |
| --- | --- |
| silence gaps only | 0.58 |
| gaps + speaker change + reply edges | 0.58 |
| embedding-similarity dips only | 0.70 |
| all four signals combined (noisy-OR) | 0.82 |

No single signal wins everywhere, and the cut threshold is the sensitive part, so any heuristic
cutoff would be brittle on real data. The batch reads a whole day of a group anyway, so the model
that summarizes it chooses where one conversation ends, at no extra call. Code checks only what
correctness needs: the episodes hold every line exactly once, in order. Interleaved topics share
an episode whose summary names both; extraction does not depend on the boundaries, because every
call sees the whole day.

## 7. Memories: provenance, history, rank and room

**Provenance, not probability.** What a reader sees of a memory is where it comes from: entered
by hand (and by whom, to members), or said in chat by the member themselves, by others to them,
or both (for the group, its members), in how many conversations and when last. No confidence
number is computed or shown. The reply prompt says how to weigh it: entered by hand is what a
member wrote, unchecked; said by the member recently or more than once is a good guide; said
once, long ago, or only by others is what was said then.

**History, not overwriting.** A replaced or ended memory is kept, with when and by what, and is
no longer shown in replies; `about_member`, `about_group` and `/memory` show it as what no longer
holds. Said again after it ended, a statement is remembered anew.

**Durability and rank.** A statement is `lasting` (where someone lives, comes from, works or
studies, family, a lasting trait), `ongoing` (tastes, hobbies, what they keep doing) or `current`
(what they are doing these weeks). Durability sets how fast it loses rank, never whether it is
true. Rank (`qbot_memory::worth`, never shown) is `n / (n + 1) * 0.5^(k / h)`: `n` distinct
supporting episodes, `k` the episodes its subject took part in since the last support (for the
group, every episode of the group), `h` the durability's `memory.rank.half_life_episodes`
(lasting 200, ongoing 40, current 10). What was entered by hand ranks first. The clock is the
conversation, not the calendar.

**What each run is shown** (`qbot-prompt`):
- In the cached prefix, **People in this group**: the roster (`PgArchive::people`). A person is
  listed when one of their accounts spoke within `memory.roster_days` (30), is linked or blocked,
  or has a name entered by hand; at most `memory.roster_people` (80), the most recently active
  first. For each: what they are and were shown as, the names people use for them with how each
  was used, linked accounts, blocks.
- In the cached prefix, **Learned about this group**: the `memory.group_memories` (40) group
  memories of most rank, entered by hand first.
- In the run's opening note, for the people of the run's conversation: every entry entered by hand
  and the `memory.person_memories` (8) memories of most rank from chat.
- On demand, `about_member(member)` and `about_group()`: everything kept, with what no longer
  holds.

**Room.** Each member keeps at most `memory.capacity.per_member` (30) memories in every state and
the group `memory.capacity.group` (120). A subject the batch would put over its room gives up
replaced and ended memories first, then current ones of least rank; never one the batch itself
touched, so new information always finds room. Eviction is ordinary forgetting: something said
again is learned again. Merging keeps duplicates from using up room.

## 8. What people manage by hand: `/memory`

One command covers everything a person registers or removes:

| | |
| --- | --- |
| `/memory [@member]`, `/memory group` | view: names, what is known, what no longer holds, each with its id |
| `/memory add [@member \| group] text`, `/memory name [@member] name` | enter by hand (origin `hand`, at once) |
| `/memory fix ID text` | correct: a hand memory replaces it, the old one kept as history |
| `/memory end ID` | mark as no longer true |
| `/memory forget ID`, `/memory forget all [@member]` | forget |

Members act on themselves and their linked accounts; a delegated admin also on the group's
ordinary members and on the group itself, but not on what a bot owner entered about someone else;
bot owners on everything. One person has at most 20 entries by hand.

**Forgetting is final.** `/memory forget` deletes the memory and its observations and records a
suppression with its text (kept, never shown). From then on:
- an observation citing a line the forgotten memory rested on, about the same subject, is
  dropped by code, so reading the old lines again learns nothing;
- a name forgotten for a member is not formed again from uses;
- a statement saying the same thing in other words is rejected by the decision (`forgotten`), and
  the same normalized text by code whatever was decided;
- a batch that read before a `/memory forget` about a subject, or that would change a memory a
  person has since corrected or ended, leaves that subject's changes for its next batch, so a
  command is never overwritten by a batch already running. A batch statement equal to something
  just entered by hand supports that entry.

Entering the same thing by hand lifts the suppression: a person's word is the authority.

## 9. Retention

Episodes, observations and memories are small text; memories are bounded per subject (section 7);
episodes and the archive are kept.

A recalled message's line keeps its place in the archive but says only `[recalled]`
([architecture.md](architecture.md) section 4), so a batch after the recall learns nothing from it.
An episode or memory made from it before the recall stays; a member removes what was remembered
from it with `/memory forget`.
