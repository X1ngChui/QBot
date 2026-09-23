"""Role-independent command catalog, authorization, and strict parsing."""

import asyncio
import os
import pathlib
import sys
import types
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))

from qqbot.core import agreement, perms
from qqbot.core.command_catalog import Access, CATALOG, PREFIXES, detail_text, find, help_text
from qqbot.core.commands import CommandRequest, CommandRouter, registered_commands
from qqbot.core.delivery import GroupDelivery
from qqbot.core.state import Registry
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.services import FactCard, NameCard, PersonCard
from qqbot.settings import config

fails: list[str] = []
GROUP = GroupId("8001")
OWNER = AccountId(config().default.owners[0])
MEMBER = AccountId("30001")
TARGET = AccountId("30001")
OTHER = AccountId("30002")
BOT_ID = AccountId("999")


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"[{'ok ' if condition else 'FAIL'}] {name}  {detail}")
    if not condition:
        fails.append(name)


def text_of(result) -> str:
    if result is None or not result.messages:
        return ""
    return result.messages[0][-1].text


class FakeBot:
    self_id = BOT_ID

    def __init__(self) -> None:
        self.sent: list[tuple[int, list[dict]]] = []

    async def send_group_msg(self, *, group_id, message):
        self.sent.append((group_id, message))
        return {"message_id": 7000 + len(self.sent)}


class FakeDirectory:
    def __init__(self) -> None:
        self.calls: list[tuple] = []
        self._account = uuid.uuid4()
        self._holder = uuid.uuid4()

    def exact_card(self, user_id: str) -> PersonCard:
        return PersonCard(
            entity_id=self._holder,
            account_id=self._account,
            user_id=user_id,
            display=f"账号-{user_id}",
            accounts=(user_id,),
            messages=2,
            names=(NameCard("测试名", "nickname", 1.0),),
            facts=(
                FactCard(
                    1,
                    uuid.uuid4(),
                    "likes",
                    "茶",
                    "茶",
                    0.8,
                    account_id=self._account,
                ),
            ),
        )

    def holder_card_value(self, user_id: str) -> PersonCard:
        return PersonCard(
            entity_id=self._holder,
            account_id=None,
            user_id=user_id,
            display=f"集合-{user_id}",
            accounts=(user_id, "linked-alt"),
            messages=5,
        )

    async def account_card(self, group_id, user_id):
        self.calls.append(("account_card", group_id, user_id))
        return self.exact_card(user_id)

    async def holder_card(self, group_id, user_id):
        self.calls.append(("holder_card", group_id, user_id))
        return self.holder_card_value(user_id)

    async def roster(self, group_id):
        self.calls.append(("roster", group_id))
        return [self.holder_card_value(str(TARGET))]

    async def linked_account_ids(self, user_id):
        self.calls.append(("linked_account_ids", user_id))
        return [user_id, str(TARGET), "linked-alt"]

    async def note(self, group_id, user_id, text, *, all_linked=False):
        self.calls.append(("note", group_id, user_id, text, all_linked))

    async def name(self, group_id, user_id, text, *, all_linked=False):
        self.calls.append(("name", group_id, user_id, text, all_linked))
        return NameCard(text, "nickname", 1.0)

    async def unname(self, group_id, user_id, text, *, all_linked=False):
        self.calls.append(("unname", group_id, user_id, text, all_linked))
        return True

    async def set_confidence(
        self,
        group_id,
        user_id,
        text,
        confidence,
        *,
        all_linked=False,
    ):
        self.calls.append(("confidence", group_id, user_id, text, confidence, all_linked))
        return NameCard(text, "nickname", confidence)

    async def forget(self, group_id, user_id, index, *, all_linked=False):
        self.calls.append(("forget", group_id, user_id, index, all_linked))
        return FactCard(index, uuid.uuid4(), "likes", "茶", "茶", 0.8)

    async def split(self, user_id):
        self.calls.append(("split", user_id))
        return uuid.uuid4()

    async def merge(self, left, right):
        self.calls.append(("merge", left, right))
        return True


class FakeLinks:
    def __init__(self) -> None:
        self.calls: list[tuple] = []

    async def issue(self, **kwargs):
        self.calls.append(("issue", kwargs))
        return object(), "12345678"

    async def confirm(self, **kwargs):
        self.calls.append(("confirm", kwargs))
        return object()

    async def cancel(self, **kwargs):
        self.calls.append(("cancel", kwargs))
        return True


def request(
    name: str,
    *,
    user: AccountId = OWNER,
    text: str | None = None,
    mentions: tuple[AccountId, ...] = (),
) -> CommandRequest:
    return CommandRequest(
        name=name,
        message_id=MessageId(f"message-{uuid.uuid4().hex}"),
        raw_event_id=uuid.uuid4(),
        group_id=GROUP,
        user_id=user,
        self_id=BOT_ID,
        text=text or name,
        mentions=mentions,
    )


def catalogue() -> None:
    listing = help_text()
    check("the command registry exactly matches the catalog", registered_commands() == PREFIXES)
    check(
        "every command appears in the shared listing", all(item.name in listing for item in CATALOG)
    )
    check("help is role-independent", "仅 owner" in listing and "详细用法" in listing)
    check(
        "open commands are exactly the protocol commands",
        {item.name for item in CATALOG if item.access is Access.OPEN}
        == {"/help", "/terms", "/agree"},
    )
    check(
        "owner-only commands are marked as authorization, not alternate semantics",
        all("仅 bot owner" in detail_text(item) for item in CATALOG if item.access is Access.OWNER),
    )
    check(
        "bare who documents exact-account scope", "默认查看当前精确账号" in detail_text(find("who"))
    )
    check("members is separate from who", {"/who", "/members"} <= set(PREFIXES))
    check(
        "retired aliases are absent", not ({"/unblock", "/unmute", "/groupstats"} & set(PREFIXES))
    )
    check(
        "every command resolves with or without slash",
        all(find(item.name) is item and find(item.name[1:]) is item for item in CATALOG),
    )
    check("an unknown command is not catalogued", find("nope") is None)

    owners = ["owner-a"]
    check("owner identity is normalized", perms.is_owner("owner-a", owners))
    check(
        "open access admits a member",
        perms.decide("m", owners=owners, access=Access.OPEN) is perms.Verdict.MEMBER,
    )
    check(
        "agreed access asks a member for consent",
        perms.decide("m", owners=owners, access=Access.AGREED) is perms.Verdict.MEMBER_IF_AGREED,
    )
    check(
        "owner access denies a member",
        perms.decide("m", owners=owners, access=Access.OWNER) is perms.Verdict.DENIED,
    )


async def router_behavior() -> None:
    directory = FakeDirectory()
    links = FakeLinks()
    providers = types.SimpleNamespace(search=types.SimpleNamespace(name="test-search"))
    router = CommandRouter(GroupDelivery(), Registry(), directory, links, providers)
    bot = FakeBot()

    original_ok = agreement.ok

    async def agreed(*_args, **_kwargs):
        return True

    agreement.ok = agreed
    try:
        owner_help = await router.handle(bot, request("/help", user=OWNER))
        member_help = await router.handle(bot, request("/help", user=MEMBER))
        check("owner and member receive the same help", text_of(owner_help) == text_of(member_help))
        check(
            "unknown commands have no behavior", await router.handle(bot, request("/nope")) is None
        )

        await router.handle(bot, request("/who", user=OWNER))
        await router.handle(bot, request("/who", user=MEMBER))
        who_calls = [call[0] for call in directory.calls if call[0].endswith("_card")]
        check(
            "bare who is exact-account for owner and member",
            who_calls[-2:] == ["account_card", "account_card"],
            str(who_calls),
        )

        await router.handle(bot, request("/who", user=MEMBER, text="/who --all"))
        check(
            "who --all selects the linked holder",
            directory.calls[-1][0] == "holder_card",
            str(directory.calls[-1]),
        )

        member_members = await router.handle(bot, request("/members", user=MEMBER))
        check("members remains owner-authorized", "bot owner" in text_of(member_members))
        owner_members = await router.handle(bot, request("/members", user=OWNER))
        check("the owner roster is not overloaded onto who", "本群 1 人" in text_of(owner_members))

        directory.calls.clear()
        for user in (OWNER, MEMBER):
            await router.handle(
                bot,
                request(
                    "/note",
                    user=user,
                    text="/note set 同一段备注",
                    mentions=(TARGET,),
                ),
            )
        note_calls = [call for call in directory.calls if call[0] == "note"]
        check(
            "the same note command has the same target and scope for both roles",
            note_calls == [("note", GROUP, str(TARGET), "同一段备注", False)] * 2,
            str(note_calls),
        )

        directory.calls.clear()
        await router.handle(
            bot,
            request(
                "/note",
                user=MEMBER,
                text="/note clear --all",
                mentions=(TARGET,),
            ),
        )
        check(
            "note clear is explicit and preserves all-linked scope",
            ("note", GROUP, str(TARGET), "", True) in directory.calls,
        )

        directory.calls.clear()
        old_note = await router.handle(bot, request("/note", user=MEMBER, text="/note -"))
        check(
            "the old dash clear sentinel is rejected",
            "用法" in text_of(old_note) and not any(call[0] == "note" for call in directory.calls),
        )
        old_alias = await router.handle(bot, request("/alias", user=MEMBER, text="/alias -旧称"))
        check(
            "the old alias sentinel is rejected",
            "用法" in text_of(old_alias)
            and not any(call[0] == "unname" for call in directory.calls),
        )

        bad_flag = await router.handle(bot, request("/who", user=MEMBER, text="/who --every"))
        check("unknown flags are rejected without fallback", "未知选项" in text_of(bad_flag))
        duplicate_flag = await router.handle(
            bot, request("/who", user=MEMBER, text="/who --all --all")
        )
        check("duplicate flags are rejected", "只能写一次" in text_of(duplicate_flag))

        directory.calls.clear()
        await router.handle(
            bot,
            request(
                "/alias",
                user=MEMBER,
                text="/alias confidence 0.6 新称呼",
            ),
        )
        check(
            "alias confidence has an explicit action and numeric score",
            ("confidence", GROUP, str(MEMBER), "新称呼", 0.6, False) in directory.calls,
        )

        await router.handle(
            bot,
            request("/link", user=MEMBER, mentions=(OTHER,)),
        )
        issue = next(call for call in links.calls if call[0] == "issue")
        check(
            "link issue records both accounts and the admitted raw event",
            issue[1]["initiator_user_id"] == str(MEMBER)
            and issue[1]["target_user_id"] == str(OTHER)
            and isinstance(issue[1]["created_event_id"], uuid.UUID),
        )

        await router.handle(
            bot,
            request("/link", user=OTHER, text="/link confirm 12345678"),
        )
        confirm = next(call for call in links.calls if call[0] == "confirm")
        check(
            "link confirmation is attributed to the confirming account and event",
            confirm[1]["actor_user_id"] == str(OTHER)
            and isinstance(confirm[1]["confirmed_event_id"], uuid.UUID),
        )

        directory.calls.clear()
        await router.handle(bot, request("/unlink", user=MEMBER))
        check(
            "unlink can only detach the authenticated sender",
            directory.calls == [("split", str(MEMBER))],
            str(directory.calls),
        )

        directory.calls.clear()
        await router.handle(
            bot,
            request("/split", user=OWNER, mentions=(OTHER,)),
        )
        check(
            "owner split detaches only the mentioned exact account",
            directory.calls == [("split", str(OTHER))],
        )

        denied = await router.handle(bot, request("/merge", user=MEMBER, mentions=(MEMBER, OTHER)))
        check(
            "owner-only commands fail by authorization, not by alternate behavior",
            "bot owner" in text_of(denied),
        )

        before = len(bot.sent)
        delivered = await router.dispatch(bot, request("/help", user=MEMBER))
        check(
            "command output uses typed group delivery",
            delivered
            and len(bot.sent) == before + 1
            and [item["type"] for item in bot.sent[-1][1]] == ["reply", "at", "text"],
        )
    finally:
        agreement.ok = original_ok


async def agreement_denial() -> None:
    directory = FakeDirectory()
    router = CommandRouter(
        GroupDelivery(),
        Registry(),
        directory,
        FakeLinks(),
        types.SimpleNamespace(search=types.SimpleNamespace(name="test-search")),
    )
    original_ok = agreement.ok

    async def not_agreed(*_args, **_kwargs):
        return False

    agreement.ok = not_agreed
    try:
        result = await router.handle(FakeBot(), request("/who", user=MEMBER))
        check(
            "an unagreed member receives the agreement pointer",
            "/terms" in text_of(result) and "/agree" in text_of(result),
        )
        check("agreement denial does not run the command", not directory.calls)
    finally:
        agreement.ok = original_ok


async def main() -> int:
    catalogue()
    await router_behavior()
    await agreement_denial()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
