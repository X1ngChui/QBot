# Memory: identity, episodes, recall

Status: implemented in `qbot-memory` (domain, pipeline, ports), `qbot-store` (Postgres), and
`qbot-tools` (`recall_episodes`, `read_episode`). Facts, group knowledge and decay are not built yet
(see Follow-ups).

## 1. Model

- **Archive.** Every group line has a dense per-group *ordinal* (1, 2, 3, ...), assigned under the
  group lock in the same transaction as the line. Duplicates consume no ordinal.
- **Batch grid.** History is organized in fixed *batches* of `history.batch_lines` consecutive
  ordinals (30 by default), and the prompt's tiers move a whole batch at a time so the prompt
  prefix changes rarely: `history.raw_batches` newest batches verbatim, `history.summary_batches`
  before them as episode summaries, older chat only through tools (design.md 4.19).
- **Slice and episode.** A *slice* is `memory.slice_batches` whole batches. An *episode* is the
  summary of exactly one slice: it owns that exact ordinal range and nothing else. The ranges of
  one group tile the archive with no gap and no overlap; Postgres enforces non-overlap with a
  range-exclusion constraint, so two workers cannot both store the same slice.
- **Context is not source.** When extracting slice N the model also sees the nearest
  `previous_context_batches` before it and `next_context_batches` after it. They are labelled
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

`EpisodeJobs` (a `JobKind::Extract` runner) loops: find the next complete slice after the group's
newest episode, read previous/target/next lines, call the model, validate, embed
`title + summary`, store episode and vector in one transaction. It is idempotent, resumes at the
failed slice after an error, and refuses an archive with a gap in a slice rather than guessing.

The model call is one forced-shape tool call (`submit_episode`). Code validates the answer
(non-empty title and summary, 1 to 3 evidence quotes copied exactly from target lines). A rejected
answer goes back to the model with every reason, at most `memory.extraction.max_attempts` times.
There is no staging state machine: a crash between the model response and the insert repeats one
slice's model call, which is cheaper than the machinery to avoid it.

## 3. Retrieval

`recall_episodes(question)` embeds the question and does an exact cosine scan over the group's
episode vectors for the configured embedding model (no approximate index: retrieval filters by
group first, so the scan is small and exact). Results are summaries; `read_episode(id)` returns
the raw lines of that episode's range, bounded by the slice size. Vectors are keyed by model, so
changing the embedding model hides old vectors until rebuilt.

## 4. Identity

Terms. A member's *group nickname* is the name they set for one group (OneBot `card`; QQ's
"group nickname", formerly "group card"). Their *account nickname* is the account's global QQ
name (OneBot `nickname`). Their *group display name* is what the group shows: the group nickname,
else the account nickname. These docs say "group nickname" and "group display name" only.

- *Account* (a platform login) belongs to a *holder* (a person). New accounts get their own
  holder in the same transaction as their first line.
- *Linking* merges holders (older wins, then lower id; accounts re-point; the loser keeps a
  `merged_into` pointer). *Splitting* moves one account to a fresh holder. Every change bumps the
  holders' revisions; holder-scoped records stay with the original holder after a split and follow
  the merge chain after a merge.
- *The current group display name* is the authoritative name of a member in a group: their
  group nickname (OneBot `card`, the name they set for this group) or, where they set none, their
  account nickname (OneBot `nickname`), exactly as QQ shows it in the group. It is the platform's own answer, not
  inferred and not added by anyone, so it is read live from NapCat when needed
  (`qbot_agent::Directory`, `get_group_member_info`) and never stored: no cache can go stale. It
  takes precedence over stored names for display and addressing: command replies use it, and
  `lookup_member` lists it first as the name to use.
- *Stored names* are the other names people use for someone: per group, pointing at an account or
  a holder. Evidence is fused in one Rust function: manual 1.0, extracted evidence 0.25 per
  distinct episode capped at 0.7, combined as independent evidence. A name resolves only when
  confirmed (`identity.confirm_at`, default 0.75), so extraction alone never confirms: only a
  person does. Two confirmed targets for a name is *ambiguous*, never a guess.
- *Invitations* to link accounts: ten minutes (`identity.invitation_ttl_secs`), one pending per
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
the extraction, so episodes normally exist before their lines leave the verbatim tier. See
design.md 4.19.

## 6. What was investigated, and why slicing is fixed

Before settling on fixed slices I measured segmentation strategies on synthetic chats with known
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

## 7. Configuration

Policy values live in configuration, not code (see `rust/deploy/README.md` for layering):
`history.batch_lines`, `history.raw_batches`, `history.summary_batches`, `memory.slice_batches`, `memory.previous_context_batches`,
`memory.next_context_batches`, `memory.language`, `memory.extraction.*`, `memory.recall.*`,
`memory.facts.*`, `identity.confirm_at`, `identity.invitation_ttl_secs`. Fusion weights, the 64-character name
bound and the evidence caps are internal tuning and stay constants.

## 8. Manual notes are not memory

Two kinds of information about people and the group are kept apart (design.md decision 6):

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

## 9. Follow-ups

- Refresh of old episodes when the model or prompt version changes (`method` and `model` are
  stored for this); not needed for migration, which rebuilds everything from imported chat
  (design.md decision 10). Facts, group knowledge, display-name evidence and compaction are
  built (design.md 4.19).
- Evaluate on real chat if fixed slicing looks insufficient; then revisit section 6.
- Episode retention is unbounded (text is small); revisit if storage or privacy requires.
