"""The repository layer: how domain objects reach PostgreSQL.

Services write no SQL and do not know table names (design doc 60). What that boundary
buys: a schema change reaches one layer only, and group isolation can be guaranteed in
one place - every method that takes a group_id takes it as a required parameter, so
forgetting it is a TypeError rather than a silent cross-group read. (The boundary is
for services: ops tables have their own module in qqbot.db.repo, and a few hot-path
readers outside the service layer query directly.)

asyncpg directly rather than an ORM: the queries themselves are not complex. What is
complex is the temporal and scope semantics, and those read more clearly as SQL - and
are far easier to follow in an EXPLAIN.
"""

from .event import EventRepository
from .identity import IdentityRepository
from .job import JobQueue
from .memory import EpisodeRepository, MemoryRepository
from .vector import VectorRepository

__all__ = [
    "EventRepository",
    "IdentityRepository",
    "JobQueue",
    "MemoryRepository",
    "EpisodeRepository",
    "VectorRepository",
]
