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

from ..ids import GroupId


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
    group_id: GroupId | None = None
    #: The message this record's quote came from. None when the quote matched no single
    #: message, which is itself the answer: the validator refuses it.
    source_event_id: uuid.UUID | None = None
    #: The durable exact-event batch that produced this output. It is optional only
    #: while pure validator tests construct candidates outside persistence.
    extraction_id: uuid.UUID | None = None
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
