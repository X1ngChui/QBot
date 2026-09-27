"""Media path: every segment type, sticker and image caching, content refusals, caps."""

import pytest


import asyncio


import _db as _test_db
from _db import pool
from qqbot.domain.ids import GroupId
from _fixtures import config
from _db import reset
from _stubs import FakeEmbedding
from dataclasses import replace
from qqbot.media.limits import MEDIA_IO
from qqbot.media.service import MediaProcessor
from qqbot.media.content import ByteFailure
from qqbot.media.service import _mime
from qqbot.services.retrieval import build_directory
from qqbot.gateway.segments import AudioRef
from qqbot.gateway.segments import ImageRef
from qqbot.gateway.segments import ParsedMessage as _PMcls
from qqbot.gateway.segments import parse_segments
from _test_owners import fresh_budget, fresh_members
from qqbot.providers import Providers, VisionModel
from qqbot.providers.registry import build as build_providers
from qqbot.providers.base import Rate

BUDGET = None
MEMBERS = None
VISION_CALLS = []
_real: Providers | None = None
_EMBED: FakeEmbedding | None = None
_PROVIDERS: Providers | None = None
MEDIA: MediaProcessor | None = None


def _PM(refs):
    pm = _PMcls()
    pm.refs = list(refs)
    return pm


class FakeVision(VisionModel):
    """A real subclass of the ABC - the code under test cannot tell the difference, and an
    incomplete fake would fail at construction rather than mid-test."""

    name = "fake"

    def rate_for(self, model):
        return Rate("Mtoken", in_miss=1.2, out=7.2, source="fake")

    async def describe(self, data, *, prompt="", mime="image/jpeg", group_id=None):
        VISION_CALLS.append(len(data))
        return "一只橘猫在键盘上打滚"

    async def aclose(self):
        pass


@pytest.fixture
async def media_state(test_database, monkeypatch):
    budget = fresh_budget()
    members = fresh_members()
    real = build_providers(config().default, budget=budget)
    providers_bundle = Providers(
        text=real.text,
        vision=FakeVision(),
        asr=real.asr,
        embedding=FakeEmbedding(),
        search=real.search,
    )
    media = MediaProcessor(
        providers_bundle,
        build_directory(
            database=_test_db.pool,
            predicates=_test_db.test_bundle().predicates,
            clock=_test_db.clock,
        ),
        budget=budget,
        members=members,
        cache=_test_db.media_cache,
        prompts=_test_db.test_bundle().prompts,
    )
    for name, value in (
        ("BUDGET", budget),
        ("MEMBERS", members),
        ("VISION_CALLS", []),
        ("_PROVIDERS", providers_bundle),
        ("_EMBED", providers_bundle.embedding),
        ("_real", real),
        ("MEDIA", media),
    ):
        monkeypatch.setitem(globals(), name, value)
    try:
        yield
    finally:
        await media.close()
        await members.close()
        await real.aclose()


def providers():
    return _PROVIDERS


def set_providers(bundle):
    global _PROVIDERS
    _PROVIDERS = bundle
    MEDIA._providers = bundle


class FakeBot:
    self_id = "999"

    async def call_api(self, api, **kw):
        return {}

    async def send_group_msg(self, *, group_id, message):
        return {"message_id": "0"}


@pytest.mark.database
@pytest.mark.asyncio
async def test_media(media_state):
    await reset()
    cfg = config().default
    bot = FakeBot()

    async def _fake_fetch(url, max_bytes):
        return b"x" * 1024

    MEDIA._fetch = _fake_fetch
    MEDIA._local = staticmethod(lambda path, max_bytes: None)  # force the URL path

    # A sticker goes to the model like any other picture. The summary its sender's client
    # supplies names the category rather than what is drawn, and the joke in a
    # sticker is the drawing - so it is the fallback, not the answer.
    ref = ImageRef(
        slot=0, sticker=True, key="emoji-1", url="http://example/s.gif", summary="动画表情"
    )
    out = (await MEDIA.describe_image(ref, bot=bot, group_id=GroupId("1"), cfg=cfg)).text
    assert out == "⟦表情:一只橘猫在键盘上打滚⟧", (
        "a sticker is described, not just labelled",
        str(out),
    )
    assert len(VISION_CALLS) == 1, ("and it keeps the sticker label", str(VISION_CALLS))
    assert await _test_db.media_cache.image_cache_get("emoji-1") == "⟦表情:一只橘猫在键盘上打滚⟧", (
        "the description is cached under the sticker id"
    )
    out = (await MEDIA.describe_image(ref, bot=bot, group_id=GroupId("1"), cfg=cfg)).text
    assert len(VISION_CALLS) == 1, (
        "a repeat sticker costs nothing - this is what makes it affordable",
        str(VISION_CALLS),
    )

    # Unreachable image: the sender's label beats saying nothing at all.
    _fetch, MEDIA._fetch = MEDIA._fetch, lambda url, max_bytes: asyncio.sleep(0, None)
    ref_gone = ImageRef(
        slot=0, sticker=True, key="emoji-2", url="http://example/gone.gif", summary="动画表情"
    )
    out = (await MEDIA.describe_image(ref_gone, bot=bot, group_id=GroupId("1"), cfg=cfg)).text
    assert out == "⟦表情:动画表情⟧", ("an unfetchable sticker falls back to its label", str(out))
    MEDIA._fetch = _fetch

    # image: first sight calls the model, second is a cache hit
    base = len(VISION_CALLS)
    key = "a" * 32
    ref = ImageRef(slot=0, key=key, url="http://example/img.jpg")
    out1 = (await MEDIA.describe_image(ref, bot=bot, group_id=GroupId("1"), cfg=cfg)).text
    assert out1 == "⟦图片:一只橘猫在键盘上打滚⟧", ("first sight describes", str(out1))
    assert len(VISION_CALLS) == base + 1, ("first sight costs one vision call", str(VISION_CALLS))
    out2 = (await MEDIA.describe_image(ref, bot=bot, group_id=GroupId("1"), cfg=cfg)).text
    assert out2 == out1, ("second sight hits the cache", str(out2))
    assert len(VISION_CALLS) == base + 1, ("cache hit costs no vision call", str(VISION_CALLS))
    hits = await pool().fetchval("SELECT hit_count FROM image_cache WHERE key=$1", key)
    assert hits == 1, ("cache hit counted", str(hits))

    # Every segment type QQ actually sends has to come out as text. markdown was the one
    # that proved this matters: bots post their whole output that way - game results,
    # divination - and with no branch for it the message reached the model empty.
    md = "\n".join(
        [
            "[](%7B%22version%22%3A2%7D)",
            "[@某人](mqqapi://markdown/mention?at_type=1&at_tinyid=1)",
            "今天的运势是中吉",
            "![图片 #120px #120px](https://example/x.png)",
            "",
            "##### 【第 3 次抽签】",
            "",
            "宜出门，忌熬夜",
            "",
            "> 签文的一段说明",
        ]
    )
    out = parse_segments([{"type": "markdown", "data": {"content": md}}], "999").render()
    assert "今天的运势是中吉" in out and "【第 3 次抽签】" in out and "签文的一段说明" in out, (
        "markdown reaches the model as its text",
        out,
    )
    assert "mqqapi" not in out and "#####" not in out, ("and its markup does not", out)
    assert "⟦图片⟧" in out, ("an inline image becomes a marker", out)

    def one(seg):
        return parse_segments([seg], "999").render()

    assert (
        one({"type": "face", "data": {"id": "425", "raw": {"faceText": "/求放过"}}})
        == "⟦表情:求放过⟧"
    ), "a named emoticon uses its name"
    assert one({"type": "face", "data": {"id": "9", "raw": {}}}) == "⟦表情:大哭⟧", (
        "a classic emoticon is looked up by id"
    )
    assert one({"type": "face", "data": {"id": "99999", "raw": {}}}) == "⟦表情⟧", (
        "an unlisted one degrades to the bare marker"
    )
    assert one({"type": "dice", "data": {"result": "4"}}) == "⟦骰子:4点⟧", "dice shows the roll"
    assert (
        one({"type": "rps", "data": {"result": "1"}}) == "⟦猜拳:布⟧"
        and one({"type": "rps", "data": {"result": "2"}}) == "⟦猜拳:剪刀⟧"
        and one({"type": "rps", "data": {"result": "3"}}) == "⟦猜拳:石头⟧"
    ), "rps maps QQ package order to paper, scissors, rock"
    # QQ keeps inventing segment types. Saying something beats dropping the message, and
    # the log line is how the next one gets noticed instead of silently vanishing.
    assert one({"type": "keyboard", "data": {"rows": []}}) == "⟦消息⟧", (
        "an unknown type still says something"
    )

    # A backend that looks at a picture and declines is behaving normally, not failing.
    # Paying to be refused again on every repost is the actual cost, so the outcome is
    # cached like any other.
    from qqbot.media.service import _is_refusal

    assert _is_refusal(
        Exception(
            "Error code: 400 - InternalError.Algo."
            "DataInspectionFailed: may contain inappropriate content"
        )
    ), "a content refusal is recognised"
    assert not _is_refusal(Exception("connection reset by peer")), "an ordinary failure is not"

    class Refusing:
        name = "refusing"

        def rate_for(self, model):
            return Rate("Mtoken")

        async def describe(self, data, *, prompt="", mime="image/jpeg", group_id=None):
            VISION_CALLS.append(len(data))
            raise RuntimeError("400 data_inspection_failed: inappropriate content")

        async def aclose(self):
            pass

    _ok = providers().vision
    set_providers(
        Providers(
            text=_real.text, vision=Refusing(), asr=_real.asr, embedding=_EMBED, search=_real.search
        )
    )
    ref_no = ImageRef(slot=0, key="c" * 32, url="http://example/nope.jpg")
    before = len(VISION_CALLS)
    out = (await MEDIA.describe_image(ref_no, bot=bot, group_id=GroupId("1"), cfg=cfg)).text
    assert out == "⟦图片⟧", ("a refused image is a terminal bare marker", str(out))
    assert len(VISION_CALLS) == before + 1, "and the refusal cost one call"
    out = (await MEDIA.describe_image(ref_no, bot=bot, group_id=GroupId("1"), cfg=cfg)).text
    assert len(VISION_CALLS) == before + 1, ("reposting it costs nothing more", str(VISION_CALLS))
    assert out == "⟦图片⟧", ("the cached outcome still reads as not received", str(out))
    set_providers(
        Providers(text=_real.text, vision=_ok, asr=_real.asr, embedding=_EMBED, search=_real.search)
    )

    # A received link carries an rkey that expires in about two hours, but the file id
    # does not - get_image trades it for a fresh one, which is why the QQ client still
    # shows pictures from days ago. So an expired link is not a lost picture, as long as
    # something falls through to it.
    class RefreshingBot:
        self_id = "999"
        calls = 0

        async def call_api(self, api, **kw):
            if api == "get_image":
                RefreshingBot.calls += 1
                return {"file": "", "url": "http://example/refreshed.jpg"}
            return {}

        async def send_group_msg(self, *, group_id, message):
            return {"message_id": "0"}

    set_providers(
        Providers(
            text=_real.text,
            vision=FakeVision(),
            asr=_real.asr,
            embedding=_EMBED,
            search=_real.search,
        )
    )
    _fetch3 = MEDIA._fetch

    async def only_fresh(url, max_bytes):
        # The original link is dead; the one get_image hands back works.
        return b"y" * 2048 if "refreshed" in url else None

    MEDIA._fetch = only_fresh
    seen0 = len(VISION_CALLS)
    ref_old = ImageRef(slot=0, key="d" * 32, url="http://example/expired.jpg", file="ABCDEF.jpg")
    out = (
        await MEDIA.describe_image(ref_old, bot=RefreshingBot(), group_id=GroupId("1"), cfg=cfg)
    ).text
    assert out == "⟦图片:一只橘猫在键盘上打滚⟧" and RefreshingBot.calls == 1, (
        "an expired link is refreshed rather than given up on",
        str(out),
    )
    assert len(VISION_CALLS) == seen0 + 1, "and the picture did reach the model"
    MEDIA._fetch = _fetch3

    # A marketplace sticker arrives as a directory link whose redirect target is
    # often absent; the same directory serves a 300x300 PNG, so that is asked first.
    seen_links = []

    async def recording_fetch(url, max_bytes):
        seen_links.append(url)
        return b"z" * 2048 if url.endswith("/300x300.png") else None

    MEDIA._fetch = recording_fetch
    _sticker = ImageRef(
        slot=0, key="e" * 32, url="https://gxh.vip.qq.com/club/item/parcel/item/74/" + "e" * 32
    )
    assert await MEDIA._bytes(
        _sticker, bot=RefreshingBot(), max_bytes=1 << 20
    ) is not None and seen_links[0].endswith("/300x300.png"), (
        "a marketplace sticker is fetched as its 300x300 PNG",
        str(seen_links),
    )
    # The shape the protocol side actually sends names the raw GIF inside the
    # directory; the PNG sits beside it and is still asked for first.
    seen_links.clear()
    _gif = ImageRef(
        slot=0,
        key="h" * 32,
        url="https://gxh.vip.qq.com/club/item/parcel/item/74/" + "f" * 32 + "/raw300.gif",
    )
    assert await MEDIA._bytes(
        _gif, bot=RefreshingBot(), max_bytes=1 << 20
    ) is not None and seen_links[0].endswith("/" + "f" * 32 + "/300x300.png"), (
        "a sticker linked by its raw GIF is fetched as the PNG beside it",
        str(seen_links),
    )
    MEDIA._fetch = _fetch3

    # A picture the platform can no longer serve does not fail get_image, it hangs;
    # the fetch has its own short deadline, and the picture is then remembered as
    # unreadable so the next reply looking at it does not wait it out again.
    dead_media = MediaProcessor(
        _PROVIDERS,
        build_directory(
            database=_test_db.pool,
            predicates=_test_db.test_bundle().predicates,
            clock=_test_db.clock,
        ),
        io=replace(MEDIA_IO, protocol_timeout_sec=0.2),
        budget=BUDGET,
        members=MEMBERS,
        cache=_test_db.media_cache,
        prompts=_test_db.test_bundle().prompts,
    )
    dead_media._local = staticmethod(lambda path, max_bytes: None)

    class HangingBot:
        self_id = "999"
        calls = 0

        async def call_api(self, api, **kw):
            HangingBot.calls += 1
            await asyncio.sleep(5)
            return {}

    async def dead_link(url, max_bytes):
        return None

    dead_media._fetch = dead_link
    _dead = ImageRef(slot=0, key="f" * 32, url="http://example/gone.jpg", file="GONE.jpg")
    _t = asyncio.get_event_loop().time()
    assert (
        await dead_media._bytes(_dead, bot=HangingBot(), max_bytes=1 << 20) is None
        and asyncio.get_event_loop().time() - _t < 2
    ), ("a hanging get_image is given up on quickly", str(HangingBot.calls))
    await dead_media._bytes(_dead, bot=HangingBot(), max_bytes=1 << 20)
    assert HangingBot.calls == 1, (
        "and an unreadable picture is not retried for a while",
        str(HangingBot.calls),
    )
    # Two readers at once share one flight: the second must not spend the
    # deadline again while the first is still finding out the picture is dead.
    dead_media._unreadable.clear()
    HangingBot.calls = 0
    _dead2 = ImageRef(slot=0, key="e" * 32, url="http://example/gone2.jpg", file="GONE2.jpg")
    _pair = await asyncio.gather(
        dead_media._bytes(_dead2, bot=HangingBot(), max_bytes=1 << 20),
        dead_media._bytes(_dead2, bot=HangingBot(), max_bytes=1 << 20),
    )
    assert HangingBot.calls == 1 and _pair == [None, None], (
        "concurrent readers of one picture share a single fetch",
        str(HangingBot.calls),
    )
    assert dead_media._work.size == 0, "the flight registry is empty once the fetch ends"

    # An oversize picture is a verdict, not a failure: it is not marked unreadable.
    async def too_big(url, max_bytes):
        return ByteFailure.TOO_LARGE

    dead_media._fetch = too_big
    _big = ImageRef(slot=0, key="d" * 32, url="http://example/big.jpg", file="BIG.jpg")
    _got = await dead_media._bytes(_big, bot=HangingBot(), max_bytes=1 << 20)
    assert (
        _got is ByteFailure.TOO_LARGE
        and dead_media._resource_key("d" * 32) not in dead_media._unreadable
    ), "an oversize picture answers the sentinel and is not marked unreadable"
    await dead_media.close()

    assert (
        _mime(b"GIF89a....", "x.image") == "image/gif"
        and _mime(b"\x89PNG\r\n", None) == "image/png"
        and _mime(b"RIFF....WEBPVP8 ", "a.jpg") == "image/webp"
        and _mime(b"\xff\xd8\xff", "a.png") == "image/jpeg"
    ), "mime sniffed from bytes beats the file name"
    assert _mime(b"????", "a.image") == "image/jpeg" and _mime(b"????", "a.bmp") == "image/bmp", (
        "mime falls back to the suffix, and .image means jpeg"
    )

    # Voice never takes the shortcut pictures take. The stored file (and the CDN
    # original) is SILK v3 wearing an .amr suffix, and SILK sent raw draws a
    # politely empty transcript - a perfectly clear clip the bot claims it cannot
    # hear. Only get_record's WAV is decodable, so
    # transcription must ignore the local file and the URL even when both exist.
    from qqbot.media.service import _audio_seconds
    from qqbot.media.service import _byte_cap

    assert abs(_audio_seconds(320000) - 10.0) < 0.1 and _byte_cap(300) == 9600000, (
        "audio arithmetic is WAV-only now",
        f"{_audio_seconds(320000)} {_byte_cap(300)}",
    )

    import base64 as _b64
    from qqbot.gateway.segments import AudioRef as _AR
    from qqbot.providers import AsrModel as _AsrABC
    from qqbot.providers.base import Rate as _Rate

    _SILK = b"\x02#!SILK_V3" + b"\x00" * 64
    _WAV = b"RIFFfakewavdata" * 40
    _asr_seen = {}

    class CapturingAsr(_AsrABC):
        name = "capturing"

        def rate_for(self, model):
            return _Rate("second", per_unit=0.0)

        async def transcribe(self, data, *, fmt="wav", seconds=None, group_id=None):
            _asr_seen.update(data=data, fmt=fmt)
            return "明天一起去吃饭"

        async def aclose(self):
            pass

    class VoiceBot(FakeBot):
        record_calls = 0

        async def call_api(self, api, **kw):
            if api == "get_record":
                VoiceBot.record_calls += 1
                assert kw.get("out_format") == "wav"
                return {"base64": _b64.b64encode(_WAV).decode()}
            return await super().call_api(api, **kw)

    _local_saved = MEDIA._local
    MEDIA._local = staticmethod(lambda p, m: _SILK)  # the trap, armed
    _prev_asr = providers().asr
    set_providers(
        Providers(
            text=_real.text,
            vision=FakeVision(),
            asr=CapturingAsr(),
            embedding=_EMBED,
            search=_real.search,
        )
    )
    _vref = _AR(slot=0, file="v.amr", url="http://cdn/v.amr")
    _vout = (await MEDIA.transcribe(_vref, bot=VoiceBot(), group_id=GroupId("9009"), cfg=cfg)).text
    assert VoiceBot.record_calls == 1, (
        "transcription goes through get_record even with a local file on offer"
    )
    assert _asr_seen.get("data") == _WAV and _asr_seen.get("fmt") == "wav", (
        "what reaches the ASR backend is the transcoded WAV, never SILK",
        str(_asr_seen.get("data", b"")[:12]),
    )
    assert _vout == "⟦语音:明天一起去吃饭⟧", ("and the transcript comes back marked", repr(_vout))
    # Voice resolves on the arrival schedule now, like pictures: the free pass
    # plus media_now must transcribe, so extraction reads text even in groups
    # the bot never answers.
    _pm_v = _PM([_AR(slot=0, file="v2.amr", url="http://cdn/v2.amr")])
    _res_v = await MEDIA.resolve(_pm_v, bot=VoiceBot(), group_id=GroupId("9009"), cfg=cfg)
    assert _res_v[0].text == "⟦语音:明天一起去吃饭⟧", (
        "a voice clip transcribes on arrival, before any reply",
        repr(_res_v),
    )
    MEDIA._local = _local_saved

    # Local ASR is CPU work, not provider spending. Even with the CNY budget forced
    # to exhausted, transcription continues; its own per-group gate and bounded
    # global recognizer queue are the admission controls.

    async def _true():
        return True

    _budget_saved = BUDGET.exceeded
    BUDGET.exceeded = _true
    MEDIA._asr_windows.clear()
    _vout = (
        await MEDIA.transcribe(
            _AR(slot=0, file="free.amr"), bot=VoiceBot(), group_id=GroupId("9010"), cfg=cfg
        )
    ).text
    assert _vout == "⟦语音:明天一起去吃饭⟧", (
        "local ASR transcribes straight through an exhausted CNY budget",
        repr(_vout),
    )
    BUDGET.exceeded = _budget_saved
    set_providers(
        Providers(
            text=_real.text,
            vision=FakeVision(),
            asr=_prev_asr,
            embedding=_EMBED,
            search=_real.search,
        )
    )

    # The sherpa backend's model-free surface: WAV parsing and the format guard.
    # Real decoding needs the wheel and the weights, which belong to the
    # container; what must hold everywhere is that the guard fires before any
    # model load, and that PCM comes out mono, normalised, at the header's rate.
    import io as _io
    import wave as _wave

    from qqbot.providers.sherpa import SherpaAsr, _pcm_from_wav

    def _wav(channels, rate, frames):
        buf = _io.BytesIO()
        with _wave.open(buf, "wb") as w:
            w.setnchannels(channels)
            w.setsampwidth(2)
            w.setframerate(rate)
            w.writeframes(frames)
        return buf.getvalue()

    _mono = _wav(1, 16000, (16384).to_bytes(2, "little", signed=True) * 160)
    _samples, _rate = _pcm_from_wav(_mono)
    assert len(_samples) == 160 and _rate == 16000 and abs(_samples[0] - 0.5) < 1e-3, (
        "wav parse: mono keeps count, rate and scale",
        f"{len(_samples)}@{_rate}",
    )
    _stereo = _wav(2, 24000, (16384).to_bytes(2, "little", signed=True) * 320)
    _samples, _rate = _pcm_from_wav(_stereo)
    assert len(_samples) == 160 and _rate == 24000, (
        "wav parse: stereo folds to mono at the declared rate",
        f"{len(_samples)}@{_rate}",
    )
    _sherpa = SherpaAsr(cfg.backends.asr, budget=BUDGET)
    assert _sherpa.rate_for("sense-voice").units(300.0) == 0.0, (
        "sherpa rate is zero in every direction"
    )
    with pytest.raises(ValueError):
        await _sherpa.transcribe(b"\x02#!SILK_V3", fmt="amr")

    # size cap
    base = len(VISION_CALLS)
    big = ImageRef(
        slot=0, key="b" * 32, url="http://x", size=int(cfg.media.max_image_mb * 1024 * 1024) + 1
    )
    out = await MEDIA.describe_image(big, bot=bot, group_id=GroupId("1"), cfg=cfg)
    assert out.text == "⟦图片⟧" and not out.retryable, (
        "oversized image gets a terminal bare marker",
        repr(out),
    )
    assert len(VISION_CALLS) == base, "oversized image costs nothing"
    _w = MEDIA._img_window(GroupId("1"), cfg.media.max_images_per_min)
    _n = len(_w._hits) if hasattr(_w, "_hits") else None
    await MEDIA.describe_image(big, bot=bot, group_id=GroupId("1"), cfg=cfg)
    assert _n is None or len(_w._hits) == _n, (
        "oversized image spends no rate-window slot",
        str(_n),
    )

    # per-minute image cap
    limited_cfg = cfg.model_copy(
        update={"media": cfg.media.model_copy(update={"max_images_per_min": 2})}
    )
    MEDIA._img_windows.clear()
    outs = []
    for i in range(4):
        r = ImageRef(slot=0, key=f"{i:032d}", url="http://x")
        outs.append(
            (await MEDIA.describe_image(r, bot=bot, group_id=GroupId("9002"), cfg=limited_cfg)).text
        )
    described = sum(1 for o in outs if o)
    assert described == 2, ("image rate limit holds", f"{described} described of 4")

    # unknown-key image (no md5 in the file field) still works, just uncacheable
    r = ImageRef(slot=0, key=None, url="http://x")
    MEDIA._img_windows.clear()
    out = (await MEDIA.describe_image(r, bot=bot, group_id=GroupId("9003"), cfg=cfg)).text
    assert out is not None, ("keyless image still described", str(out))

    # segment parsing keeps text and media interleaved in order
    pm = parse_segments(
        [
            {"type": "text", "data": {"text": "看"}},
            {"type": "image", "data": {"file": "DEADBEEF" * 4 + ".image"}},
            {"type": "text", "data": {"text": "还有这个"}},
            {"type": "record", "data": {"file": "v.amr"}},
        ],
        "999",
    )
    assert pm.render() == "看 ⟦图片⟧ 还有这个 ⟦语音⟧", ("order preserved", repr(pm.render()))
    assert isinstance(pm.refs[1], AudioRef), "audio ref built"
    assert pm.render({0: "⟦图片:猫⟧", 1: "⟦语音:你好⟧"}) == "看 ⟦图片:猫⟧ 还有这个 ⟦语音:你好⟧", (
        "resolved text substituted in place",
        repr(pm.render({0: "⟦图片:猫⟧", 1: "⟦语音:你好⟧"})),
    )

    # A quote contributes its id and no text of its own. What it points at is worked out
    # against the messages the model can actually see - an excerpt fetched over the wire
    # said what was said but not which line said it, and in a group the same words get
    # said twice.
    from qqbot.conversation.prompt import numbered
    from qqbot.conversation.state import ChatMsg as _CM
    from qqbot.conversation.state import GroupState as _GS
    from _fixtures import now_local as _now

    st = _GS(
        group_id=GroupId("5551"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    a = _CM(msg_id="m1", user_id="1", nickname="阿强", text="今天谁来收尾", ts=_now())
    b = _CM(msg_id="m2", user_id="", nickname="", text="我来吧，顺手的事", ts=_now(), is_bot=True)
    c = _CM(msg_id="m3", user_id="2", nickname="小北", text="那就辛苦了", ts=_now(), reply_to="m1")
    d = _CM(msg_id="m4", user_id="2", nickname="小北", text="+1", ts=_now(), reply_to="m2")
    e = _CM(msg_id="m5", user_id="2", nickname="小北", text="这条呢", ts=_now(), reply_to="m0")
    for m in (a, b, c, d, e):
        st.add(m)

    nums, marks = numbered([a, b, c, d, e])
    assert [nums["m1"], nums["m2"], nums["m3"]] == [1, 2, 3], (
        "every line is numbered by its position, the bot's included",
        str(nums),
    )
    assert marks["m3"] == "⟦回复 #1⟧", (
        "a quote points at the number of the line it quotes",
        marks.get("m3"),
    )
    # One mechanism, so quoting the bot is the same marker pointing at the same kind of
    # thing. Its number does land in an assistant turn; clean_reply takes it back off.
    assert marks["m4"] == "⟦回复 #2⟧", ("including a quote of the bot's own line", marks.get("m4"))
    assert marks["m5"] == "⟦回复更早的消息⟧", (
        "a quote of something off screen says so",
        marks.get("m5"),
    )
    assert "m1" not in marks, ("and a message that quotes nothing gets no mark", str(marks))
    # The stamp between number and speaker is the message's own fixed moment - it is
    # what stops the model bridging topics hours apart, and being fixed is what keeps
    # the rendered line byte-identical between turns (cache-safe).
    from _fixtures import fmt_when as _fw

    assert (
        c.render(seq=nums["m3"], quote=marks["m3"])
        == f"#3 ⟦{_fw(c.ts)}⟧ 小北: ⟦回复 #1⟧ 那就辛苦了"
    ), (
        "the mark renders in front of the line, after the time stamp",
        c.render(seq=nums["m3"], quote=marks["m3"]),
    )
    # Bot lines use the reserved display identity too. The marker shares the member
    # legend syntax but is never accepted as a tool target.
    assert b.render(seq=nums["m2"]) == f"#2 ⟦{_fw(b.ts)}⟧ 机器人⟦0⟧: 我来吧，顺手的事", (
        "the bot's line carries its reserved display identity",
        b.render(seq=nums["m2"]),
    )

    # `bot` is threaded through twenty-odd signatures and was annotated in none of them,
    # so a stand-in only had to satisfy whatever the one path under test happened to call.
    # A runtime-checkable Protocol makes the fakes answer the same question as the real
    # adapter, which is the point of typing them structurally rather than by inheritance.
    from qqbot.gateway.botapi import BotApi

    assert all(isinstance(f, BotApi) for f in (FakeBot(), RefreshingBot())), (
        "the bot stand-ins satisfy the protocol the real adapter does"
    )
    assert not isinstance(object(), BotApi), "and something without call_api does not"

    # The number lands in an assistant turn, which is an example the model may follow.
    from qqbot.delivery.output import clean_reply

    assert clean_reply("#7 那我就不抢了") == "那我就不抢了", (
        "a number copied back out of the history is removed",
        repr(clean_reply("#7 那我就不抢了")),
    )
    assert clean_reply("#7 小X: 那我就不抢了") == "那我就不抢了", (
        "with the speaker prefix if it came too",
        repr(clean_reply("#7 小X: 那我就不抢了")),
    )
    assert clean_reply("#1 号选手赢了") == "#1 号选手赢了", (
        "but an ordinary reply is left alone",
        repr(clean_reply("#1 号选手赢了")),
    )
    # The time stamp is part of the imitated format too, with or without the number.
    # Only the stamp is stripped in the bare form: a speaker after it could be the
    # reply's own words, and where the readings collide the guard declines.
    assert clean_reply("#7 ⟦08-30 14:03⟧ 小X: 那我就不抢了") == "那我就不抢了", (
        "a copied stamp goes with the number and speaker",
        repr(clean_reply("#7 ⟦08-30 14:03⟧ 小X: 那我就不抢了")),
    )
    assert clean_reply("⟦08-30 14:03⟧ 那我就不抢了") == "那我就不抢了", (
        "a bare copied stamp is removed",
        repr(clean_reply("⟦08-30 14:03⟧ 那我就不抢了")),
    )
    # The square form is a member's own imitation after defang; quoting it is content.
    assert clean_reply("[08-30 14:03] 那我就不抢了") == "[08-30 14:03] 那我就不抢了", (
        "a square-bracket stamp is text and stays",
        repr(clean_reply("[08-30 14:03] 那我就不抢了")),
    )

    # A model that wants a tool it has not been given writes the call out as text. The
    # group received one of these in full - markup, tool name and search query - as a chat
    # message. Cut rather than unwrapped: what follows the marker is the call's arguments,
    # so stripping only the tags would post the search query as though it were the answer.
    leak = (
        '<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name="web_search">\n'
        '<｜｜DSML｜｜parameter name="query" string="true">第四季 开播 时间'
        "</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>"
    )
    assert clean_reply(leak) == "", (
        "a tool call written as text never reaches the group",
        repr(clean_reply(leak)),
    )
    assert "第四季" not in clean_reply(leak), (
        "and neither do the arguments inside it",
        repr(clean_reply(leak)),
    )
    assert clean_reply("这个我查一下。\n" + leak) == "这个我查一下。", (
        "an answer that came before it survives",
        repr(clean_reply("这个我查一下。\n" + leak)),
    )
    # The half-width form, and the bare tag names other backends use.
    assert clean_reply("<|tool_calls|>whatever") == "", (
        "the half-width form is caught too",
        repr(clean_reply("<|tool_calls|>x")),
    )
    assert clean_reply("好的<tool_call>{}</tool_call>") == "好的", (
        "as are bare tool tags",
        repr(clean_reply("好的<tool_call>{}</tool_call>")),
    )
    # And ordinary text is not touched: a reply may legitimately contain an angle bracket
    # or the word "invoke" without being machinery.
    assert clean_reply("他说 a < b 就行了") == "他说 a < b 就行了", (
        "ordinary angle brackets are left alone",
        repr(clean_reply("他说 a < b 就行了")),
    )

    print()
