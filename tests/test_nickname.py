"""Nickname trigger precision, including Latin word boundaries."""

import pytest

from qqbot.gateway import nickname


NICKS = ["小夜", "小X", "X酱", "bot娘"]
CASES = [
    ("小夜 在吗", "小夜"),
    ("小X你好", "小X"),
    ("X酱早", "X酱"),
    ("bot娘出来", "bot娘"),
    ("叫一下小夜", "小夜"),
    ("@小X 看这个", "小X"),
    ("小x 你说呢", "小X"),
    ("今天听了首小夜曲", None),
    ("这个小Xbox游戏机不错", None),
    ("我的xbox坏了", None),
    ("robot娘化", None),
    ("SmallX酱", None),
    ("今天天气不错", None),
]


@pytest.fixture(scope="module", autouse=True)
def registered_nicknames():
    nickname.initialize()
    nickname.register(NICKS)


@pytest.mark.parametrize(("text", "expected"), CASES)
def test_word_hit(text, expected):
    assert nickname.word_hit(text, NICKS) == expected


def test_false_positive_is_a_real_substring():
    assert any(n in "今天听了首小夜曲" for n in NICKS)
    assert nickname.word_hit("今天听了首小夜曲", NICKS) is None
