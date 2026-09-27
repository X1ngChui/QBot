"""Configuration loading and validation.

Settings are global. Persona files may vary only model-facing identity and group context.
The complete bundle is validated once at startup and remains fixed for the process lifetime.
"""

from __future__ import annotations

import logging
import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import Literal

import yaml
from pydantic import (
    Field,
    field_serializer,
    field_validator,
)

from qqbot.configuration.schema import (
    ConfigModel as _M,
    Settings as Settings,
    AsrCfg as AsrCfg,
    DatabaseCfg as DatabaseCfg,
    EmbeddingCfg as EmbeddingCfg,
    TextCfg as TextCfg,
    VisionCfg as VisionCfg,
    SearchCfg as SearchCfg,
    TasksCfg as TasksCfg,
    MaintenanceCfg as MaintenanceCfg,
    MediaCfg as MediaCfg,
)
from qqbot.domain.ids import GroupId
from qqbot.prompting import PromptCatalog

log = logging.getLogger("qqbot.configuration")


CONFIG_DIR = Path(os.getenv("CONFIG_DIR", "/app/config"))
DEFAULT_PERSONA_KEY = "default"


class PredicateCfg(_M):
    """One thing that may be recorded about a person.

    Everything a predicate is lives in this one entry: what the extraction model may
    offer, how many of them a person may hold at once, how fast it is forgotten, and
    the words it renders as in the prompt. One entry rather than a table per aspect
    spread across code: split up, adding a predicate means several edits in several
    files, and each omission fails quietly - a missing verb puts the bare English
    predicate in front of the model.
    """

    #: How the fact reads in Chinese. `{{object}}` marks where the object goes for a
    #: predicate that does not read verb-first, the way an allergy does; without it
    #: the object simply follows the verb.
    verb: str = Field(min_length=1)

    @field_validator("verb")
    @classmethod
    def _closed_object_slot(cls, value: str) -> str:
        """Allow only one non-executable object slot in configurable wording."""

        slot = "{{object}}"
        if value.count(slot) > 1:
            raise ValueError("predicate verb may contain {{object}} at most once")
        rest = value.replace(slot, "")
        if "{{" in rest or "}}" in rest or "{" in rest or "}" in rest:
            raise ValueError("predicate verb supports only the optional {{object}} slot")
        return value

    def render(self, object_value: str) -> str:
        """Insert one object without interpreting any syntax it contains."""

        slot = "{{object}}"
        if slot in self.verb:
            return self.verb.replace(slot, object_value)
        return f"{self.verb}{object_value}"

    #: single: a person holds one at a time, and a new value closes the old one
    #: (which is what makes "he moved" expressible). multi: they sit side by side,
    #: each ageing on its own evidence.
    cardinality: Literal["single", "multi"]
    #: Which half-life this predicate is forgotten on. Where somebody lives changes
    #: over years, what they are playing over weeks; one clock for both is wrong for
    #: both.
    decay: Literal["stable", "default", "fast"] = "default"
    #: How the fact is classified once stored.
    kind: Literal["attribute", "preference", "relation"] = "attribute"
    #: Recording this retracts the named predicate about the same object: somebody
    #: who has gone off a thing is not also somebody who likes it. Must be mutual.
    opposite: str | None = None
    #: The line the extraction model is shown for this predicate: what it means and
    #: where its boundary with the neighbouring predicates runs. Rendered into the
    #: extraction prompt, so a predicate cannot be added without saying what it is.
    rule: str = Field(min_length=1)


class HalfLifeCfg(_M):
    """Base lifetimes in days, one per decay class. A fact nothing has confirmed
    for its base lifetime (stretched by how many distinct days supported it, up
    to fourfold) is expired outright - see MemoryRepository.decay."""

    stable: float = Field(default=90.0, gt=0)
    default: float = Field(default=30.0, gt=0)
    fast: float = Field(default=14.0, gt=0)


class PredicateTable(_M):
    """The whole of predicates.yaml: the clocks, and the predicates themselves."""

    half_life_days: HalfLifeCfg = Field(default_factory=HalfLifeCfg)
    person: Mapping[str, PredicateCfg]

    @field_serializer("person")
    def _serialize_person(self, value: Mapping[str, PredicateCfg]) -> dict[str, PredicateCfg]:
        return dict(value)

    @field_validator("person")
    @classmethod
    def _coherent(cls, table: dict[str, PredicateCfg]) -> Mapping[str, PredicateCfg]:
        # The name goes into a JSON-schema enum, a database column and an index, so
        # it has to be plain. Checked here rather than trusted, because a predicate
        # that differs from another by a space is two predicates nothing reconciles.
        for name, p in table.items():
            if not name.replace("_", "").isalnum() or not name.islower():
                raise ValueError(
                    f"predicate name must be lower-case letters, digits and _: {name!r}"
                )
            if name in RESERVED_PREDICATES:
                raise ValueError(f"{name!r} is reserved: it has its own tool and its own shape")
            if p.opposite is None:
                continue
            other = table.get(p.opposite)
            if other is None:
                raise ValueError(f"{name}: opposite names no predicate: {p.opposite!r}")
            if other.opposite != name:
                raise ValueError(f"{name} and {p.opposite} disagree about being opposites")
        return MappingProxyType(dict(table))


#: Names the person table may not use. `note` is explicit command input, and the
#: model must have no way to write over it; `topic` and `term` are about the group
#: rather than about a person, and reach memory through their own tools.
RESERVED_PREDICATES = frozenset({"note", "topic", "term"})


class Persona(_M):
    """A group's model-facing identity and standing context.

    A group file states only what differs from default.yaml; anything it leaves out is
    inherited. Settings are deliberately absent: behavior and resource policy are global.

    `system_prompt_extra` exists because sharing happens *inside* the prompt, not only
    between fields: the base prompt is inherited and the group's own paragraphs are
    appended to it. Setting `system_prompt` outright replaces the base entirely, for a
    group that wants nothing shared.
    """

    name: str = "小X"
    system_prompt: str = ""
    system_prompt_extra: str = ""
    group_knowledge: str = ""


def _merge_persona(base: Persona, override: Persona | None) -> Persona:
    """Fold a group persona onto the default and compose the final prompt.

    Only fields the group file actually set are taken (model_fields_set), so a field left
    out inherits rather than being overwritten by its schema default - otherwise every
    group would silently fall back to the default name.
    """
    if override is None:
        merged = base.model_copy(deep=True)
    else:
        data = base.model_dump()
        for field in override.model_fields_set:
            data[field] = getattr(override, field)
        merged = Persona.model_validate(data)

    if merged.system_prompt_extra.strip():
        merged = merged.model_copy(
            update={
                "system_prompt": (
                    merged.system_prompt.rstrip() + "\n\n" + merged.system_prompt_extra.strip()
                ),
                "system_prompt_extra": "",
            }
        )
    return merged


def _read_yaml(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f) or {}
    if not isinstance(data, dict):
        raise ValueError(f"{path}: top level must be a mapping")
    return data


@dataclass(frozen=True, slots=True)
class ConfigBundle:
    """One immutable startup snapshot of settings and prompt resources."""

    default: Settings
    personas: Mapping[GroupId, Persona]
    prompts: PromptCatalog
    predicates: PredicateTable
    _default_persona: Persona
    _resolved_personas: Mapping[GroupId, Persona]

    def __init__(
        self,
        raw_settings: dict,
        personas: Mapping[GroupId | str, Persona],
        prompts: PromptCatalog | None = None,
        predicates: PredicateTable | None = None,
    ) -> None:
        default = personas.get(DEFAULT_PERSONA_KEY, Persona())
        groups: dict[GroupId, Persona] = {}
        for key, persona in personas.items():
            if key == DEFAULT_PERSONA_KEY:
                continue
            try:
                gid = GroupId(key)
            except ValueError:
                raise ValueError(f"persona group id must be numeric: {key!r}") from None
            groups[gid] = persona
        resolved_default = _merge_persona(default, None)
        resolved = {gid: _merge_persona(default, persona) for gid, persona in groups.items()}
        object.__setattr__(self, "default", Settings.model_validate(raw_settings))
        object.__setattr__(self, "personas", MappingProxyType(groups))
        object.__setattr__(self, "prompts", prompts or PromptCatalog.empty())
        object.__setattr__(
            self,
            "predicates",
            predicates or PredicateTable(person={}),
        )
        object.__setattr__(self, "_default_persona", resolved_default)
        object.__setattr__(self, "_resolved_personas", MappingProxyType(resolved))

    def persona_for(self, group: GroupId) -> Persona:
        return self._resolved_personas.get(group, self._default_persona)

    def for_group(self, group: GroupId) -> tuple[Settings, Persona]:
        return self.default, self.persona_for(group)


def _resolve(root: Path, p: str) -> Path:
    q = Path(p)
    return q if q.is_absolute() else root / q


def load_bundle(config_dir: Path | None = None) -> ConfigBundle:
    root = config_dir or CONFIG_DIR
    raw = _read_yaml(root / "settings.yaml")
    # Validated up front: the directory layout below comes from the config
    # itself, so the schema has to hold before anything else is read.
    settings = Settings.model_validate(raw)

    personas: dict[GroupId | str, Persona] = {}
    persona_dir = _resolve(root, settings.runtime.paths.personas_dir)
    if persona_dir.is_dir():
        for path in sorted(persona_dir.glob("*.yaml")):
            stem = path.stem
            if stem == DEFAULT_PERSONA_KEY:
                key: GroupId | str = DEFAULT_PERSONA_KEY
            elif stem.startswith("group_") and stem[6:].isdigit():
                key = GroupId(stem[6:])
            else:
                raise ValueError(
                    f"{path}: persona file must be default.yaml or group_<numeric id>.yaml"
                )
            personas[key] = Persona.model_validate(_read_yaml(path))
    if DEFAULT_PERSONA_KEY not in personas:
        raise ValueError(f"required default persona is missing: {persona_dir / 'default.yaml'}")

    # Prompt wording is customizable, but its role and slot surface are code-owned.
    # The complete catalog validates before startup continues.
    prompt_dir = _resolve(root, settings.runtime.paths.prompts_dir)
    prompts = PromptCatalog.load(prompt_dir)

    # The predicate table is mandatory too: without it the extractor would offer
    # the model an empty enum and every fact would be rejected, silently and all
    # night. The prompt has to carry the block that explains it, or the model gets
    # an enum with no meanings attached.
    ppath = _resolve(root, settings.runtime.paths.predicates_file)
    try:
        predicates = PredicateTable.model_validate(_read_yaml(ppath))
    except OSError as e:
        raise ValueError(f"predicate file unreadable: {ppath}: {e}") from None
    if not predicates.person:
        raise ValueError(f"predicate file lists no predicates: {ppath}")
    bundle = ConfigBundle(raw, personas, prompts, predicates)
    # Resolve every configured persona before startup accepts the bundle.
    for group_id in bundle.personas:
        bundle.persona_for(group_id)
    return bundle
