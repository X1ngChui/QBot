"""The long-term memory the reply path uses: the whole roster, in a fixed order.

Why the whole roster rather than a retrieval (design goal 1): this block sits in the
system prompt, ahead of the history, and prefix caching only matches forward from the
start - rebuilding the roster around whoever is speaking invalidates everything after it
(measured, and rejected on the measurement). Held whole and rendered in a fixed order, it
is identical word for word between turns. A real group of thirty accounts renders to
about two thousand characters; while it fits, there is nothing to rank and nothing to
leave out.

The rows are assembled by `Directory`, the same service the ops commands read through.
That is on purpose: with a single query behind both, /who shows an owner exactly the
wording the model reads, so a wrong fact seen there is the wrong fact the model has.

Group isolation: `Directory` takes group_id on every call, and there is no path here that
omits it.
"""

from __future__ import annotations

import logging

from ..db import pool
from ..repositories import (
    EpisodeRepository, EventRepository, IdentityRepository, JobQueue,
    MemoryRepository, VectorRepository,
)
from ..services import Directory, IdentityResolver, Retriever
from ..services.context_builder import render_episodes
from ..services.memory_extractor import GROUP_TERM, GROUP_TOPIC
from .members import MEMBERS

log = logging.getLogger("qqbot.retrieval")

_IDS = IdentityRepository()
_RESOLVER = IdentityResolver(_IDS)
_DIRECTORY = Directory(
    identity=_RESOLVER,
    ids=_IDS,
    memory=MemoryRepository(),
    events=EventRepository(),
    jobs=JobQueue("commands"),
)


def directory() -> Directory:
    """The shared instance. Stateless, so one is enough."""
    return _DIRECTORY


#: The last roster built for each group: the stamp it was built from, the rendered
#: rows, and the speaker list the stamp's live-name check needs. The speaker list is
#: kept precisely so a cache hit costs no archive scan: who has ever spoken here is
#: append-only, and a first-time speaker busts the stamp anyway through the alias
#: rows their arrival writes.
_CACHE: dict[str, tuple[tuple, list[dict], list[str]]] = {}


async def _stamp(gid: int, live: dict[str, str]) -> tuple:
    """Everything the rendered roster depends on, in one query.

    The block is deliberately identical between turns - that is the entire reason it is
    ordered by account rather than by recency - so one query deciding whether to rebuild
    stands in for the hundred-odd it takes to rebuild it.

    What can actually change it: a fact written or retracted, a name added or retired, or
    somebody editing their group card. The first two move an `updated_at`; the third shows
    up in the live names, which the member list already caches. Message counts are not in
    it, because they only decide who survives truncation, not what any line says.

    Deriving the stamp rather than invalidating by hand is what makes this safe: a new
    write path cannot forget to clear a cache it does not know about.
    """
    row = await pool().fetchrow(
        """SELECT (SELECT max(updated_at) FROM memory_fact WHERE group_id=$1) AS facts,
                  (SELECT max(updated_at) FROM alias
                    WHERE group_id=$1 OR group_id IS NULL) AS names""",
        gid,
    )
    return (row["facts"], row["names"], tuple(sorted(live.items())))


async def gather(*, group_id: str, bot=None) -> list[dict]:
    """This group's roster, one row per person.

    Per *person*, not per account: two accounts an owner has merged are one row, with
    their message counts added together.

    The row shape is what prompt.py renders. Two of the fields carry the same split the
    prompt draws between certainty and guesswork - a note was typed by an owner, a card is
    the model's own reading - and two more split the names by where they came from: a name
    the account displayed is something the platform reported, while a name the group uses
    is a claim about usage that can be wrong.
    """
    gid = int(group_id)
    exclude = {str(bot.self_id)} if bot is not None else set()

    async def _live(uids: list[str]) -> dict[str, str]:
        if bot is None:
            return {}
        return await MEMBERS.names_of(
            bot, group_id, [u for u in uids if u not in exclude])

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

    cards = await _DIRECTORY.roster(gid, display=live, exclude=exclude)

    out: list[dict] = []
    for c in cards:
        # An entry that is nothing but an account number tells the model nothing, and a
        # person with neither a name nor a fact to their name is exactly that.
        if not c.display.strip():
            continue
        if not (c.summary or c.other_names or c.note):
            continue
        out.append({
            "user_id": c.user_id,
            "nickname": c.display,
            "former_names": list(c.displayed_names),
            "aliases": list(c.nicknames),
            "persona_card": c.summary,
            "manual_note": c.note,
            "msg_count": c.messages,
        })
    _CACHE[group_id] = (stamp, out, speakers)
    return out


_RETRIEVER: Retriever | None = None


def set_embedding(embed) -> None:
    """Give the reply path its vector backend. Called once, at startup.

    There is no instance until this runs, and deliberately no default that quietly ranks
    by something else: asking for episodes before this is wired is an error rather than
    a worse answer.
    """
    global _RETRIEVER
    _RETRIEVER = Retriever(
        _IDS, MemoryRepository(), EpisodeRepository(),
        vec=VectorRepository(embed.name), embed=embed,
    )


async def episodes_for(group_id: str, accounts: list[str], question: str = "") -> str:
    """Things that happened, involving the people in this turn. Rendered, or "".

    This is the one part of memory that is fetched per turn rather than held whole, and it
    goes *after* the cache boundary for exactly that reason: the roster is the same on
    every call and belongs in the cached prefix, while which episodes matter changes with
    every message. Putting these in the system block would invalidate it each turn.

    Filtered by participant before anything looks at similarity (design doc 46): whether
    this is the right person matters more than whether the text resembles the question.
    """
    gid = int(group_id)
    entity_ids = list((await _RESOLVER.entities_of(accounts)).values())
    if not entity_ids:
        return ""
    if _RETRIEVER is None:
        raise RuntimeError("retrieval.set_embedding has not been called")
    episodes = await _RETRIEVER.episodes_only(gid, entity_ids, question or None)
    return render_episodes(episodes)


async def episode_lookup(group_id: str, question: str) -> str:
    """Episodic memory searched on demand, rendered. "" when nothing is close.

    The pull counterpart of `episodes_for`: that one runs on every reply and is filtered
    by who is present, this one runs when the model asks and is filtered by nothing but
    the group. Cross-person questions live here - who promised what, when something was
    decided - because their answer is precisely about people the current turn does not
    contain.
    """
    if _RETRIEVER is None:
        raise RuntimeError("retrieval.set_embedding has not been called")
    eps = await _RETRIEVER.search_episodes(int(group_id), question)
    return "\n".join(
        f"[{e.started_at:%m-%d}] {e.summary}" if e.started_at else f"- {e.summary}"
        for e in eps
    )


async def group_knowledge(group_id: str) -> list[str]:
    """What the bot has worked out about the group itself, as individual facts.

    These are ordinary facts whose subject is the group's own entity, so they arrive with
    evidence behind them, they supersede rather than accumulate, and they age out like
    anything else - each one can be checked, deleted, or forgotten on its own, which
    prose never allows.
    """
    gid = int(group_id)
    ids = IdentityRepository()
    subject = await ids.group_entity(gid)
    facts = await MemoryRepository().current_facts(gid, [subject])

    out = []
    for f in sorted(facts, key=lambda f: (f.predicate != GROUP_TOPIC, f.predicate)):
        value = "" if f.object_value is None else str(f.object_value).strip()
        if not value:
            continue
        if f.predicate == GROUP_TERM:
            out.append(f"{f.object_key}：{value}")
        else:
            out.append(value)
    return out


__all__ = ["gather", "group_knowledge", "episodes_for", "episode_lookup", "directory",
           "set_embedding"]
