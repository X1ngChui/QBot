"""Capability wiring.

`providers()` is the only way the rest of the code reaches a model. What it returns is
decided once at startup by `build_default()`, which reads the backend names from config -
or by a test handing over fakes through `set_providers()`. Callers see the ABCs from
`base`, never a backend module.

The registry is imported inside the functions rather than at module scope, so importing
this package costs nothing and cannot create an import cycle with core.
"""

from __future__ import annotations

from .base import (
    AsrModel,
    Capability,
    ChatResult,
    Kind,
    Providers,
    SearchEngine,
    TextModel,
    VisionModel,
)

__all__ = [
    "ChatResult",
    "Kind",
    "Providers",
    "Capability",
    "TextModel",
    "VisionModel",
    "AsrModel",
    "SearchEngine",
    "providers",
    "set_providers",
    "build_default",
]

_providers: Providers | None = None


def build_default() -> Providers:
    """Build the bundle named by the top-level config."""
    from ..settings import config
    from .registry import build

    return build(config().default)


def providers() -> Providers:
    global _providers
    if _providers is None:
        _providers = build_default()
    return _providers


def set_providers(bundle: Providers | None) -> None:
    """Inject a bundle (tests, or an alternate wiring). None restores the default."""
    global _providers
    _providers = bundle
