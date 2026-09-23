"""Launch checklist, mechanised.

Run it inside the bot container after the keys are in place:

    docker compose run --rm bot python scripts/preflight.py

Checks, in order: the database answers, every key the config names resolves, and each
capability makes a real minimal request - text performs a function-call round trip, then
vision (inline base64), ASR, search and embedding each make one call.
Each call takes the production path, proxy included: search goes through the
configured proxy exactly as it will at runtime, everything else direct.
"""

from __future__ import annotations

import asyncio
import os
import struct
import sys
import traceback

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from qqbot.providers import Providers
from qqbot.providers.registry import build as build_providers
from qqbot.providers.contracts import (
    CallContext,
    CallPurpose,
    GenerationPolicy,
    Message,
    ModelRequest,
    ReasoningEffort,
    Role,
    ToolResult,
    ToolSpec,
)
from qqbot.settings import config
from qqbot.util import read_api_key


def test_png(side: int = 64) -> bytes:
    """A real PNG, built here so the check needs no image library and no asset on disk.
    Vision models reject anything under 10px a side, so a 1x1 pixel will not do."""
    import zlib

    def chunk(tag: bytes, payload: bytes) -> bytes:
        return (
            struct.pack(">I", len(payload))
            + tag
            + payload
            + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
        )

    # A checkerboard, so "describe this image" has something to describe.
    # Every scanline is prefixed with filter byte 0.
    black, white = (0, 0, 0), (255, 255, 255)
    rows = b"".join(
        bytes([0]) + bytes(v for x in range(side) for v in (black if (x + y) % 16 < 8 else white))
        for y in range(side)
    )
    signature = bytes.fromhex("89504e470d0a1a0a")
    return (
        signature
        + chunk(b"IHDR", struct.pack(">IIBBBBB", side, side, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


TEST_PNG = test_png()

RESULTS: list[tuple[str, bool, str]] = []


def record(name: str, ok: bool, detail: str = "") -> None:
    RESULTS.append((name, ok, detail))
    print(f"[{'ok ' if ok else 'FAIL'}] {name}" + (f"  {detail}" if detail else ""))


def silence_wav(seconds: float = 0.4, rate: int = 16000) -> bytes:
    n = int(rate * seconds)
    data = b"\x00\x00" * n
    header = b"RIFF" + struct.pack("<I", 36 + len(data)) + b"WAVEfmt "
    header += struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16)
    header += b"data" + struct.pack("<I", len(data))
    return header + data


async def check_db() -> None:
    """Opens the pool for the whole run - the capability checks below record their cost
    through it, so closing here would fail every one of them."""
    from qqbot.db import init_pool, pool

    try:
        await init_pool()
        from qqbot.db.repo import ensure_schema

        await ensure_schema()
        ver = await pool().fetchval("SELECT version()")
        tables = await pool().fetchval(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'"
        )
        record("postgres", tables > 0, f"{tables} tables, {ver.split(',')[0]}")
    except Exception as e:
        record("postgres", False, repr(e))


async def check_text(capabilities: Providers) -> None:
    cfg = config().default.capabilities.text
    label = f"text ({cfg.model})"
    try:
        tool = ToolSpec(
            name="preflight_echo",
            description="Call this tool once before answering.",
            parameters={"type": "object", "properties": {}, "required": []},
        )
        policy = GenerationPolicy(
            model=cfg.model,
            reasoning=ReasoningEffort.OFF,
            timeout_sec=cfg.timeout_sec,
            retries=cfg.retries,
            max_output_tokens=512,
        )
        request = ModelRequest(
            prompt=(Message(Role.USER, "Call preflight_echo, then report its result."),),
            tools=(tool,),
            policy=policy,
            context=CallContext(CallPurpose.PREFLIGHT),
        )
        async with capabilities.text.open_session(request) as session:
            first = await session.start()
            calls = first.tool_calls
            if len(calls) != 1 or calls[0].name != "preflight_echo":
                raise RuntimeError("model did not produce the preflight function call")
            result = await session.continue_with((ToolResult(calls[0].call_id, "received"),))
        reasoning = first.usage.reasoning + result.usage.reasoning
        think = f" reasoning={reasoning}" if reasoning else ""
        record(
            label,
            bool(result.text),
            f"tool round-trip, {result.text!r} "
            f"in={first.usage.input_cached + result.usage.input_cached}+"
            f"{first.usage.input_uncached + result.usage.input_uncached} "
            f"out={first.usage.output + result.usage.output}{think}",
        )
    except Exception as e:
        record(label, False, repr(e))


async def check_vision(capabilities: Providers) -> None:
    cfg = config().default.capabilities.vision
    label = f"vision, base64 inline ({cfg.model})"
    try:
        from qqbot.prompting import PromptKey
        from qqbot.settings import prompt_catalog

        desc = await capabilities.vision.describe(
            TEST_PNG,
            prompt=prompt_catalog().render(PromptKey.VISION_SYSTEM),
            mime="image/png",
        )
        record(label, bool(desc), repr(desc[:40]))
    except Exception as e:
        record(label, False, repr(e))


async def check_asr(capabilities: Providers) -> None:
    label = "ASR, local CPU SenseVoice"
    try:
        text = await capabilities.asr.transcribe(silence_wav(), fmt="wav", seconds=0.4)
        record(label, True, repr(text[:40]))
    except Exception as e:
        record(label, False, repr(e))


async def check_search(capabilities: Providers) -> None:
    cfg = config().default.capabilities.search
    options = config().default.tools.web_search
    label = f"search ({cfg.provider})"
    try:
        items = await capabilities.search.search("今天天气", options=options)
        record(label, bool(items), f"{len(items)} results, count={options.count}")
    except Exception as e:
        record(label, False, repr(e))


async def check_embedding(capabilities: Providers) -> None:
    """Checked on its own config block, one real call: a wrong embedding endpoint
    otherwise surfaces days later as episode recall quietly degrading, never as
    a boot failure."""
    cfg = config().default.capabilities.embedding
    label = f"embedding ({cfg.provider})"
    try:
        vecs = await capabilities.embedding.embed(["预检"])
        record(
            label,
            bool(vecs) and len(vecs[0]) == cfg.dimensions,
            f"{cfg.model}, {len(vecs[0])} dims",
        )
    except Exception as e:
        record(label, False, repr(e))


def check_keys(capabilities: Providers) -> None:
    """Check the key each capability actually points at, not a hardcoded list - two
    capabilities may share one name or not, and only the config knows."""
    settings = config().default.capabilities
    record("providers selected", True, capabilities.describe())
    seen: dict[str, str] = {}
    for label, name, provider in (
        ("text", settings.text.credential_env, capabilities.text),
        ("vision", settings.vision.credential_env, capabilities.vision),
        ("search", settings.search.credential_env, capabilities.search),
        ("embedding", settings.embedding.credential_env, capabilities.embedding),
    ):
        if not provider.needs_key:
            record(f"{label} key not needed ({provider.name})", True)
            continue
        key = read_api_key(name)
        shared = f", shared with {seen[name]}" if name in seen else ""
        seen.setdefault(name, label)
        record(
            f"{label} key present ({name})",
            bool(key),
            (f"{len(key)} chars{shared}" if key else "missing"),
        )


async def main() -> int:
    try:
        bundle = config()
        record("config parses", True)
    except Exception:
        record("config parses", False, traceback.format_exc(limit=1))
        return 1

    capabilities = build_providers(bundle.default)
    try:
        check_keys(capabilities)
        await capabilities.asr.start()
        await check_db()
        await check_text(capabilities)
        await check_vision(capabilities)
        await check_asr(capabilities)
        await check_search(capabilities)
        await check_embedding(capabilities)
    finally:
        from qqbot.db import close_pool

        await capabilities.aclose()
        await close_pool()

    failed = [n for n, ok, _ in RESULTS if not ok]
    print()
    if failed:
        print(f"{len(failed)} check(s) failed: {', '.join(failed)}")
        return 1
    print("all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
