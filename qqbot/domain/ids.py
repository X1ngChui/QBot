"""Validated identifier value objects shared across application boundaries."""

from __future__ import annotations


from typing import Any

from pydantic_core import core_schema


class _TextId(str):
    """A non-empty immutable identifier with canonical string behavior."""

    label = "identifier"

    @classmethod
    def __get_pydantic_core_schema__(cls, source: Any, handler: Any) -> core_schema.CoreSchema:
        return core_schema.no_info_after_validator_function(cls, core_schema.str_schema())

    def __new__(cls, value: object):
        parsed = str(value or "").strip()
        if not parsed:
            raise ValueError(f"{cls.label} cannot be empty")
        return str.__new__(cls, parsed)


class GroupId(_TextId):
    """A QQ group identifier; numeric only at OneBot and PostgreSQL boundaries."""

    label = "group id"

    def __new__(cls, value: object):
        parsed = str(value or "").strip()
        if not parsed.isdigit() or int(parsed) <= 0:
            raise ValueError(f"invalid group id: {value!r}")
        return str.__new__(cls, str(int(parsed)))

    def to_onebot(self) -> int:
        """Encode this identifier for the current OneBot adapter contract."""
        return int(str(self))

    def to_db(self) -> int:
        """Encode this identifier for the current PostgreSQL BIGINT schema."""
        return int(str(self))


class AccountId(_TextId):
    """A QQ account identifier."""

    label = "account id"


class MessageId(_TextId):
    """A QQ message identifier, including stable synthetic notice identifiers."""

    label = "message id"
