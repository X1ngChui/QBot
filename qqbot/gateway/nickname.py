"""Nickname matching: jieba tokenization, not substring.

A nickname hit is the must-answer path, and a false trigger means barging into a
conversation in public, so precision wins. Substring matching has no word boundary in
Chinese - a two-character nickname fires inside a longer word that merely contains it,
and \\b does not apply - which is why tokenization is the standard fix.

A substring hit that is not a whole token is not being addressed, and the bot stays
quiet. This match is the entire trigger - nothing downstream reconsiders it - so
precision here decides whether the bot ever speaks out of turn.

`nicknames` is always a list: a bot is called by several variants (abbreviations,
homophones, typos that stuck), and every one of them must be injected and matched.
"""

from __future__ import annotations

import logging
import threading

import jieba

log = logging.getLogger("qqbot.nickname")

_lock = threading.Lock()
_injected: set[str] = set()
_ready = False


def initialize() -> None:
    """Warm up at startup so the first message does not eat the ~1s lazy load."""
    global _ready
    with _lock:
        if not _ready:
            jieba.initialize()
            _ready = True
            log.info("jieba initialized")


def register(nicknames: list[str]) -> None:
    """Nicknames are usually coined words outside the dictionary; without injecting them
    jieba splits them apart and the match is missed.

    Matching normalizes to lower case, so the lower-cased form has to be in the
    dictionary too - otherwise a nickname mixing Latin letters and Chinese is tokenized
    correctly in its original case and shredded once the text is lowered.
    """
    with _lock:
        for nick in nicknames:
            for n in {nick.strip(), nick.strip().lower()}:
                if n and n not in _injected:
                    jieba.add_word(n, freq=100000)
                    _injected.add(n)


def _normalize(text: str) -> str:
    return text.strip().lower()


def _is_ascii_alnum(ch: str) -> bool:
    return ch.isascii() and ch.isalnum()


def _has_clean_boundary(norm: str, key: str) -> bool:
    """Guard the one case tokenization cannot settle on its own.

    Injecting a nickname at a huge frequency makes jieba prefer it, so a nickname
    containing Latin letters gets carved out of a longer Latin word - a nickname ending
    in "X" would match inside "Xbox". Chinese needs no such check - that is what
    tokenization is for - so this only fires when the nickname's own edge character is
    ASCII alphanumeric and the neighbouring character is too.
    """
    start = norm.find(key)
    while start >= 0:
        before = norm[start - 1] if start > 0 else ""
        after = norm[start + len(key)] if start + len(key) < len(norm) else ""
        left_ok = not (_is_ascii_alnum(key[0]) and before and _is_ascii_alnum(before))
        right_ok = not (_is_ascii_alnum(key[-1]) and after and _is_ascii_alnum(after))
        if left_ok and right_ok:
            return True
        start = norm.find(key, start + 1)
    return False


def word_hit(text: str, nicknames: list[str]) -> str | None:
    """Return the nickname variant that appears as a whole token, or None."""
    if not nicknames or not text:
        return None
    norm = _normalize(text)
    wanted = {n.strip().lower(): n for n in nicknames if n.strip()}
    if not wanted:
        return None
    if not any(k in norm for k in wanted):  # cheap substring prefilter first
        return None
    initialize()
    tokens = {t.lower() for t in jieba.lcut(norm)}
    for key, original in wanted.items():
        if key in tokens and _has_clean_boundary(norm, key):
            return original
    return None
