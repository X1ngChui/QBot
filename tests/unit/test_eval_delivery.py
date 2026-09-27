"""Paid evaluator output is captured locally; it never touches OneBot send APIs."""

from types import SimpleNamespace
from unittest.mock import AsyncMock
import uuid

from scripts._eval_delivery import EvaluationDelivery
import _db as _test_db
from qqbot.conversation.state import GroupState
from qqbot.delivery.segments import TextSegment
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import GroupId


async def test_evaluator_archives_and_observes_without_network_sends():
    state = GroupState(
        GroupId("311"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    archive = SimpleNamespace(
        ingest=AsyncMock(return_value=SimpleNamespace(raw_event_id=uuid.uuid4()))
    )
    bot = SimpleNamespace(self_id="999", send_group_msg=AsyncMock(side_effect=AssertionError))
    delivery = EvaluationDelivery(state, archive, clock=_test_db.clock)
    result = await delivery.deliver_one(
        bot, group_id=state.group_id, segments=(TextSegment("Fictional output"),)
    )
    observed = await delivery.echo.wait("999", state.group_id, result.message_id, timeout=0.1)
    assert observed is state.recent[-1]
    assert observed.text == "Fictional output" and observed.is_bot
    assert archive.ingest.call_args.args[0].author_kind is AuthorKind.BOT
    bot.send_group_msg.assert_not_awaited()
    assert delivery.messages[0].text == "Fictional output"
    delivery.echo.close()
