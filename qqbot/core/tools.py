"""Tool definitions for the tool loop: web search (D5), and the group's own archive.

The archive tool is the cheap one: the bot sits on a complete L0 record of everything
said in the group - pictures described, voice transcribed - and anything outside the
conversation window is reachable only through it: a link posted yesterday, what was
agreed last week. The prompt pushes a fixed window; this is the pull path for everything
behind it, and it costs a SQL query.
"""

from __future__ import annotations

import json
import logging
import re
from dataclasses import dataclass, field

from ..db import pool, repo
from ..providers import providers
from ..providers.base import QuotaExhausted
from ..settings import Settings, config, ptext
from ..util import SYS_L, SYS_R, defang, fmt_when, merge_overlapping, sysmark, why
from . import retrieval
from .media import MEDIA

log = logging.getLogger("qqbot.tools")


@dataclass
class ToolCtx:
    """What tool execution may reach beyond the database: the live protocol side,
    and the map from the prompt's line numbers to the messages they name. Only
    inspect_image needs either; the retrieval tools stay context-free."""

    bot: object = None
    by_seq: dict[int, object] = field(default_factory=dict)  # seq -> ChatMsg


def tool_defs() -> list[dict]:
    """The tool definitions, built fresh so a /reload'ed description applies.

    The schemas are code (design goal 7 - structure is the contract the executor
    matches on); only the descriptions, which are prompts, come from the registry.
    """
    return [
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": ptext("tool_web_search"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "搜索关键词，尽量短"}
                    },
                    "required": ["query"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "search_history",
                "description": ptext("tool_search_history"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "要找的关键词，可用空格分开多个，多个关键词需同时命中",
                        },
                        "speaker": {
                            "type": "string",
                            "description": "只看这个人说的话，填昵称；不填则不限发言人；"
                                           "若上文中该人名字后带同名编号标注，"
                                           "需连标注一起原样填入，以精确锁定该人，避免混入同名者",
                        },
                        "days": {
                            "type": "integer",
                            "description": "只看最近这些天；不填则不限时间",
                        },
                    },
                    "required": ["query"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "read_url",
                "description": ptext("tool_read_url"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "要读取的网页地址，须以 http:// 或 https:// 开头",
                        }
                    },
                    "required": ["url"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "inspect_image",
                "description": ptext("tool_inspect_image"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "seq": {
                            "type": "integer",
                            "description": "图片所在消息的编号（#后面的数字）",
                        },
                        "question": {
                            "type": "string",
                            "description": "要对这张图问的具体问题",
                        },
                        "which": {
                            "type": "integer",
                            "description": "该消息里的第几张图，从 1 数起；不填为第 1 张",
                        },
                    },
                    "required": ["seq", "question"],
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "recall_events",
                "description": ptext("tool_recall_events"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "一句话描述要找的事",
                        }
                    },
                    "required": ["question"],
                },
            },
        },
    ]

#: How many archive hits one call returns. Enough to answer "when did we discuss this";
#: a model that wants more can search again with better words.
HISTORY_HITS = 8
#: And how much of each message. A link survives this; a wall of text is cut where the
#: model has seen enough to decide whether to search again.
HISTORY_SNIPPET = 200


#: The namesake form current names render as: the name plus the reserved
#: namesake tag carrying the permanent serial. A speaker argument in this shape
#: narrows by the serial's account, never by the name half - the name is exactly
#: what the two people share. The legacy parenthesised form (name(N)) is still
#: accepted: old transcripts and archived @-resolutions carry it, and the model
#: copies speaker names verbatim from whatever line it read.
_SEQ_NAME = re.compile(rf"^(.+){SYS_L}同名(\d{{1,9}}){SYS_R}$")
_SEQ_NAME_LEGACY = re.compile(r"^(.+)\((\d{1,9})\)$")


async def _carried_name(group_id: int, uid: str, name: str) -> bool:
    """Whether this account has ever spoken here under this display name."""
    if not name:
        return False
    return bool(await pool().fetchval(
        """SELECT EXISTS(SELECT 1 FROM raw_event
             WHERE group_id=$1 AND platform_user_id=$2 AND event_type='message'
               AND (payload->'sender'->>'card' = $3
                    OR payload->'sender'->>'nickname' = $3))""",
        group_id, uid, name))


def _like(word: str) -> str:
    """One keyword as a LIKE pattern, with the pattern characters made literal."""
    return "%" + word.replace("\\", "\\\\").replace("%", r"\%").replace("_", r"\_") + "%"


async def search_history(group_id: int, query: str, *, speaker: str | None = None,
                         days: int | None = None) -> str:
    """The archive, searched. Free - one SQL query, no model involved.

    All keywords must hit (ILIKE ALL), because the failure mode of OR over a chat archive
    is a page of one-word matches. Newest first out of the database, shown oldest first,
    so what the model reads scans like the conversation did.

    `speaker` narrows to one person's lines by display name - "what did X say about Y"
    is unanswerable with keywords alone, which match everyone who mentioned X. A name,
    not an account id: names are all the model ever sees. The numbered form the prompt
    shows for namesakes - name(N) - is accepted too: the serial maps to exactly one
    account, where the name half would match both people. `days` narrows to the recent
    past the same way.

    Each hit comes wrapped in its surrounding lines (retrieval.history_context each
    way): chat is written in fragments, and the matched line is routinely a bare
    answer to the line above it. The filters pick the hits; the context is whatever
    actually surrounds them - group notices included, since a recall or a mute is
    often the very thing a line responds to. Windows that touch merge into one
    block; blocks are separated by an ellipsis line.
    """
    words = [w for w in (query or "").split() if w][:5]
    if not words:
        return "（关键词为空）"
    sp = (speaker or "").strip()
    uid: str | None = None
    if m := (_SEQ_NAME.fullmatch(sp) or _SEQ_NAME_LEGACY.fullmatch(sp)):
        uid = await repo.member_of_seq(group_id, int(m.group(2)))
        if uid is not None and not await _carried_name(group_id, uid,
                                                       m.group(1).strip()):
            # A member whose literal card ends in (3) must not resolve through
            # serial 3 to an unrelated account: the serial only decides when
            # its account has actually carried the name half. (The reserved form
            # cannot be a literal card, but the guard is kept uniform - the
            # model can mistype a serial either way.)
            uid = None
    rows = await pool().fetch(
        """SELECT id, occurred_at, payload, plain_text FROM raw_event
            WHERE group_id=$1 AND event_type='message'
              AND plain_text ILIKE ALL($2::text[])
              AND ($4::text IS NULL
                   OR payload->'sender'->>'card' ILIKE $4
                   OR payload->'sender'->>'nickname' ILIKE $4)
              AND ($5::int IS NULL
                   OR occurred_at >= NOW() - make_interval(days => $5))
              AND ($6::text IS NULL OR platform_user_id = $6)
            ORDER BY occurred_at DESC, id DESC LIMIT $3""",
        group_id, [_like(w) for w in words], HISTORY_HITS,
        _like(sp) if sp and uid is None else None,
        days if days and days > 0 else None,
        uid,
    )
    if not rows:
        return "（存档里没有搜到）"
    ctx = max(0, config().default.retrieval.history_context)
    if not ctx:
        return "\n".join(_history_line(r) for r in reversed(rows))
    return await _with_context(group_id, [r["id"] for r in rows], ctx)


def _history_line(r) -> str:
    payload = r["payload"] or {}
    sender = payload.get("sender") or {}
    # defang the name on render: rows filed before Sender.parse neutralized
    # names can carry anything. The text is left as stored - its markers are
    # system writing, and defanging would destroy them.
    who = defang((sender.get("card") or sender.get("nickname") or "?")).strip()
    text = (r["plain_text"] or "").strip()[:HISTORY_SNIPPET]
    # fmt_when, not strftime on the raw value: asyncpg returns timestamptz in UTC,
    # and a UTC wall time here would disagree with every stamp in the history window.
    return f"{sysmark(fmt_when(r['occurred_at']))} {who}: {text}"


async def _with_context(group_id: int, hit_ids: list, ctx: int) -> str:
    """The hits rendered inside their surrounding conversation.

    One query fetches, per hit, the ctx archive lines on either side of it (by
    the archive's own order, (occurred_at, id) - the id is a UUID, so it breaks
    ties without meaning anything). A window is contiguous by construction, so
    two windows overlap exactly when they share a row: overlapping windows are
    merged into one block, and blocks render oldest first with an ellipsis line
    between them. Worst case is HISTORY_HITS disjoint blocks of 2*ctx+1 lines.
    """
    nrows = await pool().fetch(
        """SELECT h.id AS hit, n.id, n.occurred_at, n.payload, n.plain_text
             FROM unnest($2::uuid[]) AS h(id)
             JOIN raw_event he ON he.id = h.id
            CROSS JOIN LATERAL (
              (SELECT id, occurred_at, payload, plain_text FROM raw_event
                WHERE group_id=$1 AND event_type='message'
                  AND (occurred_at, id) <= (he.occurred_at, he.id)
                ORDER BY occurred_at DESC, id DESC LIMIT $3)
              UNION ALL
              (SELECT id, occurred_at, payload, plain_text FROM raw_event
                WHERE group_id=$1 AND event_type='message'
                  AND (occurred_at, id) > (he.occurred_at, he.id)
                ORDER BY occurred_at ASC, id ASC LIMIT $4)
            ) n""",
        group_id, hit_ids, ctx + 1, ctx,
    )
    by_id = {}
    windows: dict = {}
    for r in nrows:
        by_id[r["id"]] = r
        windows.setdefault(r["hit"], set()).add(r["id"])
    blocks = merge_overlapping(list(windows.values()))
    order = lambda rid: (by_id[rid]["occurred_at"], by_id[rid]["id"])  # noqa: E731
    lines: list[str] = []
    for b in sorted(blocks, key=lambda b: min(order(rid) for rid in b)):
        if lines:
            lines.append("……")
        lines.extend(_history_line(by_id[rid]) for rid in sorted(b, key=order))
    return "\n".join(lines)


def render_results(items: list[dict]) -> str:
    if not items:
        return "（没搜到有用的结果）"
    return "\n".join(
        f"{i}. {it['title']}\n{it['content']}" for i, it in enumerate(items, 1)
    )


#: How much of one page read_url hands the model. Pages are unbounded; the reply is
#: not - this is enough for an article's substance, and the model can say what the
#: page is rather than drown in it. Page text is untrusted input like search
#: snippets; the standing rules (private_rules) already cover text that tries to
#: read as instructions.
URL_CONTENT_CHARS = 3000

class Failure(str):
    """A tool answer that obtained nothing - a transport failure, a bad argument,
    an unreachable target. Rendered to the model verbatim like any other answer;
    the *type* is the out-of-band verdict, the way media.Unsettled carries one.
    It replaces a hand-maintained list of string openings that had already
    drifted (it guarded an answer nothing returned any more). A no-result search
    is NOT a Failure on purpose - searching and finding nothing is verification
    work, and the provenance marker may certify it."""
    __slots__ = ()


def verified(out: str) -> bool:
    """Whether a tool answer represents work that actually obtained something.
    The provenance marker excludes failed calls: it certifies that the reply was
    checked, and an answer improvised after a failed lookup is exactly the guess
    it must not certify."""
    return not isinstance(out, Failure)


async def execute(call: dict, *, cfg: Settings, group_id: str,
                  ctx: ToolCtx | None = None) -> str:
    name = call.get("function", {}).get("name")
    raw_args = call.get("function", {}).get("arguments") or "{}"
    try:
        args = json.loads(raw_args)
    except json.JSONDecodeError:
        return Failure("（工具参数解析失败）")
    # "null", "[]" and "42" are valid JSON too; anything but an object would crash
    # the .get() calls below and take the whole reply down with it, when a degenerate
    # argument string deserves the same in-band answer as an unparsable one.
    if not isinstance(args, dict):
        return Failure("（工具参数解析失败）")

    if name == "inspect_image":
        # The one paid tool: a vision call billed into the reply's ambient budget
        # scope. Everything else about the loop still holds - duplicate calls are
        # answered in words, and the per-reply cap is what bounds repetition.
        seq = args.get("seq")
        question = (args.get("question") or "").strip()
        if not isinstance(seq, int) or not question:
            return Failure("（需要消息编号和一个具体问题）")
        msg = (ctx.by_seq if ctx else {}).get(seq)
        if msg is None:
            return Failure(f"（上文里没有编号为 #{seq} 的消息）")
        refs = getattr(msg, "image_refs", None) or []
        if not refs:
            return Failure(f"（#{seq} 这条消息里没有图片）")
        which = args.get("which")
        idx = (which - 1) if isinstance(which, int) and which >= 1 else 0
        if idx >= len(refs):
            return Failure(f"（#{seq} 只有 {len(refs)} 张图）")
        answer = await MEDIA.inspect(refs[idx], question=question,
                                     bot=ctx.bot if ctx else None,
                                     group_id=group_id, cfg=cfg)
        return answer or Failure("（这张图已经取不回来了，以文字描述为准）")

    if name == "read_url":
        url = (args.get("url") or "").strip()
        if not url.startswith(("http://", "https://")):
            return Failure("（需要一个 http/https 网址）")
        try:
            text = await providers().search.extract(
                url, cfg=cfg.llm.search, group_id=group_id)
        except QuotaExhausted:
            raise
        except NotImplementedError:
            return Failure("（当前搜索后端不支持读取网页）")
        except Exception as e:
            log.warning("read_url failed: %s", why(e))
            return Failure("（网页读取失败）")
        if not text:
            return Failure("（这个网页没有可读的正文）")
        # Outside text: a page carrying the system brackets must not read as
        # system markup to the model, here or later in the frozen trace digest.
        return defang(text)[:URL_CONTENT_CHARS]

    if name == "recall_events":
        question = (args.get("question") or args.get("query") or "").strip()
        if not question:
            return Failure("（问题为空）")
        try:
            found = await retrieval.episode_lookup(group_id, question)
        except QuotaExhausted:
            # A limit, not a failure: it must reach the engine and drop the reply,
            # whichever tool's backend it comes from - only transport errors below
            # get talked around.
            raise
        except Exception as e:
            log.warning("recall_events failed: %s", why(e))
            return Failure("（记忆检索失败）")
        return found or "（事件记忆里没有相关的事）"

    query = (args.get("query") or "").strip()
    if not query:
        return Failure("（搜索词为空）")

    if name == "search_history":
        try:
            return await search_history(
                int(group_id), query,
                speaker=(args.get("speaker") or "").strip() or None,
                days=args.get("days") if isinstance(args.get("days"), int) else None,
            )
        except QuotaExhausted:
            raise
        except Exception as e:
            # Told to the model rather than raised: mid-loop, an exception here kills the
            # whole reply, and "the archive is unavailable" is an answerable situation.
            log.warning("search_history failed: %s", why(e))
            return Failure("（存档查询失败）")

    if name != "web_search":
        return Failure(f"（未知工具 {name}）")
    # Free within a monthly allowance, so there is no per-reply money check here: the
    # backend meters the allowance itself and refuses at it. That refusal propagates -
    # a limit reached means the reply is dropped, not answered in degraded form (the
    # owner's rule) - while a transport failure stays a tool answer, because a broken
    # network is an error to talk around, not a limit to respect.
    try:
        items = await providers().search.search(query, cfg=cfg.llm.search, group_id=group_id)
    except QuotaExhausted:
        raise
    except Exception as e:
        log.warning("web_search failed: %s", why(e))
        return Failure("（搜索失败）")
    # Same rule as read_url: search snippets are outside text.
    return defang(render_results(items))
