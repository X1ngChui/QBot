"""Persistent descriptions and provider-scoped attachment handles."""

from __future__ import annotations

from qqbot.clock import Clock

from collections.abc import Callable
from datetime import timedelta

import asyncpg


class MediaCacheRepository:
    def __init__(self, database: Callable[[], asyncpg.Pool], clock: Clock) -> None:
        self._clock = clock
        self._database = database

    async def image_cache_get(self, key: str, *, max_age: timedelta | None = None) -> str | None:
        """The stored description, or None while there is none yet. A row whose upload
        landed before its describing call holds '' - reported as a miss, not a hit.

        `max_age` asks only for a description still worth trusting: an older one is
        reported as a miss so the caller pays to write a fresh one. Callers that may not
        spend leave it unset - outside the paid describing path a stale description
        still beats a bare marker.

        The sighting counts either way: hit_count measures how often a picture comes
        back, which is what decides whether describing it again is worth anything.
        """
        row = await self._database().fetchrow(
            """UPDATE image_cache SET hit_count = hit_count + 1, last_seen = now()
                WHERE key=$1 RETURNING description, described_at""",
            key,
        )
        if not row or not row["description"]:
            return None
        if max_age is not None and row["described_at"] < self._clock.now() - max_age:
            return None
        return row["description"]

    async def image_cache_put(self, key: str, description: str, *, refused: bool = False) -> None:
        """Store the description and stamp when it was written.

        Overwrites on conflict: the row may have been created by the upload half with an
        empty description, or hold one this call was made to replace because it had aged
        out - the same key is the same picture, and the newer description is the one the
        current model wrote.

        `refused` marks a placeholder standing in for a picture the backend declined to
        look at, so the two are told apart in the report. It expires like any other
        description; a later backend may well look at it.
        """
        await self._database().execute(
            """INSERT INTO image_cache (key, description, refused, described_at)
                    VALUES ($1,$2,$3,now())
               ON CONFLICT (key) DO UPDATE
                 SET description = EXCLUDED.description, refused = EXCLUDED.refused,
                     described_at = now(), last_seen = now()""",
            key,
            description,
            refused,
        )

    async def image_cache_file(
        self,
        key: str,
        *,
        provider: str,
        max_age: timedelta | None = None,
    ) -> str | None:
        """Return a fresh file handle issued by this exact provider, if any."""

        row = await self._database().fetchrow(
            """SELECT file_id, file_provider, file_uploaded_at
                 FROM image_cache WHERE key=$1""",
            key,
        )
        if not row or not row["file_id"] or row["file_provider"] != provider:
            return None
        if max_age is not None:
            at = row["file_uploaded_at"]
            if at is None or at < self._clock.now() - max_age:
                return None
        return row["file_id"]

    async def image_cache_set_file(self, key: str, file_id: str, *, provider: str) -> None:
        await self._database().execute(
            """INSERT INTO image_cache (key, file_id, file_provider, file_uploaded_at)
               VALUES ($1,$2,$3,now())
               ON CONFLICT (key) DO UPDATE
                 SET file_id = EXCLUDED.file_id,
                     file_provider = EXCLUDED.file_provider,
                     file_uploaded_at = now(), last_seen = now()""",
            key,
            file_id,
            provider,
        )

    async def image_cache_stats(
        self,
    ) -> dict:
        row = await self._database().fetchrow(
            """SELECT count(*) AS n, COALESCE(sum(hit_count),0) AS hits,
                      count(*) FILTER (WHERE refused) AS refused
                 FROM image_cache"""
        )
        return dict(row) if row else {"n": 0, "hits": 0, "refused": 0}
