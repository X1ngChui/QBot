# Memory: identity, episodes, recall

How QBot remembers: `qbot-memory` (domain, pipeline, ports), `qbot-store` (Postgres) and the
`recall_episodes` and `read_episode` tools, plus the people block every run starts with. The
archive of chat lines is the source of truth; everything here is derived from it.

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

Every filled batch queues its group's extraction job, and the nightly run extracts whatever was
missed. `EpisodeJobs::extract_group` runs one group at a time: it first embeds any episode the
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
the slice and to tell what lasts from what passes is put in front of it (method `slice-v5`):
- Lines are `[msg:ID] MM-DD HH:MM member:N: text` in the bot's time zone: who said what,
  when, and which line a `[reply:ID]` quotes. The target part's heading gives its first and last
  moments in full, with the year and weekday (`2026-10-15 Thu 20:30`), so "tomorrow" or "on
  Friday" can be turned into dates; the summary writes plans and deadlines as dates.
- The target lines are numbered; one batch on either side is unnumbered context.
- "Earlier in this chat" gives the title and summary of the group's episode just before the
  slice, so a slice that begins mid-conversation is read in its context and not summarized as if
  it began there.
- "Quoted from earlier" shows older lines that the shown lines reply to (`[reply:ID]` beyond the
  context batches: the newest line with that id before them), so a reply such as "agreed" can be
  understood. Context only, like the neighbouring batches.
- Ahead of the lines, "Who is who" (`SliceBackground`, implemented by the prompt layer from the
  same records replies use) lists every member the lines involve, whether as speaker or as
  `[at:N]`. For each it gives the current group display name, other names, accounts linked as
  one person in the group, a block, and facts learned before, all from the group's own records. These resolve names and references to member
  numbers, and the earlier facts (every one, the most confident first, with its confidence) are
  what a later statement can confirm or end; a value said again is given exactly as listed, so it
  supports the record instead of standing beside it as a rewording.
- "About the group" says what the bot is called (members address it by that name, which must
  never become a member's), the operator's group background and the learned topic and terms.
- Notes are left out: they are what members wrote about each other, not needed to read a
  conversation.

The prompt says that names and earlier facts help interpretation but are no evidence: a finding
still needs a quote from the target. It says what counts as lasting: a fact must still be true in
a few weeks (where someone lives, works or studies, what they like or keep doing); a mood, what
someone is doing right now, a one-off event or a plan for tonight belongs in the summary at most.
The model's reasoning effort is `providers.text.extraction_reasoning`.

The model answers with one tool call, `submit_episode`: a title and summary in the bot's writing
language (from the locale), one to three evidence quotes, and findings: names, facts about members
(by the predicates in `prompts/predicates.toml`), facts that ended (`ended`: the same predicates,
for what the target part says no longer holds: someone quit a game, moved away, stopped liking
something) and group knowledge (terms and the topic), each tied to a target line and a verbatim
quote. Code validates everything:

- The episode needs a title, a summary and one to three quotes copied exactly from target lines.
  A rejected episode goes back to the model with every reason.
- A finding needs a member's line (never the bot's, never a notice) whose text contains the quote,
  a subject who wrote the line or is mentioned in the quote, a known predicate, and for a name or a
  term, the name in the quote or the term written in the target. An ended fact is held to the
  same rules as a fact. Rejected findings go back once; if the correction is not valid, the
  episode is kept with the findings that held.
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

- *Account* (a platform login, the QQ number) is recorded in the same transaction as its first
  line. It is the one global identity record and is never shown; everything about an account
  is kept per group ([architecture.md](architecture.md) section 8, "Groups are separate").
- *Member number*: an account's number in one group (`member:N` to the model), assigned densely
  the first time the account appears there (speaks or is mentioned) and never changed, reused or
  removed: a trigger on `member_number` rejects every update, delete and truncate. Linking,
  splitting, blocks, names and other groups do not touch it. Account ids are global and never
  reach the model; member numbers are group-local, and two accounts of one person keep two
  numbers. The model learns which numbers are one person, and who is blocked, from the "People in
  this group" instruction, read from the group's block list and links on every run.
- *Linking* records a link between two accounts in the group where it was made
  (`account_link`). A *person* in a group is the accounts linked to each other there, directly or
  through others; a link made in another group plays no part, so it never joins numbers, names,
  notes, facts, blocks or command authority here. *Unlinking* removes an account's links in the
  group; the accounts it connected stay linked only where they have links of their own.
- *The current group display name* is the authoritative name of a member in a group: their
  group nickname (OneBot `card`, the name they set for this group) or, where they set none, their
  account nickname (OneBot `nickname`), exactly as QQ shows it in the group. It is the platform's own answer, not
  inferred and not added by anyone, so it is read live from NapCat when needed
  (`qbot_agent::Directory`, `get_group_member_info`) and never stored: no cache can go stale. It
  takes precedence over stored names for display and addressing: command replies use it, and
  every run's trigger note lists it for each member who speaks in the visible chat or has an
  entry in the people block (asked of the platform when the run opens).
- *Member number versus name*: `member:N` is the model's internal handle (tool arguments,
  `[at:N]`, the people block, telling accounts apart); in what the bot writes, people are called
  by their current group display name, addressed with `[at:N]`, or described naturally when no
  name is available. A number appears in a message only when someone asks about the numbers.
- *Stored names* are the other names people use for someone: per group, pointing at an account.
  A name given with `--linked` is given to each of the person's accounts in the group, and a name
  confirmed for one of a person's accounts does not stop their other accounts from having it.
  A name is a belief (section 7): set by a person (`/name`) it is *confirmed*, confidence 1,
  and stands until removed; learned from chat it is a *lead*, with the credibility of its
  distinct supporting episodes ageing with the stable half-life. Confirmed is provenance, not a
  threshold: only a person vouches, however often chat repeats a name. A lead stays a lead,
  shown with its confidence, and is never expired. A name resolves only to confirmed targets; two
  confirmed targets for a name is *ambiguous*, never a guess.
- *Invitations* to link accounts: ten minutes, one pending per
  account per group, only the target confirms (once: the archive runs each message once, and a
  confirmed invitation is no longer pending; message ids are hashes and are never compared), an
  expired or stale (either side's links in the group changed) invitation is closed, replaying the
  confirming message is a no-op.

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

## 7. Beliefs: confidence and forgetting

Memory holds two kinds of thing. *Records* say what happened: archive lines, episodes, notes
members wrote. They are true of the past by construction and carry no confidence; recall ranks
episodes by relevance and age (section 3). *Beliefs* are claims about how things are now, inferred
from records: a fact about a person (`lives_in`, `likes`), the meaning of a group term, the
group's topic, a name a person goes by. Every belief has one **confidence**, with one meaning
everywhere: *the estimated probability that the claim holds now*. It is computed on read from the
belief's evidence, in one place (`qbot_memory::belief`), so a fact, a term and a name with the same
evidence have the same confidence.

**The model.** A belief can be wrong for three independent reasons, and confidence is the product
of the three chances that it is not:

1. *The statements were wrong* (a joke, sarcasm, a misread line, the wrong subject).
   `credibility(n) = (n + 1) / (n + 2)` for `n` distinct episodes that supported it: the posterior
   mean of a uniform Beta prior after `n` agreeing observations (Laplace's rule of succession).
   One episode gives 0.67, two 0.75, four 0.83, eight 0.9; it never reaches 1, since repeating a
   claim is not proof of it. A name set by a person (`/name`) is an assignment, not an inference:
   credibility 1.
2. *It stopped holding since it was last said* (people move, quit games, change tastes).
   `persistence = 0.5^(age / half-life)`, age counted from the last support, the half-life being
   how long a claim of its decay class typically keeps holding unsaid
   (`memory.facts.half_life_days`: stable 730 days for where someone lives or works, a pet, a
   name; default 180 for tastes; fast 45 for what someone currently plays, watches or wants).
   A vouched name does not age: it stands until removed.
3. *Something said later contradicts it.* The latest statement screens off earlier ones: a
   statement made after the belief's last support that is incompatible with it multiplies its
   confidence by `1 - credibility` of that statement. Incompatible are another value of a
   single-valued predicate ("lives in Shanghai" against "lives in Beijing"), the opposite
   predicate for the same thing ("dislikes cats" against "likes cats"), another meaning of the
   same term, and a statement that it no longer holds (an `ended` finding, credibility of one
   statement). Support given after the contradiction restores it: the contradiction is then
   older than the belief's last support and no longer counts.

So "moved to Shanghai" after three mentions of Beijing leaves Shanghai at 0.67 and Beijing at
about a quarter (0.8 credibility, a little persistence lost, times 1/3); both stay, and the model sees
both with their confidence. A rewording ("Beijing" then "Beijing city") costs the older wording
two thirds of its confidence instead of deleting it, and either wording is restored by being
said again. Confidence is a number the model reads, not a gate: nothing is hidden or deleted
for having low confidence.

**Forgetting is ranking under a budget, not deletion.** What reaches the model is chosen by
confidence where space is limited:
- the people block shows each person's `memory.person_facts` (16) most confident facts, each with
  its confidence (rounded to tens of percent) and last support date, competing values side by
  side, and their names with leads marked by confidence;
- group knowledge shows the most confident topic and, per term, its most confident meaning, the
  `memory.group_terms` (40) most confident terms;
- the extractor sees every fact of the members a slice involves, most confident first, so a
  statement can confirm, reword or end any of them.

Confidence is computed as of the start of the current UTC day, so the people block and the group
knowledge change at most once a day by ageing and stay in the cached prompt prefix in between.
A belief that falls out of a budget is still kept, still shown by `/who` and to the extractor,
and comes back when it is said again. Nothing expires: a belief nobody repeats tends to zero
confidence and drops in rank; it is not discarded. Hard deletion is only by a person: `/forget`
(facts), `/name remove` (names), a recalled message (which teaches nothing afterwards). Storage
grows with distinct claims, which are short text; there is no automatic purge.

**Storage.** `fact` holds one row per claim: subject, predicate, *slot* and value. Claims of one
subject compete when they share a slot and differ in predicate or value: a single-valued
predicate's slot is the predicate (`lives_in`); a multi-valued predicate with an opposite shares
one slot with it per normalized value (`dislikes/likes:cats`); a multi-valued predicate without
one has a slot per value, so its values stand side by side; a term's slot is the term, the topic
has one. Each row keeps its distinct supporting episodes (`fact_evidence`, with the quote), its
first and last support and the latest time it was said to have ended; confidence is derived.
Statuses are `active` and `forgotten` (struck out by a person, never shown, never a competitor).

**Consolidation** applies each episode's findings in chat order, right after it is stored and, for
any left over by a crash, at the start of the group's next extraction. Names become extracted
identity evidence; facts and knowledge become observations: support for the slot's row with the
same predicate and normalized value, or a new row beside its competitors. One episode says one
thing per slot: its last statement. An episode supports a claim at most once, so applying it
again changes nothing. After its observations an episode's ended facts are applied: each records
the time on the claim with that value, and one that is not recorded changes nothing.

**Alternatives considered.**

| Approach | What it gets right | Why not here |
| --- | --- | --- |
| Wilson lower bound times decay, expire below 0.05 (the earlier design) | conservative with little evidence | not a probability (one mention 0.27, so most memory looked unreliable); a once-said fact was deleted after about ten weeks; no negative evidence, so a new value had to delete the old |
| Beta reputation / subjective logic opinion (Jøsang and Ismail 2002), positive and negative counts with a forgetting factor | evidence-based, one scale, explicit uncertainty | treats statements as exchangeable trials of a fixed proportion: three old mentions outvote one fresh "I moved", though the fresh one is about the present. Its rule of succession is kept for credibility |
| Ebbinghaus strength growing with recall (MemoryBank, Zhong et al. 2023) and ACT-R base-level activation (power-law decay of uses) | memories used often persist | model how a *mind* forgets, not whether a claim about the world still holds; being shown in a prompt is not evidence that it is true, and would make every shown fact immortal |
| Recency + importance + relevance, min-max normalized (Generative Agents, Park et al. 2023) | good for ranking records | a ranking score, not a probability; importance asked of a model is unstable across calls. Recall of episodes keeps a similarity-times-recency score in this spirit |
| Model-chosen ADD / UPDATE / DELETE (Mem0, 2025) | resolves rewording semantically | a contradiction deletes on one model call; Mem0's graph variant and Zep / Graphiti instead mark facts invalid and keep them for temporal reasoning, which is what competing claims with confidence do here without another model call |

Members see their own facts and confidence with `/who` and remove them with `/forget`; group
knowledge is removed with `/forget group`.

## 8. Manual notes are not memory

Two kinds of information about people and the group are kept apart (architecture.md section 18):

| | Manual notes | Extracted memory |
| --- | --- | --- |
| Written by | a person, with `/note` | extraction, from chat lines |
| Store | `note` | `fact` / `fact_evidence` (and aliases from evidence) |
| Removed by | `/note remove` / `/note clear` | `/forget N` (facts), `/forget group N` (group knowledge) |
| Confidence | none: it is a record of what someone wrote | the probability the claim holds now (section 7) |
| Model sees it as | a note written by a member | learned from chat, with confidence and age |

Neither path writes into the other: extraction reads chat lines, never notes, and consolidation
never creates notes; `/forget` never touches notes and `/note` never touches facts. Help texts
name the store each command acts on.

## 9. Retention

Episodes, facts and evidence are small text and are kept; a belief nobody repeats loses
confidence and rank but is not deleted (section 7). Revisit if storage or privacy requires.

A recalled message's line keeps its place in the archive but says only `[recalled]`
([architecture.md](architecture.md) section 4), so a slice extracted after the recall learns
nothing from it. An episode or fact made from it before the recall stays: episodes are never
rewritten, and a message recalled within minutes is rarely in an extracted slice yet.
