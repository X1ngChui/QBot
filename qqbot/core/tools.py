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
from ..settings import RetrievalCfg, Settings, config, ptext
from ..util import (SYS_L, SYS_R, defang, display_name, fmt_when, merge_overlapping,
                    sysmark, why)
from . import namesakes, retrieval
from .botapi import BotApi
from .media import MEDIA

log = logging.getLogger("qqbot.tools")


@dataclass
class ToolCtx:
    """What tool execution may reach beyond the database: the live protocol side,
    and the map from the numbers the prompt shows to what they name. Only
    open_images needs either; the retrieval tools stay context-free."""

    bot: BotApi | None = None
    #: Picture number -> (the message that posted it, its index in image_refs).
    by_pic: dict[int, tuple] = field(default_factory=dict)


def tool_defs(cfg: Settings | None = None) -> list[dict]:
    """The tool definitions, built fresh so a /reload'ed description applies.

    The schemas stay in code - they are the contract the executor matches on -
    while the descriptions, which are prompts, come from the registry. `cfg` is
    the group's own settings, so a limit the description states is the one the
    executor enforces for that group.
    """
    rcfg = (cfg or config().default).retrieval
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
                "name": "open_images",
                "description": ptext("tool_open_images"),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "ns": {
                            "type": "array",
                            "items": {"type": "integer"},
                            "description": "要查看的图片编号列表，取自转写里 ⟦图片N:…⟧、"
                                           "⟦表情N:…⟧ 或 ⟦图片N⟧ 的 N；一次最多 "
                                           f"{rcfg.open_images_max} 张",
                        },
                    },
                    "required": ["ns"],
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

#: The namesake form names render as: the name plus the reserved namesake tag
#: carrying the permanent serial. A speaker argument in this shape narrows by the
#: serial's account, never by the name half - the name is exactly what the two
#: people share.
_SEQ_NAME = re.compile(rf"^(.+){SYS_L}同名(\d{{1,9}}){SYS_R}$")


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


def _parse_query(q: str, cap: int):
    """The query as a validated luqum tree: boolean subset only, at most `cap` terms."""
    q = (q or "").translate(_QUERY_NORMALIZE).strip()
    if not q:
        raise QueryError("关键词为空")
    try:
        ast = _luqum_parser.parse(q)
    except _LuqumParseError as e:
        raise QueryError(f"无法解析（{e}）") from None
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
    if isinstance(node, (_lq.Group, _lq.Plus)):
        # A required term (+word) is what juxtaposition already means here.
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
                         days: int | None = None, rcfg: RetrievalCfg | None = None,
                         self_id: str | None = None) -> str:
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
    not an account id: names are all the model ever sees. The tagged form the prompt
    shows for namesakes is accepted too: the serial maps to one person - every
    account of theirs - where the name half would match both people. `days` narrows
    to the recent past the same way.

    Lines render with the same namesake tags the window shows (core.namesakes), and
    the bot's own lines - archived under the persona's name - wear the self tag, so
    what it said earlier is not read as some member's statement. `self_id` is the
    bot's account; without it, own lines render like anyone's.

    Each hit comes wrapped in its surrounding lines (retrieval.history_context each
    way): chat is written in fragments, and the matched line is routinely a bare
    answer to the line above it. The filters pick the hits; the context is whatever
    actually surrounds them - group notices included, since a recall or a mute is
    often the very thing a line responds to. Windows that touch merge into one
    block; blocks are separated by an ellipsis line.

    Every matched message is returned whole: what a search is for is the substance
    of what was said. The answer as a whole is bounded by retrieval.history_chars,
    because hits times context lines times gateway.max_msg_len can outgrow the
    model's context, where a request fails outright instead of degrading; a cut
    answer says so on its last line.
    """
    # One read of the retrieval settings for the whole call, so how many hits are
    # fetched and how much context is rendered cannot come from two different
    # configs if /reload lands in between. The tool loop passes the group's own
    # section, so a per-group override applies; a bare call reads the default.
    rcfg = rcfg or config().default.retrieval
    # Parse and compile under one roof: the subset check lives in the compile
    # walk, and a rejected feature must answer in words exactly like a syntax
    # error does. A Failure, not a plain answer: a search that never ran earns
    # no provenance badge.
    terms: list[str] = []
    try:
        cond = _condition(_parse_query(query or "", rcfg.max_query_terms), terms, offset=5)
    except QueryError as e:
        return Failure(f"（检索式有误：{e}）")
    sp = (speaker or "").strip()
    uid: str | None = None
    if m := _SEQ_NAME.fullmatch(sp):
        uid = await repo.member_of_seq(group_id, int(m.group(2)))
        if uid is not None and not await _carried_name(group_id, uid,
                                                       m.group(1).strip()):
            # The serial only decides when its account has actually carried the
            # name half; a mistyped serial must not resolve to an unrelated
            # account.
            uid = None
        if uid is None:
            # No account for the tag: fall back to the name half. The tag itself
            # is system notation nobody's card contains, so left in the pattern
            # it could only ever match nothing.
            sp = m.group(1).strip()
    # A serial names a person, and a person may hold several accounts here.
    uids = await repo.accounts_sharing_person(uid) if uid is not None else None
    # The condition string holds only this module's own connectives and ILIKE
    # placeholders numbered past the five fixed parameters; the member's words
    # travel in `terms`, never in SQL text.
    rows = await pool().fetch(
        f"""SELECT id, occurred_at, payload, plain_text, platform_user_id FROM raw_event
            WHERE group_id=$1 AND event_type='message'
              AND {cond}
              AND ($3::text IS NULL
                   OR payload->'sender'->>'card' ILIKE $3
                   OR payload->'sender'->>'nickname' ILIKE $3)
              AND ($4::int IS NULL
                   OR occurred_at >= NOW() - make_interval(days => $4))
              AND ($5::text[] IS NULL OR platform_user_id = ANY($5::text[]))
            ORDER BY occurred_at DESC, id DESC LIMIT $2""",
        group_id, rcfg.history_hits,
        _like(sp) if sp and uid is None else None,
        days if days and days > 0 else None,
        uids,
        *terms,
    )
    if not rows:
        return "（存档里没有搜到）"
    ctx = max(0, rcfg.history_context)
    if not ctx:
        text = _render_lines(list(reversed(rows)), await _tags_for(group_id, rows), self_id)
    else:
        text = await _with_context(group_id, [r["id"] for r in rows], ctx, self_id)
    if len(text) > rcfg.history_chars:
        # Cut at a line boundary so no message is shown half-said.
        head = text[:rcfg.history_chars]
        text = head[:head.rfind("\n")] if "\n" in head else head
        text += "\n（结果过长，后面的没有显示；请换更具体的检索式或缩小时间范围）"
    return text


def _who(r) -> str:
    """The archived speaker's bare display name. Defanged on render: rows filed
    before names were neutralized at ingest can carry anything."""
    sender = (r["payload"] or {}).get("sender") or {}
    return display_name(sender.get("card"), sender.get("nickname"), "成员")


async def _tags_for(group_id: int, rows: list) -> dict[str, str]:
    """Namesake tags over every row one answer shows, so two accounts sharing a
    name are told apart wherever in the answer they fall."""
    names = {str(r["platform_user_id"] or ""): _who(r) for r in rows}
    try:
        return await namesakes.tags(group_id, names)
    except Exception as e:
        log.warning("group %s: search namesake numbering unavailable: %s",
                    group_id, why(e))
        return {}


def _render_lines(rows: list, tags: dict[str, str], self_id: str | None) -> str:
    """Archive rows as transcript lines, in the order given."""
    return "\n".join(_history_line(r, tags, self_id) for r in rows)


def _history_line(r, tags: dict[str, str], self_id: str | None) -> str:
    uid = str(r["platform_user_id"] or "")
    who = _who(r) + tags.get(uid, "")
    if self_id and uid == self_id:
        # The same self tag the extraction transcript wears: the line is archived
        # under the persona's name, which reads as a member's otherwise.
        who += sysmark("你")
    # The message whole. What a search is for is the substance of what was said, and
    # the archive's long messages are where that lives. The text is left as stored -
    # its markers are system writing, and defanging would destroy them.
    text = (r["plain_text"] or "").strip()
    # fmt_when, not strftime on the raw value: asyncpg returns timestamptz in UTC,
    # and a UTC wall time here would disagree with every stamp in the history window.
    return f"{sysmark(fmt_when(r['occurred_at']))} {who}: {text}"


async def _with_context(group_id: int, hit_ids: list, ctx: int,
                        self_id: str | None = None) -> str:
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
        """SELECT h.id AS hit, n.id, n.occurred_at, n.payload, n.plain_text,
                  n.platform_user_id
             FROM unnest($2::uuid[]) AS h(id)
             JOIN raw_event he ON he.id = h.id
            CROSS JOIN LATERAL (
              (SELECT id, occurred_at, payload, plain_text, platform_user_id
                 FROM raw_event
                WHERE group_id=$1 AND event_type='message'
                  AND (occurred_at, id) <= (he.occurred_at, he.id)
                ORDER BY occurred_at DESC, id DESC LIMIT $3)
              UNION ALL
              (SELECT id, occurred_at, payload, plain_text, platform_user_id
                 FROM raw_event
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

    tags = await _tags_for(group_id, list(by_id.values()))
    parts: list[str] = []
    for b in sorted(blocks, key=lambda b: min(order(rid) for rid in b)):
        rows = [by_id[rid] for rid in sorted(b, key=order)]
        parts.append(_render_lines(rows, tags, self_id))
    return "\n……\n".join(parts)


def render_results(items: list[dict]) -> str:
    if not items:
        return "（没搜到有用的结果）"
    return "\n".join(
        f"{i}. {it['title']}\n{it['content']}" for i, it in enumerate(items, 1)
    )



class Failure(str):
    """A tool answer that obtained nothing - a transport failure, a bad argument,
    an unreachable target. Rendered to the model verbatim like any other answer;
    the *type* is the out-of-band verdict, the way media.Unsettled carries one,
    so no reader has to recognise the wording. A no-result search is NOT a
    Failure on purpose - searching and finding nothing is verification work, and
    the provenance marker may certify it."""
    __slots__ = ()


class Attachment(str):
    """A tool answer that hands the model pictures rather than describing them.

    Still a string - what it says is what everything downstream reads, so the
    provenance marker and the trajectory digest need no special case - but it
    carries the content parts that go into the tool message ahead of that text:
    each picture's number as a text part, then its file block, so the model can
    tell which number it is looking at. The vendor accepts a content array on a
    tool message, so the pictures arrive as the answer to the call rather than
    as a separate turn appended behind it.
    """

    __slots__ = ("parts",)

    def __new__(cls, text: str, parts: list[dict]):
        s = super().__new__(cls, text)
        s.parts = parts
        return s

    def content(self) -> list[dict]:
        """This answer as the tool message's content."""
        return [*self.parts, {"type": "text", "text": str(self)}]



def verified(out: str) -> bool:
    """Whether a tool answer represents work that actually obtained something.
    The provenance marker excludes failed calls: it certifies that the reply was
    checked, and an answer improvised after a failed lookup is exactly the guess
    it must not certify."""
    return not isinstance(out, Failure)


def _text(args: dict, key: str) -> str:
    """A string argument, stripped; "" for a missing one or one of another type.
    A model can send a number or a list where the schema said string, and a
    type error mid-loop would kill the whole reply where an empty argument is
    answered in words."""
    v = args.get(key)
    return v.strip() if isinstance(v, str) else ""


#: The furthest back a day filter reaches; past it the filter means "unbounded"
#: and would only overflow the interval arithmetic.
_MAX_DAYS = 36500


def _days(v: object) -> int | None:
    """The `days` argument as a positive int, or None for none. Digit strings
    are read (a common way models send integers); bools and non-positive values
    mean no filter."""
    if isinstance(v, bool):
        return None
    if isinstance(v, str) and v.strip().isdigit():
        v = int(v.strip())
    if isinstance(v, int) and v > 0:
        return min(v, _MAX_DAYS)
    return None


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

    if name == "open_images":
        # Free: bytes and an upload, no model call. The reply model reads pictures
        # itself, so this hands them over rather than asking another model to look
        # - which could only ever answer the single question it was given. Several
        # at a time, because a question is often about a set (the two screenshots
        # being compared, every picture in a forwarded record), and one round per
        # picture would spend the loop's bound on fetching.
        ns = args.get("ns")
        if (not isinstance(ns, list) or not ns
                or not all(isinstance(x, int) and not isinstance(x, bool) for x in ns)):
            return Failure("（需要图片编号列表。）")
        wanted = list(dict.fromkeys(ns))[:cfg.retrieval.open_images_max]
        parts: list[dict] = []
        shown: list[int] = []
        unknown: list[int] = []
        gone: list[int] = []
        for n in wanted:
            found = (ctx.by_pic if ctx else {}).get(n)
            if found is None:
                unknown.append(n)
                continue
            msg, idx = found
            refs = getattr(msg, "image_refs", None) or []
            fid = None
            if idx < len(refs):
                try:
                    fid = await MEDIA.ensure_uploaded(
                        refs[idx], bot=ctx.bot if ctx else None, group_id=group_id, cfg=cfg)
                except Exception as e:
                    # Answered in words like every other tool failure: an exception
                    # mid-loop would kill the whole reply, and "the picture could
                    # not be fetched" is an answerable situation.
                    log.warning("open_images failed on picture %d: %s", n, why(e))
            if not fid:
                gone.append(n)
                continue
            parts += [{"type": "text", "text": f"图片{n}："}, {"type": "image", "id": fid}]
            shown.append(n)

        def nums(xs: list[int]) -> str:
            return "、".join(str(x) for x in xs)

        notes = []
        if unknown:
            notes.append(f"记录中没有编号为 {nums(unknown)} 的图片或表情")
        if gone:
            notes.append(f"图片 {nums(gone)} 的原图已无法取回，以记录中的描述或标注为准")
        if not shown:
            return Failure("（" + "；".join(notes) + "。）")
        text = f"（以上是图片 {nums(shown)} 的原图。" + ("".join(f"{n}。" for n in notes)) + "）"
        return Attachment(text, parts)

    if name == "read_url":
        url = _text(args, "url")
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
        question = _text(args, "question") or _text(args, "query")
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

    query = _text(args, "query")
    if not query:
        return Failure("（搜索词为空）")

    if name == "search_history":
        try:
            return await search_history(
                int(group_id), query,
                speaker=_text(args, "speaker") or None,
                days=_days(args.get("days")),
                rcfg=cfg.retrieval,
                self_id=str(getattr(ctx.bot, "self_id", "") or "") if ctx else None,
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
