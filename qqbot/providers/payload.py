"""Provider-neutral local text/structure bounds and separate image-byte accounting.

The text ceiling guards complete model projections and replay, not model tokens,
HTTP bytes, heap use or SDK parsing. Inline image bytes remain subject to existing
media admission and image-count contracts, not this textual ceiling.
"""

from __future__ import annotations

from collections.abc import Iterator, Mapping
from dataclasses import dataclass, fields, is_dataclass
from itertools import chain
from typing import Any
from uuid import UUID

from qqbot.providers.contracts import ChargeState, ContextBudgetExceeded, ImageBytes

# SessionFuel caps retained tool results at 65,536 bytes per session, but does
# not cap roster, history, schema or model replay text. Two MiB closes that gap
# without trimming the accepted roster or adding an operator setting.
MAX_TEXT_BYTES = 2 * 1024 * 1024
# Tiny nested schema/replay nodes need a bound even when their strings are short.
MAX_CONTEXT_NODES = 40_000
_NODE_OVERHEAD = 32


@dataclass(frozen=True, slots=True)
class PayloadSize:
    text_bytes: int
    nodes: int
    image_bytes: int


def _string_bytes(value: str, remaining: int, *, charge: ChargeState) -> int:
    if len(value) > remaining:
        raise ContextBudgetExceeded(charge=charge)
    size = 0
    for char in value:
        code = ord(char)
        size += 1 if code < 128 else 2 if code < 2048 else 3 if code < 65536 else 4
        if size > remaining:
            raise ContextBudgetExceeded(charge=charge)
    return size


def _dataclass_parts(obj: Any) -> Iterator[Any]:
    for field in fields(obj):
        yield getattr(obj, field.name)


def _wire_image(value: Mapping[Any, Any]) -> tuple[str, int] | None:
    """Separate base64 in an adapter-produced data URL without copying it."""
    if value.get("type") != "input_image":
        return None
    url = value.get("image_url")
    if not isinstance(url, str) or not url.startswith("data:"):
        return None
    marker = url.find(";base64,", 0, 256)
    if marker < 0:
        return None
    return url[: marker + 8], (len(url) - marker - 8) * 3 // 4


def measure(value: Any, *, charge: ChargeState = ChargeState.NOT_SENT) -> PayloadSize:
    """Bound text and nodes before JSON/base64 copies, report images separately."""
    stack = [value]
    active: set[int] = set()
    total = 0
    nodes = 0
    image_bytes = 0
    while stack:
        item = stack.pop()
        if isinstance(item, _Leave):
            active.remove(item.identity)
            continue
        if isinstance(item, Iterator):
            try:
                child = next(item)
            except StopIteration:
                continue
            stack.extend((item, child))
            continue
        nodes += 1
        total += _NODE_OVERHEAD
        if nodes > MAX_CONTEXT_NODES or total > MAX_TEXT_BYTES:
            raise ContextBudgetExceeded(charge=charge)
        remaining = MAX_TEXT_BYTES - total
        if isinstance(item, str):
            total += _string_bytes(item, remaining, charge=charge)
        elif isinstance(item, ImageBytes):
            image_bytes += len(item.data)
            stack.append(item.media_type)
        elif isinstance(item, bytes):
            total += len(item)
        elif isinstance(item, UUID):
            total += 36
        elif item is None or isinstance(item, bool | int | float):
            total += len(str(item))
        else:
            identity = id(item)
            if identity in active:
                raise ContextBudgetExceeded(charge=charge)
            if isinstance(item, Mapping):
                image = _wire_image(item)
                if image is not None:
                    prefix, raw_size = image
                    image_bytes += raw_size
                    stack.append(prefix)
                    children = ((key, part) for key, part in item.items() if key != "image_url")
                else:
                    children = iter(item.items())
                stack.extend((_Leave(identity), iter(chain.from_iterable(children))))
                active.add(identity)
            elif isinstance(item, list | tuple):
                stack.extend((_Leave(identity), iter(item)))
                active.add(identity)
            elif is_dataclass(item) and not isinstance(item, type):
                stack.extend((_Leave(identity), _dataclass_parts(item)))
                active.add(identity)
            elif hasattr(item, "__dict__"):
                # The SDK has already parsed this event; inspect without model_dump.
                stack.append(_Leave(identity))
                active.add(identity)
                stack.append((vars(item), getattr(item, "__pydantic_extra__", None)))
            else:
                raise ContextBudgetExceeded(charge=charge)
        if total > MAX_TEXT_BYTES:
            raise ContextBudgetExceeded(charge=charge)
    return PayloadSize(total, nodes, image_bytes)


class _Leave:
    __slots__ = ("identity",)

    def __init__(self, identity: int) -> None:
        self.identity = identity


def check_projection(
    input_items: Any, tools: Any, output: Any = (), *, charge: ChargeState = ChargeState.NOT_SENT
) -> PayloadSize:
    """Check complete next replay and pending model output as one projection."""
    return measure((input_items, tools, output), charge=charge)
