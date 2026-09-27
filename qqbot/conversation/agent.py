"""Task-local reply agent: model session, tools, budget and observable sends."""

from __future__ import annotations

import asyncio
import json
import logging
import uuid
from collections.abc import Awaitable, Callable, Mapping
from dataclasses import dataclass
from enum import StrEnum

from pydantic import ValidationError

from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.providers.base import QuotaExhausted
from qqbot.providers.base import TextModel
from qqbot.providers.contracts import CallContext
from qqbot.providers.contracts import CallPurpose
from qqbot.providers.contracts import GenerationPolicy
from qqbot.providers.contracts import Message
from qqbot.providers.contracts import ModelRequest
from qqbot.providers.contracts import ModelTurn
from qqbot.providers.contracts import PromptItem
from qqbot.providers.contracts import ReasoningEffort
from qqbot.providers.contracts import Role
from qqbot.providers.contracts import SessionDirective
from qqbot.providers.contracts import ToolCall
from qqbot.providers.contracts import ToolResult
from qqbot.providers.contracts import ToolSpec
from qqbot.configuration import Settings
from qqbot.util import why, sysmark
from qqbot.operations import debug
from qqbot.conversation.snapshot import ContinuationSnapshot, MessageSnapshot
from qqbot.conversation import tools
from qqbot.conversation.fuel import SessionFuel, MAX_WIRE_CALLS, MAX_ARGUMENT_BYTES
from qqbot.providers.contracts import StoredImage, TextPart
from qqbot.services.budget import Budget
from qqbot.services.budget import Scope
from qqbot.conversation.member_numbers import MemberNumbers
from qqbot.delivery.segments import AtSegment
from qqbot.delivery.segments import ContactKind
from qqbot.delivery.segments import ContactSegment
from qqbot.delivery.segments import DiceSegment
from qqbot.delivery.segments import FaceSegment
from qqbot.delivery.segments import ReplySegment
from qqbot.delivery.segments import RpsSegment
from qqbot.delivery.segments import SendSegment
from qqbot.delivery.segments import TextSegment
from qqbot.delivery.segments import at_accounts
from qqbot.delivery.segments import reply_target
from qqbot.delivery.segments import text_content
from qqbot.delivery.contract import AtInput
from qqbot.delivery.contract import ContactGroupInput
from qqbot.delivery.contract import ContactMemberInput
from qqbot.delivery.contract import DiceInput
from qqbot.delivery.contract import FaceInput
from qqbot.delivery.contract import ReplyInput
from qqbot.delivery.contract import RpsInput
from qqbot.delivery.contract import SendMessageInput
from qqbot.delivery.contract import SendSegmentInput
from qqbot.delivery.contract import TextInput
from qqbot.delivery.contract import send_arguments_model
from qqbot.conversation.state import ChatMsg
from qqbot.conversation.state import TranscriptRendering
from qqbot.conversation.state import GroupState

log = logging.getLogger("qqbot.agent")

QUOTA_NOTE = "（检索额度已用完，这个查询没有执行。）"
OVERFLOW_NOTE = (
    "（本轮工具调用次数已达上限，这个调用没有执行；可先用已有结果，如需再查请下一轮再调用。）"
)
REPEAT_NOTE = "（这个查询刚执行过，结果就在上面。换个检索词，或用已有结果。）"
WRAP_UP_NOTE = (
    "（本次回复的检索额度已用完；若尚未发声且需要回应，可单独调用 send_message；"
    "已发送过则调用 finish_reply。不可继续检索，不要提及额度或系统限制。）"
)
SEND_UNREADABLE_NOTE = "（send_message 参数无法解析，没有发出。请修正。）"
SEND_EMPTY_NOTE = "（send_message 没有可发送内容，没有发出。请修正。）"
SEND_INVALID_NOTE = "（send_message 包含无效消息段或编号，没有发出。请修正。）"
EXCLUSIVE_NOTE = "（发送或结束须独占工具轮次；本轮所有工具均未执行，请分轮调用。）"


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
    def at(self) -> list[AccountId]:
        return [AccountId(account) for account in at_accounts(self.segments)]

    @property
    def reply_to(self) -> MessageId | None:
        target = reply_target(self.segments)
        return MessageId(target) if target is not None else None


@dataclass(frozen=True, slots=True)
class SendResult:
    output: str
    observed: ChatMsg | None = None
    confirmed: bool = False


@dataclass(frozen=True, slots=True)
class AgentOutcome:
    sent: bool
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
    lines: Mapping[int, TranscriptRendering],
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
    lines: Mapping[int, TranscriptRendering],
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
    lines: Mapping[int, TranscriptRendering],
    group_id: GroupId,
    max_text_chars: int,
) -> tuple[MessageDraft | None, str]:
    model = send_arguments_model(max_text_chars)
    try:
        arguments = model.model_validate_json(call.arguments or "{}")
    except ValidationError as exc:
        return None, _validation_note(exc)

    message = _resolve_message(
        SendMessageInput(content=arguments.content),
        people=people,
        lines=lines,
        group_id=group_id,
    )
    return (message, "") if message is not None else (None, SEND_INVALID_NOTE)


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
    """One addressed message or due task and one private model session."""

    def __init__(
        self,
        *,
        model: TextModel,
        request: ModelRequest,
        cfg: Settings,
        registry: tools.ToolRegistry,
        budget: Budget,
        state: GroupState,
        tool_context: tools.ToolCtx,
        people: MemberNumbers,
        lines: Mapping[int, ChatMsg | MessageSnapshot],
        on_send: Callable[[MessageDraft, tuple[ToolExecution, ...]], Awaitable[SendResult]],
        seen_messages: set[uuid.UUID | MessageId],
        initial_cursor: int | None = None,
    ) -> None:
        self._fuel = SessionFuel()
        self._model = model
        self._request = request
        self._budget = budget
        self._cfg = cfg
        self._state = state
        self._tool_context = tool_context
        self._registry = registry
        tool_context.registry = self._registry
        self._people = people
        self._lines = {
            number: message
            if isinstance(message, MessageSnapshot)
            else MessageSnapshot.capture(message)
            for number, message in lines.items()
        }
        self._cursor = (
            initial_cursor
            if initial_cursor is not None
            else max((message.arrival_seq for message in lines.values()), default=0)
        )
        self._on_send = on_send
        self._seen_messages = seen_messages
        self._sent = 0
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

    def _finish(self) -> AgentOutcome:
        self._phase = AgentPhase.FINISHED
        return AgentOutcome(self._sent > 0, self.executed)

    def _capture(self, round_no: int, turn: ModelTurn) -> None:
        debug.capture(
            self._state.group_id,
            round_no,
            tuple(self._debug_items),
            turn,
        )

    async def _arrivals(self, *, own: ChatMsg | None = None) -> tuple[PromptItem, ...]:
        """Extend stable prompt-local numbers only at a model continuation boundary."""

        snapshot = ContinuationSnapshot.capture(self._state.recent, self._cursor, own=own)
        pending = [
            message
            for message in snapshot.messages
            if (message.raw_event_id or message.msg_id) not in self._seen_messages
        ]
        if not pending:
            return ()
        await self._people.learn([msg.user_id for msg in pending if not msg.is_bot])
        self._cursor = snapshot.cursor
        lines = (
            [sysmark("context gap: some intervening messages are unavailable")]
            if snapshot.gap
            else []
        )
        next_line = max(self._lines, default=0)
        for msg in pending:
            self._seen_messages.add(msg.raw_event_id or msg.msg_id)
            next_line += 1
            self._lines[next_line] = msg
            if msg.is_bot:
                for account, _ in msg.at:
                    self._people.number(account)
            else:
                self._people.number(msg.user_id, spoke=True)
                for account, _ in msg.mentions:
                    self._people.number(account)
            pic_nums: list[int] = []
            for index, _ref in enumerate(msg.image_refs):
                number = max(self._tool_context.by_pic, default=0) + 1
                self._tool_context.by_pic[number] = (msg, index)
                pic_nums.append(number)
            if own is not None and msg.msg_id == own.msg_id:
                continue
            quoted = next(
                (number for number, target in self._lines.items() if target.msg_id == msg.reply_to),
                None,
            )
            mark = f"⟦回复 #{quoted}⟧" if quoted else "⟦回复更早的消息⟧" if msg.reply_to else ""
            lines.append(
                msg.render(
                    seq=next_line,
                    quote=mark,
                    pic_nums=pic_nums,
                    member_no=self._people.number(msg.user_id),
                    mention_number=self._people.number,
                )
            )
        if not lines:
            return ()
        return (
            Message(Role.USER, "【本轮新增的群消息（不是对原请求的替换）】\n" + "\n".join(lines)),
        )

    async def _send_round(self, call: ToolCall) -> tuple[ToolResult, bool, ChatMsg | None]:
        if self._sent >= self._cfg.conversation.max_messages_per_reply:
            return ToolResult(call.call_id, "（本轮发送条数已达上限，没有发送。）"), False, None
        message, note = parse_send(
            call,
            people=self._people,
            lines=self._lines,
            group_id=self._state.group_id,
            max_text_chars=self._cfg.conversation.max_text_chars_per_message,
        )
        if message is None:
            return ToolResult(call.call_id, note), False, None
        result = await self._on_send(message, self.executed)
        if result.confirmed:
            self._sent += 1
        return ToolResult(call.call_id, result.output), not bool(result.observed), result.observed

    async def _execute_round(
        self,
        calls: tuple[ToolCall, ...],
        *,
        spend: Scope,
    ) -> tuple[tuple[ToolResult, ...], bool]:
        cap = self._fuel.take_calls(len(calls))
        quota_hit = asyncio.Event()
        if spend.exhausted:
            quota_hit.set()

        outputs: list[str | tools.Attachment | None] = [None] * len(calls)
        executions: list[ToolExecution | None] = [None] * len(calls)
        keyed_locks: dict[tuple[str, str], asyncio.Lock] = {}
        parallel: list[tuple[int, ToolCall, tuple[str, str]]] = []
        serial: list[tuple[int, ToolCall, tuple[str, str]]] = []

        for index, call in enumerate(calls):
            if quota_hit.is_set():
                outputs[index] = QUOTA_NOTE
            elif index >= cap:
                outputs[index] = OVERFLOW_NOTE
            else:
                key = _call_key(call)
                keyed_locks.setdefault(key, asyncio.Lock())
                target = parallel if self._registry.parallel_safe(call.name) else serial
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
                text = self._fuel.retain(str(output))
                if isinstance(output, tools.Attachment):
                    images = [part for part in output.parts if isinstance(part, StoredImage)]
                    if len(images) > self._fuel.images_left:
                        output = QUOTA_NOTE
                        verified = False
                        quota_hit.set()
                    else:
                        self._fuel.take_images(len(images))
                        parts = tuple(images) + tuple(
                            TextPart(self._fuel.retain(part.text))
                            for part in output.parts
                            if isinstance(part, TextPart)
                        )
                        output = tools.Attachment(text, parts)
                else:
                    output = text
                executions[index] = ToolExecution(
                    call.name,
                    parsed if isinstance(parsed, dict) else {},
                    str(output),
                    verified,
                )
                outputs[index] = output
                if verified:
                    self._seen.add(key)
                if spend.exhausted or self._fuel.wrapping_up:
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

    def _bounded_turn(self, turn: ModelTurn) -> bool:
        if len(turn.tool_calls) > MAX_WIRE_CALLS or any(
            len(call.arguments) > MAX_ARGUMENT_BYTES
            or len(call.arguments.encode("utf-8")) > MAX_ARGUMENT_BYTES
            for call in turn.tool_calls
        ):
            log.warning("group %s: oversized tool turn rejected", self._state.group_id)
            return False
        return True

    def _action_spec(self, name: str) -> ToolSpec:
        entry = self._registry.get(name)
        if entry is None:
            raise RuntimeError(f"missing session action: {name}")
        return entry.spec

    async def run(self) -> AgentOutcome:
        if self._phase is not AgentPhase.RUNNING:
            raise RuntimeError(f"agent cannot run from {self._phase}")
        with self._budget.scope(self._cfg.budget.per_reply_cny, reuse=True) as spend:
            async with self._model.open_session(self._request) as session:
                self._fuel.begin_turn()
                turn = await session.start()
                round_no = 0
                while True:
                    if not self._bounded_turn(turn):
                        return self._finish()
                    self._capture(round_no, turn)
                    if not turn.tool_calls:
                        return self._finish()

                    self._debug_items.extend(turn.tool_calls)
                    calls = turn.tool_calls
                    special = any(self._registry.exclusive(call.name) for call in calls)
                    if special and len(calls) != 1:
                        results = tuple(ToolResult(call.call_id, EXCLUSIVE_NOTE) for call in calls)
                        stop = False
                        own = None
                    elif calls[0].name == tools.FINISH:
                        try:
                            finish_args = json.loads(calls[0].arguments or "{}")
                        except json.JSONDecodeError:
                            finish_args = None
                        if finish_args == {}:
                            return self._finish()
                        results = (ToolResult(calls[0].call_id, "（结束参数必须是空对象 {}。）"),)
                        stop = False
                        own = None
                    elif calls[0].name == tools.SEND:
                        result, stop, own = await self._send_round(calls[0])
                        results = (result,)
                    else:
                        results, quota_hit = await self._execute_round(calls, spend=spend)
                        stop = False
                        own = None
                        if quota_hit:
                            self._phase = AgentPhase.WRAPPING_UP
                    self._debug_items.extend(results)
                    if stop:
                        return self._finish()

                    arrivals = await self._arrivals(own=own)
                    if own is not None:
                        line = next(
                            (n for n, msg in self._lines.items() if msg.msg_id == own.msg_id), None
                        )
                        if line is not None:
                            result = results[0]
                            results = (
                                ToolResult(
                                    result.call_id, f"{result.output}\n本次发言编号：#{line}"
                                ),
                            )
                            self._debug_items[-1] = results[0]
                    if (
                        spend.exhausted
                        or self._fuel.wrapping_up
                        or self._phase is AgentPhase.WRAPPING_UP
                    ):
                        if self._sent:
                            return self._finish()
                        self._phase = AgentPhase.WRAPPING_UP
                        directive = SessionDirective(
                            prompt=arrivals + (Message(Role.USER, WRAP_UP_NOTE),),
                            tools=(
                                self._action_spec(tools.SEND),
                                self._action_spec(tools.FINISH),
                            ),
                        )
                        if not self._fuel.begin_turn():
                            return self._finish()
                        turn = await session.continue_with(results, directive=directive)
                        if not self._bounded_turn(turn):
                            return self._finish()
                        self._capture(round_no + 1, turn)
                        if len(turn.tool_calls) == 1 and turn.tool_calls[0].name == tools.SEND:
                            await self._send_round(turn.tool_calls[0])
                        return self._finish()

                    directive = SessionDirective(prompt=arrivals) if arrivals else None
                    if self._sent >= self._cfg.conversation.max_messages_per_reply:
                        directive = SessionDirective(
                            prompt=arrivals, tools=(self._action_spec(tools.FINISH),)
                        )
                    if not self._fuel.begin_turn():
                        return self._finish()
                    turn = await session.continue_with(results, directive=directive)
                    round_no += 1
                    log.info(
                        "group %s: tool round %d done (%.4f CNY of %.4f used)",
                        self._state.group_id,
                        round_no + 1,
                        spend.spent,
                        spend.cap,
                    )


def request_for_reply(
    prompt: tuple[PromptItem, ...],
    cfg: Settings,
    *,
    group_id: GroupId,
    registry: tools.ToolRegistry,
) -> ModelRequest:
    return ModelRequest(
        prompt=prompt,
        tools=registry.definitions,
        policy=generation_policy(cfg.backends.text),
        context=CallContext(CallPurpose.REPLY, group_id),
    )
