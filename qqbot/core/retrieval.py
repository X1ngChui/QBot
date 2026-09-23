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
That is on purpose: with a single query behind both, /who shows an owner exactly the
wording the model reads, so a wrong fact seen there is the wrong fact the model has.

Group isolation: `Directory` takes group_id on every call, and there is no path here that
omits it.
"""

from __future__ import annotations

import logging
from datetime import UTC, datetime

from ..db import pool
from ..domain.ids import GroupId
from ..settings import RecallEventsToolCfg, config
from ..util import defang, merge_overlapping, sysmark
from ..repositories import (
    EpisodeRepository,
    EventRepository,
    IdentityRepository,
    MemoryRepository,
    VectorRepository,
)
from ..providers.base import EmbeddingModel
from ..services import Directory, IdentityResolver, Retriever
from ..services.memory_extractor import GROUP_TERM, GROUP_TOPIC
from .members import MEMBERS

log = logging.getLogger("qqbot.retrieval")


def build_directory(
    *,
    identities: IdentityRepository | None = None,
    resolver: IdentityResolver | None = None,
) -> Directory:
    """Build the shared directory service for one Runtime."""

    identities = identities or IdentityRepository()
    resolver = resolver or IdentityResolver(identities)
    return Directory(
        identity=resolver,
        ids=identities,
        memory=MemoryRepository(),
        events=EventRepository(),
    )


#: The last roster built for each group: the stamp it was built from, the rendered
#: rows, and the speaker list the stamp's live-name check needs. The speaker list is
#: kept precisely so a cache hit costs no archive scan: who has ever spoken here is
#: append-only, and a first-time speaker busts the stamp anyway through the alias
#: rows their arrival writes.
_CACHE: dict[GroupId, tuple[tuple, list[dict], list[str]]] = {}


async def _stamp(gid: GroupId, live: dict[str, str]) -> tuple:
    """Everything the rendered roster depends on, in one query.

    The block is deliberately identical between turns - that is the entire reason it is
    ordered by account rather than by recency - so one query deciding whether to rebuild
    stands in for the hundred-odd it takes to rebuild it.

    What can actually change it: a fact written or retracted, a name added or retired,
    two people merged into one (or split apart), or somebody editing their group card.
    The first three move an `updated_at`; the last shows up in the live names, which
    the member list already caches. Message counts are not in it, because they only
    decide who survives truncation, not what any line says. Merges are read over every
    entity rather than this group's: they are rare, and one rebuild per group after
    one is cheaper than working out which groups the two people spoke in.

    Deriving the stamp rather than invalidating by hand is what makes this safe: a new
    write path cannot forget to clear a cache it does not know about.
    """
    row = await pool().fetchrow(
        """SELECT (SELECT max(updated_at) FROM memory_fact WHERE group_id=$1) AS facts,
                  (SELECT max(updated_at) FROM alias
                    WHERE group_id=$1 OR group_id IS NULL) AS names,
                  (SELECT max(updated_at) FROM entity) AS people""",
        gid.to_db(),
    )
    return (row["facts"], row["names"], row["people"], tuple(sorted(live.items())))


def _named(card) -> bool:
    """Whether a roster card shows a name rather than the account number the
    directory falls back to when no name is on record. A name that happens to be
    the digits of the account is still a name: it is on record."""
    shown = card.display.strip()
    if not shown:
        return False
    return shown not in card.accounts or any(
        n.text == shown for n in (*card.names, *card.candidates)
    )


async def gather(
    *,
    group_id: GroupId,
    directory: Directory,
    bot=None,
) -> list[dict]:
    """This group's roster, one row per person, in order of first appearance.

    Per *person*, not per account: two accounts an owner has merged are one row, with
    their message counts added together. Every person who has appeared here has a
    row, including those nothing is known about yet.

    Ordered by when each person first appeared, ties by account, because the order is
    the member numbering: somebody new joins at the end, and nobody else's number moves.

    The row shape is what prompt.py renders. Two of the fields carry the same split the
    prompt draws between certainty and guesswork - a note was entered explicitly, a card is
    the model's own reading - and two more split the names by where they came from: a name
    the account displayed is something the platform reported, while a name the group uses
    is a claim about usage that can be wrong.
    """
    gid = group_id
    exclude = {str(bot.self_id)} if bot is not None else set()

    async def _live(uids: list[str]) -> dict[str, str]:
        if bot is None:
            return {}
        return await MEMBERS.names_of(bot, group_id, [u for u in uids if u not in exclude])

    # The hit path never touches the archive: the cached speaker list feeds the
    # live-name check, and the stamp decides. Only a miss pays for the per-group
    # count scan (which grows with the archive forever) inside the rebuild.
    if cached := _CACHE.get(group_id):
        live = await _live(cached[2])
        if cached[0] == await _stamp(gid, live):
            return cached[1]

    speakers = list(await EventRepository().speaker_counts(gid))
    live = await _live(speakers)
    stamp = await _stamp(gid, live)

    cards = await directory.roster(gid, display=live, exclude=exclude)
    firsts = await EventRepository().first_appearances(gid)
    never = datetime.max.replace(tzinfo=UTC)

    def appeared(c) -> tuple:
        seen = [firsts[u] for u in c.accounts if firsts.get(u) is not None]
        return (min(seen) if seen else never, c.user_id)

    out: list[dict] = []
    for c in sorted(cards, key=appeared):
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
                "persona_card": c.summary,
                "manual_note": c.note,
                "msg_count": c.messages,
            }
        )
    _CACHE[group_id] = (stamp, out, speakers)
    return out


def _retriever(embed: EmbeddingModel) -> Retriever:
    """Build a lightweight retriever around this Runtime's embedding capability."""

    ids = IdentityRepository()
    return Retriever(
        ids,
        MemoryRepository(),
        EpisodeRepository(),
        vec=VectorRepository(embed.name),
        embed=embed,
    )


async def episode_lookup(
    group_id: GroupId,
    question: str,
    *,
    embed: EmbeddingModel,
    rcfg: RecallEventsToolCfg | None = None,
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
    settings = rcfg or config().default.tools.recall_events
    eps = await _retriever(embed).search_episodes(
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
    windows = (await EpisodeRepository().around(group_id, [e.id for e in eps], ctx)) if ctx else {}
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


async def group_knowledge(group_id: GroupId) -> list[str]:
    """What the bot has worked out about the group itself, as individual facts.

    These are ordinary facts whose subject is the group's own entity, so they arrive with
    evidence behind them, they supersede rather than accumulate, and they age out like
    anything else - each one can be checked, deleted, or forgotten on its own, which
    prose never allows.
    """
    gid = group_id
    ids = IdentityRepository()
    subject = await ids.group_entity(gid)
    facts = await MemoryRepository().current_facts(gid, [subject])

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
            out.append(f"{defang(str(f.object_key or ''))}：{value}")
        else:
            out.append(value)
    return out


__all__ = ["build_directory", "gather", "group_knowledge", "episode_lookup"]
