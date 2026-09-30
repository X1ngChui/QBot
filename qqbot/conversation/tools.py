"""Tool definitions for retrieval, durable scheduling and observable single sends.

The archive tool is the cheap one: the bot sits on a complete L0 record of everything
said in the group - pictures described, voice transcribed - and anything outside the
conversation window is reachable only through it: a link posted yesterday, what was
agreed last week. The prompt pushes a fixed window; this is the pull path for everything
behind it, and it costs a SQL query.
"""

from __future__ import annotations

from qqbot.domain.ids import AccountId
import logging
import uuid
from dataclasses import dataclass, field

from pydantic import ValidationError
from qqbot.conversation.tool_registry import ToolEntry, ToolRegistry
from qqbot.conversation.fuel import MAX_ARGUMENT_BYTES
from luqum import tree as _lq
from luqum.exceptions import ParseError as _LuqumParseError
from luqum.parser import parser as _luqum_parser

from collections.abc import Callable
import asyncpg
from qqbot.repositories.identity import IdentityRepository
from qqbot.domain.archive import ArchivedMessage
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import GroupId
from qqbot.prompting import PromptKey
from qqbot.prompting import tool_prompt_key
from qqbot.providers.base import Providers
from qqbot.providers.base import QuotaExhausted
from qqbot.providers.contracts import StoredImage
from qqbot.providers.contracts import TextPart
from qqbot.providers.contracts import ToolCall
from qqbot.providers.contracts import ToolSpec
from qqbot.repositories.archive import archive_columns
from qqbot.repositories.archive import archived_message
from qqbot.repositories.archive import archived_messages
from qqbot.services.scheduled_tasks import ScheduledTaskService, TaskInputError
from qqbot.repositories.scheduled_task import ScheduledTask, TaskLimit
from qqbot.conversation.limits import (
    HISTORY_LIMITS,
    IMAGE_LIMITS,
    PAGE_LIMITS,
    RECALL_LIMITS,
    SearchHistoryLimits,
)
from qqbot.providers.contracts import SearchOptions
from functools import partial
from qqbot.prompting import PromptCatalog
from qqbot.clock import Clock
from qqbot.configuration import Settings
from qqbot.util import defang
from qqbot.util import merge_overlapping
from qqbot.util import sysmark
from qqbot.util import why
from qqbot.services import retrieval
from qqbot.gateway.botapi import BotApi
from qqbot.media.service import MediaProcessor
from qqbot.conversation.member_numbers import BOT_DISPLAY_NUMBER
from qqbot.conversation.member_numbers import MemberNumbers
from qqbot.delivery.contract import send_arguments_model
from qqbot.gateway.segments import FACE_NAMES
from qqbot.gateway.segments import number_at_mentions

log = logging.getLogger("qqbot.tools")

#: Outbound actions are executed by AgentRun, not by the retrieval dispatcher.
SEND = "send_message"
FINISH = "finish_reply"


@dataclass
class ToolCtx:
    """Runtime capabilities plus the prompt-local protocol and number maps."""

    providers: Providers
    media: MediaProcessor
    bot: BotApi | None = None
    tasks: ScheduledTaskService | None = None
    parent_task: ScheduledTask | None = None
    #: Picture number -> (the message that posted it, its index in image_refs).
    by_pic: dict[int, tuple] = field(default_factory=dict)
    #: The member numbering of this prompt; search results extend it.
    people: MemberNumbers | None = None
    registry: ToolRegistry | None = None
    clock: Clock = field(kw_only=True)
    database: Callable[[], asyncpg.Pool] | None = None
    identities: IdentityRepository | None = None


def _tool(
    name: str,
    parameters: dict,
    *,
    cfg: Settings,
    prompts: PromptCatalog,
) -> ToolSpec:
    """One typed tool contract whose model-facing description lives in prompts."""

    key = tool_prompt_key(name)
    values = {}
    if key is PromptKey.TOOL_SEND_MESSAGE:
        settings = cfg
        values["face_catalog"] = "、".join(
            f"{face_id}={label}" for face_id, label in FACE_NAMES.items()
        )
        values["message_limit"] = str(settings.conversation.max_messages_per_reply)
    description = prompts.render(key, values)
    return ToolSpec(
        name=name,
        description=description,
        parameters=parameters,
    )


def send_def(cfg: Settings, *, prompts: PromptCatalog) -> ToolSpec:
    """One QQ message, acknowledged and observed before the agent resumes."""

    settings = cfg
    model = send_arguments_model(settings.conversation.max_text_chars_per_message)
    return _tool(SEND, model.model_json_schema(), cfg=settings, prompts=prompts)


def finish_def(cfg: Settings, *, prompts: PromptCatalog) -> ToolSpec:
    """End this reply without sending another message."""

    return _tool(
        FINISH,
        {"type": "object", "properties": {}, "additionalProperties": False},
        cfg=cfg,
        prompts=prompts,
    )


def tool_registry(cfg: Settings, *, prompts: PromptCatalog) -> ToolRegistry:
    """Build typed tool contracts from the immutable startup configuration."""

    catalog = prompts
    make_tool = partial(_tool, cfg=cfg, prompts=catalog)
    return ToolRegistry(
        (
            ToolEntry(send_def(cfg, prompts=catalog), None, exclusive=True),
            ToolEntry(finish_def(cfg, prompts=catalog), None, exclusive=True),
            ToolEntry(
                make_tool(
                    "schedule_task",
                    {
                        "type": "object",
                        "properties": {
                            "intent": {
                                "type": "string",
                                "description": "到期时要重新判断的任务，不是预写好的回复",
                            },
                            "run_at": {
                                "type": "string",
                                "description": "ISO 8601 时间，必须带时区偏移",
                            },
                            "delay_seconds": {
                                "type": "integer",
                                "description": "从现在起的秒数；与 run_at 二选一",
                            },
                        },
                        "required": ["intent"],
                        "additionalProperties": False,
                    },
                ),
                _execute_scheduled,
                parallel_safe=False,
            ),
            ToolEntry(
                make_tool(
                    "list_scheduled_tasks",
                    {
                        "type": "object",
                        "properties": {"page": {"type": "integer", "minimum": 1, "maximum": 10000}},
                        "additionalProperties": False,
                    },
                ),
                _execute_scheduled,
                parallel_safe=False,
            ),
            ToolEntry(
                make_tool(
                    "get_scheduled_task",
                    {
                        "type": "object",
                        "properties": {"id": {"type": "string"}},
                        "required": ["id"],
                        "additionalProperties": False,
                    },
                ),
                _execute_scheduled,
                parallel_safe=False,
            ),
            ToolEntry(
                make_tool(
                    "update_scheduled_task",
                    {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "intent": {"type": "string", "minLength": 1, "maxLength": 500},
                            "run_at": {"type": "string"},
                            "delay_seconds": {"type": "integer"},
                        },
                        "required": ["id"],
                        "additionalProperties": False,
                    },
                ),
                _execute_scheduled,
                parallel_safe=False,
            ),
            ToolEntry(
                make_tool(
                    "cancel_scheduled_task",
                    {
                        "type": "object",
                        "properties": {"id": {"type": "string"}},
                        "required": ["id"],
                        "additionalProperties": False,
                    },
                ),
                _execute_scheduled,
                parallel_safe=False,
            ),
            ToolEntry(
                make_tool(
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
                _execute_web_search,
                parallel_safe=True,
            ),
            ToolEntry(
                make_tool(
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
                _execute_search_history,
                parallel_safe=True,
            ),
            ToolEntry(
                make_tool(
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
                _execute_read_url,
                parallel_safe=True,
            ),
            ToolEntry(
                make_tool(
                    "open_images",
                    {
                        "type": "object",
                        "properties": {
                            "ns": {
                                "type": "array",
                                "items": {"type": "integer"},
                                "description": "要查看的图片编号列表，取自转写里 ⟦图片N:…⟧、"
                                "⟦表情N:…⟧ 或 ⟦图片N⟧ 的 N；一次最多 "
                                f"{IMAGE_LIMITS.max_images} 张",
                            }
                        },
                        "required": ["ns"],
                        "additionalProperties": False,
                    },
                ),
                _execute_open_images,
                parallel_safe=True,
            ),
            ToolEntry(
                make_tool(
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
                _execute_recall_events,
                parallel_safe=False,
            ),
        )
    )


def tool_defs(cfg: Settings, *, prompts: PromptCatalog) -> tuple[ToolSpec, ...]:
    return tool_registry(cfg, prompts=prompts).definitions


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

_QUERY_NORMALIZE = str.maketrans(
    {"（": "(", "）": ")", "“": '"', "”": '"', "「": '"', "」": '"', "'": '"'}
)


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
        params.append(_like(node.value[1:-1]))  # value keeps its quotes
        return f"plain_text ILIKE ${offset + len(params)}"
    if isinstance(node, (_lq.Not, _lq.Prohibit)):
        return "NOT " + _condition(node.children[0], params, offset)
    if isinstance(node, (_lq.Group, _lq.Plus)):
        # A required term (+word) is what juxtaposition already means here.
        return _condition(node.children[0], params, offset)
    if isinstance(node, (_lq.AndOperation, _lq.UnknownOperation)):
        return "(" + " AND ".join(_condition(c, params, offset) for c in node.children) + ")"
    if isinstance(node, _lq.OrOperation):
        return "(" + " OR ".join(_condition(c, params, offset) for c in node.children) + ")"
    if isinstance(node, _lq.SearchField):
        raise QueryError("不支持「字段:值」写法，直接写关键词")
    raise QueryError(f"不支持的语法（{node.__class__.__name__}）")


async def search_history(
    group_id: GroupId,
    query: str,
    *,
    database: Callable[[], asyncpg.Pool],
    identities: IdentityRepository,
    clock: Clock,
    speaker: int | None = None,
    speaker_name: str | None = None,
    days: int | None = None,
    rcfg: SearchHistoryLimits | None = None,
    self_id: AccountId | None = None,
    people: MemberNumbers | None = None,
) -> str:
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

    Each hit comes wrapped in its surrounding lines
    (tools.search_history.context_lines each way): chat is written in fragments,
    and the matched line is routinely a bare answer to the line above it. The
    filters pick the hits; the context is whatever actually surrounds them - group
    notices included, since a recall or a mute is often the very thing a line responds
    to. Windows that touch merge into one block; blocks are separated by an ellipsis.

    Every matched message is returned whole. The answer is bounded by
    tools.search_history.max_result_chars so disjoint context windows cannot outgrow
    the model context; a cut answer says so on its last line.
    """
    # Freeze one settings object for the entire call so all query and rendering bounds
    # are visibly derived from the same startup configuration.
    rcfg = rcfg or HISTORY_LIMITS
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
    uids: list[AccountId] | None = None
    if speaker is not None:
        account = people.account(speaker) if people is not None else None
        if account is not None:
            # A number names a person, and a person may hold several accounts.
            uids = await identities.linked_account_ids(account)
        elif not sp:
            return Failure(f"（记录里没有编号为 {speaker} 的成员）")
    # The condition string holds only this module's own connectives and ILIKE
    # placeholders numbered past the five fixed parameters; the member's words
    # travel in `terms`, never in SQL text.
    rows = await database().fetch(
        f"""SELECT {archive_columns()} FROM raw_event
            WHERE group_id=$1 AND event_type IN ('message','notice')
              AND {cond}
              AND ($3::text IS NULL
                   OR payload->'sender'->>'card' ILIKE $3
                   OR payload->'sender'->>'nickname' ILIKE $3)
              AND ($4::int IS NULL
                   OR occurred_at >= NOW() - make_interval(days => $4))
              AND ($5::text[] IS NULL OR platform_user_id = ANY($5::text[]))
            ORDER BY occurred_at DESC, id DESC LIMIT $2""",
        group_id.to_db(),
        rcfg.max_hits,
        _like(sp) if sp and uids is None else None,
        days if days and days > 0 else None,
        uids,
        *terms,
    )
    if not rows:
        return "（存档里没有搜到）"
    messages = archived_messages(rows)
    ctx = rcfg.context_lines
    if not ctx:
        shown = list(reversed(messages))
        await _learn(people, shown)
        text = _render_lines(shown, people, self_id, clock)
    else:
        text = await _with_context(
            group_id,
            [message.raw_event_id for message in messages],
            ctx,
            self_id,
            people,
            database=database,
            clock=clock,
        )
    if len(text) > rcfg.max_result_chars:
        # Cut at a line boundary so no message is shown half-said.
        head = text[: rcfg.max_result_chars]
        text = head[: head.rfind("\n")] if "\n" in head else head
        text += "\n（结果过长，后面的没有显示；请换更具体的检索式或缩小时间范围）"
    return text


async def _learn(
    people: MemberNumbers | None,
    messages: list[ArchivedMessage],
) -> None:
    """Load every displayed speaker and structured at target before numbering."""
    if people is None:
        return
    accounts: list[AccountId] = []
    for message in messages:
        accounts.append(message.sender.account_id)
        accounts.extend(account for account, _ in message.mentions if account != message.self_id)
    await people.learn(accounts)


def _render_lines(
    messages: list[ArchivedMessage],
    people: MemberNumbers | None,
    self_id: AccountId | None,
    clock: Clock,
) -> str:
    """Canonical archived messages as transcript lines, in the order given."""
    return "\n".join(_history_line(message, people, self_id, clock) for message in messages)


def _history_line(
    message: ArchivedMessage,
    people: MemberNumbers | None,
    self_id: AccountId | None,
    clock: Clock,
) -> str:
    uid = message.sender.account_id
    is_bot = message.author_kind is AuthorKind.BOT
    who = message.sender.display_name
    if is_bot:
        who += sysmark(str(BOT_DISPLAY_NUMBER))
    elif people is not None:
        number = people.number(uid)
        if number is not None:
            who += sysmark(str(number))
    text = message.text
    mentions = message.mentions
    if mentions:

        def mention_number(account: AccountId) -> int | None:
            if message.self_id and account == message.self_id:
                return BOT_DISPLAY_NUMBER
            if people is not None:
                return people.number(account)
            if self_id and account == self_id:
                return BOT_DISPLAY_NUMBER
            return None

        text = number_at_mentions(text, list(mentions), mention_number)
    # Database timestamps arrive in UTC; rendering uses the session display zone.
    return f"{sysmark(clock.format(message.occurred_at))} {who}: {text}"


async def _with_context(
    group_id: GroupId,
    hit_ids: list,
    ctx: int,
    self_id: AccountId | None = None,
    people: MemberNumbers | None = None,
    *,
    database: Callable[[], asyncpg.Pool],
    clock: Clock,
) -> str:
    """The hits rendered inside their surrounding conversation.

    One query fetches, per hit, the ctx archive lines on either side of it (by
    the archive's own order, (occurred_at, id) - the id is a UUID, so it breaks
    ties without meaning anything). A window is contiguous by construction, so
    two windows overlap exactly when they share a row: overlapping windows are
    merged into one block, and blocks render oldest first with an ellipsis line
    between them. Worst case is tools.search_history.max_hits disjoint blocks of
    2*ctx+1 lines.
    """
    nrows = await database().fetch(
        f"""SELECT h.id AS hit, {archive_columns("n")}
             FROM unnest($2::uuid[]) AS h(id)
             JOIN raw_event he ON he.id = h.id
            CROSS JOIN LATERAL (
              (SELECT {archive_columns("r")}
                 FROM raw_event r
                WHERE r.group_id=$1 AND r.event_type IN ('message','notice')
                  AND (r.occurred_at, r.id) <= (he.occurred_at, he.id)
                ORDER BY r.occurred_at DESC, r.id DESC LIMIT $3)
              UNION ALL
              (SELECT {archive_columns("r")}
                 FROM raw_event r
                WHERE r.group_id=$1 AND r.event_type IN ('message','notice')
                  AND (r.occurred_at, r.id) > (he.occurred_at, he.id)
                ORDER BY r.occurred_at ASC, r.id ASC LIMIT $4)
            ) n""",
        group_id.to_db(),
        hit_ids,
        ctx + 1,
        ctx,
    )
    by_id: dict = {}
    windows: dict = {}
    for row in nrows:
        message = archived_message(row)
        by_id[message.raw_event_id] = message
        windows.setdefault(row["hit"], set()).add(message.raw_event_id)
    blocks = merge_overlapping(list(windows.values()))

    def order(raw_event_id):
        message = by_id[raw_event_id]
        return message.occurred_at, message.raw_event_id

    await _learn(people, list(by_id.values()))
    parts: list[str] = []
    for block in sorted(blocks, key=lambda item: min(order(rid) for rid in item)):
        messages = [by_id[rid] for rid in sorted(block, key=order)]
        parts.append(_render_lines(messages, people, self_id, clock))
    return "\n……\n".join(parts)


def render_results(items: list[dict]) -> str:
    if not items:
        return "（没搜到有用的结果）"
    return "\n".join(f"{i}. {it['title']}\n{it['content']}" for i, it in enumerate(items, 1))


class Failure(str):
    """A tool answer that obtained nothing - a transport failure, a bad argument,
    an unreachable target. Rendered to the model verbatim like any other answer;
    the *type* is the out-of-band verdict, separate from the text payload,
    so no reader has to recognise the wording. A no-result search is NOT a
    Failure on purpose—searching and finding nothing is still completed verification
    work, and the structured evidence memo may record that bounded outcome."""

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

    Structured evidence excludes failed calls: an answer improvised after a failed lookup
    must not be recorded as a verified result.
    """
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
    group_id: GroupId,
    ctx: ToolCtx | None = None,
    prompts: PromptCatalog | None = None,
) -> str | Attachment:
    if ctx is not None and ctx.registry is not None:
        registry = ctx.registry
    elif prompts is not None:
        registry = tool_registry(cfg, prompts=prompts)
    else:
        raise ValueError("tool execution requires an explicit registry or prompt catalog")
    entry = registry.get(call.name)
    if entry is None or entry.handler is None:
        return Failure(f"（未知工具 {defang(call.name[:64])}）")
    if len(call.arguments) > MAX_ARGUMENT_BYTES:
        return Failure("（工具参数解析失败）")
    try:
        args = entry.parse(call.arguments)
    except (ValidationError, ValueError, RecursionError):
        return Failure("（工具参数解析失败）")
    return await entry.handler(call.name, args, cfg=cfg, group_id=group_id, ctx=ctx)


def task_payload(task: ScheduledTask, *, clock: Clock) -> dict:
    """A bounded public read model, independent of an initiating member."""
    return {
        "id": str(task.id),
        "status": task.status.value,
        "intent": defang(task.intent),
        "due_at": task.due_at.astimezone(clock.zone).isoformat(timespec="seconds"),
        "chain_depth": task.chain_depth,
        "outcome": task.outcome,
    }


async def _execute_scheduled(name, args, *, cfg, group_id, ctx):
    import json

    def failure(code: str, message: str) -> Failure:
        return Failure(
            json.dumps(
                {"ok": False, "scope": "current_group", "code": code, "message": message},
                ensure_ascii=False,
            )
        )

    if ctx is None or ctx.tasks is None:
        return failure("unavailable", "本轮没有可用的群任务服务。")
    tasks = ctx.tasks
    try:
        if name == "list_scheduled_tasks":
            page = await tasks.active(group_id, page=args.get("page", 1))
            return json.dumps(
                {
                    "ok": True,
                    "scope": "current_group",
                    "statuses": ["pending", "running"],
                    "page": page.page,
                    "has_more": page.has_more,
                    "next_page": page.page + 1 if page.has_more else None,
                    "tasks": [task_payload(task, clock=ctx.clock) for task in page.items],
                },
                ensure_ascii=False,
            )
        if name == "schedule_task":
            task = await tasks.create(
                group_id,
                args["intent"],
                cfg.tasks,
                run_at=args.get("run_at"),
                delay_seconds=args.get("delay_seconds"),
                parent=ctx.parent_task,
            )
        else:
            try:
                task_id = uuid.UUID(args["id"])
            except (ValueError, TypeError) as exc:
                raise TaskInputError("任务 ID 必须是完整 UUID。") from exc
            if name == "get_scheduled_task":
                task = await tasks.get(group_id, task_id)
            elif name == "update_scheduled_task":
                task = await tasks.update(
                    group_id,
                    task_id,
                    cfg.tasks,
                    intent=args.get("intent"),
                    run_at=args.get("run_at"),
                    delay_seconds=args.get("delay_seconds"),
                )
            else:
                task = await tasks.cancel(group_id, task_id)
        if task is None:
            return failure(
                "not_found" if name == "get_scheduled_task" else "not_pending_or_not_found",
                "本群没有该任务，或任务已不处于可修改的待执行状态。",
            )
        return json.dumps(
            {
                "ok": True,
                "scope": "current_group",
                "operation": name,
                "task": task_payload(task, clock=ctx.clock),
            },
            ensure_ascii=False,
        )
    except (TaskInputError, TaskLimit) as exc:
        return failure("rejected", str(exc))
    except (asyncpg.PostgresError, OSError, TimeoutError) as exc:
        log.warning("scheduled task operation failed: %s", why(exc))
        return failure("unknown", "未能确认操作结果；不要声称成功或盲目重试写入。")


async def _execute_open_images(name, args, *, cfg, group_id, ctx):
    ns = args.get("ns")
    if (
        not isinstance(ns, list)
        or not ns
        or not all(isinstance(x, int) and not isinstance(x, bool) for x in ns)
    ):
        return Failure("（需要图片编号列表。）")
    wanted = list(dict.fromkeys(ns))[: IMAGE_LIMITS.max_images]
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
                fid = await ctx.media.ensure_uploaded(
                    refs[idx], bot=ctx.bot, group_id=group_id, cfg=cfg
                )
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


async def _execute_read_url(name, args, *, cfg, group_id, ctx):
    url = _text(args, "url")
    if not url.startswith(("http://", "https://")):
        return Failure("（需要一个 http/https 网址）")
    reader = ctx.providers.page_reader if ctx is not None else None
    if reader is None:
        return Failure("（当前搜索服务不支持读取网页）")
    try:
        text = await reader.read_page(url, group_id=group_id)
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
    cap = PAGE_LIMITS.max_content_chars
    if len(body) > cap:
        return body[:cap] + "\n（网页正文过长，后面的没有读到）"
    return body


async def _execute_recall_events(name, args, *, cfg, group_id, ctx):
    question = _text(args, "question")
    if not question:
        return Failure("（问题为空）")
    if ctx is None:
        return Failure("（记忆检索不可用）")
    try:
        found = await retrieval.episode_lookup(
            group_id,
            question,
            embed=ctx.providers.embedding,
            database=ctx.database,
            clock=ctx.clock,
            rcfg=RECALL_LIMITS,
        )
    except QuotaExhausted:
        # A limit, not a failure: it must reach the engine and drop the reply,
        # whichever tool's backend it comes from - only transport errors below
        # get talked around.
        raise
    except Exception as e:
        log.warning("recall_events failed: %s", why(e))
        return Failure("（记忆检索失败）")
    return found or "（事件记忆里没有相关的事）"


async def _execute_search_history(name, args, *, cfg, group_id, ctx):
    query = _text(args, "query")
    if not query:
        return Failure("（搜索词为空）")
    try:
        return await search_history(
            group_id,
            query,
            database=ctx.database,
            identities=ctx.identities,
            clock=ctx.clock,
            speaker=number(args.get("speaker")),
            speaker_name=_text(args, "speaker_name") or None,
            days=_days(args.get("days")),
            rcfg=HISTORY_LIMITS,
            self_id=ctx.bot.self_id if ctx and ctx.bot else None,
            people=ctx.people if ctx else None,
        )
    except QuotaExhausted:
        raise
    except Exception as e:
        # Told to the model rather than raised: mid-loop, an exception here kills the
        # whole reply, and "the archive is unavailable" is an answerable situation.
        log.warning("search_history failed: %s", why(e))
        return Failure("（存档查询失败）")


async def _execute_web_search(name, args, *, cfg, group_id, ctx):
    query = _text(args, "query")
    if not query:
        return Failure("（搜索词为空）")
    # Free within a monthly allowance, so there is no per-reply money check here: the
    # backend meters the allowance itself and refuses at it. That refusal propagates -
    # a limit reached means the reply is dropped, not answered in degraded form -
    # while a transport failure stays a tool answer, because a broken network is an
    # error to talk around, not a limit to respect.
    try:
        if ctx is None:
            return Failure("（搜索服务不可用）")
        items = await ctx.providers.search.search(
            query,
            options=SearchOptions(cfg.backends.search.count, cfg.backends.search.depth),
            group_id=group_id,
        )
    except QuotaExhausted:
        raise
    except Exception as e:
        log.warning("web_search failed: %s", why(e))
        return Failure("（搜索失败）")
    # Same rule as read_url: search snippets are outside text.
    return defang(render_results(items))
