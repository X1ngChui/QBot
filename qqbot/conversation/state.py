"""Per-group runtime state.

In memory: the recent messages, the immediate context a reply reads. What has and has not
been read into long-term memory is tracked in SQL instead - a restart should not send a
nearly-full batch back to zero. The only persisted switch here is group mute; dynamic
exact-account and linked-holder block rules live in their repository.
"""

from __future__ import annotations

import asyncio
import logging
import re
import uuid
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from zoneinfo import ZoneInfo

from qqbot.clock import Clock
from qqbot.conversation.history import HistoryWindow
from qqbot.repositories.groups import GroupRepository
from qqbot.repositories.archive import ArchiveRepository
from qqbot.domain.archive import ArchivedMessage
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import AccountId
from qqbot.domain.ids import GroupId
from qqbot.domain.ids import MessageId
from qqbot.configuration import ConfigBundle
from qqbot.util import SYS_L
from qqbot.util import SYS_R
from qqbot.util import sysmark
from qqbot.conversation.member_numbers import BOT_DISPLAY_NUMBER
from qqbot.delivery.segments import HistoricalSegment
from qqbot.delivery.segments import from_onebot
from qqbot.gateway.segments import ImageRef
from qqbot.gateway.segments import number_at_mentions
from qqbot.gateway.segments import parse_segments

log = logging.getLogger("qqbot.state")

#: Worn in the system brackets so no display name can imitate it: defang()
#: neutralizes the pair in every member-controlled string before it renders.
OWNER_TAG = sysmark("拥有者")

#: A picture marker in a rendered line, either kind. The number the prompt gives
#: it is inserted right after the label, so the description stays where it was.
_PIC_MARK = re.compile(
    rf"{re.escape(SYS_L)}(图片|表情)(:[^{re.escape(SYS_R)}]*)?{re.escape(SYS_R)}"
)


class TranscriptRendering:
    """Shared rendering for mutable live messages and immutable model snapshots."""

    __slots__ = ()

    def numbered_text(self, pic_nums: list[int] | None) -> str:
        """This message's text with its picture markers carrying their prompt numbers.

        The number is how the model names a picture to open_images, and it is a position
        in one render - so it is applied here rather than stored, exactly like the line
        number. Markers are matched in order against image_refs; if the two counts
        disagree the message is left unnumbered rather than numbered wrong - a
        number that opened the wrong picture would be worse than none.
        """
        if not pic_nums:
            return self.text
        marks = _PIC_MARK.findall(self.text)
        if len(marks) != len(pic_nums):
            return self.text
        it = iter(pic_nums)
        return _PIC_MARK.sub(
            lambda m: sysmark(f"{m.group(1)}{next(it)}{m.group(2) or ''}"), self.text
        )

    def render(
        self,
        *,
        seq: int = 0,
        quote: str = "",
        pic_nums: list[int] | None = None,
        member_no: int | None = None,
        mention_number: Callable[[str], int | None] | None = None,
    ) -> str:
        """One line of transcript, as the model will read it.

        The prompt only ever sees a display name, never a QQ id, so without the owner tag
        the model cannot tell who its owner is once they change their group card. The tag
        is derived from the (stable) user id at receive time, so a given message always
        renders identically and the history stays cache-safe.

        `seq` is this line's number in whatever is being shown, `quote` the pointer to
        the line it replies to, `member_no` the speaker's member number, and
        `mention_number` numbers direct @ targets. All are worked out per prompt - see
        prompt.numbered and core.member_numbers.

        Every line carries its send time. Without one the model reads sixty messages
        as one continuous conversation and bridges topics hours apart. The stamp is
        the message's own fixed moment, so it never changes between turns and the
        history stays cache-safe - unlike any relative form ("5 minutes ago"), which
        would invalidate the prefix on every reply.
        """
        text = self.numbered_text(pic_nums)
        if mention_number is not None and self.mentions:
            text = number_at_mentions(text, self.mentions, mention_number)
        body = f"{quote} {text}".strip() if quote else text
        head = f"#{seq} " if seq else ""
        when = sysmark(self.ts.strftime("%m-%d %H:%M")) + " "
        if self.is_bot:
            name = self.nickname or "机器人"
            return f"{head}{when}{name}{sysmark(str(BOT_DISPLAY_NUMBER))}: {body}"
        no = sysmark(str(member_no)) if member_no is not None else ""
        tag = OWNER_TAG if self.is_owner else ""
        return f"{head}{when}{self.nickname}{no}{tag}: {body}"


@dataclass(slots=True, weakref_slot=True)
class ChatMsg(TranscriptRendering):
    msg_id: MessageId
    user_id: AccountId
    nickname: str
    text: str
    ts: datetime
    raw_event_id: uuid.UUID | None = None
    is_bot: bool = False
    is_owner: bool = False
    #: The QQ id of the message this one quotes, if any. What the model is shown is a
    #: pointer to that message's line number - see prompt.numbered.
    reply_to: MessageId | None = None
    #: Accounts mentioned by a member message, paired with the display name carried by
    #: the original segment. Prompt-local member numbers are projected from this list.
    mentions: list[tuple[AccountId, str]] = field(default_factory=list)
    #: Whom one of the bot's own messages @-ed, as (account, display name) pairs in
    #: send order. Kept apart from `text` so the prompt can show each target with
    #: the number it wears in that render; members' own @-mentions stay in their
    #: text as names.
    at: list[tuple[AccountId, str]] = field(default_factory=list)
    #: Exact ordered segments for the bot's own structured replies. Member messages
    #: remain empty because their parsed media references have a different purpose.
    outbound: tuple[HistoricalSegment, ...] = ()
    #: The message's picture references, forwarded ones included, in the order
    #: their markers render. They are stable inputs for open_images; asynchronous
    #: resolution and retry ownership live in MediaCoordinator, not in this value.
    image_refs: list[ImageRef] = field(default_factory=list)
    #: Assigned after durable admission, not from a pre-commit database sequence.
    arrival_seq: int = 0


@dataclass
class GroupState:
    group_id: GroupId
    history: HistoryWindow = field(default_factory=HistoryWindow, kw_only=True)
    groups: GroupRepository | None = field(default=None, kw_only=True, repr=False)
    archive: ArchiveRepository | None = field(default=None, kw_only=True, repr=False)
    display_zone: ZoneInfo = field(default_factory=lambda: ZoneInfo("UTC"), kw_only=True)
    persona_name: str = field(default="", kw_only=True)
    custom_persona: bool = field(default=False, kw_only=True)
    recent: deque[ChatMsg] = field(default_factory=deque)
    history_anchor: str | None = None
    muted: bool = False
    loaded: bool = False
    history_loaded: bool = False
    arrival_seq: int = field(default=0, init=False)
    _history_lock: asyncio.Lock = field(
        default_factory=asyncio.Lock, init=False, repr=False, compare=False
    )

    def __post_init__(self) -> None:
        initial = tuple(self.recent)
        self.recent = deque(maxlen=self.history.capacity)
        for message in initial:
            self.add(message)

    def add(self, msg: ChatMsg) -> None:
        """Append an event already accepted by the database admission gate."""

        msg.ts = msg.ts.astimezone(self.display_zone)
        self.arrival_seq += 1
        msg.arrival_seq = self.arrival_seq
        self.recent.append(msg)

    @property
    def policy(self) -> GroupRepository:
        if self.groups is None:
            raise RuntimeError("this pure conversation state has no storage capability")
        return self.groups

    async def blocked_now(self, user_id: str) -> bool:
        """Resolve exact-account and linked-holder rules against current identity."""

        return await self.policy.blocked(self.group_id, user_id)

    async def load(self) -> None:
        """Read this group's persisted switches, and claim it if it is new.

        The first touch of a group in this process is the only place that can notice the
        group is new at all: there is no allowlist to be added to, so a group appears by
        talking. Claiming here rather than in the message path costs one query per group
        per process and cannot be forgotten by a caller.
        """
        if self.loaded:
            return
        if await self.policy.note_group_seen(self.group_id):
            named = self.custom_persona
            log.info(
                "group %s: first message, now being served (%s)",
                self.group_id,
                "own persona" if named else "default persona",
            )
        self.muted = await self.policy.group_muted(self.group_id)
        self.loaded = True

    async def load_history(self, *, self_id: str, owners) -> None:
        """Load one complete archive snapshot before any concurrent caller uses it."""
        async with self._history_lock:
            if not self.history_loaded:
                await self._hydrate_history(self_id=self_id, owners=owners)

    async def _hydrate_history(self, *, self_id: str, owners) -> None:
        """Rebuild the conversation window from the archive, once per process.

        This is what lets the bot rejoin a conversation after a deploy knowing what was
        being discussed - the archive holds every message, its own replies included. The
        stored reading is used as stored: pictures arrive already described, quotes keep
        their pointer.

        `self_id` and `owners` come from the caller because history loads on the first
        real message or scheduled wakeup, not when commands touch the group. Filled
        into a local list first: a failure mid-read leaves the deque untouched and
        the flag unset, so the next message simply tries again.
        Lines already in the deque (a command's answer, a notice that arrived
        first) are kept, behind the archive's: they are the newer ones.
        """
        if self.archive is None:
            raise RuntimeError("conversation history has no archive capability")
        rows = await self.archive.recent(self.group_id, limit=self.recent.maxlen or 50)
        display_zone = self.display_zone
        msgs: list[ChatMsg] = []
        for archived in rows:
            if not isinstance(archived, ArchivedMessage):
                raise TypeError("archive repository returned a non-canonical row")
            text = archived.text
            uid = archived.sender.account_id
            if not text:
                continue
            name = archived.sender.display_name
            # The payload keeps the segments verbatim, so picture references survive
            # a restart: re-parsed here, they are what lets open_images hand over a
            # picture posted before the deploy. Parsing is pure and costs nothing.
            segs = archived.onebot_segments()
            is_bot = archived.author_kind is AuthorKind.BOT
            at: list[tuple[AccountId, str]] = []
            mentions: list[tuple[AccountId, str]] = []
            outbound: tuple[HistoricalSegment, ...] = ()
            if is_bot:
                outbound = from_onebot(segs)
                at = list(archived.mentions)
                refs = []
            else:
                parsed = (
                    parse_segments(
                        segs,
                        str(archived.self_id or self_id),
                        display_zone=display_zone,
                        self_name=self.persona_name,
                    )
                    if segs
                    else None
                )
                refs = parsed.pictures if parsed is not None else []
                mentions = list(archived.mentions)
            msgs.append(
                ChatMsg(
                    msg_id=archived.message_id,
                    user_id=uid,
                    nickname=name,
                    text=text,
                    ts=archived.occurred_at.astimezone(self.display_zone),
                    raw_event_id=archived.raw_event_id,
                    is_bot=is_bot,
                    is_owner=not is_bot and uid in owners,
                    reply_to=archived.reply_to,
                    image_refs=refs,
                    mentions=mentions,
                    at=at,
                    outbound=outbound,
                )
            )
        archived = {m.msg_id for m in msgs}
        live = [m for m in self.recent if m.msg_id not in archived]
        for seq, message in enumerate(msgs, start=-len(msgs)):
            message.arrival_seq = seq
        self.recent = deque(msgs + live, maxlen=self.history.capacity)
        self.history_loaded = True
        if msgs:
            log.info(
                "group %s: rebuilt %d message(s) of history from the archive",
                self.group_id,
                len(msgs),
            )

    async def persist(self) -> None:
        """Persist the mute flag; block rules have their own dynamic repository."""
        await self.policy.set_group_muted(self.group_id, self.muted)


class Registry:
    def __init__(
        self,
        *,
        groups: GroupRepository,
        archive: ArchiveRepository,
        bundle: ConfigBundle,
        clock: Clock,
        history_messages: int = 90,
    ) -> None:
        self._policies = groups
        self._archive = archive
        self._clock = clock
        self._bundle = bundle
        self._history = HistoryWindow(history_messages)
        self._groups: dict[GroupId, GroupState] = {}

    async def get(self, group_id: GroupId) -> GroupState:
        st = self._groups.get(group_id)
        if st is None:
            st = GroupState(
                group_id=group_id,
                history=self._history,
                groups=self._policies,
                archive=self._archive,
                display_zone=self._clock.zone,
                persona_name=self._bundle.persona_for(group_id).name,
                custom_persona=group_id in self._bundle.personas,
            )
            self._groups[group_id] = st
        if not st.loaded:
            await st.load()
        return st

    def loaded(self, group_id: GroupId) -> GroupState | None:
        """The state of a group already in memory, or None.

        For a caller that has to correct in-memory state it did not arrive
        through - a merge changes who is blocked in other groups too, and their
        cached blocklists would otherwise be stale until a restart. Deliberately
        not get(): that would claim and load a group nobody has spoken in.

        Known and accepted: the returned state may still be mid-load(), and a
        load whose switches query ran before the caller's DB write can overwrite
        the correction when its assignment lands. That needs a group's very first
        touch in the process to interleave with a /merge; the DB stays right and
        a restart heals the copy.
        """
        return self._groups.get(group_id)

    def all(self) -> list[GroupState]:
        return list(self._groups.values())
