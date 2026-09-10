"""API key resolution.

Capabilities may share a credential today, but each names its own, so splitting them
must be reachable from YAML alone. Also pins the resolution order, since a silent
fallback to the wrong credential is the kind of thing that only shows up as a 401 in
production.
"""

import os
import pathlib
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "config"))

from qqbot import util
from qqbot.settings import load_bundle

fails = []


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


def clear(*names):
    for n in names:
        os.environ.pop(n, None)


def main() -> int:
    tmp = pathlib.Path(tempfile.mkdtemp())

    # 1. <NAME>_FILE wins, and the value is stripped
    secret = tmp / "k.txt"
    secret.write_text("  file-key\n", encoding="utf-8")
    os.environ["ACME_API_KEY_FILE"] = str(secret)
    os.environ["ACME_API_KEY"] = "env-key"
    check("<NAME>_FILE takes precedence", util.read_api_key("ACME_API_KEY") == "file-key",
          repr(util.read_api_key("ACME_API_KEY")))

    # 2. plain env var when no file is pointed at
    clear("ACME_API_KEY_FILE")
    check("<NAME> env var is the fallback", util.read_api_key("ACME_API_KEY") == "env-key")

    # 3. a dangling _FILE path falls through rather than blowing up
    os.environ["ACME_API_KEY_FILE"] = str(tmp / "missing.txt")
    check("dangling _FILE path falls through", util.read_api_key("ACME_API_KEY") == "env-key")
    clear("ACME_API_KEY_FILE", "ACME_API_KEY")

    # 4. /run/secrets/<name lowercased>: what makes splitting a compose-only change
    fake_secrets = tmp / "run_secrets"
    fake_secrets.mkdir()
    (fake_secrets / "acme_api_key").write_text("mounted-key\n", encoding="utf-8")
    util.DOCKER_SECRETS_DIR = fake_secrets
    check("mounted secret found by convention", util.read_api_key("ACME_API_KEY") == "mounted-key",
          repr(util.read_api_key("ACME_API_KEY")))

    # 5. missing key resolves to empty, so the provider can raise a named error
    check("missing key is empty", util.read_api_key("NOPE_API_KEY") == "")
    check("empty name is empty", util.read_api_key("  ") == "")

    # 6. a BOM from a Windows editor must not ride along into the auth header
    (fake_secrets / "bom_api_key").write_bytes("﻿bom-key\r\n".encode())
    check("BOM and CRLF stripped from key files",
          util.read_api_key("BOM_API_KEY") == "bom-key",
          repr(util.read_api_key("BOM_API_KEY")))
    os.environ["BOMENV_API_KEY"] = "﻿env-bom-key"
    check("BOM stripped from env vars too",
          util.read_api_key("BOMENV_API_KEY") == "env-bom-key",
          repr(util.read_api_key("BOMENV_API_KEY")))
    clear("BOMENV_API_KEY")

    # 7. shipped config: names describe capabilities, never platforms
    cfg = load_bundle().default.llm
    names = [cfg.text.api_key_env, cfg.vision.api_key_env, cfg.asr.api_key_env,
             cfg.search.api_key_env]
    # Since the vision migration, text and vision run on one account (one platform
    # serves both), while ASR keeps its own; search is a third party entirely.
    check("text and vision share one name by default",
          cfg.text.api_key_env == cfg.vision.api_key_env == "TEXT_API_KEY",
          f"{cfg.text.api_key_env} / {cfg.vision.api_key_env}")
    check("asr and search have their own",
          len({cfg.text.api_key_env, cfg.asr.api_key_env, cfg.search.api_key_env}) == 3,
          " / ".join(names))
    VENDORS = ("deepseek", "dashscope", "bailian", "zhipu", "openai", "qwen", "aliyun",
               "bigmodel", "anthropic", "gemini", "tavily")
    leaked = [n for n in names if any(v in n.lower() for v in VENDORS)]
    check("no credential name mentions a platform", not leaked, str(leaked))

    # 8. the split is reachable from YAML alone
    split = load_bundle()
    split.default.llm.asr.api_key_env = "ASR_API_KEY"
    (fake_secrets / "asr_api_key").write_text("asr-only-key\n", encoding="utf-8")
    (fake_secrets / "text_api_key").write_text("shared-key\n", encoding="utf-8")
    vkey = util.read_api_key(split.default.llm.vision.api_key_env)
    akey = util.read_api_key(split.default.llm.asr.api_key_env)
    check("split config yields two different keys", vkey == "shared-key" and akey == "asr-only-key",
          f"vision={vkey!r} asr={akey!r}")

    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(main())
