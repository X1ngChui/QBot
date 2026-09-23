"""GroupDelivery ordering, fallback and failure-boundary tests."""

from __future__ import annotations

import asyncio
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from qqbot.core.delivery import GroupDelivery
from qqbot.core.outbound import ReplySegment, TextSegment
from qqbot.domain.ids import GroupId

fails: list[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"[{'ok ' if condition else 'FAIL'}] {name}  {detail}")
    if not condition:
        fails.append(name)


class ActionFailed(Exception):
    pass


class FakeBot:
    def __init__(self) -> None:
        self.calls: list[tuple[int, list[dict]]] = []
        self.next_id = 100
        self.fail_text = ""
        self.reject_reply_text = ""
        self.rejected = False

    async def send_group_msg(self, *, group_id: int, message: list[dict]):
        self.calls.append((group_id, message))
        text = "".join(
            item["data"].get("text", "")
            for item in message
            if item["type"] == "text"
        )
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


def sent_texts(bot: FakeBot) -> list[str]:
    return [
        "".join(
            item["data"].get("text", "")
            for item in message
            if item["type"] == "text"
        )
        for _, message in bot.calls
    ]


async def main() -> int:
    delivery = GroupDelivery()
    bot = FakeBot()
    group = GroupId("123")

    results = await asyncio.gather(
        delivery.deliver(
            bot,
            group_id=group,
            messages=((TextSegment("A1"),), (TextSegment("A2"),)),
        ),
        delivery.deliver(
            bot,
            group_id=group,
            messages=((TextSegment("B1"),), (TextSegment("B2"),)),
        ),
    )
    order = sent_texts(bot)
    check(
        "concurrent batches in one group never interleave",
        order in (["A1", "A2", "B1", "B2"], ["B1", "B2", "A1", "A2"]),
        repr(order),
    )
    check(
        "delivery returns every acknowledged platform message id",
        all(len(batch) == 2 and all(item.message_id for item in batch) for batch in results),
        repr(results),
    )
    check(
        "the OneBot boundary receives the encoded integer group id",
        all(group_id == 123 for group_id, _ in bot.calls),
        repr(bot.calls),
    )

    bot.reject_reply_text = "retry"
    retried = await delivery.deliver(
        bot,
        group_id=group,
        messages=((ReplySegment("old"), TextSegment("retry")),),
    )
    retry_calls = bot.calls[-2:]
    check(
        "a known reply refusal retries once without only the reply segment",
        len(retried) == 1
        and retried[0].reply_to is None
        and any(item["type"] == "reply" for item in retry_calls[0][1])
        and all(item["type"] != "reply" for item in retry_calls[1][1]),
        repr(retry_calls),
    )

    bot.fail_text = "stop"
    before = len(bot.calls)
    prefix = await delivery.deliver(
        bot,
        group_id=group,
        messages=(
            (TextSegment("kept"),),
            (TextSegment("stop"),),
            (TextSegment("never"),),
        ),
    )
    check(
        "an irrecoverable failure returns the delivered prefix and skips the suffix",
        len(prefix) == 1 and sent_texts(bot)[before:] == ["kept", "stop"],
        repr(sent_texts(bot)[before:]),
    )

    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
