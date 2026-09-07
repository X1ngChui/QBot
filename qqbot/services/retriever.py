"""L6, retrieval: episodic memory searched on demand.

The past is pulled, never pushed: episodes reach a reply only through the
recall_events tool. A per-turn pushed block, participant-filtered, used to live
here too - it sat right next to the incoming message and once captured an
elliptical question that referred to the conversation, so it was deleted; what
the prompt carries uninvited is limited to what every reply needs (the roster
and the facts, from SQL, inside the cached prefix).

Vector search runs only when the model asks (design goal 1): one embedding call
per tool use, none per message.
"""

from __future__ import annotations

import logging

from ..domain.memory import Episode
from ..providers.embedding import EmbeddingModel
from ..repositories import (
    EpisodeRepository, IdentityRepository, MemoryRepository, VectorRepository,
)

log = logging.getLogger("qqbot.retrieve")


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

    async def search_episodes(
        self, group_id: int, question: str, *, limit: int = 5
    ) -> list[Episode]:
        """Episodes by similarity alone, unfiltered by participant.

        A tool call asks what happened, whoever was in it - who promised what is
        precisely a question about people the current turn need not contain, so
        there is no participant filter here on purpose. Group isolation still
        holds twice over: the vector search is group-scoped, and by_ids checks
        again.
        """
        [qv] = await self._embed.embed([question], group_id=str(group_id))
        near = await self._vec.search(
            group_id=group_id, object_type="episode", embedding=qv, limit=limit,
        )
        return await self._eps.by_ids(group_id, [eid for eid, _ in near])
