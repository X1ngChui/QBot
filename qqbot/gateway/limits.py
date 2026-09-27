"""Parser-owned work and storage bounds, independent of outgoing message length."""

from dataclasses import dataclass, fields


@dataclass(frozen=True, slots=True)
class ParseLimits:
    forward_lines: int = 20
    forward_depth: int = 3
    forward_chars: int = 1500
    segments: int = 256
    references: int = 32
    text_chars: int = 16_384
    metadata_chars: int = 12 * 1024 * 1024
    card_chars: int = 65_536
    resolved_chars: int = 2048

    def __post_init__(self) -> None:
        if any(getattr(self, item.name) < 1 for item in fields(self)):
            raise ValueError("parser bounds must be positive")
        if self.resolved_chars < 16:
            raise ValueError("resolved text must leave room for a complete marker")


PARSE_LIMITS = ParseLimits()
