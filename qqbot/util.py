"""Small helpers: secret reading, timezones and local time, error rendering."""

from __future__ import annotations

import logging
import re
import os
from datetime import datetime, timedelta, timezone
from pathlib import Path
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

log = logging.getLogger("qqbot.util")

# Everything user-visible is in local time: the daily report, the cron jobs, the "what time
# is it" line in the prompt, and the day boundary the budget resets on. Configurable via
# `timezone` in settings.yaml; this is only the fallback until config is loaded. A named
# zone even as the fallback, because a plain offset serialises as "UTC+08:00", which SQL
# must never see (POSIX reads the sign backwards - tz_sql below). The offset form
# survives only for an environment with no tzdata at all.
try:
    _TZ: timezone | ZoneInfo = ZoneInfo("Asia/Shanghai")
except ZoneInfoNotFoundError:                            # pragma: no cover
    _TZ = timezone(timedelta(hours=8))


def set_timezone(name: str) -> None:
    """Called once at startup. An unknown name keeps the previous zone rather than
    crashing the bot over a typo in a display setting."""
    global _TZ
    try:
        _TZ = ZoneInfo(name)
    except (ZoneInfoNotFoundError, ValueError) as e:
        log.error("unknown timezone %r (%s), staying on %s", name, e, _TZ)


def tz() -> timezone | ZoneInfo:
    return _TZ


def tz_sql() -> str:
    """The active zone as text PostgreSQL will read to mean the same time.

    An IANA name passes through as itself. A fixed offset must be *inverted*: bare
    offset strings are POSIX zone syntax, in which the sign runs the other way -
    'UTC+08:00' places local time eight hours *behind* Greenwich. Serialising the
    fallback zone with str() shifted every derived date by sixteen hours (a real
    incident: the daily report saw no new groups, and a first-day platform name could
    score as multi-day).
    """
    z = _TZ
    if isinstance(z, ZoneInfo):
        return str(z)
    total = -int((z.utcoffset(None) or timedelta()).total_seconds())
    sign = "+" if total >= 0 else "-"
    total = abs(total)
    return f"UTC{sign}{total // 3600:02d}:{total % 3600 // 60:02d}"


DOCKER_SECRETS_DIR = Path("/run/secrets")


def _read_key_file(path: Path) -> str:
    """utf-8-sig, because a secret written by a Windows editor carries a BOM that strip()
    will not remove (U+FEFF is not whitespace) - it would ride along into the auth header
    and come back as an unexplained 401."""
    return path.read_text(encoding="utf-8-sig").strip().lstrip("﻿")


def read_secret(env_name: str, fallback_env: str | None = None) -> str:
    """Prefer the docker secret pointed at by *_FILE, fall back to a plain env var."""
    path = os.getenv(env_name)
    if path:
        p = Path(path)
        if p.is_file():
            return _read_key_file(p)
    if fallback_env:
        return (os.getenv(fallback_env) or "").strip().lstrip("﻿")
    return ""


def read_api_key(name: str) -> str:
    """Resolve one capability's key from the name its config section points at.

    Each provider names its own key (llm.<capability>.api_key_env), so capabilities that
    happen to share a key today can be split later by editing YAML alone. Resolution
    order for a name like MEDIA_API_KEY:

        MEDIA_API_KEY_FILE -> the file it points at   (a mounted secret file)
        MEDIA_API_KEY                                 (plain env var - the deployed form:
                                                       compose injects it from .env)
        /run/secrets/media_api_key                    (legacy docker-secret mount)

    The file forms stay supported so a deployment that prefers mounted secrets only has
    to set the variables; the shipped compose file uses plain env from .env.
    """
    name = name.strip()
    if not name:
        return ""
    direct = read_secret(f"{name}_FILE", name)
    if direct:
        return direct
    conventional = DOCKER_SECRETS_DIR / name.lower()
    if conventional.is_file():
        return _read_key_file(conventional)
    return ""



def now_local() -> datetime:
    return datetime.now(_TZ)


def today_local() -> str:
    return now_local().strftime("%Y-%m-%d")


WEEKDAYS = ("周一", "周二", "周三", "周四", "周五", "周六", "周日")


def describe_now() -> str:
    """The current moment, for the prompt. Models have no clock of their own."""
    n = now_local()
    return f"{n:%Y年%m月%d日} {WEEKDAYS[n.weekday()]} {n:%H:%M}"


def fmt_when(dt: datetime) -> str:
    """A moment as the prompt stamps it on a transcript line ("08-30 14:03").

    One format shared by the history window, the extraction transcript and the
    search_history results, so a time read in one place matches a time read in another.
    Aware datetimes are converted to the display zone first - asyncpg hands timestamptz
    back in UTC, and a UTC wall time beside the local current-time line reads as hours
    ago. A naive datetime is taken as already local."""
    if dt.tzinfo is not None:
        dt = dt.astimezone(_TZ)
    return f"{dt:%m-%d %H:%M}"

#: The system bracket pair, U+27E6/U+27E7. Every marker the system writes into a
#: transcript - timestamps, media descriptions, owner/self/namesake tags, quote
#: pointers, notice lines, provenance - uses these and only these. They are
#: unforgeable because defang() replaces them with ASCII square brackets in every
#: untrusted string before it can reach a rendered line: a card imitating the
#: owner tag, or a message body imitating a media marker, produces plain text
#: that no longer collides with any marker the model is taught to trust.
SYS_L = "⟦"
SYS_R = "⟧"


def defang(text: str) -> str:
    """Strip the system brackets out of an untrusted string, preserving its look.

    Replacement rather than deletion: the characters are legitimate (if exotic)
    typography, and a pasted maths snippet should stay readable - it just loses
    the one property that matters, being mistakable for system markup. Applied at
    the boundaries where outside text enters a transcript: segment parsing, media
    descriptions, member names, tool digests. Idempotent, cheap, total.
    """
    if not text:
        return text
    return text.replace(SYS_L, "[").replace(SYS_R, "]")


def sysmark(body: str) -> str:
    """One system marker, in the reserved brackets. The single spelling of the
    grammar, so a marker written here can never drift from the defang() pair."""
    return f"{SYS_L}{body}{SYS_R}"


def merge_overlapping(sets: list[set]) -> list[set]:
    """Union together every group of sets that share members, transitively.

    Both retrieval tools use it the same way: a hit's context window is a
    contiguous run in some archive order, so two windows overlap exactly when
    they share an element, and overlapping windows must render as one block
    rather than repeat their shared lines. Order of the returned blocks is
    unspecified - callers sort by their own key.
    """
    blocks: list[set] = []
    for w in sets:
        merged = set(w)
        rest = []
        for b in blocks:
            if b & merged:
                merged |= b
            else:
                rest.append(b)
        rest.append(merged)
        blocks = rest
    return blocks


_DURATION = re.compile(r"^(\d{1,5})\s*([mhd])$", re.I)
_UNIT_MINUTES = {"m": 1, "h": 60, "d": 1440}


def parse_duration(text: str) -> timedelta | None:
    """A duration as an owner types one - "30m", "12h", "3d".

    None for anything else, so the caller can tell "no duration given" and "typo"
    apart from a zero. The digit cap is the sanity bound: five digits of days is
    already past any meaning, and unbounded input is how an overflow gets typed.
    """
    m = _DURATION.match((text or "").strip())
    if m is None:
        return None
    minutes = int(m.group(1)) * _UNIT_MINUTES[m.group(2).lower()]
    return timedelta(minutes=minutes) if minutes else None


def why(e: BaseException) -> str:
    """An exception rendered so that the log line says something.

    str(TimeoutError()) is "", and a timeout is exactly the failure worth logging -
    logging str(e) alone writes lines that carry no information at all. The type name is
    the part that is always there.
    """
    text = str(e).strip()
    return f"{type(e).__name__}: {text}" if text else type(e).__name__
