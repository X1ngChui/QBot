"""Per-group runtime state.

In memory: the recent messages (section 6.1, immediate context) and the reply rate
window. What has and has not been read into long-term memory is tracked in SQL instead -
a restart should not send a nearly-full batch back to zero.
Persisted: the mute switch and the blocklist, in group_state, so a restart does not
lose them.
"""

from __future__ import annotations

import logging
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime

from ..db import repo
from ..settings import config
from ..util import defang, fmt_when, now_local, sysmark, why
from .segments import ImageRef, parse_segments

log = logging.getLogger("qqbot.state")

#: Worn in the system brackets so no display name can imitate it: defang()
#: neutralizes the pair in every member-controlled string before it renders.
OWNER_TAG = sysmark("拥有者")


@dataclass
class ChatMsg:
    msg_id: str
    user_id: str
    nickname: str
    text: str
    ts: datetime
    is_bot: bool = False
    is_owner: bool = False
    #: The QQ id of the message this one quotes, if any. What the model is shown is a
    #: pointer to that message's line number - see prompt.numbered.
    reply_to: str | None = None
    #: Accounts this message addressed. Carried so retrieval can load their profiles:
    #: being talked *about* is as good a reason as speaking.
    mentions: list[str] = field(default_factory=list)
    #: The parsed message, kept only while it still holds a picture or voice clip nobody
    #: has paid to understand. People post a picture and ask about it in the *next*
    #: message, by which time this one has been processed and its refs would otherwise be
    #: gone - see pipeline._settle_backlog, which pays for these late.
    #:
    #: Typed loosely because media imports nothing from here and this module must not
    #: import media back.
    pending: object | None = None
    #: Where the vision backend filed this message's pictures, in segment order. The
    #: prompt attaches the newest few of these to the reply so the model reads the
    #: original pixels, not only the one-line description in the text.
    images: list[str] = field(default_factory=list)
    #: The message's picture references (segments.ImageRef), kept for as long as the
    #: message is in the window - unlike `pending`, which is unpaid *work* and is
    #: cleared once settled. This is what lets the inspect_image tool reopen its eyes
    #: on a picture whose one-line description has already been bought: QQ's file id
    #: trades for a fresh link at any time (see media._bytes). Typed loosely for the
    #: same reason `pending` is.
    image_refs: list = field(default_factory=list)

    def render(self, *, seq: int = 0, quote: str = "") -> str:
        """One line of transcript, as the model will read it.

        The prompt only ever sees a display name, never a QQ id, so without the owner tag
        the model cannot tell who its owner is once they change their group card. The tag
        is derived from the (stable) user id at receive time, so a given message always
        renders identically and the history stays cache-safe.

        `seq` is this line's number in whatever is being shown, and `quote` the pointer to
        the line it replies to. Both are worked out per prompt - see prompt.numbered.

        Every line carries its send time. Without one the model reads sixty messages as
        one continuous conversation and bridges topics hours apart (a real complaint,
        not a hypothetical). The stamp is the message's own fixed moment, so it never
        changes between turns and the history stays cache-safe - unlike any relative
        form ("5 minutes ago"), which would invalidate the prefix on every reply.
        """
        body = f"{quote} {self.text}".strip() if quote else self.text
        head = f"#{seq} " if seq else ""
        when = sysmark(fmt_when(self.ts)) + " "
        if self.is_bot:
            return f"{head}{when}{body}"
        tag = OWNER_TAG if self.is_owner else ""
        return f"{head}{when}{self.nickname}{tag}: {body}"


@dataclass
class GroupState:
    group_id: str
    #: Must exceed prompt.HISTORY_MSGS, so the window - with its chunked, cache-stable
    #: eviction - always binds before the deque does. A deque-bound window slides one
    #: message per turn and invalidates the prefix on every reply.
    recent: deque[ChatMsg] = field(default_factory=lambda: deque(maxlen=200))
    history_anchor: str | None = None
    muted: bool = False
    #: Accounts this group's owner has told the bot not to answer: their messages
    #: still arrive, archive and feed memory - only the reply is withheld.
    #: Managed by /block from inside the group. Keyed by account
    #: id because an id is the one thing about a person that cannot be renamed
    #: around; the value is when the block lapses on its own, or None for one that
    #: waits for /unblock. Test membership through blocked_now, never `in` - a
    #: timed entry may already be dead.
    blocked: dict[str, datetime | None] = field(default_factory=dict)
    loaded: bool = False
    history_loaded: bool = False

    def add(self, msg: ChatMsg) -> None:
        # The pipeline's dedup set dies with the process, so a message the adapter
        # replays across a restart passes it - and load_history, triggered by that
        # same replay, has already rebuilt this deque from the archive including the
        # original. Without this guard both copies render, two transcript lines carry
        # the same #N, and quotes point at the wrong one until eviction clears it.
        if msg.msg_id and any(m.msg_id == msg.msg_id for m in self.recent):
            return
        self.recent.append(msg)

    async def blocked_now(self, user_id: str) -> bool:
        """Whether this account is blocked at this moment.

        A timed block expires by being noticed: the first message from a blocked
        account past its expiry lifts it - and sweeps every other lapsed entry in
        the group, since a person-wide block gave all its accounts the same clock.
        Lazy on purpose; a scheduler for something the hot path detects for free
        would be machinery for its own sake.
        """
        if user_id not in self.blocked:
            return False
        until = self.blocked[user_id]
        if until is None or until > now_local():
            return True
        self.blocked = {u: t for u, t in self.blocked.items()
                        if t is None or t > now_local()}
        try:
            await repo.unblock_expired(int(self.group_id))
        except Exception as e:
            # The sweep is opportunistic. A DB hiccup here must not take the
            # message down with it - the caller sits inside handle() holding the
            # dedup mark, and an exception would swallow the adapter's replay
            # too. The lapsed rows stay filtered at every load and get another
            # sweep on the next lapse.
            log.warning("group %s: expired-block sweep failed: %s",
                        self.group_id, why(e))
        log.info("group %s: timed block on %s lapsed", self.group_id, user_id)
        return False

    async def load(self) -> None:
        """Read this group's persisted switches, and claim it if it is new.

        The first touch of a group in this process is the only place that can notice the
        group is new at all: there is no allowlist to be added to, so a group appears by
        talking. Claiming here rather than in the message path costs one query per group
        per process and cannot be forgotten by a caller.
        """
        if self.loaded:
            return
        if await repo.note_group_seen(int(self.group_id)):
            named = self.group_id in config().personas
            log.info("group %s: first message, now being served (%s)", self.group_id,
                     "own persona" if named else "default persona")
        self.muted, self.blocked = await repo.group_switches(int(self.group_id))
        self.loaded = True

    async def load_history(self, *, self_id: str, owners) -> None:
        """Rebuild the conversation window from the archive, once per process.

        This is what lets the bot rejoin a conversation after a deploy knowing what was
        being discussed - the archive holds every message, its own replies included. The
        stored reading is used as stored: pictures arrive already described, quotes keep
        their pointer.

        `self_id` and `owners` come from the caller because only the message path has
        them, which is also why this is separate from load() - a command touching the
        group first loads the mute flag and nothing else, and the history fills in on the
        first real message. Filled into a local list first: a failure mid-read leaves the
        deque untouched and the flag unset, so the next message simply tries again.
        """
        if self.history_loaded:
            return
        msgs: list[ChatMsg] = []
        for r in await repo.recent_messages(int(self.group_id),
                                            limit=self.recent.maxlen or 50):
            payload = r["payload"] or {}
            text = (r["plain_text"] or "").strip()
            uid = (r["platform_user_id"] or "").strip()
            if not text or not uid:
                continue
            sender = payload.get("sender") or {}
            # The payload keeps the segments verbatim, so picture references survive
            # a restart: re-parsed here, they are what lets inspect_image reopen a
            # picture posted before the deploy. Parsing is pure and costs nothing.
            segs = payload.get("segments") or []
            refs = ([x for x in parse_segments(segs, self_id).refs
                     if isinstance(x, ImageRef)] if segs else [])
            msgs.append(ChatMsg(
                msg_id=str(r["platform_event_id"] or r["id"]),
                user_id=uid,
                nickname=defang((sender.get("card") or sender.get("nickname")
                                 or uid)).strip(),
                text=text,
                ts=r["occurred_at"],
                is_bot=uid == self_id,
                is_owner=uid in owners,
                reply_to=payload.get("reply_to") or None,
                image_refs=refs,
            ))
        if not self.recent:  # a live message that beat us here outranks the archive
            self.recent.extend(msgs)
        self.history_loaded = True
        if msgs:
            log.info("group %s: rebuilt %d message(s) of history from the archive",
                     self.group_id, len(msgs))

    async def persist(self) -> None:
        """The mute flag. The blocklist is not here: block/unblock write their own rows
        through repo.block/unblock at the moment the command runs, and this cache of it
        is refreshed on load()."""
        await repo.set_group_muted(int(self.group_id), self.muted)


class Registry:
    def __init__(self) -> None:
        self._groups: dict[str, GroupState] = {}

    async def get(self, group_id: str) -> GroupState:
        st = self._groups.get(group_id)
        if st is None:
            st = GroupState(group_id=group_id)
            self._groups[group_id] = st
        if not st.loaded:
            await st.load()
        return st

    def loaded(self, group_id: str) -> GroupState | None:
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


REGISTRY = Registry()
