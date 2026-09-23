"""L6, retrieval: episodic memory searched on demand.

The past is pulled, never pushed: episodes reach a reply only through the
recall_events tool. A block pushed per turn would sit right next to the incoming
message, where an elliptical question resolves against it instead of against the
conversation. What the prompt carries uninvited stays limited to what every reply
needs - the roster and the facts, from SQL, inside the cached prefix.

Vector search runs only when the model asks: one embedding call per tool use,
none per message.
"""

from __future__ import annotations

import logging

from ..domain.ids import GroupId
from ..domain.memory import Episode
from ..providers.base import EmbeddingModel
from ..repositories import (
    EpisodeRepository,
    IdentityRepository,
    MemoryRepository,
    VectorRepository,
)

log = logging.getLogger("qqbot.retrieve")


class Retriever:
    def __init__(
        self,
        ids: IdentityRepository,
        mem: MemoryRepository,
        eps: EpisodeRepository,
        vec: VectorRepository,
        embed: EmbeddingModel,
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
        self, group_id: GroupId, question: str, *, limit: int
    ) -> list[Episode]:
        """Return group-scoped episodes ordered by semantic similarity.

        A question about an event may name someone absent from the current turn, so
        identity is not a recall filter. Group isolation still holds twice: vector
        search is group-scoped, and by_ids checks the group again.
        """
        [qv] = await self._embed.embed([question], group_id=group_id)
        near = await self._vec.search(
            group_id=group_id,
            object_type="episode",
            embedding=qv,
            limit=limit,
        )
        return await self._eps.by_ids(group_id, [eid for eid, _ in near])
