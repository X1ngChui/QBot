"""Who may run a command.

One question, one answer: is this the bot's owner. The commands are an operator's
console - they read and rewrite what the bot believes about people, and every one of them
is something only the person running the bot should be doing.

There is deliberately no second, member-facing level. A member who wants to know what the
bot remembers asks it in conversation - the roster is already in its prompt - and one who
says something about themselves has it picked up by the next extraction pass as a
self-claim, so a member command would be a second interface to the same two things. A
single level also means no listing filtered per reader, no refusing out loud, and no
special case for @-ing yourself.

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
