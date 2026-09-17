"""Prompt template contracts, fixtures and audit-packet construction."""

from .templates import (
    PROMPT_SPECS,
    PromptCatalog,
    PromptKey,
    PromptRole,
    PromptTemplate,
    ReloadScope,
    SlotSpec,
    TemplateSpec,
    TemplateValidationError,
    tool_prompt_key,
)

__all__ = [
    "PROMPT_SPECS",
    "PromptCatalog",
    "PromptKey",
    "PromptRole",
    "PromptTemplate",
    "ReloadScope",
    "SlotSpec",
    "TemplateSpec",
    "TemplateValidationError",
    "tool_prompt_key",
]
