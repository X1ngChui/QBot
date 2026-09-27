"""Public lint and review packets never fall back to live configuration resources."""

import json

from qqbot import configuration
from qqbot.conversation import prompt, tools
from qqbot.prompting import PromptCatalog, PromptKey
from qqbot.prompting.lint import lint_catalog
from qqbot.prompting.packet import build_prompt_packet
from qqbot.services import memory_extractor


def reject_global_resource_reads(monkeypatch):
    def forbidden(*args, **kwargs):
        raise AssertionError("an explicit public bundle must not consult live configuration")

    for module, name in (
        (configuration, "config"),
        (tools, "config"),
        (tools, "prompt_catalog"),
        (prompt, "config"),
        (prompt, "prompt_catalog"),
        (memory_extractor, "_table"),
        (memory_extractor, "config"),
        (memory_extractor, "prompt_catalog"),
    ):
        monkeypatch.setattr(module, name, forbidden, raising=False)


def test_lint_uses_only_the_supplied_public_resources(bundle, monkeypatch):
    reject_global_resource_reads(monkeypatch)
    assert lint_catalog(bundle.prompts, bundle.default, bundle.predicates) == []


def test_fictional_review_packet_uses_the_supplied_catalog(bundle, monkeypatch):
    reject_global_resource_reads(monkeypatch)
    sources = bundle.prompts.sources()
    description = "Fictional explicit finish instruction."
    sources[PromptKey.TOOL_FINISH_REPLY.value] = description
    catalog = PromptCatalog.from_sources(sources, location="synthetic test")
    packet = json.loads(build_prompt_packet(catalog, bundle.default, bundle.predicates))
    entry = next(
        item for item in packet["code_derived"]["reply_tools"] if item["name"] == "finish_reply"
    )
    assert entry["description"] == description
