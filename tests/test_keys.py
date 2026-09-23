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

    # 4. missing key resolves to empty, so the provider can raise a named error
    check("missing key is empty", util.read_api_key("NOPE_API_KEY") == "")
    check("empty name is empty", util.read_api_key("  ") == "")

    # 5. a BOM from a Windows editor must not ride along into the auth header
    bom_file = tmp / "bom.txt"
    bom_file.write_bytes("﻿bom-key\r\n".encode())
    os.environ["BOM_API_KEY_FILE"] = str(bom_file)
    check("BOM and CRLF stripped from key files",
          util.read_api_key("BOM_API_KEY") == "bom-key",
          repr(util.read_api_key("BOM_API_KEY")))
    clear("BOM_API_KEY_FILE")
    os.environ["BOMENV_API_KEY"] = "﻿env-bom-key"
    check("BOM stripped from env vars too",
          util.read_api_key("BOMENV_API_KEY") == "env-bom-key",
          repr(util.read_api_key("BOMENV_API_KEY")))
    clear("BOMENV_API_KEY")

    # 7. shipped config: names describe capabilities, never platforms
    cfg = load_bundle().default.capabilities
    names = [cfg.text.credential_env, cfg.vision.credential_env,
             cfg.search.credential_env, cfg.embedding.credential_env]
    check("text and vision share one name by default",
          cfg.text.credential_env == cfg.vision.credential_env == "TEXT_API_KEY",
          f"{cfg.text.credential_env} / {cfg.vision.credential_env}")
    check(
        "local ASR has no provider connection settings",
        not hasattr(cfg.asr, "provider")
        and not hasattr(cfg.asr, "endpoint")
        and not hasattr(cfg.asr, "credential_env"),
    )
    VENDORS = ("deepseek", "dashscope", "bailian", "zhipu", "openai", "qwen", "aliyun",
               "bigmodel", "anthropic", "gemini", "tavily")
    leaked = [n for n in names if any(v in n.lower() for v in VENDORS)]
    check("no credential name mentions a platform", not leaked, str(leaked))

    # 8. Network capabilities can still move credentials independently.
    split = load_bundle().default
    split = split.model_copy(update={
        "capabilities": split.capabilities.model_copy(update={
            "vision": split.capabilities.vision.model_copy(
                update={"credential_env": "VISION_API_KEY"}
            ),
        }),
    })
    os.environ["VISION_API_KEY"] = "vision-only-key"
    os.environ[split.capabilities.text.credential_env] = "text-key"
    vkey = util.read_api_key(split.capabilities.vision.credential_env)
    tkey = util.read_api_key(split.capabilities.text.credential_env)
    check("split config yields two different network keys",
          vkey == "vision-only-key" and tkey == "text-key",
          f"vision={vkey!r} text={tkey!r}")
    clear("VISION_API_KEY", split.capabilities.text.credential_env)

    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(main())
