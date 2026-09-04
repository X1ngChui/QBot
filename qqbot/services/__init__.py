"""The service layer: where domain rules are carried out.

Depends on domain and repositories only - not on NoneBot, and not on the shape of any
particular model backend (providers abstracts those). So every service here can be
constructed without a bot runtime, which is what makes them testable.
"""

from .context_builder import render_episodes, render_fact
from .directory import Directory, FactCard, NameCard, NameTaken, PersonCard
from .identity_resolver import IdentityResolver, UnknownAccount
from .memory_consolidator import MemoryConsolidator, Validator, Verdict
from .memory_extractor import ExtractionInput, MemoryExtractor
from .retriever import Retriever

__all__ = [
    "IdentityResolver",
    "UnknownAccount",
    "MemoryExtractor",
    "ExtractionInput",
    "MemoryConsolidator",
    "Validator",
    "Verdict",
    "Retriever",
    "render_episodes",
    "render_fact",
    "Directory",
    "PersonCard",
    "FactCard",
    "NameCard",
    "NameTaken",
]
