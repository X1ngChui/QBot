"""Role-independent command authorization.

Parsing defines one action and target. Ownership may allow or deny that action; it never
changes what the same command text means.
"""

from __future__ import annotations

from collections.abc import Iterable
from enum import StrEnum

from .command_catalog import Access


def is_owner(user_id: str, owners: Iterable[str]) -> bool:
    listed = {str(owner).strip() for owner in owners if str(owner).strip()}
    return str(user_id) in listed


class Verdict(StrEnum):
    OWNER = "owner"
    MEMBER = "member"
    MEMBER_IF_AGREED = "member_if_agreed"
    DENIED = "denied"


def decide(user_id: str, *, owners: Iterable[str], access: Access) -> Verdict:
    if is_owner(user_id, owners):
        return Verdict.OWNER
    if access is Access.OPEN:
        return Verdict.MEMBER
    if access is Access.AGREED:
        return Verdict.MEMBER_IF_AGREED
    return Verdict.DENIED
