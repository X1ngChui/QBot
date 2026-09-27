"""Code-owned bounds for this mechanism; not operator configuration."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class SearchHistoryLimits:
    context_lines: int = 5
    max_hits: int = 8
    max_query_terms: int = 8
    max_result_chars: int = 12000


HISTORY_LIMITS = SearchHistoryLimits()


@dataclass(frozen=True, slots=True)
class RecallEventsLimits:
    context_episodes: int = 2
    max_hits: int = 5


RECALL_LIMITS = RecallEventsLimits()


@dataclass(frozen=True, slots=True)
class ReadUrlLimits:
    max_content_chars: int = 8000


PAGE_LIMITS = ReadUrlLimits()


@dataclass(frozen=True, slots=True)
class ImageToolLimits:
    max_images: int = 6


IMAGE_LIMITS = ImageToolLimits()


@dataclass(frozen=True, slots=True)
class EvidenceLimits:
    evidence_result_chars: int = 1200
    evidence_total_chars: int = 4000
    evidence_request_chars: int = 80


EVIDENCE_LIMITS = EvidenceLimits()
