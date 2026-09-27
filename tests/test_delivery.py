"""GroupDelivery ordering, fallback, and failure boundaries."""

import asyncio

import pytest

from qqbot.delivery.service import GroupDelivery
from qqbot.delivery.segments import ReplySegment, TextSegment
from qqbot.domain.ids import GroupId


class ActionFailed(Exception):
    pass


class FakeBot:
    def __init__(self):
        self.calls = []
        self.next_id = 100
        self.fail_text = ""
        self.reject_reply_text = ""
        self.rejected = False

    async def send_group_msg(self, *, group_id, message):
        self.calls.append((group_id, message))
        text = "".join(item["data"].get("text", "") for item in message if item["type"] == "text")
        await asyncio.sleep(0)
        if text == self.fail_text:
            raise RuntimeError("scripted failure")
        if (
            text == self.reject_reply_text
            and not self.rejected
            and any(item["type"] == "reply" for item in message)
        ):
            self.rejected = True
            raise ActionFailed("quoted message is gone")
        self.next_id += 1
        return {"message_id": self.next_id}


def sent_texts(bot):
    return [
        "".join(item["data"].get("text", "") for item in message if item["type"] == "text")
        for _, message in bot.calls
    ]


@pytest.fixture
def delivery_case():
    return GroupDelivery(), FakeBot(), GroupId("123")


@pytest.mark.asyncio
async def test_concurrent_batches_remain_contiguous_and_acknowledged(delivery_case):
    delivery, bot, group = delivery_case
    results = await asyncio.gather(
        delivery.deliver(
            bot, group_id=group, messages=((TextSegment("A1"),), (TextSegment("A2"),))
        ),
        delivery.deliver(
            bot, group_id=group, messages=((TextSegment("B1"),), (TextSegment("B2"),))
        ),
    )
    assert sent_texts(bot) in (["A1", "A2", "B1", "B2"], ["B1", "B2", "A1", "A2"])
    assert all(len(batch) == 2 and all(item.message_id for item in batch) for batch in results)
    assert all(group_id == 123 for group_id, _ in bot.calls)


@pytest.mark.asyncio
async def test_reply_refusal_retries_without_reply_only(delivery_case):
    delivery, bot, group = delivery_case
    bot.reject_reply_text = "retry"
    result = await delivery.deliver(
        bot, group_id=group, messages=((ReplySegment("old"), TextSegment("retry")),)
    )
    assert len(result) == 1
    assert result[0].reply_to is None
    assert len(bot.calls) == 2
    assert any(item["type"] == "reply" for item in bot.calls[0][1])
    assert all(item["type"] != "reply" for item in bot.calls[1][1])


@pytest.mark.asyncio
async def test_irrecoverable_failure_returns_prefix_and_skips_suffix(delivery_case):
    delivery, bot, group = delivery_case
    bot.fail_text = "stop"
    prefix = await delivery.deliver(
        bot,
        group_id=group,
        messages=((TextSegment("kept"),), (TextSegment("stop"),), (TextSegment("never"),)),
    )
    assert len(prefix) == 1
    assert sent_texts(bot) == ["kept", "stop"]
