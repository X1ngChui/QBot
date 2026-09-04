"""Reading what a message points at: pictures, voice, forwards, who was @-ed.

Everything here costs something - an HTTP fetch, an API call, a model call - which is the
line between this file and segments.py. Two rules shape it:

1. **Everything but voice resolves on arrival.** Who was @-ed, what was quoted and
   what was forwarded cost nothing, and an unresolved mention would archive as a bare
   account number - the thing the prompt is most careful to keep out. Pictures resolve
   on arrival too: the upload that files them with the vision backend is free and wants
   the freshest link, and the describing call is cached per unique picture, rate
   limited, and behind the daily budget cap - so paying at arrival is the same money at
   better latency, and it reaches groups the bot never speaks in. Voice alone waits for
   a reply: billed per second, never repeated, and only ever discussed right after
   being sent.

   Late resolution stays possible in reach. A received image link carries an rkey that
   expires in about two hours, but the file id does not: get_image trades it for a
   fresh link at any time, which is how the QQ client itself still shows pictures from
   days ago. So _bytes tries the shared data directory, then the link, then get_image -
   and a picture stays readable long after the link in the original message stopped
   working.
2. **Nested content gets the free budget and no more.** A quoted or forwarded message has
   its @s resolved to names and reuses any description already paid for, because a bare
   account number is the thing the prompt works hardest to keep out and a picture quoted
   from earlier in the same group is exactly the one already described. What it will not
   do is spend: a first sighting of a picture, or a voice clip, stays a placeholder down
   there, so one forwarded album cannot trigger dozens of vision calls. A forward nested
   inside a quote is left alone for the same reason - free per hop, unbounded in hops.

Raw media never touches disk: memory -> API -> discarded, only text and cache keys are
kept.
"""

from __future__ import annotations

import asyncio
import base64
import logging
import os
from pathlib import Path

import httpx

from ..db import repo
from ..providers import providers
from ..providers.openai_compat import NEVER_BILLED
from .budget import BUDGET
from ..settings import Settings, ptext
from ..services import UnknownAccount
from ..util import why
from .botapi import BotApi
from .members import MEMBERS
from .output import strip_markdown
from .retrieval import directory
from .ratelimit import SlidingWindow
from .segments import (
    AtRef,
    AudioRef,
    ForwardRef,
    ImageRef,
    ParsedMessage,
    Ref,
    parse_segments,
    segments_of,
)

#: How many entries of a merged forward to render, and how much of each.
FORWARD_NODES = 6
FORWARD_CHARS = 40

log = logging.getLogger("qqbot.media")

NAPCAT_DATA_DIR = os.getenv("NAPCAT_DATA_DIR", "/app/napcat_data")

#: Bytes per second of 16 kHz 16-bit mono WAV, the only audio this module ever sends
#: anywhere: both the size cap (config states seconds) and the billed duration derive
#: from the byte count through it. What QQ actually stores is SILK v3 at ~1600 B/s,
#: but that never reaches the ASR backend - see transcribe.
_WAV_BYTES_PER_SEC = 32000


def _audio_seconds(n_bytes: int) -> float:
    return n_bytes / _WAV_BYTES_PER_SEC


def _byte_cap(max_seconds: int) -> int:
    return max_seconds * _WAV_BYTES_PER_SEC


class Unsettled(str):
    """A fallback standing in for paid content not yet obtained.

    Renders like any resolved text - it *is* the marker's fallback wording - but tells
    the pipeline the slot is not final: the describing call was rate limited, behind
    the daily cap, or failed transiently, and paying again later may well succeed.
    Terminal outcomes (a real description, a cached verdict, a backend refusal) come
    back as plain str; MediaProcessor.settled is the reader, and ChatMsg.pending is
    what hangs on the verdict. Without the distinction, a transient failure would
    clear pending and become permanent: a burst of stickers that exhausts the
    rate window could never be described afterwards, money and quota available.
    """

#: Markers for "this backend will never describe this picture", as opposed to a transient
#: failure worth retrying. Matched on the message because the wording is the backend's own
#: and the ABC deliberately does not model provider error taxonomies.
_REFUSAL_MARKERS = (
    "data_inspection_failed",
    "content_policy",
    "content_filter",
    "inappropriate",
    "invalid_image",
    "unsupported image",
)


def _is_refusal(exc: Exception) -> bool:
    text = str(exc).lower()
    return any(m in text for m in _REFUSAL_MARKERS)

class MediaProcessor:
    def __init__(self) -> None:
        self._http: httpx.AsyncClient | None = None
        self._img_windows: dict[str, SlidingWindow] = {}
        #: Describe calls in the air, by image key - the single-flight registry.
        self._describing: dict[str, asyncio.Task] = {}
        #: And ASR calls in the air, by clip file id - same rule, dearer stakes:
        #: voice has no result cache, so a duplicate flight is a duplicate bill.
        self._transcribing: dict[str, asyncio.Task] = {}

    def _client(self) -> httpx.AsyncClient:
        if self._http is None:
            self._http = httpx.AsyncClient(timeout=20.0, follow_redirects=True)
        return self._http

    async def close(self) -> None:
        if self._http is not None:
            await self._http.aclose()
            self._http = None

    def _img_window(self, group_id: str, limit: int) -> SlidingWindow:
        w = self._img_windows.get(group_id)
        if w is None:
            w = SlidingWindow(limit)
            self._img_windows[group_id] = w
        w.limit = limit
        return w

    # -- bytes: always in memory, never written to disk --------------------
    async def _fetch(self, url: str, max_bytes: int) -> bytes | None:
        try:
            async with self._client().stream("GET", url) as r:
                r.raise_for_status()
                clen = int(r.headers.get("content-length") or 0)
                if clen and clen > max_bytes:
                    log.info("media over size limit, skipped: %d > %d", clen, max_bytes)
                    return None
                buf = bytearray()
                async for chunk in r.aiter_bytes():
                    buf.extend(chunk)
                    if len(buf) > max_bytes:
                        log.info("media over size limit mid-stream, skipped")
                        return None
                return bytes(buf)
        except Exception as e:
            log.warning("media download failed %s: %s", url[:80], why(e))
            return None

    @staticmethod
    def _local(napcat_path: str | None, max_bytes: int) -> bytes | None:
        """Read through the QQ data directory both containers mount.

        NapCat hands over an absolute path inside its own container; the same file is
        visible here under NAPCAT_DATA_DIR, which avoids a download entirely.
        """
        if not napcat_path:
            return None
        marker = "/.config/QQ/"
        if marker not in napcat_path:
            return None
        p = Path(NAPCAT_DATA_DIR) / napcat_path.split(marker, 1)[1]
        try:
            if p.is_file() and p.stat().st_size <= max_bytes:
                return p.read_bytes()
        except OSError as e:
            log.debug("local media read failed %s: %s", p, why(e))
        return None

    # -- per-kind resolution ----------------------------------------------
    async def _bytes(self, ref: Ref, *, bot, max_bytes: int) -> bytes | None:
        """Fetch the picture itself, by whichever route answers."""
        data = self._local(ref.path, max_bytes)
        if data is None and ref.url:
            data = await self._fetch(ref.url, max_bytes)
        if data is None and ref.file:
            try:
                info = await bot.call_api("get_image", file=ref.file)
                data = self._local(info.get("file"), max_bytes)
                if data is None and info.get("url"):
                    data = await self._fetch(info["url"], max_bytes)
            except Exception as e:
                log.warning("get_image failed: %s", why(e))
        return data

    @staticmethod
    async def cached(ref: ImageRef) -> str | None:
        """A description already paid for.

        Lives here rather than on the ref because segments.py knows nothing about a
        database - that is the line between the two files, and a cache lookup is on this
        side of it. It is the free half of the arrival pass, and what makes a picture
        quoted from earlier in the same group readable without spending anything.
        """
        return await repo.image_cache_get(ref.key) if ref.key else None

    async def ensure_uploaded(self, ref: ImageRef, *, bot: BotApi, group_id: str,
                              cfg: Settings) -> str | None:
        """File this picture with the vision backend, once, and remember where.

        The upload itself is free and the id is what lets a reply prompt carry the
        original pixels instead of a one-line description. Runs on arrival, while the
        message's download link is still fresh; the id lands both on the ref (for this
        process) and in image_cache (for reposts and for the message's later turns in
        the window). A backend that keeps no files answers None once and this feature
        simply stays off.
        """
        if not cfg.llm.text.reads_images:
            # Nothing would ever reference the id: the reply model cannot read file
            # blocks, and the archive runs on descriptions.
            return None
        if ref.file_id:
            return ref.file_id
        if not ref.key:
            return None
        cached = await repo.image_cache_file(ref.key)
        if cached:
            ref.file_id = cached
            return cached
        vcfg = cfg.llm.vision
        max_bytes = int(vcfg.max_image_mb * 1024 * 1024)
        if ref.size and ref.size > max_bytes:
            return None
        data = await self._bytes(ref, bot=bot, max_bytes=max_bytes)
        if not data:
            return None
        # The stored format matters to the backend (a GIF's joke is its motion), and the
        # segment's file name is the only place the format is stated.
        suffix = Path(ref.file or "").suffix.lower().lstrip(".")
        mime = f"image/{'jpeg' if suffix in ('', 'jpg') else suffix}"
        try:
            fid = await providers().vision.upload(data, cfg=vcfg, mime=mime)
        except Exception as e:
            log.warning("image upload failed: %s", why(e))
            return None
        if fid:
            ref.file_id = fid
            await repo.image_cache_set_file(ref.key, fid)
        return fid

    async def describe_image(self, ref: ImageRef, *, bot: BotApi, group_id: str,
                             cfg: Settings) -> str | None:
        """Describe a picture, whether it arrived as a photo or as a sticker.

        Single-flight per picture: a deliberating describe runs 10-20s, and in that
        window the arrival task, the reply's settle and the backlog can all want the
        same key - so late callers await the call already in the air instead of
        starting (and paying for) their own. Nothing cancels the flight; whoever
        started it persists the result through the cache for everyone after.

        Stickers go to the model too, never short-circuited on the summary the sender's
        client supplies: for a custom sticker that summary is a placeholder naming the
        category rather than saying what is drawn - and the joke in a sticker is the
        drawing. This is affordable because stickers repeat: the cache keys on emoji_id,
        so each distinct sticker is described once and every later use is free. The
        summary stays as the fallback for when the image cannot be fetched.
        """
        if ref.key and (flight := self._describing.get(ref.key)) is not None:
            return await flight
        if not ref.key:
            return await self._describe_once(ref, bot=bot, group_id=group_id, cfg=cfg)
        task = asyncio.create_task(
            self._describe_once(ref, bot=bot, group_id=group_id, cfg=cfg))
        self._describing[ref.key] = task
        # Popped when the FLIGHT ends, not when this awaiter does: a cancelled
        # awaiter must not unregister a flight still in the air, or the next
        # caller starts (and pays for) a duplicate. The callback also keeps the
        # only strong reference alive - asyncio holds tasks weakly.
        task.add_done_callback(lambda _t, k=ref.key: self._describing.pop(k, None))
        return await task

    async def _describe_once(self, ref: ImageRef, *, bot: BotApi, group_id: str,
                             cfg: Settings) -> str | None:
        vcfg = cfg.llm.vision
        label = "表情" if ref.sticker else "图片"
        fallback = f"[{label}:{ref.summary}]" if ref.summary else None

        if ref.key:
            cached = await repo.image_cache_get(ref.key)
            if cached:
                return cached

        # Transient turn-aways return Unsettled: the same fallback text, but marked
        # retryable so the pipeline keeps the message's pending work alive. Only the
        # oversize verdict below is terminal - a picture does not shrink.
        retry_later = Unsettled(fallback) if fallback else None

        if not self._img_window(group_id, vcfg.max_images_per_min).take():
            log.info("image understanding rate limited, skipped (%s)", group_id)
            return retry_later

        # Understanding now happens on arrival rather than behind a reply, so the daily
        # cap has to be asked here - there is no upstream gate on this path.
        if await BUDGET.exceeded(cfg.budget.daily_cny_cap):
            return retry_later

        max_bytes = int(vcfg.max_image_mb * 1024 * 1024)
        if ref.size and ref.size > max_bytes:
            return fallback

        data = await self._bytes(ref, bot=bot, max_bytes=max_bytes)
        if not data:
            return retry_later

        try:
            desc = await providers().vision.describe(
                data, cfg=vcfg, prompt=ptext("describe_image"), group_id=group_id)
        except Exception as e:
            if _is_refusal(e):
                # The backend looked and declined - its content filter, not a fault here.
                # Cache the outcome so the same picture is not fetched and paid for on
                # every repost, and keep it at info: the error ring is what the daily
                # report reads, and a refusal that recurs daily would crowd out the
                # failures someone actually has to act on.
                log.info("vision backend declined an image; recording it as unseen")
                if ref.key:
                    await repo.image_cache_put(ref.key, fallback or f"[{label}]")
                return fallback
            log.warning("vision model call failed: %s", why(e))
            return retry_later
        if not desc:
            return retry_later
        # The vision backend writes for a reader, so it bolds things. Left in, every
        # picture puts ** in the transcript the model is imitating, against a persona whose
        # first rule is not to use Markdown.
        desc = f"[{label}:{strip_markdown(desc)}]"
        if ref.key:
            await repo.image_cache_put(ref.key, desc)
        return desc

    async def inspect(self, ref: ImageRef, *, question: str, bot: BotApi,
                      group_id: str, cfg: Settings) -> str | None:
        """Look at one picture again, with a specific question.

        The archival describe is a single sentence; when someone asks for a detail
        beyond it ("what does the third line say"), this is the reply loop's paid
        way to reopen its eyes. Uncached on purpose - the answer is question-shaped,
        not picture-shaped - and unwindowed: what bounds it is the per-reply budget
        the tool loop already runs inside (the describe call books itself into the
        ambient scope), plus the engine's duplicate-call suppression.
        """
        vcfg = cfg.llm.vision
        data = await self._bytes(ref, bot=bot,
                                 max_bytes=int(vcfg.max_image_mb * 1024 * 1024))
        if not data:
            return None
        try:
            answer = await providers().vision.describe(
                data, cfg=vcfg, prompt=ptext("inspect_image") + "\n" + question.strip(),
                group_id=group_id)
        except Exception as e:
            log.warning("inspect_image call failed: %s", why(e))
            return None
        return " ".join(strip_markdown(answer).split()) or None

    async def transcribe(self, ref: AudioRef, *, bot: BotApi, group_id: str,
                         cfg: Settings) -> str | None:
        """Single-flight per clip, like describe_image per picture: with pending
        kept alive across turns, a slow ASR call (timeout 60s) can outlive the 25s
        paid wait, and the next turn's backlog pass would otherwise start - and
        pay for - a second identical call. Voice has no result cache to fall back
        on, so the in-flight registry is the only thing standing between a slow
        backend and double billing."""
        key = ref.file or ref.url or ref.path
        if key and (flight := self._transcribing.get(key)) is not None:
            return await flight
        if not key:
            return await self._transcribe_once(ref, bot=bot, group_id=group_id, cfg=cfg)
        task = asyncio.create_task(
            self._transcribe_once(ref, bot=bot, group_id=group_id, cfg=cfg))
        self._transcribing[key] = task
        # Same rule as describe_image's registry: pop when the flight ends, never
        # when an awaiter does.
        task.add_done_callback(lambda _t, k=key: self._transcribing.pop(k, None))
        return await task

    async def _transcribe_once(self, ref: AudioRef, *, bot: BotApi, group_id: str,
                               cfg: Settings) -> str | None:
        acfg = cfg.llm.asr
        # Never the stored file, never the CDN link: both hold QQ's native SILK v3
        # whatever the ".amr" suffix claims (magic '#!SILK_V3'), and SILK bytes labelled
        # amr get a polite empty transcript from the ASR backend - a perfectly clear
        # clip the bot claims it cannot hear.
        # get_record is the one source of decodable audio: NapCat
        # transcodes to 16 kHz mono WAV, and the cap is computed for WAV (~20x denser).
        wav_cap = _byte_cap(acfg.max_audio_sec)
        try:
            info = await bot.call_api("get_record", file=ref.file or "", out_format="wav")
        except Exception as e:
            log.warning("get_record failed: %s", why(e))
            info = {}
        data = None
        if info.get("base64"):
            raw = base64.b64decode(str(info["base64"]).split(",", 1)[-1])
            data = raw if len(raw) <= wav_cap else None
        if data is None:
            data = self._local(info.get("file"), wav_cap)
        if data is None and info.get("url"):
            data = await self._fetch(info["url"], wav_cap)
        if not data:
            return None

        try:
            text = await providers().asr.transcribe(
                data, cfg=acfg, fmt="wav", seconds=_audio_seconds(len(data)),
                group_id=group_id,
            )
        except NEVER_BILLED as e:
            # The request provably cost nothing (never sent, or refused before
            # processing), so the one-paid-attempt rule below does not apply: a
            # network blip must not abandon a clip a free retry would rescue.
            log.warning("ASR call failed before billing, retryable: %s", why(e))
            return None
        except Exception as e:
            # Terminal, not retryable: a failed attempt may already have billed the
            # clip's full duration (the vendor charges per second on arrival), and
            # with no result cache every later turn's retry would bill it again -
            # a 60s clip against a slow backend re-charging on every reply until
            # the message evicts. One paid attempt per clip; the fetch failures
            # above stay retryable because retrying them is free.
            log.warning("ASR call failed, clip abandoned: %s", why(e))
            return "[语音]"
        return f"[语音:{text}]" if text else "[语音:没听清]"

    async def name_for(self, ref: AtRef, *, bot: BotApi, group_id: str) -> str | None:
        """A bare QQ number tells the model nothing about who was addressed.

        The protocol side already knows every member's group card, so ask it for the whole
        group at once rather than keeping a name cache of our own. It also answers for
        people who have never spoken, which user_profiles cannot.
        """
        qq = ref.ident or ""
        if not qq:
            return None
        name = await MEMBERS.name_of(bot, group_id, qq)
        if not name:
            # Someone who has left the group is no longer in the member list, but may
            # still be quoted in older messages. What the directory holds is every name
            # they were ever seen under, so the quote still reads as a person.
            try:
                card = await directory().person(int(group_id), qq)
                name = card.display if card.display != qq else ""
            except (UnknownAccount, ValueError):
                name = ""
        return f"@{name}" if name else None

    async def _nested(self, segments, *, bot: BotApi, group_id: str, self_id: str) -> str:
        """Flatten nested content on the free budget: everything that costs no model call.

        The budget is the whole rule here, not the nesting depth. A mention left as its
        placeholder is a bare account number in front of the model - the thing the prompt
        works hardest to keep out - and a picture quoted from earlier in the same group is
        exactly the one whose description is already sitting in the cache.

        So: names, and any description already bought. What is refused is spending - a
        first sighting of a picture, or a voice message, stays a placeholder down here.
        That is what keeps one forwarded album from fanning out into a vision call each.
        """
        pm = parse_segments(segments or [], self_id)
        if not pm.refs:
            return pm.render()
        return pm.render(await self._free_refs(pm, bot=bot, group_id=group_id))

    async def _free_refs(self, pm: ParsedMessage, *, bot: BotApi, group_id: str) -> dict[int, str]:
        """Everything in one parsed message that can be had without spending."""

        async def free(ref: Ref):
            if isinstance(ref, AtRef):
                return await self.name_for(ref, bot=bot, group_id=group_id)
            # A description already paid for. Stickers cache under their emoji id and
            # repeat constantly, so they are the most likely of all to be answered here.
            if isinstance(ref, ImageRef):
                return await self.cached(ref)
            # A forward is free too, but it is a fetch, and a forward nested in a quote
            # would make the number of fetches a property of what the group forwarded.
            return None

        outs = await asyncio.gather(*(free(r) for r in pm.refs), return_exceptions=True)
        return {r.slot: o for r, o in zip(pm.refs, outs) if isinstance(o, str) and o}

    async def _body_of(self, msg: dict, *, bot: BotApi, group_id: str, self_id: str) -> str:
        """One forward node's text, on the free budget."""
        segs, raw = segments_of(msg)
        if segs is None:
            return raw
        return await self._nested(segs, bot=bot, group_id=group_id, self_id=self_id)

    async def read_forward(self, ref: ForwardRef, *, bot: BotApi, group_id: str, self_id: str) -> str | None:
        try:
            res = await bot.call_api("get_forward_msg", message_id=ref.ident)
        except Exception as e:
            # Forward ids expire, and re-reading one can invalidate it.
            log.info("get_forward_msg failed for %s: %s", ref.ident, why(e))
            return None
        nodes = res.get("messages") or res.get("message") or []
        lines = []
        for node in nodes[:FORWARD_NODES]:
            data = node.get("data") if node.get("type") == "node" else node
            data = data or {}
            sender = data.get("sender") or {}
            who = (
                sender.get("card") or sender.get("nickname")
                or data.get("nickname") or ""
            ).strip()
            body = await self._body_of(data, bot=bot, group_id=group_id, self_id=self_id)
            if not body:
                continue
            if len(body) > FORWARD_CHARS:
                body = body[:FORWARD_CHARS] + "…"
            lines.append(f"{who}: {body}" if who else body)
        if not lines:
            return None
        more = "" if len(nodes) <= FORWARD_NODES else f" 等{len(nodes)}条"
        return "[转发的聊天记录" + more + "：" + " / ".join(lines) + "]"

    async def resolve(
        self,
        pm: ParsedMessage,
        *,
        bot,
        group_id: str,
        cfg: Settings,
        allow_models: bool = True,
        images_now: bool = False,
    ) -> dict[int, str]:
        """`allow_models=False` keeps the free lookups (who was @-ed, what was quoted,
        what was forwarded) and skips what costs money; `images_now=True` re-admits the
        pictures alone.

        The schedule this encodes: mentions, quotes and forwards resolve on arrival
        because a bare account number must not reach the archive. Pictures also resolve
        on arrival (images_now) - the download link is freshest then, the upload it
        feeds is free, and the describing call is cached per unique picture, rate
        limited and behind the daily cap, so arrival-time understanding is the same
        money at better latency, and it reaches groups the bot never replies in. Voice
        is the one thing that still waits for a reply: billed per second, never
        repeated, and only ever discussed right after being sent. Calling this twice on
        the same message is expected and cheap - the second pass hits warm caches."""
        if not pm.refs:
            return {}
        self_id = str(getattr(bot, "self_id", ""))

        async def one(ref: Ref):
            if ref.free or allow_models or (images_now and isinstance(ref, ImageRef)):
                return await ref.resolve(self, bot=bot, group_id=group_id, cfg=cfg,
                                         self_id=self_id)
            # A description already paid for costs nothing to reuse, and stickers repeat
            # constantly - so the free pass still answers for anything seen before. Only a
            # first sighting is left as a bare marker.
            return await self.cached(ref) if isinstance(ref, ImageRef) else None

        results = await asyncio.gather(*(one(r) for r in pm.refs), return_exceptions=True)
        out: dict[int, str] = {}
        for ref, res in zip(pm.refs, results):
            if isinstance(res, Exception):
                log.warning("resolving %s failed: %s", type(ref).__name__, res)
            elif res:
                out[ref.slot] = res
        return out

    @staticmethod
    def settled(pm: ParsedMessage, resolved: dict[int, str]) -> bool:
        """Whether every paid slot in this message now holds a final answer.

        The paid pass clears ChatMsg.pending on this verdict alone. A slot that is
        missing (a voice clip that could not be fetched) or holds an Unsettled
        fallback is worth paying for again next turn; clearing pending regardless
        would make the first transient failure permanent. Retries stay cheap: the
        cache and the in-flight registry mean a retry never buys the same picture
        twice, and a message ages out of the window either way.
        """
        return all(
            r.slot in resolved and not isinstance(resolved[r.slot], Unsettled)
            for r in pm.refs if not r.free
        )


MEDIA = MediaProcessor()
