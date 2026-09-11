"""What the model produced, before anything has checked it.

The line this package draws: the model may propose candidates and may not write
long-term facts.

    LLM -> Candidate -> Validator -> Consolidator -> Repository

This is not ceremony. Left unchecked, a model records jokes as facts, files what one
person said under another's name, and gives one name to two accounts at once. The
candidate layer is what makes those mistakes stop somewhere they can be thrown away,
rather than entering a memory that is read back on every turn.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field, replace
from datetime import datetime
from enum import StrEnum
from typing import Any


class CandidateType(StrEnum):
    ALIAS = "alias"
    FACT = "fact"
    #: A fact about the group rather than about anyone in it.
    GROUP_FACT = "group_fact"
    EPISODE = "episode"


class CandidateStatus(StrEnum):
    PENDING = "pending"
    ACCEPTED = "accepted"
    #: Failed validation. Kept, with the reason: the same rejection recurring means the
    #: prompt needs changing, not that the validator should be loosened.
    REJECTED = "rejected"


@dataclass(frozen=True, slots=True)
class Candidate:
    """One unvalidated output. The payload keeps the shape the model gave it; nothing is
    interpreted ahead of time."""

    candidate_type: CandidateType
    payload: dict[str, Any]
    group_id: int | None = None
    #: The message this record's quote came from. None when the quote matched no single
    #: message, which is itself the answer: the validator refuses it.
    source_event_id: uuid.UUID | None = None
    #: The last message of the batch that produced this. Validation has to run against
    #: the same batch the model read - the quote, the name and the term all have to appear
    #: in it word for word - and validation happens later, in a separate job, by which
    #: time "the recent messages" is a different set. This is what makes the batch
    #: reproducible rather than approximately re-fetched.
    batch_event_id: uuid.UUID | None = None
    #: And how many rows that batch held. Batches are cut at conversation gaps, so
    #: their length varies; without the count, replay could only guess a fixed
    #: window ending at the anchor - a superset that would shift every account code
    #: and misattribute records. Required (the store refuses NULL, and 0 would
    #: replay an empty batch that rejects everything).
    batch_size: int = field(kw_only=True)
    confidence: float | None = None
    status: CandidateStatus = CandidateStatus.PENDING
    reject_reason: str | None = None
    id: uuid.UUID = field(default_factory=uuid.uuid4)
    created_at: datetime | None = None

    def rejected(self, why: str) -> Candidate:
        return replace(self, status=CandidateStatus.REJECTED, reject_reason=why)

    def accepted(self) -> Candidate:
        return replace(self, status=CandidateStatus.ACCEPTED, reject_reason=None)


class RejectReason(StrEnum):
    """Why the validator turned something down. A closed set, because the point is to be
    able to count which mistake the model makes most often."""

    # Also what catches a record about the bot: nothing gives the bot an entity, so it
    # never takes a code in the roster an extraction is given, and a candidate naming it
    # can only carry an invented one.
    UNKNOWN_ENTITY = "unknown_entity"           # names an account that does not exist
    AMBIGUOUS_ALIAS = "ambiguous_alias"         # one name pointing at several people
    MALFORMED = "malformed"                     # the structure does not hold up
    EMPTY = "empty"
