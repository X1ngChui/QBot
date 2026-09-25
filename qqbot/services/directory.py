"""What the bot knows about people in one group and how authorized users correct it.

This application service keeps identity and memory operations independent of the adapter;
the importable command handlers and other callers exercise the same methods directly.

The shape it returns - one card per *person*, not per account - is the point of the
identity layer. Two accounts an owner has merged are one line here, with one set of names
and one set of facts, and no caller has to know the merge happened.
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass
from datetime import UTC, datetime

from ..domain.ids import GroupId
from ..domain.identity import (
    ALIAS_MAX_CHARS,
    Alias,
    AliasEvidence,
    AliasType,
    EvidenceType,
    IdentityAccount,
    normalize,
)
from ..domain.memory import Fact, MemoryType
from ..repositories import (
    EventRepository,
    IdentityRepository,
    MemoryRepository,
)
from ..util import now_local
from .context_builder import NOTE, render_fact, render_hint
from .identity_resolver import IdentityResolver, UnknownAccount
from .memory_extractor import GROUP_TOPIC

log = logging.getLogger("qqbot.directory")

#: Confidence given to an explicit command correction. Equal to platform identity and
#: above every inference, because a correction that the next extraction round could
#: outvote would not be a correction.
MANUAL_CONFIDENCE = 1.0


def _check_length(text: str) -> None:
    """A name longer than the column is not a name; said in words, before the
    store says it in an error the command could not answer."""
    if len(text.strip()) > ALIAS_MAX_CHARS:
        raise ValueError(f"称呼太长，最多 {ALIAS_MAX_CHARS} 个字。")


class NameTaken(ValueError):
    """This name already points at somebody else in this group.

    Its own class because the command layer answers it differently from every other
    failure: nothing broke, the caller is being told who has the name.
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
        #: What the caller is told.
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

    @property
    def hint(self) -> str:
        """An unconfirmed name is context, never an identity mapping."""

        kind = "未确认显示名" if self.platform_given else "未确认别名"
        return render_hint(kind, self.text, self.confidence)


@dataclass(frozen=True, slots=True)
class FactCard:
    """One current fact, numbered so an authorized caller can point at it.

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
    #: domain.memory.earned_confidence. The displayed number combines "how sure" and
    #: "how much evidence" in one unit.
    confidence: float
    account_id: uuid.UUID | None = None

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
    live_display: bool = False
    account_id: uuid.UUID | None = None
    accounts: tuple[str, ...] = ()
    messages: int = 0
    names: tuple[NameCard, ...] = ()
    #: Unconfirmed names remain visible as scored context, not as identity keys.
    candidates: tuple[NameCard, ...] = ()
    facts: tuple[FactCard, ...] = ()

    @property
    def merged(self) -> bool:
        """More than one account files under this person."""
        return len(self.accounts) > 1

    @property
    def other_names(self) -> tuple[str, ...]:
        """Names besides the one currently shown in the group."""
        return tuple(dict.fromkeys(n.text for n in self.names if n.text != self.display))

    @property
    def displayed_names(self) -> tuple[str, ...]:
        """Names this account has itself displayed - a group card or a platform nickname -
        other than the one showing now.

        Kept apart from the nicknames below because they are a different kind of claim.
        This one the platform reported: the account really did carry that name. What the
        group calls somebody is a claim about usage, and it can be wrong.
        """
        return tuple(
            dict.fromkeys(n.text for n in self.names if n.platform_given and n.text != self.display)
        )

    @property
    def nicknames(self) -> tuple[str, ...]:
        """What people call this person, as opposed to what the account displays."""
        return tuple(
            dict.fromkeys(
                n.text for n in self.names if not n.platform_given and n.text != self.display
            )
        )

    @property
    def note(self) -> str:
        """What an owner or the account holder wrote by hand, if anything."""
        return "；".join(f.object for f in self.facts if f.manual)

    @property
    def learned(self) -> tuple[FactCard, ...]:
        """What the model worked out, as opposed to what was typed in."""
        return tuple(f for f in self.facts if not f.manual)

    @property
    def memory_hints(self) -> tuple[str, ...]:
        """Current learned facts and candidate names share one scored context view."""

        return tuple(
            render_hint("事实", fact.text, fact.confidence)
            for fact in self.learned if fact.text
        ) + tuple(
            name.hint for name in self.candidates
            if not (self.live_display and name.text == self.display)
        )

    @property
    def summary(self) -> str:
        """A compact fact-only preview for directory commands."""
        return "；".join(f.text for f in self.learned if f.text)


def _current_platform_name(aliases, *, confirmed_only: bool = False) -> str:
    """The name the account is wearing right now, judged by recency.

    Display and trust are different axes. A group card is how the account presents itself
    from its first message - what takes a second day is believing the card to be a durable
    *name*. And "right now" is decided by when each was last seen, not by confidence: after
    a rename, the old confirmed card outweighs the new one on certainty precisely because
    it endured, but the account is no longer wearing it. Without live confirmation, a
    candidate cannot label the trusted roster, nor can the older confirmed card replace it.
    """
    platform = [a for a in aliases if a.alias_type in (AliasType.GROUP_CARD, AliasType.QQ_NICKNAME)]
    if not platform:
        return ""
    epoch = datetime.min.replace(tzinfo=UTC)
    newest = max(platform, key=lambda a: (a.last_used_at or a.valid_from or epoch, a.confidence))
    return newest.alias_text if not confirmed_only or newest.is_usable else ""


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
    ) -> None:
        """Every collaborator is required.

        No optional collaborators backed by runtime None-checks: a check that fires at
        the moment somebody uses the feature is a check that ships broken. Required, a
        missing dependency is a TypeError at wiring time.
        """
        self._identity = identity
        self._ids = ids
        self._memory = memory
        self._events = events

    # -- reads ------------------------------------------------------------
    async def roster(
        self,
        group_id: GroupId,
        *,
        display: dict[str, str] | None = None,
        exclude: set[str] | None = None,
    ) -> list[PersonCard]:
        """Everyone who has spoken here, most talkative first.

        The order is deliberately unrelated to who spoke last. This list is rendered
        into the system block, ahead of the history, and prefix caching only matches
        forward from the start - ordering it by recency reshuffles it every turn and
        invalidates the whole prompt behind it.

        `display` supplies the live group card per account when the caller has one. Without
        one, only confirmed platform names may label the reply roster; an unconfirmed
        stored display name stays a scored hint instead of becoming a trusted heading.
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

    async def account_card(self, group_id: GroupId, user_id: str) -> PersonCard:
        """The record attached to one exact platform account."""

        account = await self._identity.account(user_id)
        counts = await self._events.speaker_counts(group_id)
        aliases = await self._ids.aliases_for_account(group_id, account.id)
        facts = await self._memory.current_account_facts(group_id, [account.id])
        shown = _current_platform_name(aliases) or user_id
        return self._make_card(
            entity_id=account.entity_id,
            account_id=account.id,
            primary=user_id,
            accounts=[user_id],
            display=shown,
            counts=counts,
            aliases=aliases,
            facts=facts,
        )

    async def holder_card(self, group_id: GroupId, user_id: str) -> PersonCard:
        """The aggregate record for every account linked to the named account."""

        account = await self._identity.account(user_id)
        accounts = [a.platform_user_id for a in await self._ids.accounts_of(account.entity_id)]
        counts = await self._events.speaker_counts(group_id)
        return await self._card(
            group_id,
            account.entity_id,
            accounts or [user_id],
            counts,
            {},
        )

    async def account(self, user_id: str) -> IdentityAccount:
        """Resolve one platform account for command and policy services."""

        return await self._identity.account(user_id)

    async def accounts_of_holder(self, entity_id: uuid.UUID) -> list[IdentityAccount]:
        return await self._ids.accounts_of(entity_id)

    async def linked_account_ids(self, user_id: str) -> list[str]:
        """Every exact account currently linked to this one."""

        try:
            account = await self._identity.account(user_id)
        except UnknownAccount:
            return [user_id]
        return [item.platform_user_id for item in await self._ids.accounts_of(account.entity_id)]

    async def _card(
        self,
        group_id: GroupId,
        entity_id: uuid.UUID,
        accounts: list[str],
        counts: dict[str, int],
        display: dict[str, str],
    ) -> PersonCard:
        aliases = await self._ids.aliases_for(group_id, entity_id)
        facts = await self._memory.current_facts(group_id, [entity_id])
        primary = max(accounts, key=lambda user: (counts.get(user, 0), user))
        shown = (display.get(primary) or "").strip() or next(
            (display[user] for user in accounts if (display.get(user) or "").strip()),
            "",
        ).strip()
        live_display = bool(shown)
        if not shown:
            shown = _current_platform_name(aliases, confirmed_only=True) or primary
        return self._make_card(
            entity_id=entity_id,
            account_id=None,
            primary=primary,
            accounts=accounts,
            display=shown,
            live_display=live_display,
            counts=counts,
            aliases=aliases,
            facts=facts,
        )

    @staticmethod
    def _make_card(
        *,
        entity_id: uuid.UUID,
        account_id: uuid.UUID | None,
        primary: str,
        accounts: list[str],
        display: str,
        counts: dict[str, int],
        aliases: list[Alias],
        facts: list[Fact],
        live_display: bool = False,
    ) -> PersonCard:
        usable = [alias for alias in aliases if alias.is_usable]
        ordered = sorted(
            facts,
            key=lambda fact: (
                fact.predicate != NOTE,
                fact.predicate,
                str(fact.subject_account_id or ""),
                str(fact.id),
            ),
        )
        return PersonCard(
            entity_id=entity_id,
            account_id=account_id,
            user_id=primary,
            display=display,
            live_display=live_display,
            accounts=tuple(sorted(accounts)),
            messages=sum(counts.get(user, 0) for user in accounts),
            names=tuple(
                NameCard(alias.alias_text, alias.alias_type, alias.confidence, alias.is_global)
                for alias in usable
            ),
            candidates=tuple(
                NameCard(alias.alias_text, alias.alias_type, alias.confidence, alias.is_global)
                for alias in aliases
                if not alias.is_usable
            ),
            facts=tuple(
                FactCard(
                    index=index,
                    id=fact.id,
                    predicate=fact.predicate,
                    object_key=fact.object_key,
                    object="" if fact.object_value is None else str(fact.object_value),
                    confidence=fact.confidence,
                    account_id=fact.subject_account_id,
                )
                for index, fact in enumerate(ordered, start=1)
            ),
        )

    async def group_facts(self, group_id: GroupId) -> tuple[FactCard, ...]:
        """What is known about the group itself, numbered for /forget.

        The group is an entity, so what it is for and what its words mean are facts with
        the same evidence, supersession and ageing as anything else - each line can be
        checked, deleted and forgotten on its own.
        """
        subject = await self._ids.group_entity(group_id)
        facts = await self._memory.current_facts(group_id, [subject])
        ordered = sorted(facts, key=lambda f: (f.predicate != GROUP_TOPIC, f.predicate, str(f.id)))
        return tuple(
            FactCard(
                index=i,
                id=f.id,
                predicate=f.predicate,
                object_key=f.object_key,
                object="" if f.object_value is None else str(f.object_value),
                confidence=f.confidence,
            )
            for i, f in enumerate(ordered, start=1)
        )

    async def forget_group_fact(self, group_id: GroupId, index: int) -> FactCard | None:
        """Retract one of the group's own facts by the number /card showed."""
        match = next((f for f in await self.group_facts(group_id) if f.index == index), None)
        if match is None:
            return None
        await self._memory.retract(match.id)
        log.info("group %s: retracted group fact %s (%s)", group_id, match.id, match.predicate)
        return match

    # -- corrections ------------------------------------------------------
    async def note(
        self,
        group_id: GroupId,
        user_id: str,
        text: str,
        *,
        all_linked: bool = False,
    ) -> None:
        """Write, replace, or clear one exact-account or linked-holder note."""

        account = await self._identity.account(user_id)
        facts = (
            await self._memory.current_facts(group_id, [account.entity_id])
            if all_linked
            else await self._memory.current_account_facts(group_id, [account.id])
        )
        if not text.strip():
            for fact in facts:
                if fact.predicate == NOTE and (
                    (all_linked and fact.subject_entity_id is not None)
                    or (not all_linked and fact.subject_account_id == account.id)
                ):
                    await self._memory.retract(fact.id)
            return
        await self._memory.supersede(
            Fact(
                subject_entity_id=account.entity_id if all_linked else None,
                subject_account_id=None if all_linked else account.id,
                predicate=NOTE,
                object_value=text.strip(),
                memory_type=MemoryType.ATTRIBUTE,
                group_id=group_id,
                confidence=MANUAL_CONFIDENCE,
            ),
            [],
            when=now_local(),
        )

    async def name(
        self,
        group_id: GroupId,
        user_id: str,
        text: str,
        *,
        all_linked: bool = False,
    ) -> NameCard:
        """Bind a name to one exact account or its linked holder."""

        _check_length(text)
        account = await self._identity.account(user_id)
        for other in await self._ids.lookup(group_id, text):
            holder_id = await self._ids.holder_for_alias(other)
            if holder_id != account.entity_id:
                raise NameTaken(
                    text.strip(),
                    await self._display_of(group_id, holder_id, excluding=text),
                )
        alias = await self._ids.upsert_alias(
            Alias(
                alias_text=text.strip(),
                target_entity_id=account.entity_id if all_linked else None,
                target_account_id=None if all_linked else account.id,
                group_id=group_id,
                alias_type=AliasType.NICKNAME,
            ),
            [AliasEvidence(EvidenceType.MANUAL)],
        )
        log.info("group %s: alias %r bound by hand", group_id, text.strip())
        return NameCard(alias.alias_text, alias.alias_type, alias.confidence, alias.is_global)

    async def set_confidence(
        self,
        group_id: GroupId,
        user_id: str,
        text: str,
        confidence: float,
        *,
        all_linked: bool = False,
    ) -> NameCard:
        """Set one exact-account or linked-holder alias confidence by hand."""

        confidence = max(0.0, min(1.0, confidence))
        _check_length(text)
        account = await self._identity.account(user_id)
        for other in await self._ids.lookup(group_id, text):
            holder_id = await self._ids.holder_for_alias(other)
            if holder_id != account.entity_id:
                raise NameTaken(
                    text.strip(),
                    await self._display_of(group_id, holder_id, excluding=text),
                )
        if all_linked:
            updated = await self._ids.set_alias_confidence(
                group_id, account.entity_id, text, confidence
            )
        else:
            updated = await self._ids.set_account_alias_confidence(
                group_id, account.id, text, confidence
            )
        if updated is None:
            updated = await self._ids.upsert_alias(
                Alias(
                    alias_text=text.strip(),
                    target_entity_id=account.entity_id if all_linked else None,
                    target_account_id=None if all_linked else account.id,
                    group_id=group_id,
                    alias_type=AliasType.NICKNAME,
                ),
                [AliasEvidence(EvidenceType.MANUAL, score=confidence)],
            )
        return NameCard(
            updated.alias_text,
            updated.alias_type,
            updated.confidence,
            updated.is_global,
        )

    async def unname(
        self,
        group_id: GroupId,
        user_id: str,
        text: str,
        *,
        all_linked: bool = False,
    ) -> bool:
        """Retire a name from one exact account or linked-holder view."""

        account = await self._identity.account(user_id)
        aliases = (
            await self._ids.aliases_for(group_id, account.entity_id)
            if all_linked
            else await self._ids.aliases_for_account(group_id, account.id)
        )
        wanted = normalize(text)
        gone = 0
        for alias in aliases:
            if alias.normalized_text == wanted and not alias.is_global:
                await self._ids.retire_alias(alias.id)
                gone += 1
        return gone > 0

    async def forget(
        self,
        group_id: GroupId,
        user_id: str,
        index: int,
        *,
        all_linked: bool = False,
    ) -> FactCard | None:
        """Retract one fact by the number from the matching account or holder card."""

        card = (
            await self.holder_card(group_id, user_id)
            if all_linked
            else await self.account_card(group_id, user_id)
        )
        match = next((fact for fact in card.facts if fact.index == index), None)
        if match is None:
            return None
        await self._memory.retract(match.id)
        log.info("group %s: retracted fact %s (%s)", group_id, match.id, match.predicate)
        return match

    async def merge(self, left: str, right: str) -> bool:
        """Union the current holder sets of two exact accounts."""

        return await self._identity.merge(left, right)

    async def split(self, user_id: str) -> uuid.UUID:
        """Give one account its own person again. See IdentityResolver.split.

        Raises NotMerged when the account is already alone. Exact-account rows follow
        the detached account through their foreign key; holder-scoped rows remain with
        the original linked set because their account provenance is unknown.
        """
        try:
            return await self._identity.split(user_id)
        except ValueError as exc:
            raise NotMerged(user_id) from exc

    async def _display_of(
        self, group_id: GroupId, entity_id: uuid.UUID, *, excluding: str = ""
    ) -> str:
        """How somebody shows up here, for an answer that has to name them.

        Cheaper than a whole card: this is only ever wanted to say who already holds a
        name, and the card would cost a fact read and an evidence count to say it.
        `excluding` is the contested name itself, which would name nobody; a person
        with no other name here is called another member rather than by an id.
        """
        aliases = await self._ids.aliases_for(group_id, entity_id)
        skip = normalize(excluding)
        return (
            _current_platform_name(aliases)
            or next(
                (a.alias_text for a in aliases if a.is_usable and a.normalized_text != skip), ""
            )
            or "另一位成员"
        )

    async def _entity(self, user_id: str) -> uuid.UUID:
        # The live person: merge() repoints every account, so no chase is needed.
        return (await self._identity.account(user_id)).entity_id


__all__ = [
    "Directory",
    "PersonCard",
    "FactCard",
    "NameCard",
    "UnknownAccount",
    "NameTaken",
    "NotMerged",
]
