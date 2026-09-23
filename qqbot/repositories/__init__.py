"""The repository layer: how domain objects reach PostgreSQL.

Services write no SQL and do not know table names. What that boundary
buys: a schema change reaches one layer only, and group isolation can be guaranteed in
one place - every method that takes a group_id takes it as a required parameter, so
forgetting it is a TypeError rather than a silent cross-group read. (The boundary is
for services: ops tables have their own module in qqbot.db.repo, and a few hot-path
readers outside the service layer query directly.)

asyncpg directly rather than an ORM: the queries themselves are not complex. What is
complex is the temporal and scope semantics, and those read more clearly as SQL - and
are far easier to follow in an EXPLAIN.
"""

from .archive import ArchiveRepository
from .event import EventRepository
from .extraction import ExtractionRepository
from .identity import IdentityRepository
from .identity_link import IdentityLinkRepository, LinkChallenge, LinkChallengeError
from .job import JobQueue
from .memory import EpisodeRepository, MemoryRepository
from .vector import VectorRepository

__all__ = [
    "ArchiveRepository",
    "EventRepository",
    "ExtractionRepository",
    "IdentityRepository",
    "IdentityLinkRepository",
    "LinkChallenge",
    "LinkChallengeError",
    "JobQueue",
    "MemoryRepository",
    "EpisodeRepository",
    "VectorRepository",
]
