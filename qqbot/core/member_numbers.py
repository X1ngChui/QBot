"""Member numbers: how one prompt tells people apart and how the model names them.

A display name is all the model sees of a member, and display names collide - two
members can carry the same group card, and a card can be changed to match anyone's.
Every person the prompt shows therefore wears a small number in the system brackets
behind their name, and the send tool takes those numbers to say whom to @.

The number belongs to a person, not an account: accounts an owner has merged share
one, so the same number always means the same person. The numbers follow the roster
in the system block, which lists everyone who has appeared in the group in order of
first appearance (core.retrieval.gather): numbered from the roster alone, they stay
put while the conversation moves, which keeps the prefix cache intact, and a
newcomer joins at the end. Anybody a prompt shows who is not in the roster yet - a
first message still being archived, an account only a search turned up - is
numbered after it, in the order they appear.

Numbers are never stored. Anything frozen—the archive and structured evidence—holds
stable account data or names only, because a number means something only inside the one
render that assigned it.
"""

from __future__ import annotations

import logging

from ..db import repo
from ..util import why

log = logging.getLogger("qqbot.numbers")

BOT_DISPLAY_NUMBER = 0


class MemberNumbers:
    """The numbering of one prompt: person -> number, and back to accounts."""

    def __init__(self, self_id: str = "") -> None:
        self._self = str(self_id or "")
        #: account -> the person it belongs to; an account missing here is a
        #: person of its own.
        self._person: dict[str, object] = {}
        self._number: dict[object, int] = {}
        #: number -> that person's accounts, in the order they were numbered.
        self._accounts: dict[int, list[str]] = {}
        #: number -> the account that most recently spoke in the conversation.
        self._latest: dict[int, str] = {}

    def teach(self, account: str, person: object) -> None:
        """Record which person an account belongs to. Only effective before the
        account is numbered: a number, once shown, keeps meaning what it meant."""
        if account and account not in self._person:
            self._person[account] = person

    async def learn(self, accounts: list[str]) -> None:
        """Look up the person behind every account not yet known, in one query.

        A lookup failure degrades to one number per account: merged accounts then
        read as two people, which is what the prompt assumes of unmerged ones
        anyway, and the reply still goes out.
        """
        want = [
            a for a in dict.fromkeys(accounts) if a and a != self._self and a not in self._person
        ]
        if not want:
            return
        try:
            found = await repo.holder_ids_for_accounts(want)
        except Exception as e:
            log.warning(
                "person lookup for member numbers failed, numbering accounts separately: %s", why(e)
            )
            return
        for account, person in found.items():
            self.teach(account, person)

    def number(self, account: str, *, spoke: bool = False) -> int | None:
        """Assign and return this account's display number.

        Zero is the bot's reserved display identity, positive values are people,
        and ``None`` is the only representation of an absent account.
        """
        account = str(account or "")
        if not account:
            return None
        if account == self._self:
            return BOT_DISPLAY_NUMBER
        # Pinned on first sight: a person learned about this account later must not
        # move it to another number once one has been shown.
        person = self._person.setdefault(account, ("account", account))
        n = self._number.get(person)
        if n is None:
            n = len(self._number) + 1
            self._number[person] = n
            self._accounts[n] = []
        if account not in self._accounts[n]:
            self._accounts[n].append(account)
        if spoke:
            self._latest[n] = account
        return n

    def known(self, account: str) -> int | None:
        """Return an existing display number without assigning one."""
        account = str(account or "")
        if not account:
            return None
        if account == self._self:
            return BOT_DISPLAY_NUMBER
        person = self._person.get(account, ("account", account))
        return self._number.get(person)

    def account(self, n: int) -> str | None:
        """The account to address for number `n`: the one that spoke last in the
        conversation, else the first one numbered. None for a number not shown."""
        if n <= BOT_DISPLAY_NUMBER or n not in self._accounts:
            return None
        return self._latest.get(n) or self._accounts[n][0]

    def accounts(self, n: int) -> list[str]:
        """Every account shown under number `n`."""
        if n <= BOT_DISPLAY_NUMBER:
            return []
        return list(self._accounts.get(n, ()))
