"""Tool definitions for the tool loop: web search, and the group's own archive.

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

from luqum import tree as _lq
from luqum.exceptions import ParseError as _LuqumParseError
from luqum.parser import parser as _luqum_parser

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
    and the map from the numbers the prompt shows to what they name. Only
    open_image needs either; the retrieval tools stay context-free."""

    bot: object = None
    #: Picture number -> (the message that posted it, its index in image_refs).
    by_pic: dict[int, tuple] = field(default_factory=dict)


def tool_defs() -> list[dict]:
    """The tool definitions, built fresh so a /reload'ed description applies.

    The schemas stay in code - they are the contract the executor matches on -
    while the descriptions, which are prompts, come from the registry.
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
                            "description": "检索式。空格分开的词都要命中（AND）；"
                                           "OR 表示任一命中，-词 表示排除，括号分组，"
                                           "引号内是含空格的原文片段。"
                                           "例：(打印机 OR 打印) -复印",
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
                "name": "open_image",
                "description": ptext("tool_open_image"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "n": {
                            "type": "integer",
                            "description": "图片编号，取自转写里 ⟦图片N:…⟧ 或 ⟦表情N:…⟧ 的 N",
                        },
                    },
                    "required": ["n"],
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


# -- the boolean query language ----------------------------------------------
# Lucene syntax, parsed by luqum rather than by hand. Juxtaposition is AND;
# OR, NOT/-, parentheses and quoted phrases compose freely, and models write
# this syntax fluently already. Only the boolean subset is accepted: fields,
# ranges, fuzziness and the rest of the Lucene DSL are rejected at the AST
# with a message the model can act on. _condition() walks the tree into one
# parameterized ILIKE expression - member text reaches SQL only as ILIKE
# parameters, never as SQL text.

_QUERY_NORMALIZE = str.maketrans({"（": "(", "）": ")", "“": '"', "”": '"',
                                  "「": '"', "」": '"', "'": '"'})


class QueryError(ValueError):
    """A query the grammar cannot read, with a message meant for the model."""


def _terms_of(node) -> int:
    if isinstance(node, (_lq.Word, _lq.Phrase)):
        return 1
    return sum(_terms_of(c) for c in node.children)


def _parse_query(q: str):
    """The query as a validated luqum tree: boolean subset only, term cap applied."""
    q = (q or "").translate(_QUERY_NORMALIZE).strip()
    if not q:
        raise QueryError("关键词为空")
    try:
        ast = _luqum_parser.parse(q)
    except _LuqumParseError as e:
        raise QueryError(f"无法解析（{e}）") from None
    cap = config().default.retrieval.max_query_terms
    if _terms_of(ast) > cap:
        raise QueryError(f"关键词太多（最多 {cap} 个），请拆成两次检索")
    return ast


def _condition(node, params: list, offset: int) -> str:
    """The luqum tree as one SQL boolean expression over plain_text.

    Only this function's own connectives reach the SQL string; every member
    word travels as an ILIKE parameter, numbered past the query's fixed ones.
    Node types outside the boolean subset raise, so an exotic Lucene feature
    fails the call in words instead of silently matching everything.
    """
    if isinstance(node, _lq.Word):
        params.append(_like(node.value))
        return f"plain_text ILIKE ${offset + len(params)}"
    if isinstance(node, _lq.Phrase):
        params.append(_like(node.value[1:-1]))       # value keeps its quotes
        return f"plain_text ILIKE ${offset + len(params)}"
    if isinstance(node, (_lq.Not, _lq.Prohibit)):
        return "NOT " + _condition(node.children[0], params, offset)
    if isinstance(node, _lq.Group):
        return _condition(node.children[0], params, offset)
    if isinstance(node, (_lq.AndOperation, _lq.UnknownOperation)):
        return ("(" + " AND ".join(_condition(c, params, offset)
                                   for c in node.children) + ")")
    if isinstance(node, _lq.OrOperation):
        return ("(" + " OR ".join(_condition(c, params, offset)
                                  for c in node.children) + ")")
    if isinstance(node, _lq.SearchField):
        raise QueryError("不支持「字段:值」写法，直接写关键词")
    raise QueryError(f"不支持的语法（{node.__class__.__name__}）")


async def search_history(group_id: int, query: str, *, speaker: str | None = None,
                         days: int | None = None) -> str:
    """The archive, searched. Free - one SQL query, no model involved.

    The query is a boolean expression (_parse_query): juxtaposition is AND -
    the default stays conjunction because the failure mode of OR over a chat
    archive is a page of one-word matches - with OR, -exclusion, parentheses
    and quoted phrases on top. The OR group is the home for synonyms:
    colloquial chat rarely uses the word the question used, and stacking
    guesses into AND is how a search comes back empty. A query the grammar
    cannot read is answered in-band with the parser's own message. Newest
    first out of the database, shown oldest first, so what the model reads
    scans like the conversation did.

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

    Every matched message is returned whole, and the result carries no length quota
    of its own. What it holds is already settled by retrieval.history_hits hits, each
    with its context lines, each line a message bounded by gateway.max_msg_len - and
    past that by money, the only limit this system has. A character budget on top
    would be a second bound on the same thing and the cruder one: it cannot tell a
    result worth its size from one that is not, while money can.
    """
    # One read of the retrieval settings for the whole call, so how many hits are
    # fetched and how much context is rendered cannot come from two different
    # configs if /reload lands in between.
    rcfg = config().default.retrieval
    # Parse and compile under one roof: the subset check lives in the compile
    # walk, and a rejected feature must answer in words exactly like a syntax
    # error does.
    terms: list[str] = []
    try:
        cond = _condition(_parse_query(query or ""), terms, offset=5)
    except QueryError as e:
        return f"（检索式有误：{e}）"
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
    # The condition string holds only this module's own connectives and ILIKE
    # placeholders numbered past the five fixed parameters; the member's words
    # travel in `terms`, never in SQL text.
    rows = await pool().fetch(
        f"""SELECT id, occurred_at, payload, plain_text FROM raw_event
            WHERE group_id=$1 AND event_type='message'
              AND {cond}
              AND ($3::text IS NULL
                   OR payload->'sender'->>'card' ILIKE $3
                   OR payload->'sender'->>'nickname' ILIKE $3)
              AND ($4::int IS NULL
                   OR occurred_at >= NOW() - make_interval(days => $4))
              AND ($5::text IS NULL OR platform_user_id = $5)
            ORDER BY occurred_at DESC, id DESC LIMIT $2""",
        group_id, rcfg.history_hits,
        _like(sp) if sp and uid is None else None,
        days if days and days > 0 else None,
        uid,
        *terms,
    )
    if not rows:
        return "（存档里没有搜到）"
    ctx = max(0, rcfg.history_context)
    if not ctx:
        return "\n".join(_history_line(r) for r in reversed(rows))
    return await _with_context(group_id, [r["id"] for r in rows], ctx)


def _history_line(r) -> str:
    payload = r["payload"] or {}
    sender = payload.get("sender") or {}
    # defang the name on render: rows filed before Sender.parse neutralized
    # names can carry anything. The text is left as stored - its markers are
    # system writing, and defanging would destroy them.
    who = defang(sender.get("card") or sender.get("nickname") or "?").strip()
    # The message whole. What a search is for is the substance of what was said, and
    # the archive's long messages are where that lives.
    text = (r["plain_text"] or "").strip()
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
    between them. Worst case is retrieval.history_hits disjoint blocks of 2*ctx+1
    lines.
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
    def order(rid):
        return by_id[rid]["occurred_at"], by_id[rid]["id"]

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



class Failure(str):
    """A tool answer that obtained nothing - a transport failure, a bad argument,
    an unreachable target. Rendered to the model verbatim like any other answer;
    the *type* is the out-of-band verdict, the way media.Unsettled carries one.
    It replaces a hand-maintained list of string openings that had already
    drifted (it guarded an answer nothing returned any more). A no-result search
    is NOT a Failure on purpose - searching and finding nothing is verification
    work, and the provenance marker may certify it."""
    __slots__ = ()


class Attachment(str):
    """A tool answer that hands the model a picture rather than describing one.

    Still a string - what it says is what everything downstream reads, so the
    provenance marker and the trajectory digest need no special case - but it
    carries the file blocks that go into the tool message beside that text. The
    vendor accepts a content array on a tool message, so the picture arrives as
    the answer to the call rather than as a separate turn appended behind it.
    """

    __slots__ = ("blocks",)

    def __new__(cls, text: str, blocks: list[dict]):
        s = super().__new__(cls, text)
        s.blocks = blocks
        return s

    def content(self) -> list[dict]:
        """This answer as the tool message's content."""
        return [*self.blocks, {"type": "text", "text": str(self)}]


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

    if name == "open_image":
        # Free: bytes and an upload, no model call. The reply model reads pictures
        # itself, so this hands it one rather than asking another model to look -
        # which could only ever answer the single question it was given.
        n = args.get("n")
        if not isinstance(n, int):
            return Failure("（需要一个图片编号）")
        found = (ctx.by_pic if ctx else {}).get(n)
        if found is None:
            return Failure(f"（上文里没有编号为 {n} 的图片）")
        msg, idx = found
        refs = getattr(msg, "image_refs", None) or []
        if idx >= len(refs):
            return Failure(f"（图片 {n} 已经取不回来了，以文字描述为准）")
        fid = await MEDIA.ensure_uploaded(refs[idx], bot=ctx.bot if ctx else None,
                                          group_id=group_id, cfg=cfg)
        if not fid:
            return Failure(f"（图片 {n} 已经取不回来了，以文字描述为准）")
        return Attachment(f"（这是图片 {n} 的原图。）", [{"type": "image", "id": fid}])

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
        body = defang(text)
        # A web page is the one input with no bound of its own; past the model's
        # context the request fails outright rather than degrading. A cut page is
        # told it was cut, or a sentence stopping mid-thought reads as the end of
        # the article.
        cap = cfg.retrieval.url_content_chars
        if len(body) > cap:
            return body[:cap] + "\n（网页正文过长，后面的没有读到）"
        return body

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
    # a limit reached means the reply is dropped, not answered in degraded form -
    # while a transport failure stays a tool answer, because a broken network is an
    # error to talk around, not a limit to respect.
    try:
        items = await providers().search.search(query, cfg=cfg.llm.search, group_id=group_id)
    except QuotaExhausted:
        raise
    except Exception as e:
        log.warning("web_search failed: %s", why(e))
        return Failure("（搜索失败）")
    # Same rule as read_url: search snippets are outside text.
    return defang(render_results(items))
