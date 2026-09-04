"""Nickname matching: word-level hits, plus the boundary cases tokenization alone cannot
settle.

This is the must-answer path, so a false positive means barging into a conversation in
public. Precision wins; a near-miss simply draws no reply - this gate is the entire
trigger, with nothing behind it, which is exactly why the misses pinned here have to
stay misses.
"""

import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
os.environ.setdefault("PROMPTS_DIR", str(ROOT / "config" / "prompts"))

from qqbot.core import nickname

nickname.initialize()
NICKS = ["小夜", "小X", "X酱", "bot娘"]
nickname.register(NICKS)

CASES = [
    # must answer
    ("小夜 在吗", "小夜"),
    ("小X你好", "小X"),
    ("X酱早", "X酱"),
    ("bot娘出来", "bot娘"),
    ("叫一下小夜", "小夜"),
    ("@小X 看这个", "小X"),
    ("小x 你说呢", "小X"),           # matching is case-insensitive
    # must NOT force an answer
    ("今天听了首小夜曲", None),       # substring, not a word
    ("这个小Xbox游戏机不错", None),   # latin nickname carved out of a longer latin word
    ("我的xbox坏了", None),
    ("robot娘化", None),
    ("SmallX酱", None),
    ("今天天气不错", None),
]


def main() -> int:
    bad = []
    for text, want in CASES:
        got = nickname.word_hit(text, NICKS)
        ok = got == want
        print(f"[{'ok ' if ok else 'FAIL'}] {text!r} -> {got!r} (want {want!r})")
        if not ok:
            bad.append(text)

    # The near-miss above really does contain a nickname. That is the whole case for
    # tokenizing: substring matching would have answered, in public, uninvited.
    near = any(n in "今天听了首小夜曲" for n in NICKS)
    print(f"[{'ok ' if near else 'FAIL'}] the near-miss is a substring, and still not a hit")
    if not near:
        bad.append("near-miss is a substring")

    print()
    print("FAILED:", bad or "none")
    return 1 if bad else 0


sys.exit(main())
