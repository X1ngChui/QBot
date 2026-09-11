"""Who may run a command.

Two levels, and the boundary is data subjecthood. The owner holds the whole
console - it reads and rewrites what the bot believes about people, and the
dangerous half of that is only the operator's to touch. A plain member holds
the commands the catalog flags `self_serve` (/who, /note, /alias, /forget)
against themselves only - their own record, note and names are theirs to read,
correct and prune, at the same authority as the owner's hand - plus the
read-only surfaces flagged `member` (/card, /stats, /top, /groupstats) whole.
The command path answers for free where asking in conversation costs a model
call. "Themselves" means the person, not the account: the handlers resolve it
through accounts_of_person, so a merged alt operates its main's record, the
same way /block treats them.

The member surface opens only past the user agreement: before /agree, the only
commands that answer are /agree itself and /terms, which shows what is being
agreed to. Everything outside the boundary is
answered with silence, never a refusal: to whoever cannot run a command it
does not exist, and /help's listing is filtered the same way.

The QQ group role is never consulted - running the QQ group is not running the bot - and
nothing the platform says about a speaker can reach this decision, because there is no
parameter to pass it through.

Kept out of plugins/commands.py because that file cannot be imported by a test:
on_command() runs at import time and needs a NoneBot runtime.
"""

from __future__ import annotations

from collections.abc import Iterable
from enum import StrEnum


def is_owner(user_id: str, owners: Iterable[str]) -> bool:
    """`owners` is the group's own list rather than the global one, because a per-group
    config may override it - the gateway already reads it that way."""
    listed = {str(o).strip() for o in owners if str(o).strip()}
    return str(user_id) in listed


class Verdict(StrEnum):
    """What the gate decided about one caller of one command."""

    OWNER = "owner"
    #: A member let through; the handler narrows every operation to their own person.
    MEMBER = "member"
    #: A member let through only if they have accepted the user agreement - the one
    #: fact this decision cannot read for itself, so the gate resolves it.
    MEMBER_IF_AGREED = "member_if_agreed"
    DENIED = "denied"


def decide(user_id: str, *, owners: Iterable[str], global_owners: Iterable[str],
           global_only: bool = False, self_serve: bool = False,
           open_to_members: bool = False, pre_agreement: bool = False) -> Verdict:
    """The whole decision table of the command gate, as a pure function.

    `owners` is the group's list (a per-group config may override it), and
    `global_owners` the default one: `global_only` commands - whose blast radius
    is every group at once - answer only to the latter, so an owner a single
    group's override added holds none of them. A member reaches a `self_serve`
    or `open_to_members` command, but only past the user agreement; the commands
    that consent itself needs (`pre_agreement`) are open before it.
    """
    if is_owner(user_id, global_owners if global_only else owners):
        return Verdict.OWNER
    if global_only or not (self_serve or open_to_members):
        return Verdict.DENIED
    return Verdict.MEMBER if pre_agreement else Verdict.MEMBER_IF_AGREED
