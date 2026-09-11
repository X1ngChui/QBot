"""Reading what a message points at: pictures, voice, forwards, who was @-ed.

Everything here costs something - an HTTP fetch, an API call, a model call - which is the
line between this file and segments.py. Two rules shape it:

1. **Everything resolves on arrival.** Who was @-ed, what was quoted and what was
   forwarded cost nothing, and an unresolved mention would archive as a bare account
   number - the thing the prompt is most careful to keep out. Pictures resolve on
   arrival too: the upload that files them with the vision backend is free and wants
   the freshest link, and the describing call is cached per unique picture, rate
   limited, and behind the daily budget cap - so paying at arrival is the same money
   at better latency, and it reaches groups the bot never speaks in. Voice is
   transcribed on arrival as well: clips are rarer than pictures, and without an
   arrival transcript they were a blind spot in extraction, which reads only text.
   The per-minute gates and the daily cap apply here as on any paid path, and a
   clip turned away stays pending, so the next reply's backlog pass tries again.

   Late resolution stays possible in reach. A received image link carries an rkey that
   expires in about two hours, but the file id does not: get_image trades it for a
   fresh link at any time, which is how the QQ client itself still shows pictures from
   days ago. So _bytes tries the link, then get_image - and a picture stays readable
   long after the link in the original message stopped working, for as long as the
   platform itself still serves it.
2. **Forwarded content gets the free budget and no more.** The entries of a forwarded
   record have their @s resolved to names and reuse any description already paid for,
   because a bare account number is the thing the prompt works hardest to keep out and
   a picture forwarded from earlier in the same group is exactly the one already
   described. What it will not do is spend: a first sighting of a picture, or a voice
   clip, stays a bare marker down there, so one forwarded album cannot trigger dozens
   of vision calls. The pictures are still filed (free), numbered with the carrying
   message's own, and the model opens any it wants to see with open_image.

Raw media never touches disk: memory -> API -> discarded, only text and cache keys are
kept.
"""

from __future__ import annotations

import asyncio
import base64
import logging
import os
import re
import time
from datetime import timedelta
from pathlib import Path

import httpx

from ..db import repo
from ..providers import providers
from ..providers.base import retire
from ..providers.openai_compat import NEVER_BILLED
from ..services import UnknownAccount
from ..settings import Settings, VisionCfg, config, ptext
from ..util import defang, sysmark, why
from .botapi import BotApi
from .budget import BUDGET
from .members import MEMBERS
from .output import strip_markdown
from .ratelimit import SlidingWindow
from .retrieval import directory
from .segments import AtRef, AudioRef, ImageRef, ParsedMessage, Ref

log = logging.getLogger("qqbot.media")

NAPCAT_DATA_DIR = os.getenv("NAPCAT_DATA_DIR", "/app/napcat_data")

#: Bytes per second of 16 kHz 16-bit mono WAV, the only audio this module ever sends
#: anywhere: both the size cap (config states seconds) and the billed duration derive
#: from the byte count through it. What QQ actually stores is SILK v3 at ~1600 B/s,
#: but that never reaches the ASR backend - see transcribe.
_WAV_BYTES_PER_SEC = 32000


#: The marketplace-sticker CDN: a directory link per sticker.
_STICKER_CDN = re.compile(r"^https?://gxh\.vip\.qq\.com/club/item/parcel/item/")


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


def _ttl(vcfg: VisionCfg) -> timedelta | None:
    """How current a stored description has to be for the paid path to reuse it.

    None where expiry is switched off. Only the describing call asks: it is the one
    that can afford to replace what it rejects.
    """
    days = vcfg.description_ttl_days
    return timedelta(days=days) if days else None


class MediaProcessor:
    def __init__(self) -> None:
        self._http: httpx.AsyncClient | None = None
        self._http_timeout: float | None = None
        self._img_windows: dict[str, SlidingWindow] = {}
        self._asr_windows: dict[str, SlidingWindow] = {}
        #: Describe calls in the air, by image key - the single-flight registry.
        self._describing: dict[str, asyncio.Task] = {}
        #: And ASR calls in the air, by clip file id - same rule, dearer stakes:
        #: voice has no result cache, so a duplicate flight is a duplicate bill.
        self._transcribing: dict[str, asyncio.Task] = {}
        #: Pictures no route could read, by key, with when that was found out.
        #: Consulted by _bytes so one dead picture does not cost every reply that
        #: looks at it a full timeout.
        self._unreadable: dict[str, float] = {}

    def _client(self) -> httpx.AsyncClient:
        # Rebuilt when the deadline changes, so a /reload applies without a restart.
        timeout = config().default.gateway.media_http_timeout_sec
        if self._http is None or self._http_timeout != timeout:
            if self._http is not None:
                retire(self._http.aclose())
            self._http = httpx.AsyncClient(timeout=timeout, follow_redirects=True)
            self._http_timeout = timeout
        return self._http

    @staticmethod
    async def _call(bot: BotApi, api: str, **params):
        """One protocol-side media call under gateway.protocol_call_timeout_sec.

        The protocol side's own deadline is half a minute, and a file the platform
        can no longer serve does not fail there, it hangs - a reply would stand
        still for the whole of it.
        """
        return await asyncio.wait_for(
            bot.call_api(api, **params),
            timeout=config().default.gateway.protocol_call_timeout_sec)

    async def close(self) -> None:
        if self._http is not None:
            await self._http.aclose()
            self._http = None

    def _img_window(self, group_id: str, limit: int) -> SlidingWindow:
        return self._window(self._img_windows, group_id, limit)

    def _asr_window(self, group_id: str, limit: int) -> SlidingWindow:
        return self._window(self._asr_windows, group_id, limit)

    @staticmethod
    def _window(table: dict[str, SlidingWindow], group_id: str,
                limit: int) -> SlidingWindow:
        w = table.get(group_id)
        if w is None:
            w = SlidingWindow(limit)
            table[group_id] = w
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
            # Info, not warning: a received link is one of three routes to the bytes
            # and the one that expires (its rkey lasts about two hours), so it fails
            # routinely for any picture read late. _bytes reports when no route
            # answered, which is the failure worth a warning.
            log.info("media download failed %s: %s", url[:80], why(e))
            return None

    @staticmethod
    def _local(napcat_path: str | None, max_bytes: int) -> bytes | None:
        """Read a file NapCat has on disk, through the QQ data directory both
        containers mount.

        The paths come from NapCat's own answers - get_image and get_record report
        where they put the file, as an absolute path inside NapCat's container - and
        the same file is visible here under NAPCAT_DATA_DIR, which spares a second
        download. Message segments themselves carry no path.
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

    @staticmethod
    def _links(url: str) -> list[str]:
        """The links worth trying for a picture, best first.

        A marketplace sticker arrives as a directory link on the sticker CDN, which
        redirects to a raw GIF that is often absent; the same directory serves a
        300x300 PNG for every sticker, so that is asked for first.
        """
        if _STICKER_CDN.match(url) and not url.rstrip("/").rsplit("/", 1)[-1].count("."):
            return [url.rstrip("/") + "/300x300.png", url]
        return [url]

    # -- per-kind resolution ----------------------------------------------
    async def _bytes(self, ref: Ref, *, bot, max_bytes: int) -> bytes | None:
        """Fetch the picture itself, by whichever route answers: the link the
        message carried, then get_image for a fresh copy.

        A picture no route could read is remembered for a while and not tried
        again until that passes: get_image on a picture the platform can no longer
        serve does not fail, it hangs until the timeout, and every reply that
        looked at the same picture would otherwise wait it out anew.
        """
        key = ref.key or ref.file or ref.url or ""
        hold = config().default.gateway.unreadable_retry_sec
        failed_at = self._unreadable.get(key) if key else None
        if failed_at is not None and time.monotonic() - failed_at < hold:
            log.debug("picture %s recently unreadable, not retried", key[:40])
            return None
        data = None
        for link in (self._links(ref.url) if ref.url else []):
            data = await self._fetch(link, max_bytes)
            if data is not None:
                break
        if data is None and ref.file:
            try:
                info = await self._call(bot, "get_image", file=ref.file)
                data = self._local(info.get("file"), max_bytes)
                if data is None and info.get("url"):
                    data = await self._fetch(info["url"], max_bytes)
            except Exception as e:
                log.info("get_image failed: %s", why(e))
        if data is None:
            if key:
                self._unreadable[key] = time.monotonic()
            log.warning("picture unreadable by every route (link=%s file=%s), "
                        "not retried for %ds", bool(ref.url), bool(ref.file), hold)
        return data

    @staticmethod
    async def cached(ref: ImageRef) -> str | None:
        """A description already paid for.

        Lives here rather than on the ref because segments.py knows nothing about a
        database - that is the line between the two files, and a cache lookup is on this
        side of it. It is the free half of the arrival pass, and what makes a picture
        quoted from earlier in the same group readable without spending anything.

        Deliberately does not ask how old the description is. Expiry means "worth
        paying to describe again", and nothing on this path may pay - so here an aged
        description is served as it stands, which beats a bare marker.
        """
        return await repo.image_cache_get(ref.key) if ref.key else None

    async def ensure_uploaded(self, ref: ImageRef, *, bot: BotApi, group_id: str,
                              cfg: Settings) -> str | None:
        """File this picture with the reply model's backend, once, and remember where.

        The upload itself is free and the id is what lets a reply prompt carry the
        original pixels instead of a one-line description. Runs on arrival, while the
        message's download link is still fresh; the id lands both on the ref (for this
        process) and in image_cache (for reposts and for the message's later turns in
        the window). A backend that keeps no files answers None once and this feature
        simply stays off.
        """
        if ref.file_id:
            return ref.file_id
        if not ref.key:
            return None
        # Only an id young enough to still exist at the backend: a dead one fails
        # the whole request it rides in, and re-uploading is free.
        cached = await repo.image_cache_file(
            ref.key, max_age=timedelta(days=cfg.llm.vision.file_max_age_days))
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
            # Through the text capability: it is the model that will be handed the
            # file block, so it is the one that has to resolve the id.
            fid = await providers().text.upload(data, cfg=cfg.llm.text, mime=mime)
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
        so each distinct sticker is described once and every later use is free until
        that description ages out (llm.vision.description_ttl_days). The summary stays
        as the fallback for when the image cannot be fetched.
        """
        if ref.key and (flight := self._describing.get(ref.key)) is not None:
            return await asyncio.shield(flight)
        if not ref.key:
            return await self._describe_once(ref, bot=bot, group_id=group_id, cfg=cfg)
        task = asyncio.create_task(
            self._describe_once(ref, bot=bot, group_id=group_id, cfg=cfg))
        self._describing[ref.key] = task
        # Popped when the FLIGHT ends, not when this awaiter does: a cancelled
        # awaiter must not unregister a flight still in the air, or the next
        # caller starts (and pays for) a duplicate. The callback also keeps the
        # only strong reference alive - asyncio holds tasks weakly. Awaited
        # through shield for the same reason: cancelling a task cancels what it
        # is awaiting, and one cancelled awaiter must not abort the paid call
        # every other waiter is sharing.
        task.add_done_callback(lambda _t, k=ref.key: self._describing.pop(k, None))
        return await asyncio.shield(task)

    async def _describe_once(self, ref: ImageRef, *, bot: BotApi, group_id: str,
                             cfg: Settings) -> str | None:
        vcfg = cfg.llm.vision
        label = "表情" if ref.sticker else "图片"
        # ref.summary was defanged at segment parse; the wrap is system-authored.
        fallback = sysmark(f"{label}:{ref.summary}") if ref.summary else None

        if ref.key:
            # The paid path is the one that asks for a *current* description: an
            # aged-out one is a miss here, and this call is what pays to replace it.
            cached = await repo.image_cache_get(ref.key, max_age=_ttl(vcfg))
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
                    await repo.image_cache_put(ref.key, fallback or sysmark(label),
                                               refused=True)
                return fallback
            log.warning("vision model call failed: %s", why(e))
            return retry_later
        if not desc:
            return retry_later
        # The vision backend writes for a reader, so it bolds things. Left in, every
        # picture puts ** in the transcript the model is imitating, against a persona whose
        # first rule is not to use Markdown.
        # defang the model's words too: it read the picture, and a picture can carry
        # printed text - which makes the description a channel for outside bytes.
        desc = sysmark(f"{label}:{defang(strip_markdown(desc))}")
        if ref.key:
            await repo.image_cache_put(ref.key, desc)
        return desc

    async def transcribe(self, ref: AudioRef, *, bot: BotApi, group_id: str,
                         cfg: Settings) -> str | None:
        """Single-flight per clip, like describe_image per picture: with pending
        kept alive across turns, a slow ASR call (timeout 60s) can outlive the 25s
        paid wait, and the next turn's backlog pass would otherwise start - and
        pay for - a second identical call. Voice has no result cache to fall back
        on, so the in-flight registry is the only thing standing between a slow
        backend and double billing."""
        key = ref.file or ref.url
        if key and (flight := self._transcribing.get(key)) is not None:
            return await asyncio.shield(flight)
        if not key:
            return await self._transcribe_once(ref, bot=bot, group_id=group_id, cfg=cfg)
        task = asyncio.create_task(
            self._transcribe_once(ref, bot=bot, group_id=group_id, cfg=cfg))
        self._transcribing[key] = task
        # Same rules as describe_image's registry: pop when the flight ends, never
        # when an awaiter does, and await through shield so a cancelled awaiter
        # cannot abort the shared call.
        task.add_done_callback(lambda _t, k=key: self._transcribing.pop(k, None))
        return await asyncio.shield(task)

    async def _transcribe_once(self, ref: AudioRef, *, bot: BotApi, group_id: str,
                               cfg: Settings) -> str | None:
        acfg = cfg.llm.asr
        # Transcription now happens on arrival rather than behind a reply, so
        # the arrival gates live here - there is no upstream gate on this path.
        # None keeps the slot unsettled, so a reply-path settle can retry later.
        if not self._asr_window(group_id, acfg.max_clips_per_min).take():
            log.info("voice transcription rate limited, deferred (%s)", group_id)
            return None
        # The daily cap guards spending, so a backend whose rate is zero (the
        # in-process one) transcribes right through it: a group that exhausted
        # the budget on replies should not also get a degraded archive for free
        # audio. The per-minute gate above still applies - it paces CPU now.
        if (providers().asr.rate_for(acfg.model).units(1.0) > 0
                and await BUDGET.exceeded(cfg.budget.daily_cny_cap)):
            return None
        # Never the stored file, never the CDN link: both hold QQ's native SILK v3
        # whatever the ".amr" suffix claims (magic '#!SILK_V3'), and SILK bytes labelled
        # amr get a polite empty transcript from the ASR backend - a perfectly clear
        # clip the bot claims it cannot hear.
        # get_record is the one source of decodable audio: NapCat
        # transcodes to 16 kHz mono WAV, and the cap is computed for WAV (~20x denser).
        wav_cap = _byte_cap(acfg.max_audio_sec)
        try:
            info = await self._call(bot, "get_record", file=ref.file or "", out_format="wav")
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
            return sysmark("语音")
        # defang the transcription: it is the speaker's words, machine-transcribed,
        # and spoken text is as member-controlled as typed text.
        return sysmark(f"语音:{defang(text)}") if text else sysmark("语音:没听清")

    async def name_for(self, ref: AtRef, *, bot: BotApi, group_id: str) -> str | None:
        """A bare QQ number tells the model nothing about who was addressed.

        The protocol side already knows every member's group card, so ask it for the whole
        group at once rather than keeping a second name cache here. It also answers for
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

    async def resolve(
        self,
        pm: ParsedMessage,
        *,
        bot,
        group_id: str,
        cfg: Settings,
    ) -> dict[int, str]:
        """Every reference in one message, resolved: the free lookups (who was
        @-ed, what was quoted, what was forwarded) and the paid media alike.

        The paid kinds carry their own gates - the per-picture cache, the
        per-minute windows, the daily cap - so this runs on arrival for every
        message, and calling it again on the same message is expected and cheap:
        the second pass hits warm caches or the in-flight registries. The
        free-only path for nested content is _free_refs.
        """
        if not pm.refs:
            return {}
        self_id = str(getattr(bot, "self_id", ""))

        async def one(ref: Ref):
            return await ref.resolve(self, bot=bot, group_id=group_id, cfg=cfg,
                                     self_id=self_id)

        results = await asyncio.gather(*(one(r) for r in pm.refs), return_exceptions=True)
        out: dict[int, str] = {}
        for ref, res in zip(pm.refs, results, strict=True):
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
