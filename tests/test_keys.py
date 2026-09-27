"""Local API key precedence and credential configuration."""

import pytest

from _fixtures import example_bundle
from qqbot import util


@pytest.mark.parametrize("missing", ["NOPE_API_KEY", "  "])
def test_missing_key_is_empty(monkeypatch, missing):
    monkeypatch.delenv("NOPE_API_KEY", raising=False)
    monkeypatch.delenv("NOPE_API_KEY_FILE", raising=False)
    assert util.read_api_key(missing) == ""


def test_key_file_precedes_environment_and_missing_file_falls_back(tmp_path, monkeypatch):
    secret = tmp_path / "key.txt"
    secret.write_text("  file-key\n", encoding="utf-8")
    monkeypatch.setenv("ACME_API_KEY_FILE", str(secret))
    monkeypatch.setenv("ACME_API_KEY", "env-key")
    assert util.read_api_key("ACME_API_KEY") == "file-key"
    monkeypatch.delenv("ACME_API_KEY_FILE")
    assert util.read_api_key("ACME_API_KEY") == "env-key"
    monkeypatch.setenv("ACME_API_KEY_FILE", str(tmp_path / "missing.txt"))
    assert util.read_api_key("ACME_API_KEY") == "env-key"


def test_bom_and_newline_are_removed(tmp_path, monkeypatch):
    secret = tmp_path / "bom.txt"
    secret.write_bytes("﻿bom-key\r\n".encode())
    monkeypatch.setenv("BOM_API_KEY_FILE", str(secret))
    assert util.read_api_key("BOM_API_KEY") == "bom-key"
    monkeypatch.setenv("BOMENV_API_KEY", "﻿env-bom-key")
    assert util.read_api_key("BOMENV_API_KEY") == "env-bom-key"


def test_example_capability_credentials_are_provider_neutral():
    backends = example_bundle().default.backends
    assert backends.text.credential_env == backends.vision.credential_env == "TEXT_API_KEY"
    assert not any(
        hasattr(backends.asr, field) for field in ("provider", "endpoint", "credential_env")
    )
    names = [
        backends.text.credential_env,
        backends.vision.credential_env,
        backends.search.credential_env,
        backends.embedding.credential_env,
    ]
    vendors = (
        "deepseek",
        "dashscope",
        "bailian",
        "zhipu",
        "openai",
        "qwen",
        "aliyun",
        "bigmodel",
        "anthropic",
        "gemini",
        "tavily",
    )
    assert not [name for name in names if any(v in name.lower() for v in vendors)]


def test_separate_capabilities_resolve_separate_fake_keys(monkeypatch):
    settings = example_bundle().default
    split = settings.model_copy(
        update={
            "backends": settings.backends.model_copy(
                update={
                    "vision": settings.backends.vision.model_copy(
                        update={"credential_env": "VISION_API_KEY"}
                    ),
                }
            ),
        }
    )
    monkeypatch.setenv("VISION_API_KEY", "vision-only-key")
    monkeypatch.setenv(split.backends.text.credential_env, "text-key")
    monkeypatch.delenv("VISION_API_KEY_FILE", raising=False)
    monkeypatch.delenv(f"{split.backends.text.credential_env}_FILE", raising=False)
    assert util.read_api_key(split.backends.vision.credential_env) == "vision-only-key"
    assert util.read_api_key(split.backends.text.credential_env) == "text-key"
