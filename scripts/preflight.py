"""Launch checklist, mechanised.

Run it inside the bot container after the keys are in place:

    docker compose run --rm bot python scripts/preflight.py

Checks, in order: the database answers, every key the config names resolves, and each
capability makes one real minimal call - text, vision and ASR (both
inline base64), search.
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

from qqbot.providers import providers
from qqbot.settings import config
from qqbot.util import read_api_key

def test_png(side: int = 64) -> bytes:
    """A real PNG, built here so the check needs no image library and no asset on disk.
    Vision models reject anything under 10px a side, so a 1x1 pixel will not do."""
    import struct
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
        bytes([0])
        + bytes(v for x in range(side) for v in (black if (x + y) % 16 < 8 else white))
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
        ver = await pool().fetchval("SELECT version()")
        tables = await pool().fetchval(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='public'"
        )
        record("postgres", tables > 0, f"{tables} tables, {ver.split(',')[0]}")
    except Exception as e:
        record("postgres", False, repr(e))


async def check_text() -> None:
    cfg = config().default.llm.text
    label = f"text ({cfg.model})"
    try:
        res = await providers().text.chat(
            [{"role": "user", "content": "只回复两个字：收到"}],
            cfg=cfg,
            max_tokens=512,
            effort="off",
            kind="preflight",
        )
        think = f" reasoning={res.reasoning}" if res.reasoning else ""
        record(label, bool(res.text),
               f"{res.text!r} in={res.in_hit}+{res.in_miss} out={res.out}{think}")
    except Exception as e:
        record(label, False, repr(e))


async def check_vision() -> None:
    cfg = config().default.llm.vision
    label = f"vision, base64 inline ({cfg.model})"
    try:
        from qqbot.settings import ptext
        desc = await providers().vision.describe(
            TEST_PNG, cfg=cfg, prompt=ptext("describe_image"), mime="image/png")
        record(label, True, repr(desc[:40]))
    except Exception as e:
        record(label, False, repr(e))


async def check_asr() -> None:
    cfg = config().default.llm.asr
    label = f"ASR, base64 inline ({cfg.model})"
    try:
        text = await providers().asr.transcribe(silence_wav(), cfg=cfg, fmt="wav", seconds=0.4)
        record(label, True, repr(text[:40]))
    except Exception as e:
        record(label, False, repr(e))


async def check_search() -> None:
    cfg = config().default.llm.search
    label = f"search ({cfg.backend})"
    try:
        items = await providers().search.search("今天天气", cfg=cfg)
        record(label, bool(items), f"{len(items)} results, count={cfg.count}")
    except Exception as e:
        record(label, False, repr(e))


async def check_embedding() -> None:
    """Checked on its own config block, one real call: a wrong embedding endpoint
    otherwise surfaces days later as episode recall quietly degrading, never as
    a boot failure."""
    cfg = config().default.llm.embedding
    label = f"embedding ({cfg.backend})"
    try:
        vecs = await providers().embedding.embed(["预检"], cfg=cfg)
        record(label, bool(vecs) and len(vecs[0]) == cfg.dimensions,
               f"{cfg.model}, {len(vecs[0])} dims")
    except Exception as e:
        record(label, False, repr(e))


def check_keys() -> None:
    """Check the key each capability actually points at, not a hardcoded list - two
    capabilities may share one name or not, and only the config knows."""
    llm = config().default.llm
    record("backends selected", True, providers().describe())
    seen: dict[str, str] = {}
    for label, name in (
        ("text", llm.text.api_key_env),
        ("vision", llm.vision.api_key_env),
        ("asr", llm.asr.api_key_env),
        ("search", llm.search.api_key_env),
        ("embedding", llm.embedding.api_key_env),
    ):
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
        config()
        record("config parses", True)
    except Exception:
        record("config parses", False, traceback.format_exc(limit=1))
        return 1

    check_keys()
    await check_db()
    await check_text()
    await check_vision()
    await check_asr()
    await check_search()
    await check_embedding()

    from qqbot.db import close_pool

    await providers().aclose()
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
