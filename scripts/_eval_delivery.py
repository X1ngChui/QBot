"""Archive simulated evaluator sends without calling any external bot transport."""

from dataclasses import dataclass
import uuid

from qqbot.conversation.agent import MessageDraft
from qqbot.conversation.state import ChatMsg, GroupState
from qqbot.delivery.observation import SelfEcho
from qqbot.delivery.segments import SendSegment, display_text, to_onebot
from qqbot.delivery.service import DeliveredMessage
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import GroupId, MessageId
from qqbot.gateway.ingest import Ingestor
from qqbot.gateway.onebot import GroupMessage, Sender
from qqbot.clock import Clock


@dataclass(frozen=True, slots=True)
class EvaluationSends:
    messages: tuple[MessageDraft, ...]

    @property
    def at(self) -> tuple[str, ...]:
        return tuple(dict.fromkeys(account for message in self.messages for account in message.at))


class EvaluationDelivery:
    def __init__(self, state: GroupState, archive: Ingestor, *, clock: Clock) -> None:
        self._state = state
        self._archive = archive
        self._clock = clock
        self.echo = SelfEcho()
        self.messages: list[MessageDraft] = []

    async def deliver_one(
        self, bot, *, group_id: GroupId, segments: tuple[SendSegment, ...]
    ) -> DeliveredMessage:
        if group_id != self._state.group_id:
            raise ValueError("evaluation delivery cannot cross groups")
        identifier = MessageId("eval-" + uuid.uuid4().hex)
        draft = MessageDraft(segments)
        when = self._clock.now()
        text = display_text(segments)
        admitted = await self._archive.ingest(
            GroupMessage(
                message_id=identifier,
                group_id=group_id,
                sender=Sender(user_id=bot.self_id, nickname="Fictional evaluator"),
                segments=[to_onebot(segment) for segment in segments],
                self_id=bot.self_id,
                occurred_at=when,
                plain_text=text,
                typed_text=draft.text,
                reply_to_message_id=draft.reply_to,
                author_kind=AuthorKind.BOT,
            )
        )
        if admitted is None:
            raise RuntimeError("evaluation message identifier was already archived")
        message = ChatMsg(
            msg_id=identifier,
            user_id=bot.self_id,
            nickname="Fictional evaluator",
            text=text,
            ts=when,
            is_bot=True,
            raw_event_id=admitted.raw_event_id,
            reply_to=draft.reply_to,
            at=[(account, account) for account in draft.at],
        )
        self._state.add(message)
        self.messages.append(draft)
        self.echo.publish(str(bot.self_id), group_id, message)
        return DeliveredMessage(identifier, segments, draft.reply_to)
