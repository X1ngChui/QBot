"""Strict, code-owned contracts for customizable model prompt templates."""

from __future__ import annotations

import re
from collections.abc import Mapping
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from types import MappingProxyType
import yaml


class PromptKey(StrEnum):
    SHARED_LEGEND = "shared_legend"
    SHARED_PRAGMATICS = "shared_pragmatics"
    REPLY_SYSTEM = "reply_system"
    REPLY_DEVELOPER = "reply_developer"
    REPLY_USER = "reply_user"
    EXTRACT_SYSTEM = "extract_system"
    EXTRACT_USER = "extract_user"
    VISION_SYSTEM = "vision_system"
    TOOL_WEB_SEARCH = "tool_web_search"
    TOOL_SEARCH_HISTORY = "tool_search_history"
    TOOL_RECALL_EVENTS = "tool_recall_events"
    TOOL_READ_URL = "tool_read_url"
    TOOL_OPEN_IMAGES = "tool_open_images"
    TOOL_SEND_MESSAGES = "tool_send_messages"


class PromptRole(StrEnum):
    PARTIAL = "partial"
    SYSTEM = "system"
    DEVELOPER = "developer"
    USER = "user"
    TOOL = "tool"


class ReloadScope(StrEnum):
    RELOADABLE = "reloadable"
    RESTART_REQUIRED = "restart_required"


@dataclass(frozen=True, slots=True)
class SlotSpec:
    name: str
    allow_empty: bool = False
    source: PromptKey | None = None


@dataclass(frozen=True, slots=True)
class TemplateSpec:
    key: PromptKey
    role: PromptRole
    reload_scope: ReloadScope
    slots: tuple[SlotSpec, ...] = ()

    @property
    def path(self) -> str:
        return f"prompts.yaml:templates.{self.key.value}"

    @property
    def slot_names(self) -> frozenset[str]:
        return frozenset(slot.name for slot in self.slots)


class TemplateValidationError(ValueError):
    """A template source or render value violates its closed contract."""


def _slot(
    name: str,
    *,
    allow_empty: bool = False,
    source: PromptKey | None = None,
) -> SlotSpec:
    return SlotSpec(name, allow_empty, source)


_SPECS = (
    TemplateSpec(
        PromptKey.SHARED_LEGEND,
        PromptRole.PARTIAL,
        ReloadScope.RESTART_REQUIRED,
    ),
    TemplateSpec(
        PromptKey.SHARED_PRAGMATICS,
        PromptRole.PARTIAL,
        ReloadScope.RESTART_REQUIRED,
    ),
    TemplateSpec(
        PromptKey.REPLY_SYSTEM,
        PromptRole.SYSTEM,
        ReloadScope.RELOADABLE,
        (
            _slot("shared_legend", source=PromptKey.SHARED_LEGEND),
            _slot("shared_pragmatics", source=PromptKey.SHARED_PRAGMATICS),
        ),
    ),
    TemplateSpec(
        PromptKey.REPLY_DEVELOPER,
        PromptRole.DEVELOPER,
        ReloadScope.RELOADABLE,
        (
            _slot("persona"),
            _slot("group_context", allow_empty=True),
            _slot("member_roster", allow_empty=True),
        ),
    ),
    TemplateSpec(
        PromptKey.REPLY_USER,
        PromptRole.USER,
        ReloadScope.RELOADABLE,
        (_slot("now"), _slot("current_message")),
    ),
    TemplateSpec(
        PromptKey.EXTRACT_SYSTEM,
        PromptRole.SYSTEM,
        ReloadScope.RESTART_REQUIRED,
        (
            _slot("shared_legend", source=PromptKey.SHARED_LEGEND),
            _slot("shared_pragmatics", source=PromptKey.SHARED_PRAGMATICS),
            _slot("predicate_table"),
        ),
    ),
    TemplateSpec(
        PromptKey.EXTRACT_USER,
        PromptRole.USER,
        ReloadScope.RESTART_REQUIRED,
        (
            _slot("bot_names"),
            _slot("account_roster"),
            _slot("known_memory", allow_empty=True),
            _slot("transcript"),
        ),
    ),
    TemplateSpec(
        PromptKey.VISION_SYSTEM,
        PromptRole.SYSTEM,
        ReloadScope.RELOADABLE,
    ),
    TemplateSpec(
        PromptKey.TOOL_WEB_SEARCH,
        PromptRole.TOOL,
        ReloadScope.RELOADABLE,
    ),
    TemplateSpec(
        PromptKey.TOOL_SEARCH_HISTORY,
        PromptRole.TOOL,
        ReloadScope.RELOADABLE,
    ),
    TemplateSpec(
        PromptKey.TOOL_RECALL_EVENTS,
        PromptRole.TOOL,
        ReloadScope.RELOADABLE,
    ),
    TemplateSpec(
        PromptKey.TOOL_READ_URL,
        PromptRole.TOOL,
        ReloadScope.RELOADABLE,
    ),
    TemplateSpec(
        PromptKey.TOOL_OPEN_IMAGES,
        PromptRole.TOOL,
        ReloadScope.RELOADABLE,
    ),
    TemplateSpec(
        PromptKey.TOOL_SEND_MESSAGES,
        PromptRole.TOOL,
        ReloadScope.RELOADABLE,
        (_slot("face_catalog"), _slot("message_limit")),
    ),
)

PROMPT_SPECS: Mapping[PromptKey, TemplateSpec] = MappingProxyType(
    {spec.key: spec for spec in _SPECS}
)

_SLOT = re.compile(r"{{([a-z][a-z0-9_]*)}}")


class _UniqueKeyLoader(yaml.SafeLoader):
    """Safe YAML loader that refuses silently shadowed mapping entries."""


def _construct_unique_mapping(loader: _UniqueKeyLoader, node, deep: bool = False):
    mapping = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in mapping:
            raise yaml.constructor.ConstructorError(
                "while constructing a mapping",
                node.start_mark,
                f"found duplicate key {key!r}",
                key_node.start_mark,
            )
        mapping[key] = loader.construct_object(value_node, deep=deep)
    return mapping


_UniqueKeyLoader.add_constructor(
    yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG,
    _construct_unique_mapping,
)


@dataclass(frozen=True, slots=True)
class PromptTemplate:
    spec: TemplateSpec
    source: str

    @classmethod
    def parse(cls, spec: TemplateSpec, source: str) -> PromptTemplate:
        source = source.strip()
        if not source:
            raise TemplateValidationError(f"{spec.path}: template is empty")
        matches = tuple(_SLOT.finditer(source))
        names = [match.group(1) for match in matches]
        if any(
            (match.start() > 0 and source[match.start() - 1] == "{")
            or (match.end() < len(source) and source[match.end()] == "}")
            for match in matches
        ):
            raise TemplateValidationError(f"{spec.path}: malformed slot marker")
        unknown = sorted(set(names) - spec.slot_names)
        if unknown:
            raise TemplateValidationError(
                f"{spec.path}: unknown slot(s): {', '.join(unknown)}"
            )
        for slot in spec.slots:
            count = names.count(slot.name)
            if count != 1:
                raise TemplateValidationError(
                    f"{spec.path}: slot {slot.name!r} must occur exactly once, got {count}"
                )
        stripped = _SLOT.sub("", source)
        if "{{" in stripped:
            raise TemplateValidationError(f"{spec.path}: malformed slot marker")
        return cls(spec, source)

    def render(self, values: Mapping[str, str] | None = None, /, **kwargs: str) -> str:
        supplied = dict(values or {})
        overlap = supplied.keys() & kwargs.keys()
        if overlap:
            raise TemplateValidationError(
                f"{self.spec.path}: duplicate render value(s): {', '.join(sorted(overlap))}"
            )
        supplied.update(kwargs)
        expected = self.spec.slot_names
        missing = sorted(expected - supplied.keys())
        extra = sorted(supplied.keys() - expected)
        if missing or extra:
            details = []
            if missing:
                details.append("missing " + ", ".join(missing))
            if extra:
                details.append("unexpected " + ", ".join(extra))
            raise TemplateValidationError(f"{self.spec.path}: {'; '.join(details)}")
        for slot in self.spec.slots:
            value = supplied[slot.name]
            if not isinstance(value, str):
                raise TemplateValidationError(
                    f"{self.spec.path}: slot {slot.name!r} must be text"
                )
            if not slot.allow_empty and not value.strip():
                raise TemplateValidationError(
                    f"{self.spec.path}: slot {slot.name!r} cannot be empty"
                )
        return _SLOT.sub(lambda match: supplied[match.group(1)], self.source).strip()


@dataclass(frozen=True, slots=True)
class PromptCatalog:
    templates: Mapping[PromptKey, PromptTemplate]

    @classmethod
    def empty(cls) -> PromptCatalog:
        return cls(MappingProxyType({}))

    @classmethod
    def load(cls, root: Path) -> PromptCatalog:
        path = root / "prompts.yaml"
        try:
            raw = yaml.load(
                path.read_text(encoding="utf-8-sig"),
                Loader=_UniqueKeyLoader,
            ) or {}
        except (OSError, yaml.YAMLError) as exc:
            raise TemplateValidationError(
                f"prompt bundle unreadable: {path}: {exc}"
            ) from None
        if not isinstance(raw, dict) or set(raw) != {"version", "templates"}:
            raise TemplateValidationError(
                f"{path}: top level must contain exactly version and templates"
            )
        if raw["version"] != 1:
            raise TemplateValidationError(
                f"{path}: unsupported prompt bundle version {raw['version']!r}"
            )
        bodies = raw["templates"]
        if not isinstance(bodies, dict):
            raise TemplateValidationError(f"{path}: templates must be a mapping")
        return cls.from_sources(bodies, location=str(path))

    @classmethod
    def from_sources(
        cls, bodies: Mapping[str, str], *, location: str = "prompt bundle"
    ) -> PromptCatalog:
        expected = {key.value for key in PROMPT_SPECS}
        actual = set(bodies)
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        if missing or extra:
            details = []
            if missing:
                details.append("missing " + ", ".join(missing))
            if extra:
                details.append("unexpected " + ", ".join(extra))
            raise TemplateValidationError(
                f"prompt template manifest mismatch in {location}: {'; '.join(details)}"
            )
        templates: dict[PromptKey, PromptTemplate] = {}
        for key, spec in PROMPT_SPECS.items():
            source = bodies[key.value]
            if not isinstance(source, str):
                raise TemplateValidationError(f"{spec.path}: template must be text")
            templates[key] = PromptTemplate.parse(spec, source)
        return cls(MappingProxyType(templates))

    def source(self, key: PromptKey) -> str:
        return self.templates[key].source

    def render(
        self, key: PromptKey, values: Mapping[str, str] | None = None, /, **kwargs: str
    ) -> str:
        supplied = dict(values or {})
        duplicate = supplied.keys() & kwargs.keys()
        if duplicate:
            raise TemplateValidationError(
                f"{self.templates[key].spec.path}: duplicate render value(s): "
                + ", ".join(sorted(duplicate))
            )
        supplied.update(kwargs)
        internal = {
            slot.name: self.source(slot.source)
            for slot in self.templates[key].spec.slots
            if slot.source is not None
        }
        overlap = supplied.keys() & internal.keys()
        if overlap:
            raise TemplateValidationError(
                f"{self.templates[key].spec.path}: code-owned slot(s) cannot be supplied: "
                + ", ".join(sorted(overlap))
            )
        return self.templates[key].render(internal | supplied)

    def sources(self) -> dict[str, str]:
        return {key.value: self.source(key) for key in PROMPT_SPECS}

    def restart_fingerprint(self) -> dict[str, str]:
        return {
            f"prompts.{key.value}": template.source
            for key, template in self.templates.items()
            if template.spec.reload_scope is ReloadScope.RESTART_REQUIRED
        }


def tool_prompt_key(name: str) -> PromptKey:
    try:
        return PromptKey(f"tool_{name}")
    except ValueError as exc:
        raise KeyError(f"no prompt template for tool {name!r}") from exc
