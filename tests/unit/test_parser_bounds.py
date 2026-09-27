"""The parser bounds work before retaining references or expanding nested records."""

from dataclasses import replace
from datetime import UTC

from hypothesis import given, strategies as st

from qqbot.gateway.limits import PARSE_LIMITS
from qqbot.gateway.segments import ImageRef, parse_segments


def text(value):
    return {"type": "text", "data": {"text": value}}


def image():
    return {"type": "image", "data": {"file": "a" * 32 + ".png"}}


def forward(*segments):
    return {
        "type": "forward",
        "data": {"content": [{"sender": {"nickname": "Fictional"}, "message": list(segments)}]},
    }


def test_parser_does_not_load_configuration(monkeypatch):
    import qqbot.configuration as configuration

    def unexpected():
        raise AssertionError("pure parsing must not load configuration")

    monkeypatch.setattr(configuration, "config", unexpected, raising=False)
    parsed = parse_segments([text("example")], "999", display_zone=UTC)
    assert parsed.render() == "example"
    assert not hasattr(ImageRef(), "resolve")


def test_forward_budget_is_spent_before_registering_later_images():
    limits = replace(PARSE_LIMITS, forward_chars=80)
    parsed = parse_segments([forward(text("x" * 10_000), image())], "999", limits)
    assert not parsed.pictures
    assert len(parsed.render()) < 200
    assert parsed.truncated


def test_nested_records_cannot_overspend_the_shared_line_budget():
    limits = replace(PARSE_LIMITS, forward_lines=1)
    parsed = parse_segments([forward(forward(image()))], "999", limits)
    assert not parsed.pictures
    assert "Fictional" in parsed.render()


def test_top_level_text_does_not_retain_invisible_references():
    limits = replace(PARSE_LIMITS, text_chars=64)
    parsed = parse_segments([text("x" * 1_000_000), image()], "999", limits)
    assert len(parsed.typed_text) <= 64
    assert not parsed.pictures
    assert parsed.truncated


def test_reference_metadata_has_a_shared_bound():
    limits = replace(PARSE_LIMITS, metadata_chars=64)
    parsed = parse_segments(
        [{"type": "image", "data": {"url": "https://example.invalid/" + "x" * 1_000}}],
        "999",
        limits,
    )
    assert parsed.pictures[0].url is None
    assert parsed.truncated


def test_deep_json_card_degrades_without_raising():
    parsed = parse_segments(
        [{"type": "json", "data": {"data": "[" * 2_000 + "0" + "]" * 2_000}}], "999"
    )
    assert parsed.render() == "⟦卡片消息⟧"


@given(st.integers(min_value=1, max_value=1_000), st.integers(min_value=1, max_value=48))
def test_reference_count_and_render_order_are_bounded(count, bound):
    limits = replace(PARSE_LIMITS, references=bound, segments=64)
    parsed = parse_segments([image()] * count, "999", limits)
    assert len(parsed.refs) <= min(bound, 64)
    assert parsed.render().count("⟦图片⟧") == len(parsed.pictures)
    assert [ref.slot for ref in parsed.refs] == list(range(len(parsed.refs)))


def test_resolved_content_is_bounded_without_losing_picture_markers():
    limits = replace(PARSE_LIMITS, resolved_chars=64)
    parsed = parse_segments([image(), image()], "999", limits)
    rendered = parsed.render({0: "⟦图片:" + "x" * 1_000_000 + "⟧"})
    assert len(rendered) < 140
    assert rendered.count("⟦") == rendered.count("⟧") == 2
