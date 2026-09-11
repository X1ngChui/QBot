"""Names.

A name is not a string field. It has three dimensions:

    who it points at   target_entity_id
    where it holds     group_id (null = everywhere, and only a human may establish that)
    what backs it      AliasEvidence + confidence + status

Who uses the name is not a column: every piece of evidence points at the message that
produced it, and the message has a sender, so usage is counted from the trail. Evidence
and conclusion never blur - the machine can push a name no further than CONFIRMED, every
step up to that keeps its evidence, and a name attached to the wrong person can always
be traced back to how it got there.
"""

from __future__ import annotations

import unicodedata
import uuid
from dataclasses import dataclass, field, replace
from datetime import datetime
from enum import StrEnum


class AliasType(StrEnum):
    """Where a name came from.

    The type is itself part of how strong the evidence is: a group card is a fact the
    platform reports, while a joke name rarely outlives the week.
    """

    QQ_NICKNAME = "qq_nickname"
    GROUP_CARD = "group_card"
    NICKNAME = "nickname"
    SHORT_NAME = "short_name"
    JOKE_NAME = "joke_name"
    RELATIONSHIP_NAME = "relationship_name"
    TITLE = "title"


class AliasStatus(StrEnum):
    """candidate -> confirmed -> inactive.

    There is no `rejected`. Messages already archived may still depend on a name that
    later fell out of use, and deleting it would make them unreadable again. A name that
    no longer holds becomes inactive.
    """

    CANDIDATE = "candidate"
    CONFIRMED = "confirmed"
    INACTIVE = "inactive"


class EvidenceType(StrEnum):
    """The grounds for believing this name points at this person."""

    PLATFORM_IDENTITY = "platform_identity"
    EXPLICIT_AT = "explicit_at"
    GROUP_CARD = "group_card"
    SELF_CLAIM = "self_claim"
    REPLY_CONTEXT = "reply_context"
    SPEAKER_USAGE = "speaker_usage"
    MULTI_USER_USAGE = "multi_user_usage"
    MANUAL = "manual"
    LLM_INFERENCE = "llm_inference"


#: The base score for each kind of evidence. A fact the platform states outweighs the
#: model's inference, and several people using a name outweighs one person using it - a
#: nickname one person shouts may be that person amusing themselves; three people using it
#: is what a name is.
#:
#: PLATFORM_IDENTITY is 0.95 rather than 1.00: fusion multiplies survival probabilities,
#: and a channel at exactly 1.0 saturates the result forever, unreachable by any later
#: contradiction. Only MANUAL may do that, because only an owner's verdict is final.
EVIDENCE_WEIGHT: dict[EvidenceType, float] = {
    EvidenceType.PLATFORM_IDENTITY: 0.95,
    EvidenceType.MANUAL: 1.00,
    EvidenceType.EXPLICIT_AT: 0.90,
    EvidenceType.GROUP_CARD: 0.80,
    EvidenceType.SELF_CLAIM: 0.45,
    EvidenceType.MULTI_USER_USAGE: 0.55,
    EvidenceType.REPLY_CONTEXT: 0.40,
    EvidenceType.SPEAKER_USAGE: 0.30,
    EvidenceType.LLM_INFERENCE: 0.25,
}

#: Which evidence kinds share a generating premise. Evidence within one channel is
#: correlated - re-reads of the same profile field, the same people using the name
#: again - so it merges by maximum; distinct channels are approximately independent
#: ways for a name to be real, so their support combines: a platform card plus an @ by
#: another person is more than either alone. (Pooling rules: Tammet et al.,
#: arXiv 2608.11275 - max for nested events, noisy-OR for independent ones.)
#:
#: SELF_CLAIM sits at 0.45 partly because of this fusion: a first-day joke card (0.5)
#: plus a joking self-claim must still fall short of the 0.75 line
#: (1 - 0.5*0.55 = 0.725).
CHANNELS: dict[EvidenceType, str] = {
    EvidenceType.GROUP_CARD: "platform",
    EvidenceType.PLATFORM_IDENTITY: "platform",
    EvidenceType.EXPLICIT_AT: "address",
    EvidenceType.REPLY_CONTEXT: "address",
    EvidenceType.SELF_CLAIM: "self",
    EvidenceType.MULTI_USER_USAGE: "usage",
    EvidenceType.SPEAKER_USAGE: "usage",
    EvidenceType.LLM_INFERENCE: "usage",
    EvidenceType.MANUAL: "manual",
}


def fused_confidence(evidence: list[AliasEvidence]) -> float:
    """Two-level fusion: maximum within a channel, noisy-OR across channels.

    The shape follows the correlation structure rather than a single rule: repeats of the
    same kind of sighting are one event however often they recur (a hundred re-reads of
    one profile field prove nothing new), while genuinely different kinds of evidence
    reinforce. No global coefficient can substitute for this grouping - partial sharing
    is exactly the case a single tuned constant provably cannot express.
    """
    best: dict[str, float] = {}
    for e in evidence:
        ch = CHANNELS.get(e.evidence_type, e.evidence_type.value)
        best[ch] = max(best.get(ch, 0.0), e.weight)
    miss = 1.0
    for w in best.values():
        miss *= 1.0 - min(w, 1.0)
    return 1.0 - miss


#: What it takes to reach confirmed. One inference by the model (0.25) does not get
#: there; a platform name that endures into a second day, or three different people
#: using the same nickname, does - which is exactly the line worth drawing.
#: (EXPLICIT_AT / SELF_CLAIM / REPLY_CONTEXT are weighted above but no ingest path
#: writes them yet: an @ arrives as an account id, not as evidence for a name.)
CONFIRM_THRESHOLD = 0.75

#: How many different people have to be seen using a name before it counts as a name the
#: group uses, rather than one person's habit.
USAGE_CONFIRMS_AT = 3


def usage_weight(distinct_users: int) -> float:
    """What repeated use is worth, by how many *different* people did it.

    This is the one path by which a name the model merely observed can become certain,
    and the quantity it turns on is deliberate. One person using a name a hundred times
    is one person's habit - it stays at SPEAKER_USAGE and never confirms, however often
    it recurs, which is the same reason `scored` takes a maximum rather than a sum. Three
    different people using it is what a name *is*: the group has converged on it, and no
    amount of repetition by any one of them would have shown that.

    Three rather than two because two is a pair of friends, and a name only one of them
    would recognise is not one the bot should use in front of the rest.

    The count is over the whole evidence trail, not over one batch, so a name that takes
    a fortnight to catch on gets there in the end - and one that catches on with nobody
    ages out instead.
    """
    if distinct_users >= USAGE_CONFIRMS_AT:
        return EVIDENCE_WEIGHT[EvidenceType.GROUP_CARD]
    if distinct_users == 2:
        return EVIDENCE_WEIGHT[EvidenceType.MULTI_USER_USAGE]
    return EVIDENCE_WEIGHT[EvidenceType.SPEAKER_USAGE]


def platform_weight(kind: EvidenceType, distinct_days: int) -> float:
    """What a platform-reported name is worth, by how long it has actually been worn.

    A group card is strong evidence about a durable *name* only if it endures - on first
    sight it may be three minutes into a renaming joke. A card seen on one day is worth
    0.5: enough to sit as a candidate, below the 0.75 that would let the bot use it.
    Seen again on a second day, it earns the full weight of its kind - real renames
    persist, jokes are gone by morning.
    """
    if kind not in (EvidenceType.GROUP_CARD, EvidenceType.PLATFORM_IDENTITY):
        return EVIDENCE_WEIGHT[kind]
    return EVIDENCE_WEIGHT[kind] if distinct_days >= 2 else 0.5


def normalize(text: str) -> str:
    """The form used for matching.

    Width, case, whitespace and zero-width characters all collapse: hiding invisible
    characters in a group card is a common enough joke, and a name spelled in full-width
    characters, in capitals, or with spaces between the letters means the same person.
    What gets displayed is always the original.
    """
    s = unicodedata.normalize("NFKC", text)
    s = "".join(c for c in s if not unicodedata.category(c).startswith("C"))
    return "".join(s.split()).casefold()


@dataclass(frozen=True, slots=True)
class AliasEvidence:
    """One piece of evidence. It points at the event that produced it, so any conclusion
    can be traced back to the line that caused it."""

    evidence_type: EvidenceType
    raw_event_id: uuid.UUID | None = None
    score: float | None = None

    @property
    def weight(self) -> float:
        return self.score if self.score is not None else EVIDENCE_WEIGHT[self.evidence_type]


@dataclass(frozen=True, slots=True)
class Alias:
    """One name pointing at one person, within one scope."""

    alias_text: str
    target_entity_id: uuid.UUID
    group_id: int | None = None
    alias_type: AliasType = AliasType.NICKNAME
    confidence: float = 0.0
    status: AliasStatus = AliasStatus.CANDIDATE
    valid_from: datetime | None = None
    valid_to: datetime | None = None
    last_used_at: datetime | None = None
    id: uuid.UUID = field(default_factory=uuid.uuid4)

    @property
    def normalized_text(self) -> str:
        return normalize(self.alias_text)

    @property
    def is_global(self) -> bool:
        """Holds across groups. The only channel that crosses them, and one no automatic
        path may open: group isolation is the default, and only a person can waive it."""
        return self.group_id is None

    @property
    def is_usable(self) -> bool:
        """Whether it may be used to identify anyone. A candidate may not - being a
        candidate is precisely what "not yet certain" means."""
        return self.status is AliasStatus.CONFIRMED and self.valid_to is None

    def scored(self, evidence: list[AliasEvidence]) -> Alias:
        """Recompute confidence from the evidence, promoting to confirmed if it earns it.

        Fusion is channelled - see fused_confidence. Within a channel repetition buys
        nothing (ten sightings of one profile field are one fact about the profile), so
        the certainty one @ carries still cannot be bought with a hundred guesses by
        bystanders; across channels independent kinds of support genuinely add.

        A MANUAL row is not one voice among the others - it is the owner's decision, and
        it alone sets the number. In both directions: a hand-set 0.4 is not outvoted by
        the platform re-reporting the name tomorrow, because a correction that the next
        automatic sighting could overrule would not be a correction. The repository keeps
        at most one MANUAL row per alias, replacing it on each manual action, so "the"
        manual verdict is well-defined.
        """
        if not evidence:
            return self
        manual = [e for e in evidence if e.evidence_type is EvidenceType.MANUAL]
        if manual:
            conf = manual[-1].weight
            status = (AliasStatus.CONFIRMED if conf >= CONFIRM_THRESHOLD
                      else AliasStatus.CANDIDATE)
            return replace(self, confidence=conf, status=status)
        conf = fused_confidence(evidence)
        status = (AliasStatus.CONFIRMED if conf >= CONFIRM_THRESHOLD
                  else AliasStatus.CANDIDATE)
        # Something already confirmed is not demoted by one weak sighting arriving later.
        if self.status is AliasStatus.CONFIRMED:
            status = AliasStatus.CONFIRMED
            conf = max(conf, self.confidence)
        return replace(self, confidence=conf, status=status)
