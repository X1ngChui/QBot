"""One-time data migration: legacy square-bracket markers to the reserved pair.

Rendered transcripts now write every system marker in the reserved brackets
(util.SYS_L/SYS_R) and neutralize the pair in member text, so a marker can no
longer be forged. The archive predates that grammar: raw_event.plain_text and
image_cache.description still carry markers in ASCII square brackets, which the
new legend no longer describes - unmigrated, an old picture description would
read as text a member typed.

Best-effort by construction. The old grammar is genuinely ambiguous - that is
why it was replaced - so this rewrites only the shapes the system itself
produced, in order of confidence:

  1. whole-string markers (a voice-only or picture-only message, notice lines);
  2. bare markers anywhere ("[图片]" and friends, no colon, no free text);
  3. described markers anywhere, non-greedy to the first closing bracket - a
     description containing "]" converts short, leaving stray tail characters,
     which is harmless;
  4. the provenance suffix on the bot's own archived replies.

A member-typed lookalike gets converted too; those lines were unreadable either
way, and the conversion at least matches how every reader parsed them at the
time. Idempotent: the patterns cannot match their own output.

Run inside the bot container, with the bot stopped or idle, and only after the
candidate queue is empty (consolidation re-renders old batches and validates
quotes against them - a quote taken across a rewritten marker would stop
matching):

    docker compose run --rm bot python scripts/migrate_markers.py
"""

from __future__ import annotations

import asyncio
import pathlib
import re
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

# Whole-line notice texts and self-contained markers: highest confidence.
_WHOLE = (
    "[一条消息被管理员撤回]", "[撤回了自己的一条消息]", "[加入了本群]",
    "[退出了本群]", "[被移出了本群]", "[被解除禁言]", "[被禁言]",
    "[戳了戳你]", "[戳了戳别人]",
)
#: Bare markers, no payload; safe to rewrite wherever they sit.
_BARE = (
    "[图片]", "[表情]", "[语音]", "[视频]", "[文件]", "[消息]", "[戳一戳]",
    "[卡片消息]", "[转发的聊天记录]", "[骰子]", "[猜拳]",
)
#: Prefixed whole-string patterns: "[被禁言 30 秒]", "[戳了戳 某人]".
_PREFIX_WHOLE = re.compile(r"^\[(被禁言 [^\]]+|戳了戳 [^\]]+)\]$")
#: Described markers, non-greedy: "[图片:...]", "[语音:...]", "[依据:...]" etc.
_DESCRIBED = re.compile(
    r"\[(图片|表情|语音|文件|分享|骰子|猜拳|依据)([:：][^\]]*)\]")
#: The forward wrap holds nested markers, so it closes at the LAST bracket of
#: the string tail it opened - old forwards were rendered as one marker whose
#: nested bodies could themselves carry "[图片]" etc. Greedy, anchored to the
#: string end, matches how the wrap was actually written (nothing followed it).
_FORWARD = re.compile(r"\[(转发的聊天记录[^：\]]*)：(.*)\]$", re.S)


def convert(text: str) -> str:
    if not text or "[" not in text:
        return text
    out = text
    if out in _WHOLE or _PREFIX_WHOLE.fullmatch(out):
        return "⟦" + out[1:-1] + "⟧"
    out = _FORWARD.sub(lambda m: "⟦" + m.group(1) + "：" + m.group(2) + "⟧", out)
    out = _DESCRIBED.sub(lambda m: "⟦" + m.group(1) + m.group(2) + "⟧", out)
    for b in _BARE:
        out = out.replace(b, "⟦" + b[1:-1] + "⟧")
    return out


async def main() -> int:
    from qqbot.db import close_pool, init_pool, pool

    await init_pool()
    try:
        changed_msgs = 0
        rows = await pool().fetch(
            """SELECT id, plain_text FROM raw_event
                WHERE event_type='message' AND plain_text LIKE '%[%'""")
        for r in rows:
            new = convert(r["plain_text"] or "")
            if new != r["plain_text"]:
                await pool().execute(
                    "UPDATE raw_event SET plain_text=$2 WHERE id=$1",
                    r["id"], new)
                changed_msgs += 1

        changed_cache = 0
        for r in await pool().fetch(
                "SELECT key, description FROM image_cache "
                "WHERE description LIKE '[%'"):
            d = r["description"] or ""
            # Cache rows are exactly one marker: "[图片:...]" / "[表情:...]",
            # or the bare refusal form - rewrite the whole value.
            if d.startswith("[") and d.endswith("]"):
                await pool().execute(
                    "UPDATE image_cache SET description=$2 WHERE key=$1",
                    r["key"], "⟦" + d[1:-1] + "⟧")
                changed_cache += 1

        print(f"rewrote {changed_msgs} raw_event rows, "
              f"{changed_cache} image_cache rows")
        return 0
    finally:
        await close_pool()


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
