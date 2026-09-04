"""Shared setup for the DB-backed suites.

Every suite starts from an empty database. Truncating only the tables a suite writes
leaves the previous run's rows in the others, and the failures that produces point at the
code rather than at the leftovers - `test_repo.py` counting one row too many was the usual
shape, and it only appeared on the second run.

Discovered from the catalog rather than listed here, so a new table is covered the day it
is added instead of the day someone remembers this file.
"""

from qqbot.db import pool


async def reset() -> None:
    rows = await pool().fetch(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'public'"
    )
    names = [r["tablename"] for r in rows]
    if names:
        await pool().execute("TRUNCATE " + ", ".join(names))
