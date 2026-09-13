"""L1, the identity layer: people, accounts, names.

Keeping the three apart is the foundation of the whole design:

    Entity (a person)
       +-- IdentityAccount (a QQ account: strong identity)
       +-- Alias (a name: weak identity, with a scope and evidence behind it)

The split is what carries the questions the system runs on: whether two names belong to
one person, and which group a nickname holds in. Flattened into one table per account,
nothing in the model could hold either question.
"""

from .alias import (
    ALIAS_MAX_CHARS,
    Alias, AliasEvidence, AliasStatus, AliasType, EvidenceType, normalize,
    fused_confidence, platform_weight, usage_weight,
)
from .entity import Entity, EntityStatus, EntityType, IdentityAccount

__all__ = [
    "ALIAS_MAX_CHARS", "Alias", "AliasEvidence", "AliasStatus", "AliasType",
    "EvidenceType", "normalize",
    "fused_confidence", "platform_weight", "usage_weight",
    "Entity", "EntityStatus", "EntityType", "IdentityAccount",
]
