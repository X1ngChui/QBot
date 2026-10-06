# Memory: identity, episodes, recall

How QBot remembers: `qbot-memory` (domain, pipeline, ports), `qbot-store` (Postgres) and the
`recall_episodes`, `read_episode` and `lookup_member` tools. The archive of chat lines is the
source of truth; everything here is derived from it.

## 1. Model

- **Archive.** Every group line has a dense per-group *ordinal* (1, 2, 3, ...), assigned under the
  group lock in the same transaction as the line. Duplicates consume no ordinal.
- **Batch grid.** History is organized in fixed *batches* of `history.batch_lines` consecutive
  ordinals (30 by default), and the prompt's tiers move a whole batch at a time so the prompt
  prefix changes rarely: `history.raw_batches` newest batches verbatim, `history.summary_batches`
  before them as episode summaries, older chat only through tools (design.md 7).
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

Every filled batch queues its group's extraction job, and the nightly run extracts whatever was
missed. `EpisodeJobs::extract_group` runs one group at a time: it first embeds any episode the
current embedding index lacks, then loops: find the next complete slice after the group's newest
episode, read the previous, target and next lines, call the model, validate, embed
`title + summary`, store the episode and its vector in one transaction, and apply its findings. It
is idempotent, resumes at a failed slice, and refuses an archive with a gap in a slice rather than
guessing.

The model answers with one tool call, `submit_episode`: a title and summary in the bot's writing
language (from the locale), one to three evidence quotes, and findings: names, facts about members
(by the predicates in `prompts/predicates.toml`) and group knowledge (terms and the topic), each
tied to a target line and a verbatim quote. Code validates everything:

- The episode needs a title, a summary and one to three quotes copied exactly from target lines.
  A rejected episode goes back to the model with every reason.
- A finding needs a member's line (never the bot's, never a notice) whose text contains the quote,
  a subject who wrote the line or is mentioned in the quote, a known predicate, and for a name or a
  term, the name in the quote or the term written in the target. Rejected findings go back once;
  if the correction is not valid, the episode is kept with the findings that held.
- An answer cut off at the provider's output limit, or whose arguments are not JSON, is lost
  rather than wrong: the same request is sent again. At most three model calls per slice.

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
  model and width. When either changes, recall finds an episode again once the nightly run has
  embedded it anew from its stored title and summary; nothing is extracted again.

## 4. Identity

Terms. A member's *group nickname* is the name they set for one group (OneBot `card`). Their *account nickname* is the account's global QQ
name (OneBot `nickname`). Their *group display name* is what the group shows: the group nickname,
else the account nickname. These docs say "group nickname" and "group display name" only.

- *Account* (a platform login, the QQ number) belongs to a *holder* (a person). New accounts get
  their own holder in the same transaction as their first line.
- *Member number*: an account's number in one group (`member:N` to the model), assigned densely
  the first time the account appears there (speaks or is mentioned) and never changed, reused or
  removed: a trigger on `member_number` rejects every update, delete and truncate. Linking,
  splitting, blocks, names and other groups do not touch it. Account ids are global and never
  reach the model; member numbers are group-local, and two accounts of one person keep two
  numbers. The model learns which numbers are one person, and who is blocked, from the "People in
  this group" instruction, read from the block list and the holders on every run.
- *Linking* merges holders (older wins, then lower id; accounts re-point; the loser keeps a
  `merged_into` pointer). *Splitting* moves one account to a fresh holder. Every change bumps the
  holders' revisions; holder-scoped records stay with the original holder after a split and follow
  the merge chain after a merge.
- *The current group display name* is the authoritative name of a member in a group: their
  group nickname (OneBot `card`, the name they set for this group) or, where they set none, their
  account nickname (OneBot `nickname`), exactly as QQ shows it in the group. It is the platform's own answer, not
  inferred and not added by anyone, so it is read live from NapCat when needed
  (`qbot_agent::Directory`, `get_group_member_info`) and never stored: no cache can go stale. It
  takes precedence over stored names for display and addressing: command replies use it,
  `lookup_member` lists it first as the name to use, and every run's trigger note lists it for
  each member who speaks in the visible chat (asked of the platform when the run opens).
- *Member number versus name*: `member:N` is the model's internal handle (tool arguments,
  `[at:N]`, the people block, telling accounts apart); in what the bot writes, people are called
  by their current group display name, addressed with `[at:N]`, or described naturally when no
  name is available. A number appears in a message only when someone asks about the numbers.
- *Stored names* are the other names people use for someone: per group, pointing at an account or
  a holder. Evidence is fused in one Rust function: manual 1.0, extracted evidence 0.25 per
  distinct episode capped at 0.7, combined as independent evidence. A name resolves only when
  confirmed (at 0.75), so extraction alone never confirms: only a person does. Two confirmed targets for a name is *ambiguous*, never a guess.
- *Invitations* to link accounts: ten minutes, one pending per
  account per group, only the target confirms, the confirmation must come after the invitation, an
  expired or stale (a holder changed) invitation is closed, replaying the confirming message is a
  no-op.

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

## 7. Facts and group knowledge

- **Consolidation** applies each episode's findings in order, right after it is stored and on
  startup for any left over. Names become extracted identity evidence; facts and knowledge become
  observations in `fact` / `fact_evidence`. A single-valued predicate supersedes its previous
  value; a multi-valued one keys on the normalized object; an opposite predicate (likes /
  dislikes) supersedes the other. An episode supports a fact at most once.
- **Confidence** is the Wilson lower bound of the supporting episodes times `0.5^(age /
  half-life)`, with the half-life chosen by the predicate's decay class
  (`memory.facts.half_life_days`); a fact whose confidence falls below 0.05 is expired by the
  nightly run. It is computed on read, so it is deterministic for a given time.
- **Where facts reach the model.** Member facts only through `lookup_member`: by predicate, the
  most recently confirmed first, with confidence and age, across the person's linked accounts.
  Group knowledge as an instruction: the topic and the 40 most recently confirmed terms (design.md
  7). Members see their own facts with `/who` and remove them with `/forget`.

## 8. Manual notes are not memory

Two kinds of information about people and the group are kept apart (design.md 18):

| | Manual notes | Extracted memory |
| --- | --- | --- |
| Written by | a person, with `/note` | extraction, from chat lines |
| Store | `note` | `fact` / `fact_evidence` (and aliases from evidence) |
| Removed by | `/note remove` / `/note clear` | `/forget N` (facts), `/forget group N` (group knowledge) |
| Confidence | none: it is what someone wrote | Wilson bound of evidence, decaying |
| Model sees it as | a note written by a member | learned from chat, with confidence and age |

Neither path writes into the other: extraction reads chat lines, never notes, and consolidation
never creates notes; `/forget` never touches notes and `/note` never touches facts. Help texts
name the store each command acts on.

## 9. Retention

Episodes, facts and evidence are small text and are kept; expired and superseded facts stay as
history. Revisit if storage or privacy requires.
