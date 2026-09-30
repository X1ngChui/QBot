"""Consistent bulk inputs for command, reply and extraction identity projections."""

from dataclasses import dataclass
from datetime import datetime
from qqbot.domain.ids import AccountId
import uuid

from qqbot.domain.identity.alias import Alias
from qqbot.domain.identity.entity import IdentityAccount
from qqbot.domain.memory.fact import Fact


@dataclass(frozen=True, slots=True)
class SpeakerActivity:
    user_id: AccountId
    messages: int
    first_seen: datetime


@dataclass(frozen=True, slots=True)
class HolderReading:
    entity_id: uuid.UUID
    speakers: tuple[SpeakerActivity, ...]
    aliases: tuple[Alias, ...]
    facts: tuple[Fact, ...]


@dataclass(frozen=True, slots=True)
class ExtractionIdentities:
    accounts: tuple[IdentityAccount, ...]
    names: tuple[tuple[IdentityAccount, str], ...]
    aliases: tuple[Alias, ...]
