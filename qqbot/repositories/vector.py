"""L6, the vector projection.

Vectors are decoupled from the things they project (design doc 47): changing embedding
model rebuilds this one table and leaves fact and episode alone. The model and version
are part of the unique key, so two sets of vectors can coexist and a switchover needs no
downtime to recompute.

Group isolation holds here too: the group filter comes first and the distance second,
with no whole-table nearest neighbour. Otherwise one group's question matches another
group's content - and vector search is the least noticeable way that can leak.
"""

from __future__ import annotations

import uuid
from collections.abc import Sequence

from ..db import pool


def _vec(values: Sequence[float]) -> str:
    """pgvector's literal form. asyncpg does not know the vector type, so it goes as
    text."""
    return "[" + ",".join(f"{v:.7g}" for v in values) + "]"


class VectorRepository:
    def __init__(self, model: str, version: int = 1) -> None:
        self._model = model
        self._version = version

    async def put(
        self, *, group_id: int | None, object_type: str, object_id: uuid.UUID,
        embedding: Sequence[float],
    ) -> None:
        await pool().execute(
            """INSERT INTO embedding_index
                   (group_id, object_type, object_id, embedding,
                    embedding_model, embedding_version)
               VALUES ($1,$2,$3,$4::vector,$5,$6)
               ON CONFLICT (object_type, object_id, embedding_model, embedding_version)
               DO UPDATE SET embedding = EXCLUDED.embedding""",
            group_id, object_type, object_id, _vec(embedding),
            self._model, self._version,
        )

    async def search(
        self, *, group_id: int, object_type: str, embedding: Sequence[float],
        limit: int = 10, max_distance: float = 0.45,
    ) -> list[tuple[uuid.UUID, float]]:
        """The closest objects within this group.

        With a distance ceiling: when nothing is close enough it returns nothing, rather
        than handing back the least dissimilar thing as an answer. Failure in a retrieval
        layer should read as "not found", not as "here is something that looks like it".
        """
        rows = await pool().fetch(
            """SELECT object_id, embedding <=> $3::vector AS distance
                 FROM embedding_index
                WHERE group_id=$1 AND object_type=$2
                  AND embedding_model=$4 AND embedding_version=$5
                  AND embedding <=> $3::vector < $6
                ORDER BY embedding <=> $3::vector
                LIMIT $7""",
            group_id, object_type, _vec(embedding), self._model, self._version,
            max_distance, limit,
        )
        return [(r["object_id"], r["distance"]) for r in rows]

    async def forget(self, object_type: str, object_id: uuid.UUID) -> None:
        await pool().execute(
            "DELETE FROM embedding_index WHERE object_type=$1 AND object_id=$2",
            object_type, object_id,
        )

    async def unembedded_episodes(self, group_id: int) -> list[tuple[uuid.UUID, str]]:
        """Active episodes with no vector for the current model, as (id, summary).

        The anti-join keeps the nightly fill incremental in the query itself -
        fetching every episode to diff in Python re-read the whole store per group
        per night. Keyed on model and version like every read here, so switching
        embedding model makes the old vectors invisible and the fill re-covers
        everything, exactly as the coexist-then-switch design intends.
        """
        rows = await pool().fetch(
            """SELECT e.id, e.summary FROM episode e
                 LEFT JOIN embedding_index x
                        ON x.object_type='episode' AND x.object_id=e.id
                       AND x.embedding_model=$2 AND x.embedding_version=$3
                WHERE e.group_id=$1 AND e.status='active' AND x.object_id IS NULL""",
            group_id, self._model, self._version,
        )
        return [(r["id"], r["summary"]) for r in rows]

