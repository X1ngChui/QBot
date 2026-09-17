"""What the bot knows about the people in one group, and how an owner corrects it.

This is the application service the ops commands sit on. It is its own module because
`plugins/commands.py` cannot be imported without a live NoneBot runtime - `on_command()`
runs at import time - so any logic written inside a handler sits in a blind spot the
test suite cannot reach. Everything here is plain async Python over the repositories,
so the suite exercises the real thing.

The shape it returns - one card per *person*, not per account - is the point of the
identity layer. Two accounts an owner has merged are one line here, with one set of names
and one set of facts, and no caller has to know the merge happened.
"""

from __future__ import annotations

import logging
import uuid
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime, UTC

from ..domain.identity import (ALIAS_MAX_CHARS, Alias, AliasEvidence, AliasType,
                               EvidenceType, normalize)
from ..domain.memory import Fact, MemoryType
from ..repositories import (
    EventRepository, IdentityRepository, JobQueue, MemoryRepository,
)
from ..db import repo as db_repo
from ..repositories.job import JobType
from ..settings import config
from ..util import now_local
from .context_builder import NOTE, render_fact
from .identity_resolver import IdentityResolver, UnknownAccount
from .memory_extractor import GROUP_TOPIC

log = logging.getLogger("qqbot.directory")

#: Confidence given to anything an owner types. Equal to platform identity and above
#: every inference, because it is the one input the model is not allowed to overrule -
#: a correction that the next extraction round could outvote would not be a correction.
MANUAL_CONFIDENCE = 1.0


def _check_length(text: str) -> None:
    """A name longer than the column is not a name; said in words, before the
    store says it in an error the command could not answer."""
    if len(text.strip()) > ALIAS_MAX_CHARS:
        raise ValueError(f"称呼太长，最多 {ALIAS_MAX_CHARS} 个字。")


class NameTaken(ValueError):
    """This name already points at somebody else in this group.

    Its own class because the command layer answers it differently from every other
    failure: nothing broke, the owner is being told who has the name.
    """

    def __init__(self, text: str, holder: str) -> None:
        super().__init__(f"{text!r} already resolves to {holder}")
        self.text = text
        #: How the other person shows up in this group, so the answer can name them.
        self.holder = holder


class NotMerged(ValueError):
    """This account is already a person of its own; there is nothing to split.

    Its own class because the command layer answers it as a plain "nothing to do":
    splitting a lone account would create an empty person and strand every fact
    under the old one, so it is refused rather than performed.
    """

    def __init__(self, user_id: str) -> None:
        super().__init__(f"account {user_id} is not merged with anything")
        self.user_id = user_id
        #: What the owner is told.
        self.message = "这个账号没有和别的账号合并过，不需要拆分。"


@dataclass(frozen=True, slots=True)
class NameCard:
    """One name this person answers to, with what backs it."""

    text: str
    kind: AliasType
    confidence: float
    is_global: bool = False

    @property
    def platform_given(self) -> bool:
        """Reported by QQ rather than observed in conversation."""
        return self.kind in (AliasType.GROUP_CARD, AliasType.QQ_NICKNAME)


@dataclass(frozen=True, slots=True)
class FactCard:
    """One current fact, numbered so that an owner can point at it.

    The number is what /forget takes. It is positional within a single listing rather
    than stored, so it has to be produced by the same call that displays them - which is
    why the listing and the deletion both go through this service instead of the handler
    assembling one of them itself.
    """

    index: int
    id: uuid.UUID
    predicate: str
    object_key: str | None
    object: str
    #: The Wilson lower bound earned from distinct supporting events - see
    #: domain.memory.earned_confidence. The number shown to an owner: "how sure" and
    #: "how much evidence" in one unit.
    confidence: float

    @property
    def manual(self) -> bool:
        return self.predicate == NOTE

    @property
    def text(self) -> str:
        return render_fact(self.predicate, self.object, self.object_key)


@dataclass(frozen=True, slots=True)
class PersonCard:
    """Everything the group knows about one person."""

    entity_id: uuid.UUID
    user_id: str
    display: str
    accounts: tuple[str, ...] = ()
    messages: int = 0
    names: tuple[NameCard, ...] = ()
    #: Names on record but below the line the bot uses - the model's unconfirmed guesses
    #: and first-day platform names. Shown to an owner because these are exactly what the
    #: confidence-setting form of /alias adjusts, and a lever over invisible state is not
    #: a lever.
    candidates: tuple[NameCard, ...] = ()
    facts: tuple[FactCard, ...] = ()

    @property
    def merged(self) -> bool:
        """More than one account files under this person."""
        return len(self.accounts) > 1

    @property
    def other_names(self) -> tuple[str, ...]:
        """Names besides the one currently shown in the group."""
        return tuple(dict.fromkeys(
            n.text for n in self.names if n.text != self.display))

    @property
    def displayed_names(self) -> tuple[str, ...]:
        """Names this account has itself displayed - a group card or a platform nickname -
        other than the one showing now.

        Kept apart from the nicknames below because they are a different kind of claim.
        This one the platform reported: the account really did carry that name. What the
        group calls somebody is a claim about usage, and it can be wrong.
        """
        return tuple(dict.fromkeys(
            n.text for n in self.names
            if n.platform_given and n.text != self.display))

    @property
    def nicknames(self) -> tuple[str, ...]:
        """What people call this person, as opposed to what the account displays."""
        return tuple(dict.fromkeys(
            n.text for n in self.names
            if not n.platform_given and n.text != self.display))

    @property
    def note(self) -> str:
        """What an owner wrote by hand, if anything."""
        return "；".join(f.object for f in self.facts if f.manual)

    @property
    def learned(self) -> tuple[FactCard, ...]:
        """What the model worked out, as opposed to what was typed in."""
        return tuple(f for f in self.facts if not f.manual)

    @property
    def summary(self) -> str:
        """The learned facts as one phrase, in the same words the prompt uses."""
        return "；".join(f.text for f in self.learned if f.text)


def _current_platform_name(aliases) -> str:
    """The name the account is wearing right now, judged by recency.

    Display and trust are different axes. A group card is how the account presents itself
    from its first message - what takes a second day is believing the card to be a durable
    *name*. And "right now" is decided by when each was last seen, not by confidence: after
    a rename, the old confirmed card outweighs the new one on certainty precisely because
    it endured, but the account is no longer wearing it.
    """
    platform = [a for a in aliases
                if a.alias_type in (AliasType.GROUP_CARD, AliasType.QQ_NICKNAME)]
    if not platform:
        return ""
    epoch = datetime.min.replace(tzinfo=UTC)
    newest = max(platform,
                 key=lambda a: (a.last_used_at or a.valid_from or epoch, a.confidence))
    return newest.alias_text


class Directory:
    """Reads and corrections over one group's people.

    Every method takes group_id explicitly. It is not stored on the instance on purpose:
    a Directory bound to a group would be one shared instance away from answering with
    another group's memory, and that failure would be silent.
    """

    def __init__(
        self,
        identity: IdentityResolver,
        ids: IdentityRepository,
        memory: MemoryRepository,
        events: EventRepository,
        jobs: JobQueue,
    ) -> None:
        """Every collaborator is required, `jobs` included.

        No optional collaborators backed by runtime None-checks: a check that fires at
        the moment somebody uses the feature is a check that ships broken. Required, a
        missing dependency is a TypeError at wiring time.
        """
        self._identity = identity
        self._ids = ids
        self._memory = memory
        self._events = events
        self._jobs = jobs

    # -- reads ------------------------------------------------------------
    async def roster(
        self, group_id: int, *, display: dict[str, str] | None = None,
        exclude: set[str] | None = None,
    ) -> list[PersonCard]:
        """Everyone who has spoken here, most talkative first.

        The order is deliberately unrelated to who spoke last. This list is rendered
        into the system block, ahead of the history, and prefix caching only matches
        forward from the start - ordering it by recency reshuffles it every turn and
        invalidates the whole prompt behind it.

        `display` supplies the live group card per account when the caller has one; the
        stored aliases are the fallback, which is what a member who has since left still
        renders as.
        """
        counts = await self._events.speaker_counts(group_id)
        for uid in exclude or ():
            counts.pop(uid, None)
        if not counts:
            return []

        by_entity: dict[uuid.UUID, list[str]] = {}
        for uid in counts:
            acc = await self._ids.account_of("qq", uid)
            if acc is None:
                continue
            # An account's entity is always the live one (merge repoints every
            # account), so two merged accounts land on one card without a chase.
            by_entity.setdefault(acc.entity_id, []).append(uid)

        cards = [
            await self._card(group_id, eid, uids, counts, display or {})
            for eid, uids in by_entity.items()
        ]
        cards.sort(key=lambda c: (-c.messages, c.user_id))
        return cards

    async def person(self, group_id: int, user_id: str) -> PersonCard:
        """One person, named by the account that was @-ed.

        Raises UnknownAccount if this account has never been seen - which is a different
        answer from "nothing is known about them yet", and the caller says so differently.
        """
        acc = await self._identity.account(user_id)
        eid = acc.entity_id
        accounts = [a.platform_user_id for a in await self._ids.accounts_of(eid)]
        counts = await self._events.speaker_counts(group_id)
        return await self._card(group_id, eid, accounts or [user_id], counts, {})

    async def accounts_of_person(self, user_id: str) -> list[str]:
        """Every account the person behind this one holds, the given one included.

        A merge makes several accounts one person, and anything an owner decides
        about a person has to reach all of them - being blocked on the account
        that was @-ed while the alt keeps talking is the whole reason this exists.
        An account nobody has seen yet is a person of one: the caller acts on what
        it was given rather than failing.
        """
        return await db_repo.accounts_sharing_person(user_id)

    async def _card(
        self, group_id: int, entity_id: uuid.UUID, accounts: list[str],
        counts: dict[str, int], display: dict[str, str],
    ) -> PersonCard:
        aliases = await self._ids.aliases_for(group_id, entity_id)
        usable = [a for a in aliases if a.is_usable]
        facts = await self._memory.current_facts(group_id, [entity_id])

        # Whichever of this person's accounts talks the most is the one they are filed
        # under: it is the account whose group card people actually see.
        primary = max(accounts, key=lambda u: (counts.get(u, 0), u))
        shown = (display.get(primary) or "").strip() or next(
            (display[u] for u in accounts if (display.get(u) or "").strip()), ""
        ).strip()
        if not shown:
            shown = _current_platform_name(aliases) or primary

        # Sorted by predicate rather than by confidence so the numbering an owner reads
        # off /who is still the same numbering a moment later when they type /forget.
        ordered = sorted(facts, key=lambda f: (f.predicate != NOTE, f.predicate, str(f.id)))
        return PersonCard(
            entity_id=entity_id,
            user_id=primary,
            display=shown,
            accounts=tuple(sorted(accounts)),
            messages=sum(counts.get(u, 0) for u in accounts),
            names=tuple(
                NameCard(a.alias_text, a.alias_type, a.confidence, a.is_global)
                for a in usable
            ),
            candidates=tuple(
                NameCard(a.alias_text, a.alias_type, a.confidence, a.is_global)
                for a in aliases if not a.is_usable
            ),
            facts=tuple(
                FactCard(
                    index=i, id=f.id, predicate=f.predicate, object_key=f.object_key,
                    object="" if f.object_value is None else str(f.object_value),
                    confidence=f.confidence,
                )
                for i, f in enumerate(ordered, start=1)
            ),
        )

    async def group_facts(self, group_id: int) -> tuple[FactCard, ...]:
        """What is known about the group itself, numbered for /forget.

        The group is an entity, so what it is for and what its words mean are facts with
        the same evidence, supersession and ageing as anything else - each line can be
        checked, deleted and forgotten on its own.
        """
        subject = await self._ids.group_entity(group_id)
        facts = await self._memory.current_facts(group_id, [subject])
        ordered = sorted(facts,
                         key=lambda f: (f.predicate != GROUP_TOPIC, f.predicate,
                                        str(f.id)))
        return tuple(
            FactCard(
                index=i, id=f.id, predicate=f.predicate, object_key=f.object_key,
                object="" if f.object_value is None else str(f.object_value),
                confidence=f.confidence,
            )
            for i, f in enumerate(ordered, start=1)
        )

    async def forget_group_fact(self, group_id: int, index: int) -> FactCard | None:
        """Retract one of the group's own facts by the number /card showed."""
        match = next((f for f in await self.group_facts(group_id) if f.index == index),
                     None)
        if match is None:
            return None
        await self._memory.retract(match.id)
        log.info("group %s: retracted group fact %s (%s)",
                 group_id, match.id, match.predicate)
        return match

    # -- corrections ------------------------------------------------------
    async def note(self, group_id: int, user_id: str, text: str) -> None:
        """Write, replace or clear the hand-written note about somebody.

        Stored as an ordinary fact under its own predicate rather than off to one side.
        That gets it three things for free: it flows into the prompt through the same
        path as everything else, `supersede` guarantees there is only ever one of them,
        and clearing it is a retraction that leaves a record instead of a deletion that
        does not.

        It never collides with an extracted fact because no extracted predicate is
        `note` - the tool schema the model answers through does not offer it.
        """
        entity_id = await self._entity(user_id)
        if not text.strip():
            for f in await self._memory.current_facts(group_id, [entity_id]):
                if f.predicate == NOTE:
                    await self._memory.retract(f.id)
            return
        await self._memory.supersede(
            Fact(
                subject_entity_id=entity_id,
                predicate=NOTE,
                object_value=text.strip(),
                memory_type=MemoryType.ATTRIBUTE,
                group_id=group_id,
                confidence=MANUAL_CONFIDENCE,
            ),
            [],
            when=now_local(),
        )

    async def name(self, group_id: int, user_id: str, text: str) -> NameCard:
        """Bind a name to somebody by hand.

        The escape hatch for names the group uses that the transcript never spells out -
        the ones people say out loud and type at nobody. MANUAL evidence carries the
        weight of platform identity, so the alias is confirmed on the spot rather than
        waiting for a second sighting.

        Raises NameTaken if somebody else here already answers to it. A name belongs to
        one person - the same rule under which the validator refuses a batch pointing one
        name at two accounts, and the path that carries the most authority must not be
        the one path allowed to break it. Retire the name from the other person first if
        that is really what is wanted.
        """
        _check_length(text)
        entity_id = await self._entity(user_id)
        for other in await self._ids.lookup(group_id, text):
            if other.target_entity_id != entity_id:
                raise NameTaken(text.strip(),
                                await self._display_of(group_id, other.target_entity_id,
                                                       excluding=text))
        alias = await self._ids.upsert_alias(
            Alias(
                alias_text=text.strip(),
                target_entity_id=entity_id,
                group_id=group_id,
                alias_type=AliasType.NICKNAME,
            ),
            [AliasEvidence(EvidenceType.MANUAL)],
        )
        log.info("group %s: alias %r bound to entity %s by hand",
                 group_id, text.strip(), entity_id)
        return NameCard(alias.alias_text, alias.alias_type, alias.confidence,
                        alias.is_global)

    async def set_confidence(
        self, group_id: int, user_id: str, text: str, confidence: float,
    ) -> NameCard:
        """Set how much one name is trusted, by hand.

        The lever between "certain" and "struck out", for the common correction: a name
        that is real but over-trusted - a short-lived joke card the platform reported, a
        nickname only half the group uses. Below 0.75 the name stays
        on record but the bot stops using it; at or above, it is usable on the spot. The
        number is the owner's verdict: automatic sightings cannot outvote it either way.

        Setting a name this person does not yet carry coins it at that confidence.
        Raises NameTaken if somebody else here answers to it.
        """
        confidence = max(0.0, min(1.0, confidence))
        _check_length(text)
        entity_id = await self._entity(user_id)
        for other in await self._ids.lookup(group_id, text):
            if other.target_entity_id != entity_id:
                raise NameTaken(text.strip(),
                                await self._display_of(group_id, other.target_entity_id,
                                                       excluding=text))
        updated = await self._ids.set_alias_confidence(
            group_id, entity_id, text, confidence)
        if updated is None:
            updated = await self._ids.upsert_alias(
                Alias(
                    alias_text=text.strip(),
                    target_entity_id=entity_id,
                    group_id=group_id,
                    alias_type=AliasType.NICKNAME,
                ),
                [AliasEvidence(EvidenceType.MANUAL, score=confidence)],
            )
        log.info("group %s: alias %r confidence set to %.2f by hand",
                 group_id, text.strip(), confidence)
        return NameCard(updated.alias_text, updated.alias_type, updated.confidence,
                        updated.is_global)

    async def unname(self, group_id: int, user_id: str, text: str) -> bool:
        """Retire a name. False if this person does not answer to it.

        Retired, not deleted: messages already in the archive still need it to be
        resolvable, and dropping the row would make them unreadable in a way nothing
        could reconstruct.
        """
        entity_id = await self._entity(user_id)
        wanted = normalize(text)
        gone = 0
        # Every matching row, not the first. A merged person can carry the same name once
        # per pre-merge account, and retiring one row of two leaves the name showing -
        # which reads as the command having silently failed.
        for a in await self._ids.aliases_for(group_id, entity_id):
            # A group's command reaches the group's rows only; a global alias
            # (group_id NULL) is not this group's to retire.
            if a.normalized_text == wanted and not a.is_global:
                await self._ids.retire_alias(a.id)
                gone += 1
        return gone > 0

    async def forget(self, group_id: int, user_id: str, index: int) -> FactCard | None:
        """Retract one fact by the number /who showed against it.

        Numbered rather than described because the alternative is matching on text, and
        an owner retyping a fact they want gone is the one moment where a near-miss
        deletes the wrong one.
        """
        card = await self.person(group_id, user_id)
        match = next((f for f in card.facts if f.index == index), None)
        if match is None:
            return None
        await self._memory.retract(match.id)
        log.info("group %s: retracted fact %s (%s) on entity %s",
                 group_id, match.id, match.predicate, card.entity_id)
        return match

    async def merge(self, loser: str, winner: str) -> bool:
        """File two accounts under one person. See IdentityResolver.merge.

        A block follows the merge. Being blocked is a decision about a *person*,
        and the moment two accounts become one person a block that reached only one
        of them is half a decision: wherever either was blocked, both now are.
        Returns whether the merge itself did anything; the propagation is reported
        through the returned group list of `blocks_after_merge`.
        """
        return await self._identity.merge(loser, winner)

    async def blocks_after_merge(
        self, user_id: str, *, shielded: Callable[[int], bool] | None = None,
    ) -> list[tuple[int, datetime | None]]:
        """Extend each existing block to every account of this person.

        Called right after a merge. Returns (group, lapses-at) for each group whose
        blocklist changed - the expiry rides along so the caller can refresh the
        in-memory copies (state.GroupState.blocked; a row written behind its back
        would not take effect until a restart). A timed block spreads with its own
        clock: the merge widens who is blocked, never for how long.

        `shielded(gid)` lets the caller veto a group: /block refuses the owner and
        the bot itself, and a merge must not smuggle either onto a blocklist. The
        veto is a callback because who counts as owner is per-group config, which
        this layer does not read.
        """
        accounts = await self.accounts_of_person(user_id)
        if len(accounts) < 2:
            return []
        touched = []
        for gid, until in await db_repo.groups_blocking(accounts):
            if shielded is not None and shielded(gid):
                continue
            await db_repo.block(gid, accounts, until=until)
            touched.append((gid, until))
        return touched

    async def split(self, user_id: str) -> uuid.UUID:
        """Give one account its own person again. See IdentityResolver.split.

        Raises NotMerged when the account is the only one its person holds. A split
        moves the account to a fresh person and leaves facts and episodes with the
        old one, on the evidence that they may belong to the other account - with no
        other account, that would orphan everything ever learned about this person
        for nothing.
        """
        acc = await self._identity.account(user_id)
        if len(await self._ids.accounts_of(acc.entity_id)) < 2:
            raise NotMerged(user_id)
        return await self._identity.split(user_id)

    async def relearn(self, group_id: int) -> uuid.UUID | None:
        """Ask for an extraction pass now instead of at the next batch boundary.

        The watermark is reset first, because the worker refuses to pay for a batch
        nobody has added to since the last pass - correct for every automatic trigger,
        and exactly wrong for an owner explicitly asking for a re-read. Resetting makes
        the recent window count as unread again, so whichever queued job runs (this one,
        or a pending duplicate that swallowed it) does the work.

        Queued rather than run inline: it is a paid model call, and a command that waits
        on one holds the handler open long enough for QQ to time the reply out. The
        worker reports what it found through the ordinary path.
        """
        # Pull back exactly one window, and force past the drain floor: an owner
        # asking for a re-read gets one however few messages there are.
        await db_repo.reset_extract_watermark(
            group_id, keep=config().default.memory.extract_window)
        jid = await self._jobs.submit(
            JobType.EXTRACT_MEMORY, {"group_id": group_id, "force": True}, priority=2
        )
        if jid is None:
            # A pending twin (the nightly job, or one in retry backoff) swallowed
            # the submit - and with it the force flag, which must reach whichever
            # job actually runs: without it a group under the drain floor answers
            # the owner's explicit /relearn with "not worth a pass".
            amended = await self._jobs.amend_pending(
                JobType.EXTRACT_MEMORY, group_id, {"force": True})
            if not amended:
                # The twin was claimed between the collision and the amend, so it
                # runs with whatever payload it had. It is no longer pending, so a
                # fresh submit is legal - the dedup index covers pending rows only.
                jid = await self._jobs.submit(
                    JobType.EXTRACT_MEMORY, {"group_id": group_id, "force": True},
                    priority=2)
        return jid

    async def _display_of(self, group_id: int, entity_id: uuid.UUID, *,
                          excluding: str = "") -> str:
        """How somebody shows up here, for an answer that has to name them.

        Cheaper than a whole card: this is only ever wanted to say who already holds a
        name, and the card would cost a fact read and an evidence count to say it.
        `excluding` is the contested name itself, which would name nobody; a person
        with no other name here is called another member rather than by an id.
        """
        aliases = await self._ids.aliases_for(group_id, entity_id)
        skip = normalize(excluding)
        return (_current_platform_name(aliases)
                or next((a.alias_text for a in aliases
                         if a.is_usable and a.normalized_text != skip), "")
                or "另一位成员")

    async def _entity(self, user_id: str) -> uuid.UUID:
        # The live person: merge() repoints every account, so no chase is needed.
        return (await self._identity.account(user_id)).entity_id


__all__ = ["Directory", "PersonCard", "FactCard", "NameCard", "UnknownAccount",
           "NameTaken", "NotMerged"]
