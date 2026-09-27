"""Public, deterministic configuration fixtures with no deployment-file reads."""

from pathlib import Path

import yaml

from qqbot.prompting import PromptCatalog
from qqbot.configuration import ConfigBundle
from qqbot.configuration import Persona
from qqbot.configuration import PredicateTable

ROOT = Path(__file__).resolve().parent.parent


def example_bundle() -> ConfigBundle:
    """Validate the complete shipped example, including every persona example."""
    config_dir = ROOT / "config"
    raw = yaml.safe_load((config_dir / "settings.yaml.example").read_text(encoding="utf-8"))
    personas = {}
    for path in (config_dir / "personas").glob("*.yaml.example"):
        persona = Persona.model_validate(yaml.safe_load(path.read_text(encoding="utf-8")))
        if path.name == "default.yaml.example":
            personas["default"] = persona
    predicates = PredicateTable.model_validate(
        yaml.safe_load((config_dir / "predicates.yaml").read_text(encoding="utf-8"))
    )
    return ConfigBundle(raw, personas, PromptCatalog.load(config_dir / "prompts"), predicates)


_bundle: ConfigBundle | None = None


def config() -> ConfigBundle:
    global _bundle
    if _bundle is None:
        from qqbot.configuration import load_bundle

        _bundle = load_bundle(ROOT / "tests" / "fixtures" / "config")
    return _bundle


def prompt_catalog() -> PromptCatalog:
    return config().prompts


from qqbot.clock import Clock

clock = Clock("Asia/Shanghai")
now_local = clock.now
fmt_when = clock.format
describe_now = clock.describe


def today_local() -> str:
    return clock.today().isoformat()
