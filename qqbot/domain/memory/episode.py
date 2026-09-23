"""L4 episodic memory: one durable summary backed by exact group events.

Facts answer what somebody is like; an episode answers what happened that time. Episodes
are recalled by group-scoped semantic similarity. Exact source-event links and the
extraction batch are their auditable provenance.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field
from datetime import datetime
from enum import StrEnum

from ..ids import GroupId


class EpisodeType(StrEnum):
    """One value. The recording tool does not ask for a type, so nothing can produce
    another - unreachable enum values would only pretend to a taxonomy. The enum exists
    because the column does, and a future type is a deliberate addition to the tool
    schema, not a value waiting here."""

    DISCUSSION = "discussion"


@dataclass(frozen=True, slots=True)
class Episode:
    """One episode. The summary is what people and the model read; the events are what
    back it up."""

    group_id: GroupId
    summary: str
    episode_type: EpisodeType = EpisodeType.DISCUSSION
    title: str | None = None
    started_at: datetime | None = None
    ended_at: datetime | None = None
    #: Whether this is worth bringing back up later. Extraction currently writes one
    #: placeholder score for every episode, so fixed age - not this field - controls
    #: retention until the extractor produces a meaningful importance judgement.
    importance: float = 0.0
    confidence: float = 0.0
    status: str = "active"
    revision: int = 1
    extraction_id: uuid.UUID | None = None
    event_ids: tuple[uuid.UUID, ...] = ()
    id: uuid.UUID = field(default_factory=uuid.uuid4)
