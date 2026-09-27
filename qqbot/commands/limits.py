"""Code-owned bounds for this mechanism; not operator configuration."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class CommandLimits:
    roster_max_entries: int = 60
    top_default_entries: int = 5
    top_max_entries: int = 20


COMMAND_LIMITS = CommandLimits()
