"""Transport-neutral authorship recorded with archived messages."""

from enum import StrEnum


class AuthorKind(StrEnum):
    """Who authored an archived message, independent of account identity."""

    MEMBER = "member"
    BOT = "bot"
