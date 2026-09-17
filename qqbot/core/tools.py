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
from dataclasses import dataclass, field

from luqum import tree as _lq
from luqum.exceptions import ParseError as _LuqumParseError
from luqum.parser import parser as _luqum_parser

from ..db import pool, repo
from ..providers import providers
from ..providers.base import QuotaExhausted
from ..providers.contracts import StoredImage, TextPart, ToolCall, ToolSpec
from ..settings import RetrievalCfg, Settings, config, ptext
from ..util import defang, display_name, fmt_when, merge_overlapping, sysmark, why
from . import retrieval
from .botapi import BotApi
from .media import MEDIA
from .member_numbers import MemberNumbers
from .segments import FACE_NAMES

log = logging.getLogger("qqbot.tools")

#: The tool every reply is sent through. Not executed here: the engine reads its
#: arguments and sends them (see engine.respond). Named, like its parameters, after
#: the OneBot message it becomes: text, at and reply segments.
SEND = "send_message"


@dataclass
class ToolCtx:
    """What tool execution may reach beyond the database: the live protocol side,
    and the maps from the numbers the prompt shows to what they name."""

    bot: BotApi | None = None
    #: Picture number -> (the message that posted it, its index in image_refs).
    by_pic: dict[int, tuple] = field(default_factory=dict)
    #: The member numbering of this prompt; search results extend it.
    people: MemberNumbers | None = None


def _tool(name: str, parameters: dict) -> ToolSpec:
    """One typed tool contract whose model-facing description lives in prompts."""

    description = ptext(f"tool_{name}")
    if name == SEND:
        catalog = "、".join(f"{face_id}={label}" for face_id, label in FACE_NAMES.items())
        description = description.replace("{{FACE_CATALOG}}", catalog)
    return ToolSpec(
        name=name,
        description=description,
        parameters=parameters,
    )


def _segment(kind: str, properties: dict, required: list[str] | None = None) -> dict:
    """One closed nested message-segment schema."""

    return {
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": [kind]},
            "data": {
                "type": "object",
                "properties": properties,
                "required": required or list(properties),
                "additionalProperties": False,
            },
        },
        "required": ["type", "data"],
        "additionalProperties": False,
    }


def _send_segments() -> list[dict]:
    text = _segment("text", {"text": {"type": "string"}})
    at = _segment(
        "at",
        {"member": {"type": "integer", "description": "成员名字后的 ⟦N⟧ 编号"}},
    )
    reply = _segment(
        "reply",
        {"line": {"type": "integer", "description": "发言行首的 #N 编号"}},
    )
    face = _segment("face", {"id": {"type": "integer", "minimum": 0}})
    empty = [_segment(kind, {}, []) for kind in ("dice", "rps")]
    member_contact = _segment(
        "contact_member",
        {"member": {"type": "integer", "description": "要推荐的成员编号"}},
    )
    group_contact = _segment("contact_group", {}, [])
    music = _segment(
        "music",
        {
            "platform": {
                "type": "string",
                "enum": ["qq", "163", "kugou", "kuwo", "migu"],
            },
            "id": {"type": "string"},
        },
    )
    custom_music = _segment(
        "music_custom",
        {
            "url": {"type": "string"},
            "audio": {"type": "string"},
            "title": {"type": "string"},
            "image": {"type": "string"},
            "singer": {"type": "string"},
        },
        ["url", "audio", "title", "image"],
    )
    json_card = _segment(
        "json",
        {"payload": {"type": "object", "additionalProperties": True}},
    )
    return [
        text,
        at,
        reply,
        face,
        *empty,
        member_contact,
        group_contact,
        music,
        custom_music,
        json_card,
    ]


def send_def() -> ToolSpec:
    """The only egress from the agent: an ordered, closed QQ segment sequence."""

    return _tool(
        SEND,
        {
            "type": "object",
            "properties": {
                "content": {
                    "type": "array",
                    "items": {"anyOf": _send_segments()},
                    "minItems": 1,
                    "maxItems": 32,
                    "description": "按发送顺序排列的 QQ 消息段；@ 可插在任意位置。",
                },
            },
            "required": ["content"],
            "additionalProperties": False,
        },
    )


def tool_defs(cfg: Settings | None = None) -> tuple[ToolSpec, ...]:
    """Build typed tool contracts after each prompt/config reload."""

    rcfg = (cfg or config().default).retrieval
    return (
        send_def(),
        _tool(
            "web_search",
            {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "搜索关键词，尽量短"}
                },
                "required": ["query"],
                "additionalProperties": False,
            },
        ),
        _tool(
            "search_history",
            {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "检索式。空格分开的词都要命中（AND）；"
                        "OR 表示任一命中，-词 表示排除，括号分组，"
                        "引号内是含空格的原文片段。例：(打印机 OR 打印) -复印",
                    },
                    "speaker": {
                        "type": "integer",
                        "description": "只看这位成员说的话，填成员编号（名字后的 ⟦N⟧）；"
                        "不填则不限发言人",
                    },
                    "speaker_name": {
                        "type": "string",
                        "description": "要找的人在上文中没有编号时，按昵称只看其发言；"
                        "有编号时用 speaker",
                    },
                    "days": {
                        "type": "integer",
                        "description": "只看最近这些天；不填则不限时间",
                    },
                },
                "required": ["query"],
                "additionalProperties": False,
            },
        ),
        _tool(
            "read_url",
            {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "要读取的网页地址，须以 http:// 或 https:// 开头",
                    }
                },
                "required": ["url"],
                "additionalProperties": False,
            },
        ),
        _tool(
            "open_images",
            {
                "type": "object",
                "properties": {
                    "ns": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "要查看的图片编号列表，取自转写里 ⟦图片N:…⟧、"
                        "⟦表情N:…⟧ 或 ⟦图片N⟧ 的 N；一次最多 "
                        f"{rcfg.open_images_max} 张",
                    }
                },
                "required": ["ns"],
                "additionalProperties": False,
            },
        ),
        _tool(
            "recall_events",
            {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "一句话描述要找的事",
                    }
                },
                "required": ["question"],
                "additionalProperties": False,
            },
        ),
    )


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


async def search_history(group_id: int, query: str, *, speaker: int | None = None,
                         speaker_name: str | None = None,
                         days: int | None = None, rcfg: RetrievalCfg | None = None,
                         self_id: str | None = None,
                         people: MemberNumbers | None = None) -> str:
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

    `speaker` narrows to one person's lines by member number - "what did X say about
    Y" is unanswerable with keywords alone, which match everyone who mentioned X. The
    number names a person, so every account of theirs counts, and two members sharing
    a name stay apart. `speaker_name` is the fallback for somebody the prompt shows no
    number for: a display-name match, which cannot tell two members sharing a name
    apart. A number this prompt never showed falls back to the name when one is
    given, and otherwise answers in words. `days` narrows to the recent past.

    Speakers render with member numbers from `people`, the prompt's own numbering -
    somebody who appears only here is numbered on sight, so the model can name them
    in a later call or @ them. The bot's own lines, archived under the persona's
    name, wear the self tag, so what it said earlier is not read as some member's
    statement. `self_id` is the bot's account; without it, own lines render like
    anyone's.

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
    sp = (speaker_name or "").strip()
    uids: list[str] | None = None
    if speaker is not None:
        account = people.account(speaker) if people is not None else None
        if account is not None:
            # A number names a person, and a person may hold several accounts.
            uids = await repo.accounts_sharing_person(account)
        elif not sp:
            return Failure(f"（记录里没有编号为 {speaker} 的成员）")
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
        _like(sp) if sp and uids is None else None,
        days if days and days > 0 else None,
        uids,
        *terms,
    )
    if not rows:
        return "（存档里没有搜到）"
    ctx = max(0, rcfg.history_context)
    if not ctx:
        shown = list(reversed(rows))
        await _learn(people, shown)
        text = _render_lines(shown, people, self_id)
    else:
        text = await _with_context(group_id, [r["id"] for r in rows], ctx, self_id,
                                   people)
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


async def _learn(people: MemberNumbers | None, rows: list) -> None:
    """Look up the persons behind every speaker one answer shows, before any is
    numbered, so accounts of one person share a number here as in the prompt."""
    if people is not None:
        await people.learn([str(r["platform_user_id"] or "") for r in rows])


def _render_lines(rows: list, people: MemberNumbers | None, self_id: str | None) -> str:
    """Archive rows as transcript lines, in the order given."""
    return "\n".join(_history_line(r, people, self_id) for r in rows)


def _history_line(r, people: MemberNumbers | None, self_id: str | None) -> str:
    uid = str(r["platform_user_id"] or "")
    who = _who(r)
    if people is not None and (n := people.number(uid)):
        who += sysmark(str(n))
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
                        self_id: str | None = None,
                        people: MemberNumbers | None = None) -> str:
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

    await _learn(people, list(by_id.values()))
    parts: list[str] = []
    for b in sorted(blocks, key=lambda b: min(order(rid) for rid in b)):
        rows = [by_id[rid] for rid in sorted(b, key=order)]
        parts.append(_render_lines(rows, people, self_id))
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


@dataclass(frozen=True, slots=True)
class Attachment:
    """A textual tool answer accompanied by provider-neutral image parts."""

    text: str
    parts: tuple[TextPart | StoredImage, ...]

    def content(self) -> tuple[TextPart | StoredImage, ...]:
        return (*self.parts, TextPart(self.text))

    def __str__(self) -> str:
        return self.text


def verified(out: str | Attachment) -> bool:
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


def number(v: object) -> int | None:
    """A number argument - a member or line number - as a positive int, or None.
    Digit strings are read, since models send integers that way too; bools,
    non-positive values and anything else are no number."""
    if isinstance(v, bool):
        return None
    if isinstance(v, str) and v.strip().isdigit():
        v = int(v.strip())
    return v if isinstance(v, int) and v > 0 else None



def _days(v: object) -> int | None:
    """The `days` argument as a bounded positive integer, or None."""
    n = number(v)
    return min(n, _MAX_DAYS) if n is not None else None


async def execute(
    call: ToolCall,
    *,
    cfg: Settings,
    group_id: str,
    ctx: ToolCtx | None = None,
) -> str | Attachment:
    name = call.name
    raw_args = call.arguments or "{}"
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
        parts: list[TextPart | StoredImage] = []
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
            parts += [TextPart(f"图片{n}："), fid]
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
        return Attachment(text, tuple(parts))

    if name == "read_url":
        url = _text(args, "url")
        if not url.startswith(("http://", "https://")):
            return Failure("（需要一个 http/https 网址）")
        reader = providers().page_reader
        if reader is None:
            return Failure("（当前搜索服务不支持读取网页）")
        try:
            text = await reader.read_page(
                url, cfg=cfg.capabilities.search, group_id=group_id
            )
        except QuotaExhausted:
            raise
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
            found = await retrieval.episode_lookup(group_id, question,
                                                   rcfg=cfg.retrieval)
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
                speaker=number(args.get("speaker")),
                speaker_name=_text(args, "speaker_name") or None,
                days=_days(args.get("days")),
                rcfg=cfg.retrieval,
                self_id=str(getattr(ctx.bot, "self_id", "") or "") if ctx else None,
                people=ctx.people if ctx else None,
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
        items = await providers().search.search(
            query, cfg=cfg.capabilities.search, group_id=group_id
        )
    except QuotaExhausted:
        raise
    except Exception as e:
        log.warning("web_search failed: %s", why(e))
        return Failure("（搜索失败）")
    # Same rule as read_url: search snippets are outside text.
    return defang(render_results(items))
