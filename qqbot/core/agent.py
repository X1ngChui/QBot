"""Task-local reply agent: model session, tools, budget and terminal send choice."""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import dataclass, field
from enum import StrEnum

from pydantic import ValidationError

from ..domain.evidence import EvidenceMemo
from ..domain.ids import GroupId
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
    DiceSegment,
    FaceSegment,
    ReplySegment,
    RpsSegment,
    SendSegment,
    TextSegment,
    at_accounts,
    reply_target,
    text_content,
)
from .send_contract import (
    AtInput,
    ContactGroupInput,
    ContactMemberInput,
    DiceInput,
    FaceInput,
    ReplyInput,
    RpsInput,
    SendMessageInput,
    SendSegmentInput,
    TextInput,
    send_arguments_model,
)
from .state import ChatMsg, GroupState

log = logging.getLogger("qqbot.agent")

QUOTA_NOTE = "（检索额度已用完，这个查询没有执行。）"
OVERFLOW_NOTE = (
    "（本轮工具调用次数已达上限，这个调用没有执行；可先用已有结果，如需再查请下一轮再调用。）"
)
REPEAT_NOTE = "（这个查询刚执行过，结果就在上面。换个检索词，或用已有结果。）"
WRAP_UP_NOTE = (
    "（本次回复的额度已用完，不能再执行任何检索或查看；"
    "若按回复规则需要回应，只依据上文已有材料用 send_messages 发出；"
    "若符合允许静默的情形，直接结束。不要提及额度或系统限制。）"
)
SEND_UNREADABLE_NOTE = "（send_messages 的参数无法解析，没有发出。请重新调用。）"
SEND_EMPTY_NOTE = "（send_messages 没有可发送的内容，没有发出。请重新调用。）"
SEND_INVALID_NOTE = "（send_messages 含有无效的消息段或编号，没有发出。请修正后重新调用。）"
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


@dataclass(frozen=True, slots=True)
class MessageDraft:
    """One independently delivered QQ message in a terminal reply batch."""

    segments: tuple[SendSegment, ...]

    def __post_init__(self) -> None:
        if not self.segments:
            raise ValueError("message draft must contain at least one segment")

    @property
    def text(self) -> str:
        return text_content(self.segments)

    @property
    def at(self) -> list[str]:
        return at_accounts(self.segments)

    @property
    def reply_to(self) -> str | None:
        return reply_target(self.segments)


@dataclass(slots=True)
class ReplyDraft:
    messages: tuple[MessageDraft, ...]
    evidence: EvidenceMemo | None = None
    names: dict[str, str] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.messages:
            raise ValueError("reply draft must contain at least one message")

    @property
    def at(self) -> list[str]:
        return [account for message in self.messages for account in message.at]


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


def _resolve_segment(
    item: SendSegmentInput,
    *,
    people: MemberNumbers,
    lines: dict[int, ChatMsg],
    group_id: GroupId,
) -> SendSegment | None:
    """Resolve one prompt-local member or line number into a platform identifier."""

    if isinstance(item, TextInput):
        return TextSegment(item.data.text)
    if isinstance(item, AtInput):
        account = people.account(item.data.member)
        return AtSegment(account) if account else None
    if isinstance(item, ReplyInput):
        target = lines.get(item.data.line)
        return ReplySegment(target.msg_id) if target else None
    if isinstance(item, FaceInput):
        return FaceSegment(item.data.id)
    if isinstance(item, DiceInput):
        return DiceSegment()
    if isinstance(item, RpsInput):
        return RpsSegment()
    if isinstance(item, ContactMemberInput):
        account = people.account(item.data.member)
        return ContactSegment(ContactKind.MEMBER, account) if account else None
    if isinstance(item, ContactGroupInput):
        return ContactSegment(ContactKind.CURRENT_GROUP, group_id)
    return None


def _resolve_message(
    value: SendMessageInput,
    *,
    people: MemberNumbers,
    lines: dict[int, ChatMsg],
    group_id: GroupId,
) -> MessageDraft | None:
    segments: list[SendSegment] = []
    for item in value.content:
        segment = _resolve_segment(
            item,
            people=people,
            lines=lines,
            group_id=group_id,
        )
        if segment is None:
            return None
        segments.append(segment)
    return MessageDraft(tuple(segments))


def _validation_note(exc: ValidationError) -> str:
    kinds = {error["type"] for error in exc.errors()}
    if kinds & {"json_invalid", "json_type", "model_type"}:
        return SEND_UNREADABLE_NOTE
    if kinds & {"send_empty", "too_short"}:
        return SEND_EMPTY_NOTE
    return SEND_INVALID_NOTE


def parse_send(
    call: ToolCall,
    *,
    people: MemberNumbers,
    lines: dict[int, ChatMsg],
    group_id: GroupId,
    max_messages: int,
    max_text_chars: int,
) -> tuple[ReplyDraft | None, str]:
    model = send_arguments_model(max_messages, max_text_chars)
    try:
        arguments = model.model_validate_json(call.arguments or "{}")
    except ValidationError as exc:
        return None, _validation_note(exc)

    messages: list[MessageDraft] = []
    for value in arguments.messages:
        message = _resolve_message(
            value,
            people=people,
            lines=lines,
            group_id=group_id,
        )
        if message is None:
            return None, SEND_INVALID_NOTE
        messages.append(message)
    return ReplyDraft(tuple(messages)), ""


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
                max_messages=self._cfg.tools.send_messages.max_messages_per_call,
                max_text_chars=self._cfg.tools.send_messages.max_text_chars_per_message,
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
        cap = self._cfg.tools.max_calls_per_round
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
                for round_no in range(self._cfg.tools.max_rounds):
                    self._capture(round_no, turn)
                    if not turn.tool_calls:
                        log.debug(
                            "group %s: model ended without %s; nothing sent",
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
                            tools=(tools.send_def(self._cfg),),
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
            self._cfg.tools.max_rounds,
        )
        return self._finish(None)


def request_for_reply(
    prompt: tuple[PromptItem, ...], cfg: Settings, *, group_id: GroupId
) -> ModelRequest:
    return ModelRequest(
        prompt=prompt,
        tools=tools.tool_defs(cfg),
        policy=generation_policy(cfg.capabilities.text),
        context=CallContext(CallPurpose.REPLY, group_id),
    )
