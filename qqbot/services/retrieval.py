"""The long-term memory the reply path uses: the whole roster, in a fixed order.

Why the whole roster rather than a retrieval: this block sits in the
system prompt, ahead of the history, and prefix caching only matches forward from the
start - rebuilding the roster around whoever is speaking invalidates everything after
it, which costs more than the tokens it saves. Held whole and rendered in a fixed
order, it is identical word for word between turns.

Whole means everyone who has appeared in the group, whether or not anything is known
about them: the roster is where member numbers are assigned (core.member_numbers), so
it is the one list in which the model can find anybody it wants to @ or search for.

The rows are assembled by `Directory`, the same service the ops commands read through.
Both views use the same scoped records; the prompt separates reliable identities and
notes from scored facts and candidate-name hints.

Group isolation: `Directory` takes group_id on every call, and there is no path here that
omits it.
"""

from __future__ import annotations

import logging
from datetime import UTC, datetime
from dataclasses import replace

from collections.abc import Callable
import asyncpg
from qqbot.clock import Clock
from qqbot.configuration import PredicateTable
from qqbot.domain.ids import GroupId
from qqbot.conversation.limits import RecallEventsLimits, RECALL_LIMITS
from qqbot.util import defang
from qqbot.util import merge_overlapping
from qqbot.util import sysmark
from qqbot.repositories.roster import RosterRepository
from qqbot.repositories import EpisodeRepository
from qqbot.repositories import EventRepository
from qqbot.repositories import IdentityRepository
from qqbot.repositories import MemoryRepository
from qqbot.repositories import VectorRepository
from qqbot.providers.base import EmbeddingModel
from qqbot.services import Directory
from qqbot.services import IdentityResolver
from qqbot.services import Retriever
from qqbot.services.context_builder import render_hint
from qqbot.services.memory_extractor import GROUP_TERM
from qqbot.services.memory_extractor import GROUP_TOPIC
from qqbot.services.members import MemberDirectory

log = logging.getLogger("qqbot.retrieval")


def build_directory(
    *,
    predicates: PredicateTable,
    clock: Clock,
    database: Callable[[], asyncpg.Pool],
    identities: IdentityRepository | None = None,
    resolver: IdentityResolver | None = None,
) -> Directory:
    """Build the shared directory service for one Runtime."""

    identities = identities or IdentityRepository(database=database, clock=clock)
    resolver = resolver or IdentityResolver(identities)
    memory = MemoryRepository(database=database, clock=clock)
    events = EventRepository(database=database)
    return Directory(
        predicates=predicates,
        clock=clock,
        identity=resolver,
        ids=identities,
        memory=memory,
        events=events,
        roster=RosterRepository(database, identities=identities, memory=memory, events=events),
    )


def _named(card) -> bool:
    """Whether a roster card shows a name rather than the account number the
    directory falls back to when no name is on record. A name that happens to be
    the digits of the account is still a name: it is on record."""
    shown = card.display.strip()
    if not shown:
        return False
    return (
        shown not in card.accounts or card.live_display or any(n.text == shown for n in card.names)
    )


async def gather(
    *,
    group_id: GroupId,
    directory: Directory,
    members: MemberDirectory,
    bot=None,
) -> list[dict]:
    """This group's roster, one row per person, in order of first appearance.

    Per *person*, not per account: two accounts an owner has merged are one row, with
    their message counts added together. Every person who has appeared here has a
    row, including those nothing is known about yet.

    Ordered by when each person first appeared, ties by account, because the order is
    the member numbering: somebody new joins at the end, and nobody else's number moves.

    The row shape is what prompt.py renders. Reliable names and explicit notes remain
    separate from scored memory_hints, which carry learned facts and candidate names.
    Historical platform display names and conversational aliases are distinct claims.
    """
    gid = group_id
    exclude = {str(bot.self_id)} if bot is not None else set()

    async def _live(uids: list[str]) -> dict[str, str]:
        if bot is None:
            return {}
        return await members.names_of(bot, group_id, [u for u in uids if u not in exclude])

    cache_key = (str(bot.self_id) if bot is not None else "", group_id)
    cached = directory.roster_cache.get(cache_key)
    revision = await directory.roster_revision(gid)
    if cached is not None and cached.revision == revision:
        live = await _live(cached.speakers)
        if cached.live == tuple(sorted(live.items())):
            return cached.project()

    cards = await directory.roster(gid, exclude=exclude)
    speakers = [account for card in cards for account in card.accounts]
    live = await _live(speakers)
    displayed = []
    for card in cards:
        shown = (live.get(card.user_id) or "").strip() or next(
            (live[user].strip() for user in card.accounts if (live.get(user) or "").strip()), ""
        )
        displayed.append(replace(card, display=shown, live_display=True) if shown else card)
    never = datetime.max.replace(tzinfo=UTC)
    cards = sorted(displayed, key=lambda card: (card.first_seen or never, card.user_id))

    out: list[dict] = []
    for c in cards:
        out.append(
            {
                "user_id": c.user_id,
                # The person and every account of theirs, so the prompt's member
                # numbers give merged accounts one number without another lookup.
                "entity_id": c.entity_id,
                "accounts": list(c.accounts),
                # A card with no name on record falls back to the account number, which
                # the model is never shown; the generic member word stands in for it.
                "nickname": c.display if _named(c) else "成员",
                "former_names": list(c.displayed_names),
                "aliases": list(c.nicknames),
                "memory_hints": c.memory_hints,
                "manual_note": c.note,
                "msg_count": c.messages,
            }
        )
    if revision == await directory.roster_revision(gid):
        directory.roster_cache.put(cache_key, revision, live, out)
    return out


def _retriever(
    embed: EmbeddingModel, database: Callable[[], asyncpg.Pool], clock: Clock
) -> Retriever:
    """Build a lightweight retriever around this Runtime's embedding capability."""

    ids = IdentityRepository(database=database, clock=clock)
    return Retriever(
        ids,
        MemoryRepository(database=database, clock=clock),
        EpisodeRepository(database=database),
        vec=VectorRepository(embed.name, database=database),
        embed=embed,
    )


async def episode_lookup(
    group_id: GroupId,
    question: str,
    *,
    embed: EmbeddingModel,
    clock: Clock,
    database: Callable[[], asyncpg.Pool],
    rcfg: RecallEventsLimits | None = None,
) -> str:
    """Episodic memory searched on demand, rendered. "" when nothing is close.

    On demand is the only way the past reaches a reply: a block pushed per turn
    sits right next to the incoming message, where an elliptical question
    resolves against it instead of against the conversation. The model pulls
    when it wants the past. Filtered by nothing but the group: cross-person
    questions - who promised what, when something was decided - are precisely
    about people the current turn does not contain.

    Each recalled episode comes framed by its neighbours in group time
    (tools.recall_events.context_episodes each way): an episode summarises one stretch of
    conversation, and what led to it or came of it is usually the adjacent
    stretch. Touching windows merge into one block, blocks are separated by an
    ellipsis line, and an undated episode stands alone.
    """
    settings = rcfg or RECALL_LIMITS
    eps = await _retriever(embed, database, clock).search_episodes(
        group_id,
        question,
        limit=settings.max_hits,
    )
    if not eps:
        return ""

    def line(e) -> str:
        # defang the summary: episodes are prose the extractor wrote and carry
        # no legitimate system markup; the date wears the reserved brackets.
        return (
            f"{sysmark(f'{e.started_at:%m-%d}')} {defang(e.summary)}"
            if e.started_at
            else f"- {defang(e.summary)}"
        )

    ctx = settings.context_episodes
    windows = (
        (await EpisodeRepository(database=database).around(group_id, [e.id for e in eps], ctx))
        if ctx
        else {}
    )
    if not windows:
        return "\n".join(line(e) for e in eps)
    by_id = {e.id: e for w in windows.values() for e in w} | {e.id: e for e in eps}
    covered = {i for w in windows.values() for i in (e.id for e in w)}
    blocks = merge_overlapping(
        [{e.id for e in w} for w in windows.values()]
        # A hit the window query could not place (undated) still renders,
        # as a block of one - after the dated story, in recall order.
        + [{e.id} for e in eps if e.id not in covered]
    )

    def order(i) -> tuple:
        e = by_id[i]
        # None-dated blocks are singletons, so the naive fallback datetime is
        # only ever compared against itself - never against an aware one.
        return (e.started_at is None, e.started_at or datetime.min, str(i))

    out: list[str] = []
    for b in sorted(blocks, key=lambda b: min(order(i) for i in b)):
        if out:
            out.append("……")
        out.extend(line(by_id[i]) for i in sorted(b, key=order))
    return "\n".join(out)


async def group_knowledge(
    group_id: GroupId, *, database: Callable[[], asyncpg.Pool], clock: Clock
) -> list[str]:
    """What the bot has worked out about the group itself, as individual facts.

    These are ordinary facts whose subject is the group's own entity, so they arrive with
    evidence behind them, they supersede rather than accumulate, and they age out like
    anything else - each one can be checked, deleted, or forgotten on its own, which
    prose never allows.
    """
    gid = group_id
    ids = IdentityRepository(database=database, clock=clock)
    subject = await ids.group_entity(gid)
    facts = await MemoryRepository(database=database, clock=clock).current_facts(gid, [subject])

    # Fully ordered: this block sits above the history in the prompt, and two facts
    # of equal confidence swapping places between turns would miss the prefix cache
    # for every reply that follows.
    out = []
    for f in sorted(
        facts,
        key=lambda f: (
            f.predicate != GROUP_TOPIC,
            f.predicate,
            f.object_key or "",
            str(f.object_value or ""),
        ),
    ):
        # defang on render: the values are extractor output over member text.
        value = "" if f.object_value is None else defang(str(f.object_value)).strip()
        if not value:
            continue
        if f.predicate == GROUP_TERM:
            value = f"{defang(str(f.object_key or ''))}：{value}"
        out.append(render_hint("事实", value, f.confidence))
    return out


__all__ = ["build_directory", "gather", "group_knowledge", "episode_lookup"]
