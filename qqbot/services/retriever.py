"""L6, retrieval: choosing what belongs in the prompt for this turn.

The fusion order follows the strength of the evidence, not a score (design doc 46):

    entity match      VERY HIGH   -- is this the right person
    time proximity    HIGH
    evidence weight   HIGH
    semantic match    MEDIUM
    recency           MEDIUM

Filter by person first, then ask what resembles what. The reverse order - nearest
neighbour first, identity second - loses a short form of a name to a longer name that
merely looks similar, and recalls nothing for abbreviations (measured, and rejected for
it). Vectors do exactly one job here: once the person is settled, choose which of
*their* episodes are relevant.

Economy (design goal 1):
- The roster and the facts come from SQL, change slowly, and sit in the system block
  inside the prefix cache.
- Vector search happens only when the conversation contains a specific question, not on
  every turn.
"""

from __future__ import annotations

import logging
import uuid

from ..domain.memory import Episode
from ..providers.embedding import EmbeddingModel
from ..repositories import (
    EpisodeRepository, IdentityRepository, MemoryRepository, VectorRepository,
)

log = logging.getLogger("qqbot.retrieve")

#: How many episodes one reply may carry. More pushes the system block out of the cache,
#: and the return diminishes.
MAX_EPISODES = 3


class Retriever:
    def __init__(
        self, ids: IdentityRepository, mem: MemoryRepository,
        eps: EpisodeRepository, vec: VectorRepository, embed: EmbeddingModel,
    ) -> None:
        """The vector backend is required.

        Not optional with a silent importance-ranked fallback: a backend accidentally
        left unwired has to fail at construction, not quietly answer from the wrong
        signal while embeddings are computed for nothing.
        """
        self._ids = ids
        self._mem = mem
        self._eps = eps
        self._vec = vec
        self._embed = embed

    async def episodes_only(
        self, group_id: int, entity_ids: list[uuid.UUID], question: str | None = None
    ) -> list[Episode]:
        """Just the episodes. The reply path already has facts and names from the
        roster - assembling them here again would pay one query per person on every
        message for work that gets discarded."""
        return await self._episodes(group_id, entity_ids, question)

    async def search_episodes(
        self, group_id: int, question: str, *, limit: int = 5
    ) -> list[Episode]:
        """Episodes by similarity alone, unfiltered by participant. The tool path.

        The passive path filters by who is present first (design doc 46), which is right
        when the question is what somebody has been up to. A tool call asks something
        else - what happened, whoever was in it. Who promised what is precisely a
        question about people the current turn does not contain, so the participant
        filter is not relaxed here, it is wrong here. Group isolation still holds twice
        over: the vector search is group-scoped, and by_ids checks again.
        """
        [qv] = await self._embed.embed([question], group_id=str(group_id))
        near = await self._vec.search(
            group_id=group_id, object_type="episode", embedding=qv, limit=limit,
        )
        return await self._eps.by_ids(group_id, [eid for eid, _ in near])

    async def _episodes(
        self, group_id: int, entity_ids: list[uuid.UUID], question: str | None
    ) -> list[Episode]:
        """Episodes these people took part in, narrowed by vector when there is a
        question to narrow by.

        With no question it takes the most important recent ones: absent a specific
        question, what somebody has been up to is more use than whichever episode most
        resembles the last line typed.
        """
        pool: list[Episode] = []
        seen: set[uuid.UUID] = set()
        for eid in entity_ids:
            for ep in await self._eps.involving(group_id, eid, limit=MAX_EPISODES * 2):
                if ep.id not in seen:
                    seen.add(ep.id)
                    pool.append(ep)

        # Nothing to rank: everything in the pool is going to be returned anyway, so an
        # embedding call would be paid for and then make no difference. This is the common
        # case - most people are in a handful of episodes - and it is why the vector path
        # costs nothing until somebody has accumulated enough history for ranking to
        # decide anything.
        if len(pool) <= MAX_EPISODES:
            return sorted(pool, key=lambda e: e.importance, reverse=True)
        # No question to rank against - the caller wants what this person has been up to,
        # not what resembles a sentence. Importance is the answer here, not a fallback.
        if not question:
            return sorted(pool, key=lambda e: e.importance, reverse=True)[:MAX_EPISODES]

        # Vectors only once the people are settled: the candidate set is already their
        # episodes, so what is being ranked is which of those bear on the question - not
        # who in the group looks most similar.
        #
        # An embedding failure is not caught here, and deliberately. Ranking by importance
        # instead would return a plausible-looking answer built from the wrong signal, and
        # nothing downstream could tell the difference. It raises; the caller decides.
        [qv] = await self._embed.embed([question], group_id=str(group_id))

        near = dict(await self._vec.search(
            group_id=group_id, object_type="episode", embedding=qv,
            limit=MAX_EPISODES * 3,
        ))
        ranked = [e for e in pool if e.id in near]
        ranked.sort(key=lambda e: near[e.id])
        if ranked:
            return ranked[:MAX_EPISODES]
        # Nothing was close enough: carry no episodes this turn rather than padding with
        # the least dissimilar one.
        return []
