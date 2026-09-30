"""Code-owned bounds for this mechanism; not operator configuration."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class ChallengeLimits:
    challenge_ttl_sec: int = 600
    max_pending_challenges: int = 1000


CHALLENGE_LIMITS = ChallengeLimits()
