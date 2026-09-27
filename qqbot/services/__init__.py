"""The service layer: where domain rules are carried out.

Depends on domain and repositories only - not on NoneBot, and not on the shape of any
particular model backend (providers abstracts those). So every service here can be
constructed without a bot runtime, which is what makes them testable.
"""

from qqbot.services.context_builder import render_fact
from qqbot.services.directory import Directory
from qqbot.services.directory import FactCard
from qqbot.services.directory import NameCard
from qqbot.services.directory import NameTaken
from qqbot.services.directory import NotMerged
from qqbot.services.directory import PersonCard
from qqbot.services.identity_link import IdentityLinkService
from qqbot.services.identity_resolver import IdentityResolver
from qqbot.services.identity_resolver import UnknownAccount
from qqbot.services.memory_consolidator import MemoryConsolidator
from qqbot.services.memory_consolidator import Validator
from qqbot.services.memory_consolidator import Verdict
from qqbot.services.memory_extractor import ExtractionInput
from qqbot.services.memory_extractor import MemoryExtractor
from qqbot.services.retriever import Retriever

__all__ = [
    "IdentityResolver",
    "IdentityLinkService",
    "UnknownAccount",
    "MemoryExtractor",
    "ExtractionInput",
    "MemoryConsolidator",
    "Validator",
    "Verdict",
    "Retriever",
    "render_fact",
    "Directory",
    "PersonCard",
    "FactCard",
    "NameCard",
    "NameTaken",
    "NotMerged",
]
