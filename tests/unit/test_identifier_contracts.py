"""Platform IDs remain nominal values through internal records and wire decoding."""

from pathlib import Path
import json
import subprocess
import sys

import pytest

from qqbot.configuration.schema import BotCfg
from qqbot.delivery.segments import AtSegment, ReplySegment, at_accounts, from_onebot, reply_target
from qqbot.domain.ids import AccountId, MessageId
from qqbot.gateway.segments import parse_segments

ROOT = Path(__file__).parents[2]


def test_configuration_and_protocol_boundaries_construct_nominal_ids():
    bot = BotCfg(owners=("fictional-owner",))
    assert type(bot.owners[0]) is AccountId
    segments = from_onebot(
        [
            {"type": "at", "data": {"qq": "fictional-account"}},
            {"type": "reply", "data": {"id": "fictional-message"}},
        ]
    )
    assert type(segments[0].account) is AccountId
    assert type(segments[1].message_id) is MessageId
    assert type(at_accounts(segments)[0]) is AccountId
    assert type(reply_target(segments)) is MessageId
    parsed = parse_segments(
        [
            {"type": "at", "data": {"qq": "fictional-account", "name": "Fictional"}},
            {"type": "reply", "data": {"id": "fictional-message"}},
        ],
        AccountId("fictional-bot"),
    )
    assert type(parsed.mentions[0]) is AccountId and type(parsed.reply_to) is MessageId


@pytest.mark.parametrize(
    "kind,accepted",
    [
        ("account", True),
        ("message", True),
        ("account-str", False),
        ("message-str", False),
        ("account-as-message", False),
        ("message-as-account", False),
        ("block-account", True),
        ("block-str", False),
        ("bot-account", True),
        ("bot-str", False),
    ],
)
def test_internal_id_contracts_reject_erasure_and_interchange(tmp_path, kind, accepted):
    values = {
        "account": "AtSegment(account)",
        "message": "ReplySegment(message)",
        "account-str": "AtSegment(str(account))",
        "message-str": "ReplySegment(str(message))",
        "account-as-message": "ReplySegment(account)",
        "message-as-account": "AtSegment(message)",
        "block-account": "groups.block(group, account)",
        "block-str": "groups.block(group, str(account))",
        "bot-account": "AtSegment(bot.self_id)",
        "bot-str": "AtSegment(str(bot.self_id))",
    }
    sample = tmp_path / "nominal_contract.py"
    sample.write_text(
        "from qqbot.domain.ids import AccountId, GroupId, MessageId\n"
        "from qqbot.delivery.segments import AtSegment, ReplySegment\n"
        "from qqbot.repositories.groups import GroupRepository\n"
        "from qqbot.gateway.botapi import BotApi\n"
        "def call(account: AccountId, message: MessageId, group: GroupId, "
        "groups: GroupRepository, bot: BotApi):\n"
        "    return " + values[kind] + "\n",
        encoding="utf-8",
    )
    run = subprocess.run(
        [
            sys.executable,
            "-m",
            "pyright",
            "--project",
            str(ROOT / "pyrightconfig.json"),
            "--outputjson",
            str(sample),
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=60,
    )
    errors = [d for d in json.loads(run.stdout)["generalDiagnostics"] if d["severity"] == "error"]
    assert run.returncode == (0 if accepted else 1)
    assert len(errors) == (0 if accepted else 1)
    if errors:
        assert errors[0]["rule"] == "reportArgumentType"


def test_value_objects_preserve_internal_identity():
    account, message = AccountId("fictional-account"), MessageId("fictional-message")
    assert AtSegment(account).account is account
    assert ReplySegment(message).message_id is message


async def test_framework_client_normalizes_identity_and_forwards_wire_calls():
    from types import SimpleNamespace
    from unittest.mock import AsyncMock

    from qqbot.gateway.nonebot_adapter import OneBotClient

    adapter = SimpleNamespace(
        self_id="fictional-bot",
        call_api=AsyncMock(return_value={"members": []}),
        send_group_msg=AsyncMock(return_value={"message_id": 17}),
    )
    client = OneBotClient(adapter)
    assert type(client.self_id) is AccountId
    assert await client.call_api("get_group_member_list", group_id=311) == {"members": []}
    adapter.call_api.assert_awaited_once_with("get_group_member_list", group_id=311)
    segments = [{"type": "text", "data": {"text": "Fictional"}}]
    assert await client.send_group_msg(group_id=311, message=segments) == {"message_id": 17}
    adapter.send_group_msg.assert_awaited_once_with(group_id=311, message=segments)
