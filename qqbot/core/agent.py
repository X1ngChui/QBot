"""Task-local reply agent: model session, tools, budget and terminal send choice."""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from enum import StrEnum

from ..domain.evidence import EvidenceMemo
from ..providers.base import QuotaExhausted, TextModel
from ..providers.contracts import (
    CallContext,
    CallPurpose,
    GenerationPolicy,
    Message,
    ModelRequest,
    ModelTurn,
    PromptItem,
    ReasoningEffort,
    Role,
    SessionDirective,
    ToolCall,
    ToolCallId,
    ToolResult,
)
from ..settings import Settings
from ..util import why
from . import debug, tools
from .budget import BUDGET, Scope
from .member_numbers import MemberNumbers
from .outbound import (
    AtSegment,
    ContactKind,
    ContactSegment,
    CustomMusicSegment,
    DiceSegment,
    FaceSegment,
    JsonCardSegment,
    MarketFaceSegment,
    MusicPlatform,
    MusicSegment,
    OutboundSegment,
    ReplySegment,
    RpsSegment,
    TextSegment,
    at_accounts,
    reply_target,
    text_content,
)
from .state import ChatMsg, GroupState

log = logging.getLogger("qqbot.agent")

QUOTA_NOTE = "（检索额度已用完，这个查询没有执行。）"
OVERFLOW_NOTE = (
    "（本轮工具调用次数已达上限，这个调用没有执行；"
    "可先用已有结果，如需再查请下一轮再调用。）"
)
REPEAT_NOTE = "（这个查询刚执行过，结果就在上面。换个检索词，或用已有结果。）"
WRAP_UP_NOTE = (
    "（本次回复的额度已用完，不能再执行任何检索或查看；"
    "请只依据上文已有的材料，直接用 send_message 发出回复，"
    "不要提及额度或系统限制。）"
)
SEND_UNREADABLE_NOTE = "（send_message 的参数无法解析，没有发出。请重新调用。）"
SEND_EMPTY_NOTE = "（send_message 没有可发送的内容，没有发出。请重新调用。）"
SEND_INVALID_NOTE = "（send_message 含有无效的消息段或编号，没有发出。请修正后重新调用。）"
MAX_AT = 5
MAX_SEGMENTS = 32
MAX_JSON_CHARS = 16_384
PARALLEL_SAFE_TOOLS = frozenset({"web_search", "read_url", "search_history", "open_images"})


class AgentPhase(StrEnum):
    RUNNING = "running"
    WRAPPING_UP = "wrapping_up"
    FINISHED = "finished"


@dataclass(frozen=True, slots=True)
class ToolExecution:
    name: str
    arguments: dict
    output: str
    verified: bool


@dataclass(slots=True)
class ReplyDraft:
    segments: tuple[OutboundSegment, ...]
    provenance: str = ""
    evidence: EvidenceMemo | None = None
    names: dict[str, str] = field(default_factory=dict)

    @property
    def text(self) -> str:
        return text_content(self.segments)

    @property
    def at(self) -> list[str]:
        return at_accounts(self.segments)

    @property
    def reply_to(self) -> str | None:
        return reply_target(self.segments)


@dataclass(frozen=True, slots=True)
class AgentOutcome:
    reply: ReplyDraft | None
    executed: tuple[ToolExecution, ...]


def generation_policy(cfg, *, max_output_tokens: int | None = None) -> GenerationPolicy:
    return GenerationPolicy(
        model=cfg.model,
        reasoning=ReasoningEffort(cfg.reasoning_effort),
        timeout_sec=cfg.timeout_sec,
        retries=cfg.retries,
        max_output_tokens=max_output_tokens,
    )


def _fields(data: dict, required: set[str], optional: set[str] = frozenset()) -> bool:
    return required <= data.keys() <= required | optional


def _nonempty(data: dict, key: str) -> str | None:
    value = data.get(key)
    return value.strip() if isinstance(value, str) and value.strip() else None


def _http_url(data: dict, key: str) -> str | None:
    value = _nonempty(data, key)
    return value if value and value.startswith(("http://", "https://")) else None


def _parse_segment(
    item: object,
    *,
    people: MemberNumbers,
    lines: dict[int, ChatMsg],
    group_id: str,
) -> OutboundSegment | None:
    if not isinstance(item, dict) or set(item) != {"type", "data"}:
        return None
    kind, data = item.get("type"), item.get("data")
    if not isinstance(kind, str) or not isinstance(data, dict):
        return None

    if kind == "text" and _fields(data, {"text"}):
        text = data.get("text")
        return TextSegment(text) if isinstance(text, str) else None
    if kind == "at" and _fields(data, {"member"}):
        number = tools.number(data.get("member"))
        account = people.account(number) if number is not None else None
        return AtSegment(account) if account else None
    if kind == "reply" and _fields(data, {"line"}):
        number = tools.number(data.get("line"))
        target = lines.get(number or 0)
        return ReplySegment(target.msg_id) if target else None
    if kind == "face" and _fields(data, {"id"}):
        face_id = data.get("id")
        if isinstance(face_id, int) and not isinstance(face_id, bool) and face_id >= 0:
            return FaceSegment(face_id)
        return None
    if kind == "mface" and _fields(
        data,
        {"package_id", "emoji_id", "key"},
        {"summary"},
    ):
        package_id = _nonempty(data, "package_id")
        emoji_id = _nonempty(data, "emoji_id")
        key = _nonempty(data, "key")
        summary = data.get("summary", "")
        if package_id and emoji_id and key and isinstance(summary, str):
            return MarketFaceSegment(package_id, emoji_id, key, summary)
        return None
    if kind == "dice" and not data:
        return DiceSegment()
    if kind == "rps" and not data:
        return RpsSegment()
    if kind == "contact_member" and _fields(data, {"member"}):
        number = tools.number(data.get("member"))
        account = people.account(number) if number is not None else None
        return ContactSegment(ContactKind.MEMBER, account) if account else None
    if kind == "contact_group" and not data:
        return ContactSegment(ContactKind.CURRENT_GROUP, group_id)
    if kind == "music" and _fields(data, {"platform", "id"}):
        track_id = _nonempty(data, "id")
        try:
            platform = MusicPlatform(data.get("platform"))
        except (TypeError, ValueError):
            return None
        return MusicSegment(platform, track_id) if track_id else None
    if kind == "music_custom" and _fields(
        data,
        {"url", "audio", "title", "image"},
        {"singer"},
    ):
        url = _http_url(data, "url")
        audio = _http_url(data, "audio")
        image = _http_url(data, "image")
        title = _nonempty(data, "title")
        singer = data.get("singer", "")
        if url and audio and image and title and isinstance(singer, str):
            return CustomMusicSegment(url, audio, title, image, singer)
        return None
    if kind == "json" and _fields(data, {"payload"}):
        payload = data.get("payload")
        if not isinstance(payload, dict):
            return None
        encoded = json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        return JsonCardSegment(encoded) if len(encoded) <= MAX_JSON_CHARS else None
    return None


def parse_send(
    call: ToolCall,
    *,
    people: MemberNumbers,
    lines: dict[int, ChatMsg],
    group_id: str,
) -> tuple[ReplyDraft | None, str]:
    try:
        arguments = json.loads(call.arguments or "{}")
    except json.JSONDecodeError:
        return None, SEND_UNREADABLE_NOTE
    if not isinstance(arguments, dict) or set(arguments) != {"content"}:
        return None, SEND_UNREADABLE_NOTE
    content = arguments.get("content")
    if not isinstance(content, list) or not content or len(content) > MAX_SEGMENTS:
        return None, SEND_EMPTY_NOTE if content == [] else SEND_INVALID_NOTE

    segments: list[OutboundSegment] = []
    for item in content:
        segment = _parse_segment(item, people=people, lines=lines, group_id=group_id)
        if segment is None:
            return None, SEND_INVALID_NOTE
        segments.append(segment)

    if sum(isinstance(segment, ReplySegment) for segment in segments) > 1:
        return None, SEND_INVALID_NOTE
    if sum(isinstance(segment, AtSegment) for segment in segments) > MAX_AT:
        return None, SEND_INVALID_NOTE
    visible = any(
        not isinstance(segment, ReplySegment)
        and (not isinstance(segment, TextSegment) or bool(segment.text.strip()))
        for segment in segments
    )
    if not visible:
        return None, SEND_EMPTY_NOTE
    return ReplyDraft(tuple(segments)), ""


def _call_key(call: ToolCall) -> tuple[str, str]:
    raw = call.arguments.strip()
    try:
        arguments = json.loads(raw or "{}")
    except json.JSONDecodeError:
        arguments = None
    canonical = (
        json.dumps(arguments, sort_keys=True, ensure_ascii=False)
        if isinstance(arguments, dict)
        else raw
    )
    return call.name, canonical


class AgentRun:
    """One addressed message and one model session; never shared between tasks."""

    def __init__(
        self,
        *,
        model: TextModel,
        request: ModelRequest,
        cfg: Settings,
        state: GroupState,
        tool_context: tools.ToolCtx,
        people: MemberNumbers,
        lines: dict[int, ChatMsg],
    ) -> None:
        self._model = model
        self._request = request
        self._cfg = cfg
        self._state = state
        self._tool_context = tool_context
        self._people = people
        self._lines = lines
        self._phase = AgentPhase.RUNNING
        self._seen: set[tuple[str, str]] = set()
        self._executed: list[ToolExecution] = []
        self._debug_items: list[PromptItem] = list(request.prompt)

    @property
    def phase(self) -> AgentPhase:
        return self._phase

    @property
    def executed(self) -> tuple[ToolExecution, ...]:
        return tuple(self._executed)

    def _finish(self, reply: ReplyDraft | None) -> AgentOutcome:
        self._phase = AgentPhase.FINISHED
        return AgentOutcome(reply, self.executed)

    def _capture(self, round_no: int, turn: ModelTurn) -> None:
        debug.capture(
            self._state.group_id,
            round_no,
            tuple(self._debug_items),
            turn,
        )

    def _find_send(self, turn: ModelTurn) -> tuple[ReplyDraft | None, dict[ToolCallId, str]]:
        notes: dict[ToolCallId, str] = {}
        for call in turn.tool_calls:
            if call.name != tools.SEND:
                continue
            reply, note = parse_send(
                call,
                people=self._people,
                lines=self._lines,
                group_id=self._state.group_id,
            )
            if reply is not None:
                if len(turn.tool_calls) > 1:
                    log.info(
                        "group %s: %d other call(s) sent alongside the reply left unexecuted",
                        self._state.group_id,
                        len(turn.tool_calls) - 1,
                    )
                return reply, notes
            notes[call.call_id] = note
        return None, notes

    async def _execute_round(
        self,
        calls: tuple[ToolCall, ...],
        *,
        send_notes: dict[ToolCallId, str],
        spend: Scope,
    ) -> tuple[tuple[ToolResult, ...], bool]:
        cap = self._cfg.retrieval.max_tool_calls_per_round
        quota_hit = asyncio.Event()
        if spend.exhausted:
            quota_hit.set()

        outputs: list[str | tools.Attachment | None] = [None] * len(calls)
        executions: list[ToolExecution | None] = [None] * len(calls)
        keyed_locks: dict[tuple[str, str], asyncio.Lock] = {}
        parallel: list[tuple[int, ToolCall, tuple[str, str]]] = []
        serial: list[tuple[int, ToolCall, tuple[str, str]]] = []

        for index, call in enumerate(calls):
            if call.name == tools.SEND:
                outputs[index] = send_notes.get(call.call_id, SEND_UNREADABLE_NOTE)
            elif quota_hit.is_set():
                outputs[index] = QUOTA_NOTE
            elif index >= cap:
                outputs[index] = OVERFLOW_NOTE
            else:
                key = _call_key(call)
                keyed_locks.setdefault(key, asyncio.Lock())
                target = parallel if call.name in PARALLEL_SAFE_TOOLS else serial
                target.append((index, call, key))

        async def invoke(
            index: int,
            call: ToolCall,
            key: tuple[str, str],
        ) -> None:
            async with keyed_locks[key]:
                if quota_hit.is_set() or spend.exhausted:
                    quota_hit.set()
                    outputs[index] = QUOTA_NOTE
                    return
                if key in self._seen:
                    outputs[index] = REPEAT_NOTE
                    return
                try:
                    output = await tools.execute(
                        call,
                        cfg=self._cfg,
                        group_id=self._state.group_id,
                        ctx=self._tool_context,
                    )
                except QuotaExhausted as exc:
                    log.info(
                        "group %s: %s - wrapping up on what is already fetched",
                        self._state.group_id,
                        why(exc),
                    )
                    quota_hit.set()
                    outputs[index] = QUOTA_NOTE
                    return

                try:
                    parsed = json.loads(call.arguments or "{}")
                except json.JSONDecodeError:
                    parsed = {}
                verified = tools.verified(output)
                executions[index] = ToolExecution(
                    call.name,
                    parsed if isinstance(parsed, dict) else {},
                    str(output),
                    verified,
                )
                outputs[index] = output
                if verified:
                    self._seen.add(key)
                if spend.exhausted:
                    quota_hit.set()

        # Archive/search/page/image reads have no monetary charge and are independent,
        # so execute them together. Potentially paid tools remain serial: this keeps one
        # completed charge able to close the reply budget before the next call starts.
        await asyncio.gather(*(invoke(*item) for item in parallel))
        for item in serial:
            await invoke(*item)
        self._executed.extend(item for item in executions if item is not None)

        if len(calls) > cap:
            log.info(
                "group %s: %d tool calls in one round, %d past the cap left unexecuted",
                self._state.group_id,
                len(calls),
                len(calls) - cap,
            )
        return (
            tuple(
                ToolResult(
                    call.call_id,
                    output.content() if isinstance(output, tools.Attachment) else str(output),
                )
                for call, output in zip(calls, outputs, strict=True)
            ),
            quota_hit.is_set(),
        )

    async def run(self) -> AgentOutcome:
        if self._phase is not AgentPhase.RUNNING:
            raise RuntimeError(f"agent cannot run from {self._phase}")
        with BUDGET.scope(self._cfg.budget.per_reply_cny) as spend:
            async with self._model.open_session(self._request) as session:
                turn = await session.start()
                for round_no in range(self._cfg.retrieval.max_rounds):
                    self._capture(round_no, turn)
                    if not turn.tool_calls:
                        if turn.text.strip():
                            log.warning(
                                "group %s: the model wrote %d chars without calling %s; "
                                "nothing sent: %r",
                                self._state.group_id,
                                len(turn.text),
                                tools.SEND,
                                turn.text[:60],
                            )
                        else:
                            log.warning(
                                "group %s: the model ended without calling %s; nothing sent",
                                self._state.group_id,
                                tools.SEND,
                            )
                        return self._finish(None)

                    reply, send_notes = self._find_send(turn)
                    if reply is not None:
                        return self._finish(reply)

                    self._debug_items.extend(turn.tool_calls)
                    results, quota_hit = await self._execute_round(
                        turn.tool_calls,
                        send_notes=send_notes,
                        spend=spend,
                    )
                    self._debug_items.extend(results)
                    if spend.exhausted or quota_hit:
                        self._phase = AgentPhase.WRAPPING_UP
                        log.info(
                            "group %s: reply allowance exhausted after %d round(s), "
                            "wrapping up with send only",
                            self._state.group_id,
                            round_no + 1,
                        )
                        directive = SessionDirective(
                            prompt=(Message(Role.USER, WRAP_UP_NOTE),),
                            tools=(tools.send_def(),),
                        )
                        turn = await session.continue_with(results, directive=directive)
                        self._capture(round_no + 1, turn)
                        reply, _ = self._find_send(turn)
                        return self._finish(reply)

                    turn = await session.continue_with(results)
                    log.info(
                        "group %s: tool round %d done (%.4f CNY of %.4f used)",
                        self._state.group_id,
                        round_no + 1,
                        spend.spent,
                        spend.cap,
                    )

        log.error(
            "group %s: %d tool rounds without running out of money - a backend "
            "is billing zero; giving up",
            self._state.group_id,
            self._cfg.retrieval.max_rounds,
        )
        return self._finish(None)


def request_for_reply(
    prompt: tuple[PromptItem, ...], cfg: Settings, *, group_id: str
) -> ModelRequest:
    return ModelRequest(
        prompt=prompt,
        tools=tools.tool_defs(cfg),
        policy=generation_policy(cfg.capabilities.text),
        context=CallContext(CallPurpose.REPLY, group_id),
    )
