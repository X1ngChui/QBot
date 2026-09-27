"""Prompt template contracts, fixtures and audit-packet construction."""

from qqbot.prompting.templates import PROMPT_SPECS
from qqbot.prompting.templates import PromptCatalog
from qqbot.prompting.templates import PromptKey
from qqbot.prompting.templates import PromptRole
from qqbot.prompting.templates import PromptTemplate
from qqbot.prompting.templates import SlotSpec
from qqbot.prompting.templates import TemplateSpec
from qqbot.prompting.templates import TemplateValidationError
from qqbot.prompting.templates import tool_prompt_key

__all__ = [
    "PROMPT_SPECS",
    "PromptCatalog",
    "PromptKey",
    "PromptRole",
    "PromptTemplate",
    "SlotSpec",
    "TemplateSpec",
    "TemplateValidationError",
    "tool_prompt_key",
]
