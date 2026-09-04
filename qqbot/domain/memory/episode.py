"""L4, episodic memory: one thing that happened in a group, with a beginning and an end.

The division of labour against facts (design doc 36-40): a fact answers what somebody is
like, an episode answers what actually happened that time. The first is rewritten
repeatedly; the second is fixed once it is written.

An episode carries its participants and the raw events behind it, not just a summary and
a vector: a recommendation somebody made has to resolve to the specific messages, not to
a paragraph that merely reads as though it were about them.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field
from datetime import datetime
from enum import StrEnum


class EpisodeType(StrEnum):
    """One value. The recording tool does not ask for a type, so nothing can produce
    another - unreachable enum values would only pretend to a taxonomy. The enum exists
    because the column does, and a future type is a deliberate addition to the tool
    schema, not a value waiting here."""

    DISCUSSION = "discussion"


@dataclass(frozen=True, slots=True)
class Participant:
    entity_id: uuid.UUID
    role: str | None = None


@dataclass(frozen=True, slots=True)
class Episode:
    """One episode. The summary is what people and the model read; the events are what
    back it up."""

    group_id: int
    summary: str
    episode_type: EpisodeType = EpisodeType.DISCUSSION
    title: str | None = None
    started_at: datetime | None = None
    ended_at: datetime | None = None
    #: Whether this is worth bringing back up later. Recorded, not yet acted on:
    #: episodes have no decay path today (design doc 58 promises aging by
    #: importance; deliberately unbuilt until episode volume makes it worth a pass),
    #: so nothing currently reads this.
    importance: float = 0.0
    confidence: float = 0.0
    status: str = "active"
    revision: int = 1
    #: Written with the episode and never read back onto it: retrieval filters by
    #: participant in SQL, one join before anything is built here. Nothing may derive a
    #: "who was involved" answer from this field - on an episode read back from the
    #: database it is empty, so such a helper would answer "nobody" in exactly the
    #: direction that looks like a plausible result.
    participants: tuple[Participant, ...] = ()
    event_ids: tuple[uuid.UUID, ...] = ()
    id: uuid.UUID = field(default_factory=uuid.uuid4)
