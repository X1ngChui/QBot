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
   message's own, and the model opens any it wants to see with open_images.

Raw media never touches disk: memory -> API -> discarded, only text and cache keys are
kept.
"""

from __future__ import annotations

import asyncio
import base64
import binascii
import logging
import os
import re
import time
import uuid
import weakref
from dataclasses import dataclass
from datetime import timedelta
from enum import StrEnum
from pathlib import Path
from typing import TYPE_CHECKING

import httpx

from ..db import repo
from ..domain.ids import GroupId, MessageId
from ..prompting import PromptKey
from ..providers.base import Providers
from ..providers.contracts import StoredImage
from ..services import Directory, UnknownAccount
from ..settings import MediaCfg, Settings, VisionCfg, prompt_catalog
from ..util import cut_text, defang, sysmark, why
from .botapi import BotApi
from .budget import BUDGET
from .members import MEMBERS
from .output import strip_markdown
from .ratelimit import SlidingWindow
from .segments import AtRef, AudioRef, ImageRef, ParsedMessage, Ref

if TYPE_CHECKING:
    from .state import ChatMsg

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


#: What the byte routes answer for a file that exists but is over the caller's cap.
#: Distinct from None (nothing could be read) because the verdicts differ: an
#: unreadable picture is worth trying again later; an oversize one never shrinks.
#: Its own type, not the empty bytes object: every empty payload *is* b"" (the
#: interned singleton), and an empty body must fall through to the next route.
class _Oversize(bytes):
    __slots__ = ()


OVERSIZE = _Oversize()

#: "Not fetched yet" for the per-arrival byte closure in resolve_picture, where None
#: already means "fetched and failed".
_UNFETCHED = object()

#: Picture formats told apart by their first bytes.
_MAGIC = (
    (b"\x89PNG", "image/png"),
    (b"GIF8", "image/gif"),
    (b"\xff\xd8", "image/jpeg"),
    (b"BM", "image/bmp"),
)


def _mime(data: bytes, name: str | None) -> str:
    """The picture's format, from its bytes first and its file name second.

    The format matters to the backend that files it (a GIF's joke is its motion),
    and the name alone is not enough: a stored picture is often called `.image`,
    and a marketplace sticker carries no name at all.
    """
    for magic, mime in _MAGIC:
        if data.startswith(magic):
            return mime
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    suffix = Path(name or "").suffix.lower().lstrip(".")
    return f"image/{'jpeg' if suffix in ('', 'jpg', 'image') else suffix}"


class Unsettled(str):
    """A fallback standing in for paid content not yet obtained.

    Renders like any resolved text - it *is* the marker's fallback wording - but tells
    the pipeline the slot is not final: the describing call was rate limited, behind
    the daily cap, or failed transiently, and paying again later may well succeed.
    Terminal outcomes (a real description, a cached verdict, a backend refusal) come
    back as plain str; MediaProcessor.settled is the reader, and MediaTicket.state
    keeps the retry verdict outside the conversation message. Without the
    distinction, a transient failure would
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
    def __init__(
        self,
        cfg: MediaCfg,
        providers: Providers,
        directory: Directory,
    ) -> None:
        self._cfg = cfg
        self._providers = providers
        self._directory = directory
        self._http: httpx.AsyncClient | None = None
        self._img_windows: dict[GroupId, SlidingWindow] = {}
        self._asr_windows: dict[GroupId, SlidingWindow] = {}
        #: Describe calls in the air, by image key - the single-flight registry.
        self._describing: dict[str, asyncio.Task] = {}
        #: And ASR calls in the air, by clip file id - same rule, dearer stakes:
        #: voice has no result cache, so a duplicate flight is a duplicate bill.
        self._transcribing: dict[str, asyncio.Task] = {}
        #: Byte fetches in the air, by picture key. The same rule again: a second
        #: reader of a dead picture waits on the first's verdict instead of
        #: spending the same protocol deadline finding it out for itself.
        self._fetching: dict[str, asyncio.Task] = {}
        #: Pictures no route could read, by key, with when that was found out.
        #: Consulted by _bytes so one dead picture does not cost every reply that
        #: looks at it a full timeout. Entries past their hold are dropped as new
        #: ones are written, so it holds at most one hold's worth of dead pictures.
        self._unreadable: dict[str, float] = {}

    def _client(self) -> httpx.AsyncClient:
        if self._http is None:
            self._http = httpx.AsyncClient(
                timeout=self._cfg.http_timeout_sec,
                follow_redirects=True,
            )
        return self._http

    async def _call(self, bot: BotApi, api: str, **params):
        """One protocol-side media call under media.protocol_timeout_sec.

        The protocol side's own deadline is half a minute, and a file the platform
        can no longer serve does not fail there, it hangs - a reply would stand
        still for the whole of it.
        """
        return await asyncio.wait_for(
            bot.call_api(api, **params), timeout=self._cfg.protocol_timeout_sec
        )

    @staticmethod
    async def _flight(registry: dict[str, asyncio.Task], key: str, start):
        """Run `start()` once per key at a time; late callers share the result.

        The registry entry is popped when the FLIGHT ends, not when an awaiter
        does: a cancelled awaiter must not unregister a call still in the air, or
        the next caller starts (and pays for) a duplicate. The done-callback also
        holds the only strong reference - asyncio keeps tasks weakly. Awaiting
        through shield is the other half of the rule: cancelling a task cancels
        what it awaits, and one cancelled awaiter must not abort the call every
        other waiter is sharing.
        """
        flight = registry.get(key)
        if flight is None:
            flight = asyncio.create_task(start())
            registry[key] = flight
            flight.add_done_callback(lambda _t, k=key: registry.pop(k, None))
        return await asyncio.shield(flight)

    async def close(self) -> None:
        flights = {
            task
            for registry in (self._describing, self._transcribing, self._fetching)
            for task in registry.values()
            if not task.done()
        }
        for task in flights:
            task.cancel()
        if flights:
            await asyncio.gather(*flights, return_exceptions=True)
        self._describing.clear()
        self._transcribing.clear()
        self._fetching.clear()
        if self._http is not None:
            await self._http.aclose()
            self._http = None

    def _img_window(self, group_id: GroupId, limit: int) -> SlidingWindow:
        return self._window(self._img_windows, group_id, limit)

    def _asr_window(self, group_id: GroupId, limit: int) -> SlidingWindow:
        return self._window(self._asr_windows, group_id, limit)

    @staticmethod
    def _window(
        table: dict[GroupId, SlidingWindow],
        group_id: GroupId,
        limit: int,
    ) -> SlidingWindow:
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
                    return OVERSIZE
                buf = bytearray()
                async for chunk in r.aiter_bytes():
                    buf.extend(chunk)
                    if len(buf) > max_bytes:
                        log.info("media over size limit mid-stream, skipped")
                        return OVERSIZE
                # An empty body is no picture: None lets the next route try.
                return bytes(buf) or None
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
            if p.is_file():
                return (p.read_bytes() or None) if p.stat().st_size <= max_bytes else OVERSIZE
        except OSError as e:
            log.debug("local media read failed %s: %s", p, why(e))
        return None

    @staticmethod
    def _links(url: str) -> list[str]:
        """The links worth trying for a picture, best first.

        A marketplace sticker arrives as a link into its directory on the sticker
        CDN - usually the raw GIF (`.../raw300.gif`), sometimes the directory alone -
        and the GIF redirects to a file that is often absent. The same directory
        serves a 300x300 PNG for every sticker, so that is asked for first.
        """
        if not _STICKER_CDN.match(url):
            return [url]
        base = url.rstrip("/")
        if base.rsplit("/", 1)[-1].count("."):
            base = base.rsplit("/", 1)[0]
        png = base + "/300x300.png"
        return [png] if url == png else [png, url]

    # -- per-kind resolution ----------------------------------------------
    async def _bytes(self, ref: ImageRef, *, bot, max_bytes: int) -> bytes | None:
        """Fetch the picture itself, by whichever route answers: the link the
        message carried, then get_image for a fresh copy. Single-flight per
        picture, and a picture no route could read is remembered for a while.

        Both guard the same cost: get_image on a picture the platform can no
        longer serve does not fail, it hangs until the deadline, and without them
        every reader of the same picture - the arrival pass, a reply's backlog
        pass, open_images - would wait it out anew.
        """
        key = ref.key or ref.file or ref.url or ""
        if not key:
            return await self._bytes_once(ref, bot=bot, max_bytes=max_bytes)
        # The cap is part of the key: a flight started under one group's
        # max_image_mb must not hand its oversize verdict to a group with a wider one.
        return await self._flight(
            self._fetching,
            f"{key}:{max_bytes}",
            lambda: self._bytes_once(ref, bot=bot, max_bytes=max_bytes),
        )

    async def _bytes_once(self, ref: ImageRef, *, bot, max_bytes: int) -> bytes | None:
        key = ref.key or ref.file or ref.url or ""
        hold = self._cfg.unreadable_retry_sec
        now = time.monotonic()
        failed_at = self._unreadable.get(key) if key else None
        if failed_at is not None and now - failed_at < hold:
            log.debug("picture %s recently unreadable, not retried", key[:40])
            return None
        data = None
        for link in self._links(ref.url) if ref.url else []:
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
        if data is OVERSIZE:
            log.info("picture over the size cap, skipped")
        elif data is None:
            if key:
                self._unreadable = {k: t for k, t in self._unreadable.items() if now - t < hold}
                self._unreadable[key] = now
            log.warning(
                "picture unreadable by every route (link=%s file=%s), not retried for %ds",
                bool(ref.url),
                bool(ref.file),
                hold,
            )
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

    async def resolve_picture(
        self, ref: ImageRef, *, bot: BotApi, group_id: GroupId, cfg: Settings
    ) -> str | None:
        """Everything one picture gets on arrival: filed with the reply model's
        backend (free), then described - unless it is only forwarded, in which
        case a description already paid for is all it may have.

        The bytes are fetched at most once for both steps, through a closure they
        share. Neither step fetches before passing its own gates, so a picture
        already described, with nothing to file, costs no download at all.
        """
        data: object = _UNFETCHED
        max_bytes = int(cfg.capabilities.vision.max_image_mb * 1024 * 1024)

        async def fetch() -> bytes | None:
            nonlocal data
            if data is _UNFETCHED:
                data = await self._bytes(ref, bot=bot, max_bytes=max_bytes)
            return data  # type: ignore[return-value]

        await self.ensure_uploaded(ref, bot=bot, group_id=group_id, cfg=cfg, fetch=fetch)
        if ref.nested:
            return await self.cached(ref)
        return await self.describe_image(ref, bot=bot, group_id=group_id, cfg=cfg, fetch=fetch)

    async def ensure_uploaded(
        self, ref: ImageRef, *, bot: BotApi, group_id: GroupId, cfg: Settings, fetch=None
    ) -> StoredImage | None:
        """File this picture with the reply model's attachment store, once.

        The upload itself is free and the opaque handle lets open_images put the
        original pixels in front of the model. Runs on arrival, while the message's
        download link is still fresh; the id lands both on the ref (for this
        process) and in image_cache (for reposts and for the message's later turns
        in the window). A backend that keeps no files is skipped before any
        download, so the feature costs nothing where it is off.

        `fetch` is the arrival pass's shared byte closure; without one the bytes
        are fetched here.
        """
        text_model = self._providers.text
        store = text_model.attachments
        if ref.file_id and ref.file_provider == text_model.name:
            return StoredImage(text_model.name, ref.file_id)
        if not ref.key or store is None:
            return None
        # Only an id young enough to still exist at the backend: a dead one fails
        # the whole request it rides in, and re-uploading is free.
        cached = await repo.image_cache_file(
            ref.key,
            provider=text_model.name,
            max_age=timedelta(days=cfg.capabilities.vision.file_max_age_days),
        )
        if cached:
            ref.file_id = cached
            ref.file_provider = text_model.name
            return StoredImage(text_model.name, cached)
        vcfg = cfg.capabilities.vision
        max_bytes = int(vcfg.max_image_mb * 1024 * 1024)
        if ref.size and ref.size > max_bytes:
            return None
        data = await (fetch() if fetch else self._bytes(ref, bot=bot, max_bytes=max_bytes))
        if not data:
            return None
        try:
            stored = await store.store(data, _mime(data, ref.file))
        except Exception as e:
            log.warning("image upload failed: %s", why(e))
            return None
        ref.file_id = stored.handle
        ref.file_provider = stored.provider
        await repo.image_cache_set_file(ref.key, stored.handle, provider=stored.provider)
        return stored

    async def describe_image(
        self, ref: ImageRef, *, bot: BotApi, group_id: GroupId, cfg: Settings, fetch=None
    ) -> str | None:
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
        that description ages out (capabilities.vision.description_ttl_days). The summary stays
        as the fallback for when the image cannot be fetched.
        """
        if not ref.key:
            return await self._describe_once(ref, bot=bot, group_id=group_id, cfg=cfg, fetch=fetch)
        return await self._flight(
            self._describing,
            ref.key,
            lambda: self._describe_once(ref, bot=bot, group_id=group_id, cfg=cfg, fetch=fetch),
        )

    async def _describe_once(
        self, ref: ImageRef, *, bot: BotApi, group_id: GroupId, cfg: Settings, fetch=None
    ) -> str | None:
        vcfg = cfg.capabilities.vision
        label = "表情" if ref.sticker else "图片"
        # ref.summary was defanged at segment parse; the wrap is system-authored.
        fallback = sysmark(f"{label}:{ref.summary}") if ref.summary else None
        # The verdict on a picture that is never going to be described. Terminal,
        # so the pipeline stops paying attention to the slot.
        final = fallback or sysmark(label)

        if ref.key:
            # The paid path is the one that asks for a *current* description: an
            # aged-out one is a miss here, and this call is what pays to replace it.
            cached = await repo.image_cache_get(ref.key, max_age=_ttl(vcfg))
            if cached:
                return cached

        # A picture does not shrink, so the size verdict is final - and it is
        # reached before any gate below is spent on it.
        max_bytes = int(vcfg.max_image_mb * 1024 * 1024)
        if ref.size and ref.size > max_bytes:
            return final

        # Transient turn-aways return Unsettled: the same fallback text, but marked
        # retryable so the coordinator can retry the ticket for a later reply.
        retry_later = Unsettled(fallback) if fallback else None

        if not self._img_window(group_id, vcfg.max_images_per_min).take():
            log.info("image understanding rate limited, skipped (%s)", group_id)
            return retry_later

        # This path runs on arrival, with no reply-side gate above it, so the daily
        # cap is asked here.
        if await BUDGET.exceeded(cfg.budget.daily_cny_cap):
            return retry_later

        data = await (fetch() if fetch else self._bytes(ref, bot=bot, max_bytes=max_bytes))
        if data is OVERSIZE:
            return final
        if not data:
            return retry_later

        try:
            desc = await self._providers.vision.describe(
                data,
                prompt=prompt_catalog().render(PromptKey.VISION_SYSTEM),
                mime=_mime(data, ref.file),
                group_id=group_id,
            )
        except Exception as e:
            if _is_refusal(e):
                # The backend looked and declined - its content filter, not a fault here.
                # Cache the outcome so the same picture is not fetched and paid for on
                # every repost, and keep it at info: the error ring is what the daily
                # report reads, and a refusal that recurs daily would crowd out the
                # failures someone actually has to act on.
                log.info("vision backend declined an image; recording it as unseen")
                if ref.key:
                    await repo.image_cache_put(ref.key, final, refused=True)
                return final
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

    async def transcribe(
        self, ref: AudioRef, *, bot: BotApi, group_id: GroupId, cfg: Settings
    ) -> str | None:
        """Single-flight per clip, like describe_image per picture: with pending
        kept alive across turns, a slow ASR call (timeout 60s) can outlive the 25s
        paid wait, and the next turn's backlog pass would otherwise start - and
        pay for - a second identical call. Voice has no result cache to fall back
        on, so the in-flight registry is the only thing standing between a slow
        backend and double billing."""
        key = ref.file or ref.url
        if not key:
            return await self._transcribe_once(ref, bot=bot, group_id=group_id, cfg=cfg)
        return await self._flight(
            self._transcribing,
            key,
            lambda: self._transcribe_once(ref, bot=bot, group_id=group_id, cfg=cfg),
        )

    async def _transcribe_once(
        self, ref: AudioRef, *, bot: BotApi, group_id: GroupId, cfg: Settings
    ) -> str | None:
        acfg = cfg.capabilities.asr
        # This path runs on arrival, with no reply-side gate above it, so the
        # per-minute window and the daily cap are asked here. None keeps the slot
        # unsettled, so a reply-path settle can retry later.
        if not self._asr_window(group_id, acfg.max_clips_per_min).take():
            log.info("voice transcription rate limited, deferred (%s)", group_id)
            return None
        # Local ASR spends no provider money. The per-group window above and the
        # recognizer's bounded global queue are its admission controls.
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
            try:
                data = base64.b64decode(str(info["base64"]).split(",", 1)[-1]) or None
            except (binascii.Error, ValueError) as e:
                log.info("get_record answered malformed base64: %s", why(e))
                data = None
            if data is not None and len(data) > wav_cap:
                data = OVERSIZE
        if data is None:
            data = self._local(info.get("file"), wav_cap)
        if data is None and info.get("url"):
            data = await self._fetch(info["url"], wav_cap)
        if data is OVERSIZE:
            # Terminal: a clip does not get shorter, and leaving the slot unsettled
            # would have the protocol side transcode it again on every turn.
            log.info("voice clip over the length cap, skipped")
            return sysmark("语音")
        if not data:
            return None

        try:
            text = await self._providers.asr.transcribe(
                data,
                fmt="wav",
                seconds=_audio_seconds(len(data)),
                group_id=group_id,
            )
        except Exception as e:
            # Local failures are never billable. Keep the slot unsettled so a later
            # backlog pass may retry after overload or a transient decoder failure.
            log.warning("local ASR failed, retryable: %s", why(e))
            return None
        # defang the transcription: it is the speaker's words, machine-transcribed,
        # and spoken text is as member-controlled as typed text.
        return sysmark(f"语音:{defang(text)}") if text else sysmark("语音:没听清")

    async def name_for(self, ref: AtRef, *, bot: BotApi, group_id: GroupId) -> str | None:
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
                card = await self._directory.holder_card(group_id, qq)
                name = card.display if card.display != qq else ""
            except (UnknownAccount, ValueError):
                name = ""
        return f"@{name}" if name else None

    async def resolve(
        self,
        pm: ParsedMessage,
        *,
        bot,
        group_id: GroupId,
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
            return await ref.resolve(self, bot=bot, group_id=group_id, cfg=cfg, self_id=self_id)

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

        The coordinator keeps a retryable ticket on this verdict alone. A slot that is
        missing (a voice clip that could not be fetched) or holds an Unsettled
        fallback is worth paying for again next turn; clearing pending regardless
        would make the first transient failure permanent. Retries stay cheap: the
        cache and the in-flight registry mean a retry never buys the same picture
        twice, and a message ages out of the window either way.
        """
        return all(
            r.slot in resolved and not isinstance(resolved[r.slot], Unsettled)
            for r in pm.refs
            if not r.free
        )


class MediaStatus(StrEnum):
    PENDING = "pending"
    RETRYABLE = "retryable"
    FINAL = "final"


@dataclass(slots=True)
class MediaTicket:
    """One admitted message's media work, owned outside the chat transcript."""

    raw_event_id: uuid.UUID
    message_id: MessageId
    parsed: ParsedMessage
    message_ref: weakref.ReferenceType[ChatMsg]
    bot: BotApi
    group_id: GroupId
    cfg: Settings
    default_who: str
    status: MediaStatus = MediaStatus.PENDING
    task: asyncio.Task[None] | None = None


class MediaCoordinator:
    """Own per-message resolution, retries, patches, and shutdown draining."""

    def __init__(self, processor: MediaProcessor) -> None:
        self._processor = processor
        self._tickets: dict[uuid.UUID, MediaTicket] = {}

    def admit(
        self,
        raw_event_id: uuid.UUID,
        parsed: ParsedMessage,
        message: ChatMsg,
        *,
        bot: BotApi,
        group_id: GroupId,
        cfg: Settings,
    ) -> MediaTicket:
        """Start the arrival pass exactly once for one admitted raw event."""

        current = self._tickets.get(raw_event_id)
        if current is not None:
            return current
        ticket = MediaTicket(
            raw_event_id=raw_event_id,
            message_id=message.msg_id,
            parsed=parsed,
            message_ref=weakref.ref(message),
            bot=bot,
            group_id=group_id,
            cfg=cfg,
            default_who=str(message.user_id),
        )
        self._tickets[raw_event_id] = ticket
        self._start(ticket, who=ticket.default_who)
        return ticket

    def ticket(self, raw_event_id: uuid.UUID) -> MediaTicket | None:
        return self._tickets.get(raw_event_id)

    @property
    def processor(self) -> MediaProcessor:
        return self._processor

    def _start(self, ticket: MediaTicket, *, who: str | None) -> asyncio.Task[None] | None:
        if ticket.status is MediaStatus.FINAL:
            return None
        if ticket.task is not None and not ticket.task.done():
            return ticket.task
        if ticket.message_ref() is None:
            self._tickets.pop(ticket.raw_event_id, None)
            return None
        task = asyncio.create_task(self._resolve(ticket, who=who or ticket.default_who))
        ticket.task = task

        def finished(done: asyncio.Task[None]) -> None:
            if ticket.task is done:
                ticket.task = None
            if ticket.status is MediaStatus.FINAL or ticket.message_ref() is None:
                self._tickets.pop(ticket.raw_event_id, None)

        task.add_done_callback(finished)
        return task

    async def _resolve(self, ticket: MediaTicket, *, who: str) -> None:
        message = ticket.message_ref()
        if message is None:
            ticket.status = MediaStatus.FINAL
            return
        try:
            with BUDGET.attribute(who):
                resolved = await self._processor.resolve(
                    ticket.parsed,
                    bot=ticket.bot,
                    group_id=ticket.group_id,
                    cfg=ticket.cfg,
                )
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            ticket.status = MediaStatus.RETRYABLE
            log.warning(
                "group %s: media resolution failed: %s",
                ticket.group_id,
                why(exc),
            )
            return

        ticket.status = (
            MediaStatus.FINAL
            if self._processor.settled(ticket.parsed, resolved)
            else MediaStatus.RETRYABLE
        )
        new_text = cut_text(
            ticket.parsed.render(resolved),
            ticket.cfg.tools.send_messages.max_text_chars_per_message,
        )
        if new_text and new_text != message.text:
            message.text = new_text
            try:
                await repo.backfill_plain_text(ticket.message_id, new_text)
            except Exception:
                log.exception("failed to backfill plain_text for %s", ticket.message_id)

    async def settle(
        self,
        messages: list[ChatMsg],
        *,
        wait_sec: float,
        who: str | None,
        cfg: Settings | None = None,
    ) -> None:
        """Start retryable work in a frozen reply window and wait once, bounded."""

        tasks: set[asyncio.Task[None]] = set()
        for message in messages:
            if message.raw_event_id is None or message.is_bot:
                continue
            ticket = self._tickets.get(message.raw_event_id)
            if ticket is None:
                continue
            if cfg is not None:
                ticket.cfg = cfg
            if task := self._start(ticket, who=who):
                tasks.add(task)
        if tasks:
            await asyncio.wait(tasks, timeout=wait_sec)

    async def close(self, *, timeout: float) -> None:
        """Drain admitted patch tasks, then cancel any that exceed shutdown's bound."""

        tasks = {
            ticket.task
            for ticket in self._tickets.values()
            if ticket.task is not None and not ticket.task.done()
        }
        if tasks:
            _done, pending = await asyncio.wait(tasks, timeout=timeout)
            for task in pending:
                task.cancel()
            if pending:
                await asyncio.gather(*pending, return_exceptions=True)
        self._tickets.clear()
