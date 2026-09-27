"""Code-owned bounds for this mechanism; not operator configuration."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class DiagnosticLimits:
    debug_max_rounds: int = 50
    log_tail_default_lines: int = 15
    log_tail_max_lines: int = 60
    log_tail_scan_bytes: int = 65536
    error_ring_entries: int = 200
    error_message_chars: int = 300
    daily_report_recent_errors: int = 8


DIAGNOSTIC_LIMITS = DiagnosticLimits()
