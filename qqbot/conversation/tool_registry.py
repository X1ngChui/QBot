"""One immutable binding of tool schema, argument validation and execution properties."""

from collections.abc import Callable, Iterable
from dataclasses import dataclass
from functools import lru_cache
import json
from types import MappingProxyType

from pydantic import BaseModel, ConfigDict, Field, create_model

from qqbot.providers.contracts import ToolSpec


@lru_cache(maxsize=32)
def _argument_model(name: str, encoded_schema: str) -> type[BaseModel]:
    schema = json.loads(encoded_schema)
    required = set(schema.get("required", ()))

    def value_type(item):
        match item.get("type"):
            case "string":
                return str
            case "integer":
                return int
            case "array":
                return list[value_type(item["items"])]
            case _:
                raise ValueError(f"unsupported internal tool field in {name}")

    fields = {}
    for key, item in schema.get("properties", {}).items():
        constraints = {
            target: item[source]
            for source, target in (
                ("minLength", "min_length"),
                ("maxLength", "max_length"),
                ("minItems", "min_length"),
                ("maxItems", "max_length"),
                ("minimum", "ge"),
                ("maximum", "le"),
            )
            if source in item
        }
        fields[key] = (value_type(item), Field(... if key in required else None, **constraints))
    return create_model(name, __config__=ConfigDict(extra="forbid", strict=True), **fields)


@dataclass(frozen=True, slots=True)
class ToolEntry:
    spec: ToolSpec
    handler: Callable | None
    parallel_safe: bool = False
    exclusive: bool = False

    def parse(self, arguments: str) -> dict:
        model = _argument_model(self.spec.name, json.dumps(self.spec.parameters, sort_keys=True))
        return model.model_validate_json(arguments or "{}").model_dump(exclude_none=True)


class ToolRegistry:
    def __init__(self, entries: Iterable[ToolEntry]) -> None:
        entries = tuple(entries)
        indexed = {entry.spec.name: entry for entry in entries}
        if len(indexed) != len(entries):
            raise ValueError("duplicate tool registration")
        if any(entry.handler is None and not entry.exclusive for entry in entries):
            raise ValueError("only session-owned actions can omit a tool handler")
        self._entries = MappingProxyType(indexed)

    @property
    def definitions(self) -> tuple[ToolSpec, ...]:
        return tuple(entry.spec for entry in self._entries.values())

    def get(self, name: str) -> ToolEntry | None:
        return self._entries.get(name)

    def parallel_safe(self, name: str) -> bool:
        entry = self.get(name)
        return entry is not None and entry.parallel_safe

    def exclusive(self, name: str) -> bool:
        entry = self.get(name)
        return entry is not None and entry.exclusive
