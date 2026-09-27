"""L1, the identity layer: people, accounts, names.

Keeping the three apart is the foundation of the whole design:

    Entity (a person)
       +-- IdentityAccount (a QQ account: strong identity)
       +-- Alias (a name: weak identity, with a scope and evidence behind it)

The split is what carries the questions the system runs on: whether two names belong to
one person, and which group a nickname holds in. Flattened into one table per account,
nothing in the model could hold either question.
"""

from qqbot.domain.identity.alias import ALIAS_MAX_CHARS
from qqbot.domain.identity.alias import Alias
from qqbot.domain.identity.alias import AliasEvidence
from qqbot.domain.identity.alias import AliasStatus
from qqbot.domain.identity.alias import AliasType
from qqbot.domain.identity.alias import EvidenceType
from qqbot.domain.identity.alias import normalize
from qqbot.domain.identity.alias import fused_confidence
from qqbot.domain.identity.alias import platform_weight
from qqbot.domain.identity.alias import usage_weight
from qqbot.domain.identity.entity import Entity
from qqbot.domain.identity.entity import EntityStatus
from qqbot.domain.identity.entity import EntityType
from qqbot.domain.identity.entity import IdentityAccount

__all__ = [
    "ALIAS_MAX_CHARS",
    "Alias",
    "AliasEvidence",
    "AliasStatus",
    "AliasType",
    "EvidenceType",
    "normalize",
    "fused_confidence",
    "platform_weight",
    "usage_weight",
    "Entity",
    "EntityStatus",
    "EntityType",
    "IdentityAccount",
]
