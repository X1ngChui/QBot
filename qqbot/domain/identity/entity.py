"""People and accounts.

`Entity` is the thing in the world; `IdentityAccount` is that thing's account on one
platform. A person may hold several accounts; an account belongs to exactly one person.

The separation costs one indirection. It buys three things: merging two accounts
without losing either history, recording facts against a *person* rather than an
account, and still recognising somebody after they switch accounts.
"""

from __future__ import annotations

import uuid
from dataclasses import dataclass, field
from datetime import datetime
from enum import StrEnum


class EntityType(StrEnum):
    """What an entity stands for: a member, or the group itself (the subject of
    group-level facts, see IdentityRepository.group_entity)."""

    PERSON = "person"
    GROUP = "group"


class EntityStatus(StrEnum):
    ACTIVE = "active"
    #: Merged into somebody else. The row stays, and a read follows merged_into.
    #: Never deleted outright: historical facts, alias evidence and episode participants
    #: all still point at this id.
    MERGED = "merged"


@dataclass(frozen=True, slots=True)
class Entity:
    """A person (or, later, a thing).

    Frozen: an entity's identity does not change in place. A rename produces a new copy
    with a different canonical_name, a merge sets merged_into, and the repository is what
    persists either.

    Purely a record: merge semantics live in `IdentityRepository.merge`, in one
    transaction, and are deliberately not restated here. Two statements of one rule
    where only the SQL runs is not a safety net - it is a second version to keep true,
    and a test suite that passes while they disagree.
    """

    id: uuid.UUID
    entity_type: EntityType = EntityType.PERSON
    canonical_name: str | None = None
    status: EntityStatus = EntityStatus.ACTIVE
    merged_into: uuid.UUID | None = None
    revision: int = 1
    created_at: datetime | None = None
    updated_at: datetime | None = None


@dataclass(frozen=True, slots=True)
class IdentityAccount:
    """A platform account. Strong identity: the platform guarantees the number is unique
    and does not change.

    Neither the nickname nor the group card is here - both change, and both are aliases.
    This table records only whose account this is, and when it was first and last seen.
    """

    entity_id: uuid.UUID
    platform: str
    platform_user_id: str
    id: uuid.UUID = field(default_factory=uuid.uuid4)
    first_seen_at: datetime | None = None
    last_seen_at: datetime | None = None
