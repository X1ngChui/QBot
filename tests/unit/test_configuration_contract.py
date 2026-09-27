"""Startup rejects incomplete inputs and publishes deeply immutable values."""

import shutil
from pathlib import Path

import pytest
from pydantic import ValidationError

from _fixtures import example_bundle
from qqbot.configuration import DatabaseCfg, Settings, load_bundle


@pytest.mark.parametrize("field", ["owners", "nicknames", "predicates"])
def test_startup_collections_are_read_only(bundle, field):
    with pytest.raises((AttributeError, TypeError, ValidationError)):
        match field:
            case "owners":
                bundle.default.bot.owners.append("fictional")
            case "nicknames":
                bundle.default.bot.nicknames.append("fictional")
            case "predicates":
                bundle.predicates.person["fictional"] = next(
                    iter(bundle.predicates.person.values())
                )


def test_public_examples_form_a_complete_bundle():
    bundle = example_bundle()
    assert bundle.default.backends.text.model
    assert bundle._default_persona.system_prompt
    assert (
        bundle.predicates.model_validate_json(bundle.predicates.model_dump_json())
        == bundle.predicates
    )


def test_invalid_timezone_fails_before_runtime(bundle):
    with pytest.raises(ValidationError, match="IANA timezone"):
        Settings.model_validate({**bundle.default.model_dump(), "bot": {"timezone": "Not/AZone"}})


def test_pool_min_cannot_exceed_capacity():
    with pytest.raises(ValidationError, match="pool_min"):
        DatabaseCfg(pool_min=4, pool_max=2)


def test_missing_default_persona_is_not_a_silent_identity_fallback(tmp_path):
    fixture = Path(__file__).parents[1] / "fixtures" / "config"
    target = tmp_path / "config"
    shutil.copytree(fixture, target)
    (target / "personas" / "default.yaml").unlink()
    with pytest.raises(ValueError, match="required default persona"):
        load_bundle(target)
