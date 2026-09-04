"""Stand-ins the DB-backed suites share.

Kept apart from `_db.py`, which is about the database. What is here is the one capability
every suite has to supply now that the reply path will not start without it.
"""

import hashlib

from qqbot.providers.base import Rate
from qqbot.providers.embedding import EmbeddingModel


class FakeEmbedding(EmbeddingModel):
    """Answers, so the vector path runs rather than being skipped.

    The real one is a paid network call. What the suites check is that the chain asks for
    a vector and reads one back - which nothing checked while the reply path was built
    without a backend at all and quietly ranked by importance instead.
    """

    name = "fake-embed"
    #: The column is fixed at this width, so a stub answering with anything else fails at
    #: the insert rather than somewhere further downstream.
    DIMS = 2048
    EMBED_CALLS = 0

    @property
    def dimensions(self) -> int:
        return self.DIMS

    def rate_for(self, model):
        return Rate("Mtoken", in_miss=0.5)

    async def embed(self, texts, *, group_id=None):
        type(self).EMBED_CALLS += len(texts)
        # Deterministic across processes, which the first version was not: it used the
        # built-in hash(), which is salted per process, so the vector geometry changed
        # from run to run and retrieval checks flickered against the search layer's
        # distance ceiling. md5 is stable. The shared base keeps any two texts inside
        # that ceiling - what these suites test is the plumbing, not the geometry - while
        # the per-text perturbation keeps distinct texts distinct.
        out = []
        for t in texts:
            h = hashlib.md5(t.encode("utf-8")).digest()
            out.append([1.0 + h[i % 16] / 1275.0 for i in range(self.DIMS)])
        return out

    async def aclose(self):
        pass
