"""Role-independent command authorization.

Parsing defines one action and target. Ownership may allow or deny that action; it never
changes what the same command text means.
"""

from __future__ import annotations

from collections.abc import Iterable
from enum import StrEnum

from qqbot.domain.ids import AccountId
from qqbot.commands.catalog import Access


def is_owner(user_id: AccountId, owners: Iterable[AccountId]) -> bool:
    return user_id in owners


class Verdict(StrEnum):
    OWNER = "owner"
    MEMBER = "member"
    DENIED = "denied"


def decide(user_id: AccountId, *, owners: Iterable[AccountId], access: Access) -> Verdict:
    if is_owner(user_id, owners):
        return Verdict.OWNER
    if access is Access.MEMBER:
        return Verdict.MEMBER
    return Verdict.DENIED
