"""Byte routes stop before oversized allocations and retain typed outcomes."""

import base64
from types import SimpleNamespace

import httpx
import pytest

from _budget import fake_budget
import _db as _test_db
from qqbot.services.members import MemberDirectory
from qqbot.media import content, service
from qqbot.media.content import ByteFailure, decode_base64
from qqbot.media.service import MediaProcessor


def test_base64_checks_size_before_invoking_decoder(monkeypatch):
    def unexpected(*args, **kwargs):
        raise AssertionError("oversized payload must never reach the decoder")

    monkeypatch.setattr(content.base64, "b64decode", unexpected)
    assert decode_base64("A" * 10_000, 64) is ByteFailure.TOO_LARGE


@pytest.mark.parametrize("value", [None, {}, "not base64", ""])
def test_malformed_encoded_audio_degrades(value):
    assert decode_base64(value, 64) is None


def test_base64_padding_and_data_url_boundaries():
    assert decode_base64("YQ==", 1) == b"a"
    assert decode_base64("YWJj", 1) is ByteFailure.TOO_LARGE
    assert decode_base64("data:audio/wav;base64," + base64.b64encode(b"abc").decode(), 3) == b"abc"


def test_local_read_is_bounded_and_cannot_escape_the_mount(tmp_path, monkeypatch):
    root = tmp_path / "mount"
    root.mkdir()
    (root / "clip").write_bytes(b"123456789")
    (tmp_path / "outside").write_bytes(b"private")
    monkeypatch.setattr(service, "NAPCAT_DATA_DIR", str(root))
    assert MediaProcessor._local("/home/.config/QQ/clip", 4) is ByteFailure.TOO_LARGE
    assert MediaProcessor._local("/home/.config/QQ/clip", 9) == b"123456789"
    assert MediaProcessor._local("/home/.config/QQ/../outside", 64) is None


async def test_streamed_download_stops_at_the_byte_bound():
    class Stream(httpx.AsyncByteStream):
        sent = 0

        async def __aiter__(self):
            for _ in range(1_000):
                self.sent += 1
                yield b"x" * 65_536

    stream = Stream()
    processor = MediaProcessor(
        SimpleNamespace(),
        SimpleNamespace(),
        budget=fake_budget(),
        members=MemberDirectory(),
        cache=_test_db.media_cache,
        prompts=_test_db.test_bundle().prompts,
    )
    processor._http = httpx.AsyncClient(
        transport=httpx.MockTransport(lambda request: httpx.Response(200, stream=stream))
    )
    try:
        assert await processor._fetch("https://example.invalid/image", 100) is ByteFailure.TOO_LARGE
        assert stream.sent == 1
    finally:
        await processor.close()
