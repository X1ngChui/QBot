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


def is_owner(user_id: str, owners: Iterable[str]) -> bool:
    """`owners` is the group's own list rather than the global one, because a per-group
    config may override it - the gateway already reads it that way."""
    listed = {str(o).strip() for o in owners if str(o).strip()}
    return str(user_id) in listed
