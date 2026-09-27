"""Media state is explicit and never encoded in the type of its text payload."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class Resolution:
    text: str | None = None
    retryable: bool = False
