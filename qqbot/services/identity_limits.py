"""Code-owned bounds for this mechanism; not operator configuration."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class ChallengeLimits:
    challenge_ttl_sec: int = 600
    max_pending_challenges: int = 1000
    max_pending_per_account: int = 3
    challenge_code_length: int = 8


CHALLENGE_LIMITS = ChallengeLimits()
