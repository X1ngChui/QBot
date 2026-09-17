"""Media path: every segment type, sticker and image caching, content refusals, caps."""
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
from _db import configure_test_database

configure_test_database()
import asyncio


from qqbot.db import init_pool, close_pool, pool, repo
from qqbot.settings import config
from _db import reset
from _stubs import FakeEmbedding

#: One stub for every bundle in this suite.
_EMBED = FakeEmbedding()
from qqbot.core.media import MEDIA, OVERSIZE, Unsettled, _mime
from qqbot.core.segments import (AudioRef, ImageRef, ParsedMessage as _PMcls,
                                 parse_segments)
from qqbot.providers import Providers, VisionModel, build_default, providers, set_providers
from qqbot.providers.base import Rate

fails = []
VISION_CALLS = []


def _PM(refs):
    pm = _PMcls()
    pm.refs = list(refs)
    return pm


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


class FakeVision(VisionModel):
    """A real subclass of the ABC - the code under test cannot tell the difference, and an
    incomplete fake would fail at construction rather than mid-test."""

    name = "fake"

    def rate_for(self, model):
        return Rate("Mtoken", in_miss=1.2, out=7.2, source="fake")

    async def describe(self, data, *, cfg, prompt="", mime="image/jpeg", group_id=None):
        VISION_CALLS.append(len(data))
        return "一只橘猫在键盘上打滚"

    async def aclose(self):
        pass


_real = build_default()
set_providers(Providers(text=_real.text, vision=FakeVision(), asr=_real.asr,
                        embedding=_EMBED, search=_real.search))


class FakeBot:
    self_id = "999"

    async def call_api(self, api, **kw):
        return {}

    async def send_group_msg(self, *, group_id, message):
        return {"message_id": "0"}


async def main():
    await init_pool()
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
    ref = ImageRef(slot=0, sticker=True, key="emoji-1",
              url="http://example/s.gif", summary="动画表情")
    out = await MEDIA.describe_image(ref, bot=bot, group_id="g", cfg=cfg)
    check("a sticker is described, not just labelled",
          out == "⟦表情:一只橘猫在键盘上打滚⟧", str(out))
    check("and it keeps the sticker label", len(VISION_CALLS) == 1, str(VISION_CALLS))
    check("the description is cached under the sticker id",
          await repo.image_cache_get("emoji-1") == "⟦表情:一只橘猫在键盘上打滚⟧")
    out = await MEDIA.describe_image(ref, bot=bot, group_id="g", cfg=cfg)
    check("a repeat sticker costs nothing - this is what makes it affordable",
          len(VISION_CALLS) == 1, str(VISION_CALLS))

    # Unreachable image: the sender's label beats saying nothing at all.
    _fetch, MEDIA._fetch = MEDIA._fetch, lambda url, max_bytes: asyncio.sleep(0, None)
    ref_gone = ImageRef(slot=0, sticker=True, key="emoji-2",
                   url="http://example/gone.gif", summary="动画表情")
    out = await MEDIA.describe_image(ref_gone, bot=bot, group_id="g", cfg=cfg)
    check("an unfetchable sticker falls back to its label", out == "⟦表情:动画表情⟧", str(out))
    MEDIA._fetch = _fetch

    # image: first sight calls the model, second is a cache hit
    base = len(VISION_CALLS)
    key = "a" * 32
    ref = ImageRef(slot=0, key=key, url="http://example/img.jpg")
    out1 = await MEDIA.describe_image(ref, bot=bot, group_id="g", cfg=cfg)
    check("first sight describes", out1 == "⟦图片:一只橘猫在键盘上打滚⟧", str(out1))
    check("first sight costs one vision call", len(VISION_CALLS) == base + 1, str(VISION_CALLS))
    out2 = await MEDIA.describe_image(ref, bot=bot, group_id="g", cfg=cfg)
    check("second sight hits the cache", out2 == out1, str(out2))
    check("cache hit costs no vision call", len(VISION_CALLS) == base + 1, str(VISION_CALLS))
    hits = await pool().fetchval("SELECT hit_count FROM image_cache WHERE key=$1", key)
    check("cache hit counted", hits == 1, str(hits))

    # Every segment type QQ actually sends has to come out as text. markdown was the one
    # that proved this matters: bots post their whole output that way - game results,
    # divination - and with no branch for it the message reached the model empty.
    md = "\n".join([
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
    ])
    out = parse_segments([{"type": "markdown", "data": {"content": md}}], "999").render()
    check("markdown reaches the model as its text",
          "今天的运势是中吉" in out and "【第 3 次抽签】" in out and "签文的一段说明" in out,
          out)
    check("and its markup does not", "mqqapi" not in out and "#####" not in out, out)
    check("an inline image becomes a marker", "⟦图片⟧" in out, out)

    def one(seg):
        return parse_segments([seg], "999").render()

    check("a named emoticon uses its name",
          one({"type": "face", "data": {"id": "425", "raw": {"faceText": "/求放过"}}})
          == "⟦表情:求放过⟧")
    check("a classic emoticon is looked up by id",
          one({"type": "face", "data": {"id": "9", "raw": {}}}) == "⟦表情:大哭⟧")
    check("an unlisted one degrades to the bare marker",
          one({"type": "face", "data": {"id": "99999", "raw": {}}}) == "⟦表情⟧")
    check("dice shows the roll", one({"type": "dice", "data": {"result": "4"}}) == "⟦骰子:4点⟧")
    check("rps maps 2 to scissors, not paper",
          one({"type": "rps", "data": {"result": "2"}}) == "⟦猜拳:剪刀⟧")
    # QQ keeps inventing segment types. Saying something beats dropping the message, and
    # the log line is how the next one gets noticed instead of silently vanishing.
    check("an unknown type still says something",
          one({"type": "keyboard", "data": {"rows": []}}) == "⟦keyboard⟧")

    # A backend that looks at a picture and declines is behaving normally, not failing.
    # Paying to be refused again on every repost is the actual cost, so the outcome is
    # cached like any other.
    from qqbot.core.media import _is_refusal
    check("a content refusal is recognised",
          _is_refusal(Exception("Error code: 400 - InternalError.Algo."
                                "DataInspectionFailed: may contain inappropriate content")))
    check("an ordinary failure is not", not _is_refusal(Exception("connection reset by peer")))

    class Refusing:
        name = "refusing"

        def rate_for(self, model):
            return Rate("Mtoken")

        async def describe(self, data, *, cfg, prompt="", mime="image/jpeg", group_id=None):
            VISION_CALLS.append(len(data))
            raise RuntimeError("400 data_inspection_failed: inappropriate content")

        async def aclose(self):
            pass

    _ok = providers().vision
    set_providers(Providers(text=_real.text, vision=Refusing(),
                            asr=_real.asr, embedding=_EMBED, search=_real.search))
    ref_no = ImageRef(slot=0, key="c" * 32, url="http://example/nope.jpg")
    before = len(VISION_CALLS)
    out = await MEDIA.describe_image(ref_no, bot=bot, group_id="g", cfg=cfg)
    check("a refused image is a terminal bare marker", out == "⟦图片⟧", str(out))
    check("and the refusal cost one call", len(VISION_CALLS) == before + 1)
    out = await MEDIA.describe_image(ref_no, bot=bot, group_id="g", cfg=cfg)
    check("reposting it costs nothing more", len(VISION_CALLS) == before + 1,
          str(VISION_CALLS))
    check("the cached outcome still reads as not received",
          out == "⟦图片⟧", str(out))
    set_providers(Providers(text=_real.text, vision=_ok, asr=_real.asr,
                            embedding=_EMBED, search=_real.search))

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

    set_providers(Providers(text=_real.text, vision=FakeVision(),
                            asr=_real.asr, embedding=_EMBED, search=_real.search))
    _fetch3 = MEDIA._fetch

    async def only_fresh(url, max_bytes):
        # The original link is dead; the one get_image hands back works.
        return b"y" * 2048 if "refreshed" in url else None

    MEDIA._fetch = only_fresh
    seen0 = len(VISION_CALLS)
    ref_old = ImageRef(slot=0, key="d" * 32, url="http://example/expired.jpg",
                  file="ABCDEF.jpg")
    out = await MEDIA.describe_image(ref_old, bot=RefreshingBot(), group_id="g", cfg=cfg)
    check("an expired link is refreshed rather than given up on",
          out == "⟦图片:一只橘猫在键盘上打滚⟧" and RefreshingBot.calls == 1, str(out))
    check("and the picture did reach the model", len(VISION_CALLS) == seen0 + 1)
    MEDIA._fetch = _fetch3

    # A marketplace sticker arrives as a directory link whose redirect target is
    # often absent; the same directory serves a 300x300 PNG, so that is asked first.
    seen_links = []

    async def recording_fetch(url, max_bytes):
        seen_links.append(url)
        return b"z" * 2048 if url.endswith("/300x300.png") else None

    MEDIA._fetch = recording_fetch
    _sticker = ImageRef(slot=0, key="e" * 32,
                        url="https://gxh.vip.qq.com/club/item/parcel/item/74/" + "e" * 32)
    check("a marketplace sticker is fetched as its 300x300 PNG",
          await MEDIA._bytes(_sticker, bot=RefreshingBot(), max_bytes=1 << 20) is not None
          and seen_links[0].endswith("/300x300.png"), str(seen_links))
    # The shape the protocol side actually sends names the raw GIF inside the
    # directory; the PNG sits beside it and is still asked for first.
    seen_links.clear()
    _gif = ImageRef(slot=0, key="h" * 32,
                    url="https://gxh.vip.qq.com/club/item/parcel/item/74/" + "f" * 32
                        + "/raw300.gif")
    check("a sticker linked by its raw GIF is fetched as the PNG beside it",
          await MEDIA._bytes(_gif, bot=RefreshingBot(), max_bytes=1 << 20) is not None
          and seen_links[0].endswith("/" + "f" * 32 + "/300x300.png"), str(seen_links))
    MEDIA._fetch = _fetch3

    # A picture the platform can no longer serve does not fail get_image, it hangs;
    # the fetch has its own short deadline, and the picture is then remembered as
    # unreadable so the next reply looking at it does not wait it out again.
    _gw = config().default.gateway
    _saved_to, _gw.protocol_call_timeout_sec = _gw.protocol_call_timeout_sec, 0.2

    class HangingBot:
        self_id = "999"
        calls = 0

        async def call_api(self, api, **kw):
            HangingBot.calls += 1
            await asyncio.sleep(5)
            return {}

    async def dead_link(url, max_bytes):
        return None

    MEDIA._fetch = dead_link
    _dead = ImageRef(slot=0, key="f" * 32, url="http://example/gone.jpg", file="GONE.jpg")
    _t = asyncio.get_event_loop().time()
    check("a hanging get_image is given up on quickly",
          await MEDIA._bytes(_dead, bot=HangingBot(), max_bytes=1 << 20) is None
          and asyncio.get_event_loop().time() - _t < 2, str(HangingBot.calls))
    await MEDIA._bytes(_dead, bot=HangingBot(), max_bytes=1 << 20)
    check("and an unreadable picture is not retried for a while", HangingBot.calls == 1,
          str(HangingBot.calls))
    # Two readers at once share one flight: the second must not spend the
    # deadline again while the first is still finding out the picture is dead.
    MEDIA._unreadable.clear()
    HangingBot.calls = 0
    _dead2 = ImageRef(slot=0, key="e" * 32, url="http://example/gone2.jpg", file="GONE2.jpg")
    _pair = await asyncio.gather(
        MEDIA._bytes(_dead2, bot=HangingBot(), max_bytes=1 << 20),
        MEDIA._bytes(_dead2, bot=HangingBot(), max_bytes=1 << 20))
    check("concurrent readers of one picture share a single fetch",
          HangingBot.calls == 1 and _pair == [None, None], str(HangingBot.calls))
    check("the flight registry is empty once the fetch ends", not MEDIA._fetching)
    # An oversize picture is a verdict, not a failure: it is not marked unreadable.
    async def too_big(url, max_bytes):
        return OVERSIZE
    MEDIA._fetch = too_big
    _big = ImageRef(slot=0, key="d" * 32, url="http://example/big.jpg", file="BIG.jpg")
    _got = await MEDIA._bytes(_big, bot=HangingBot(), max_bytes=1 << 20)
    check("an oversize picture answers the sentinel and is not marked unreadable",
          _got is OVERSIZE and "d" * 32 not in MEDIA._unreadable)
    MEDIA._unreadable.clear()
    MEDIA._fetch = _fetch3
    _gw.protocol_call_timeout_sec = _saved_to

    check("mime sniffed from bytes beats the file name",
          _mime(b"GIF89a....", "x.image") == "image/gif"
          and _mime(b"\x89PNG\r\n", None) == "image/png"
          and _mime(b"RIFF....WEBPVP8 ", "a.jpg") == "image/webp"
          and _mime(b"\xff\xd8\xff", "a.png") == "image/jpeg")
    check("mime falls back to the suffix, and .image means jpeg",
          _mime(b"????", "a.image") == "image/jpeg" and _mime(b"????", "a.bmp") == "image/bmp")

    # Voice never takes the shortcut pictures take. The stored file (and the CDN
    # original) is SILK v3 wearing an .amr suffix, and SILK sent raw draws a
    # politely empty transcript - a perfectly clear clip the bot claims it cannot
    # hear. Only get_record's WAV is decodable, so
    # transcription must ignore the local file and the URL even when both exist.
    from qqbot.core.media import _audio_seconds, _byte_cap
    check("audio arithmetic is WAV-only now",
          abs(_audio_seconds(320000) - 10.0) < 0.1 and _byte_cap(300) == 9600000,
          f"{_audio_seconds(320000)} {_byte_cap(300)}")

    import base64 as _b64
    from qqbot.core.segments import AudioRef as _AR
    from qqbot.providers import AsrModel as _AsrABC
    from qqbot.providers.base import Rate as _Rate

    _SILK = b"\x02#!SILK_V3" + b"\x00" * 64
    _WAV = b"RIFFfakewavdata" * 40
    _asr_seen = {}

    class CapturingAsr(_AsrABC):
        name = "capturing"

        def rate_for(self, model):
            return _Rate("second", per_unit=0.0)

        async def transcribe(self, data, *, cfg, fmt="wav", seconds=None, group_id=None):
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
    MEDIA._local = staticmethod(lambda p, m: _SILK)   # the trap, armed
    _prev_asr = providers().asr
    set_providers(Providers(text=_real.text, vision=FakeVision(),
                            asr=CapturingAsr(), embedding=_EMBED, search=_real.search))
    _vref = _AR(slot=0, file="v.amr",
                url="http://cdn/v.amr")
    _vout = await MEDIA.transcribe(_vref, bot=VoiceBot(), group_id="g9", cfg=cfg)
    check("transcription goes through get_record even with a local file on offer",
          VoiceBot.record_calls == 1)
    check("what reaches the ASR backend is the transcoded WAV, never SILK",
          _asr_seen.get("data") == _WAV and _asr_seen.get("fmt") == "wav",
          str(_asr_seen.get("data", b"")[:12]))
    check("and the transcript comes back marked", _vout == "⟦语音:明天一起去吃饭⟧", repr(_vout))
    # Voice resolves on the arrival schedule now, like pictures: the free pass
    # plus media_now must transcribe, so extraction reads text even in groups
    # the bot never answers.
    _pm_v = _PM([_AR(slot=0, file="v2.amr", url="http://cdn/v2.amr")])
    _res_v = await MEDIA.resolve(_pm_v, bot=VoiceBot(), group_id="g9", cfg=cfg)
    check("a voice clip transcribes on arrival, before any reply",
          _res_v.get(0) == "⟦语音:明天一起去吃饭⟧", repr(_res_v))
    MEDIA._local = _local_saved

    # Local ASR is CPU work, not provider spending. Even with the CNY budget forced
    # to exhausted, transcription continues; its own per-group gate and bounded
    # global recognizer queue are the admission controls.
    from qqbot.core import media as _media_mod

    async def _true(cap):
        return True

    _budget_saved = _media_mod.BUDGET.exceeded
    _media_mod.BUDGET.exceeded = _true
    MEDIA._asr_windows.clear()
    _vout = await MEDIA.transcribe(_AR(slot=0, file="free.amr"), bot=VoiceBot(),
                                   group_id="g10", cfg=cfg)
    check("local ASR transcribes straight through an exhausted CNY budget",
          _vout == "⟦语音:明天一起去吃饭⟧", repr(_vout))
    _media_mod.BUDGET.exceeded = _budget_saved
    set_providers(Providers(text=_real.text, vision=FakeVision(),
                            asr=_prev_asr, embedding=_EMBED, search=_real.search))

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
    check("wav parse: mono keeps count, rate and scale",
          len(_samples) == 160 and _rate == 16000
          and abs(_samples[0] - 0.5) < 1e-3, f"{len(_samples)}@{_rate}")
    _stereo = _wav(2, 24000, (16384).to_bytes(2, "little", signed=True) * 320)
    _samples, _rate = _pcm_from_wav(_stereo)
    check("wav parse: stereo folds to mono at the declared rate",
          len(_samples) == 160 and _rate == 24000, f"{len(_samples)}@{_rate}")
    _sherpa = SherpaAsr()
    check("sherpa rate is zero in every direction",
          _sherpa.rate_for("sense-voice").units(300.0) == 0.0)
    try:
        await _sherpa.transcribe(b"\x02#!SILK_V3", cfg=cfg.capabilities.asr, fmt="amr")
        check("sherpa refuses non-wav input", False, "no exception")
    except ValueError:
        check("sherpa refuses non-wav input", True)

    # size cap
    base = len(VISION_CALLS)
    big = ImageRef(slot=0, key="b" * 32, url="http://x",
                   size=int(cfg.capabilities.vision.max_image_mb * 1024 * 1024) + 1)
    out = await MEDIA.describe_image(big, bot=bot, group_id="g", cfg=cfg)
    check("oversized image gets a terminal bare marker",
          out == "⟦图片⟧" and not isinstance(out, Unsettled), repr(out))
    check("oversized image costs nothing", len(VISION_CALLS) == base)
    _w = MEDIA._img_window("g", cfg.capabilities.vision.max_images_per_min)
    _n = len(_w._hits) if hasattr(_w, "_hits") else None
    await MEDIA.describe_image(big, bot=bot, group_id="g", cfg=cfg)
    check("oversized image spends no rate-window slot",
          _n is None or len(_w._hits) == _n, str(_n))

    # per-minute image cap
    cfg.capabilities.vision.max_images_per_min = 2
    MEDIA._img_windows.clear()
    outs = []
    for i in range(4):
        r = ImageRef(slot=0, key=f"{i:032d}", url="http://x")
        outs.append(await MEDIA.describe_image(r, bot=bot, group_id="g2", cfg=cfg))
    described = sum(1 for o in outs if o)
    check("image rate limit holds", described == 2, f"{described} described of 4")

    # unknown-key image (no md5 in the file field) still works, just uncacheable
    r = ImageRef(slot=0, key=None, url="http://x")
    MEDIA._img_windows.clear()
    cfg.capabilities.vision.max_images_per_min = 6
    out = await MEDIA.describe_image(r, bot=bot, group_id="g3", cfg=cfg)
    check("keyless image still described", out is not None, str(out))

    # segment parsing keeps text and media interleaved in order
    pm = parse_segments([
        {"type": "text", "data": {"text": "看"}},
        {"type": "image", "data": {"file": "DEADBEEF" * 4 + ".image"}},
        {"type": "text", "data": {"text": "还有这个"}},
        {"type": "record", "data": {"file": "v.amr"}},
    ], "999")
    check("order preserved", pm.render() == "看 ⟦图片⟧ 还有这个 ⟦语音⟧", repr(pm.render()))
    check("audio ref built", isinstance(pm.refs[1], AudioRef))
    check("resolved text substituted in place",
          pm.render({0: "⟦图片:猫⟧", 1: "⟦语音:你好⟧"}) == "看 ⟦图片:猫⟧ 还有这个 ⟦语音:你好⟧",
          repr(pm.render({0: "⟦图片:猫⟧", 1: "⟦语音:你好⟧"})))

    # A quote contributes its id and no text of its own. What it points at is worked out
    # against the messages the model can actually see - an excerpt fetched over the wire
    # said what was said but not which line said it, and in a group the same words get
    # said twice.
    from qqbot.core.prompt import numbered
    from qqbot.core.state import ChatMsg as _CM, GroupState as _GS
    from qqbot.util import now_local as _now

    st = _GS(group_id="5551")
    a = _CM(msg_id="m1", user_id="1", nickname="阿强", text="今天谁来收尾", ts=_now())
    b = _CM(msg_id="m2", user_id="", nickname="", text="我来吧，顺手的事", ts=_now(),
            is_bot=True)
    c = _CM(msg_id="m3", user_id="2", nickname="小北", text="那就辛苦了", ts=_now(),
            reply_to="m1")
    d = _CM(msg_id="m4", user_id="2", nickname="小北", text="+1", ts=_now(), reply_to="m2")
    e = _CM(msg_id="m5", user_id="2", nickname="小北", text="这条呢", ts=_now(),
            reply_to="m0")
    for m in (a, b, c, d, e):
        st.add(m)

    nums, marks = numbered([a, b, c, d, e])
    check("every line is numbered by its position, the bot's included",
          [nums["m1"], nums["m2"], nums["m3"]] == [1, 2, 3], str(nums))
    check("a quote points at the number of the line it quotes",
          marks["m3"] == "⟦回复 #1⟧", marks.get("m3"))
    # One mechanism, so quoting the bot is the same marker pointing at the same kind of
    # thing. Its number does land in an assistant turn; clean_reply takes it back off.
    check("including a quote of the bot's own line", marks["m4"] == "⟦回复 #2⟧",
          marks.get("m4"))
    check("a quote of something off screen says so",
          marks["m5"] == "⟦回复更早的消息⟧", marks.get("m5"))
    check("and a message that quotes nothing gets no mark", "m1" not in marks, str(marks))
    # The stamp between number and speaker is the message's own fixed moment - it is
    # what stops the model bridging topics hours apart, and being fixed is what keeps
    # the rendered line byte-identical between turns (cache-safe).
    from qqbot.util import fmt_when as _fw
    check("the mark renders in front of the line, after the time stamp",
          c.render(seq=nums["m3"], quote=marks["m3"])
          == f"#3 ⟦{_fw(c.ts)}⟧ 小北: ⟦回复 #1⟧ 那就辛苦了",
          c.render(seq=nums["m3"], quote=marks["m3"]))
    # Bot lines use the reserved display identity too. The marker shares the member
    # legend syntax but is never accepted as a tool target.
    check("the bot's line carries its reserved display identity",
          b.render(seq=nums["m2"])
          == f"#2 ⟦{_fw(b.ts)}⟧ 机器人⟦0⟧: 我来吧，顺手的事",
          b.render(seq=nums["m2"]))

    # `bot` is threaded through twenty-odd signatures and was annotated in none of them,
    # so a stand-in only had to satisfy whatever the one path under test happened to call.
    # A runtime-checkable Protocol makes the fakes answer the same question as the real
    # adapter, which is the point of typing them structurally rather than by inheritance.
    from qqbot.core.botapi import BotApi
    check("the bot stand-ins satisfy the protocol the real adapter does",
          all(isinstance(f, BotApi) for f in (FakeBot(), RefreshingBot())))
    check("and something without call_api does not", not isinstance(object(), BotApi))

    # The number lands in an assistant turn, which is an example the model may follow.
    from qqbot.core.output import clean_reply
    check("a number copied back out of the history is removed",
          clean_reply("#7 那我就不抢了") == "那我就不抢了",
          repr(clean_reply("#7 那我就不抢了")))
    check("with the speaker prefix if it came too",
          clean_reply("#7 小X: 那我就不抢了") == "那我就不抢了",
          repr(clean_reply("#7 小X: 那我就不抢了")))
    check("but an ordinary reply is left alone",
          clean_reply("#1 号选手赢了") == "#1 号选手赢了", repr(clean_reply("#1 号选手赢了")))
    # The time stamp is part of the imitated format too, with or without the number.
    # Only the stamp is stripped in the bare form: a speaker after it could be the
    # reply's own words, and where the readings collide the guard declines.
    check("a copied stamp goes with the number and speaker",
          clean_reply("#7 ⟦08-30 14:03⟧ 小X: 那我就不抢了") == "那我就不抢了",
          repr(clean_reply("#7 ⟦08-30 14:03⟧ 小X: 那我就不抢了")))
    check("a bare copied stamp is removed",
          clean_reply("⟦08-30 14:03⟧ 那我就不抢了") == "那我就不抢了",
          repr(clean_reply("⟦08-30 14:03⟧ 那我就不抢了")))
    # The square form is a member's own imitation after defang; quoting it is content.
    check("a square-bracket stamp is text and stays",
          clean_reply("[08-30 14:03] 那我就不抢了") == "[08-30 14:03] 那我就不抢了",
          repr(clean_reply("[08-30 14:03] 那我就不抢了")))

    # A model that wants a tool it has not been given writes the call out as text. The
    # group received one of these in full - markup, tool name and search query - as a chat
    # message. Cut rather than unwrapped: what follows the marker is the call's arguments,
    # so stripping only the tags would post the search query as though it were the answer.
    leak = ('<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name="web_search">\n'
            '<｜｜DSML｜｜parameter name="query" string="true">第四季 开播 时间'
            '</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>')
    check("a tool call written as text never reaches the group",
          clean_reply(leak) == "", repr(clean_reply(leak)))
    check("and neither do the arguments inside it",
          "第四季" not in clean_reply(leak), repr(clean_reply(leak)))
    check("an answer that came before it survives",
          clean_reply("这个我查一下。\n" + leak) == "这个我查一下。",
          repr(clean_reply("这个我查一下。\n" + leak)))
    # The half-width form, and the bare tag names other backends use.
    check("the half-width form is caught too",
          clean_reply("<|tool_calls|>whatever") == "", repr(clean_reply("<|tool_calls|>x")))
    check("as are bare tool tags",
          clean_reply("好的<tool_call>{}</tool_call>") == "好的",
          repr(clean_reply("好的<tool_call>{}</tool_call>")))
    # And ordinary text is not touched: a reply may legitimately contain an angle bracket
    # or the word "invoke" without being machinery.
    check("ordinary angle brackets are left alone",
          clean_reply("他说 a < b 就行了") == "他说 a < b 就行了",
          repr(clean_reply("他说 a < b 就行了")))

    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
