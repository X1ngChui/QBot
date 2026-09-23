"""L3, semantic facts.

A fact is not a boolean. That somebody likes a game is true with a time
attached: they say so in August and quit the following year, and the old row should not
be deleted but closed with a valid_to. Deleting it makes "did they use to play it"
unanswerable forever, and makes the same sentence get learned over and over.

So this layer is temporal semantic memory:

    (subject, predicate, object) + confidence + [valid_from, valid_to) + evidence
"""

from __future__ import annotations

import math
import uuid
from dataclasses import dataclass, field
from datetime import datetime
from enum import StrEnum
from typing import Any

from ..ids import GroupId


def earned_confidence(supports: int, contradicts: int = 0) -> float:
    """How much a fact has earned from its evidence: a Wilson lower bound.

    The one-sided 95% Wilson score lower bound on the supported fraction (Wilson 1927;
    the subjective-logic Beta posterior tells the same story): confidence is a floor
    under the evidence, not an average of it. One unanimous observation scores ~0.27,
    three ~0.53, eight ~0.75 - the curve that makes "said once in passing" and "the
    group keeps saying it" different numbers, which is what the word confidence is meant
    to mean. A contradiction drags the bound down harder than a support lifts it, which
    is the asymmetry wanted here: the cost of trusting a wrong fact is a bot repeating
    it.
    """
    n = supports + contradicts
    if n <= 0:
        return 0.0
    z = 1.6449  # one-sided 95%
    p = supports / n
    z2 = z * z
    centre = p + z2 / (2 * n)
    margin = z * math.sqrt(p * (1 - p) / n + z2 / (4 * n * n))
    return max(0.0, (centre - margin) / (1 + z2 / n))


class MemoryType(StrEnum):
    """What kind of fact this is. It decides how the fact ages, and whether it is allowed
    into the prompt at all."""

    #: Stable properties: occupation, where somebody lives, a long-standing preference.
    #: Slow to change, and worth keeping.
    ATTRIBUTE = "attribute"
    #: Preferences: likes and dislikes. These change, and valid_to is how they turn over.
    PREFERENCE = "preference"
    #: Relations: who somebody is to somebody else.
    RELATION = "relation"
    #: Something true of the group rather than of any person: what it is for, what its
    #: jargon means. The subject of such a fact is the group's own entity.
    #:
    #: Deliberately no category for in-jokes: unfalsifiable, the model will always find
    #: one, and once written down it gets used, quoted back into the archive, and
    #: re-confirmed forever. What is stored is the useful half - the words a reader
    #: needs in order to follow the conversation.
    GROUP = "group"


class FactStatus(StrEnum):
    ACTIVE = "active"
    #: Overturned by a newer fact. The row stays, because somebody changing their mind
    #: is itself information.
    SUPERSEDED = "superseded"
    #: Struck out by hand.
    RETRACTED = "retracted"
    #: Aged out: nothing confirmed it within its class's half-life. Kept apart from
    #: SUPERSEDED because the two mean different things about the world - a fact that
    #: was contradicted is known to be false now, while one that merely went quiet may
    #: still hold and simply stopped coming up.
    EXPIRED = "expired"


class EvidenceRelation(StrEnum):
    SUPPORTS = "supports"
    CONTRADICTS = "contradicts"


@dataclass(frozen=True, slots=True)
class FactEvidence:
    """A raw event that supports or contradicts a fact. Contradictions are recorded too:
    for a fact that keeps being argued with, how its confidence fell is itself readable."""

    raw_event_id: uuid.UUID
    relation: EvidenceRelation = EvidenceRelation.SUPPORTS
    score: float | None = None


@dataclass(frozen=True, slots=True)
class Fact:
    """One fact, with a validity window.

    The object is one of two things: a pointer to another entity (object_entity_id) or a
    literal value (object_value). "A works with B" is the first; "A lives in Hangzhou" is
    the second. Leaving both empty is invalid, and is caught at construction - which is
    the one rule this class enforces rather than describes.

    The rest is a record. Supersession lives in `MemoryRepository.supersede`, in one
    transaction - find the current row for this subject, predicate and group, close it
    out if the object differs - and is deliberately not restated here, where a second
    copy of the rule could drift with every test still green.
    """

    subject_entity_id: uuid.UUID | None
    predicate: str
    subject_account_id: uuid.UUID | None = None
    #: What distinguishes rows of a multi-valued predicate: the thing liked, the word
    #: defined. None for single-valued predicates, where the predicate alone is the key.
    object_key: str | None = None
    memory_type: MemoryType = MemoryType.ATTRIBUTE
    group_id: GroupId | None = None
    object_entity_id: uuid.UUID | None = None
    object_value: Any = None
    confidence: float = 0.0
    status: FactStatus = FactStatus.ACTIVE
    valid_from: datetime | None = None
    valid_to: datetime | None = None
    first_observed_at: datetime | None = None
    last_confirmed_at: datetime | None = None
    revision: int = 1
    id: uuid.UUID = field(default_factory=uuid.uuid4)

    def __post_init__(self) -> None:
        if (self.subject_entity_id is None) == (self.subject_account_id is None):
            raise ValueError("fact must have exactly one account or entity subject")
        if self.object_entity_id is None and self.object_value is None:
            raise ValueError(f"fact {self.predicate!r} has no object")
