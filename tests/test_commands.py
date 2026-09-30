"""Role-independent command catalog, authorization, and strict parsing."""

import _db as _test_db
from _test_owners import fresh_tasks
import types
import uuid

import pytest

from _budget import fake_budget
from _db import clock, fact_card, pool, test_bundle as fixture_bundle
from qqbot.services.members import MemberDirectory
from qqbot.commands.catalog import Access, CATALOG, PREFIXES, detail_text, find, help_text
from qqbot.commands.router import CommandRequest, CommandRouter, registered_commands
from qqbot.conversation.state import Registry
from qqbot.delivery.service import GroupDelivery
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.services import NameCard, PersonCard, permissions as perms


GROUP = GroupId("8001")
OWNER = AccountId("10000")
MEMBER = AccountId("30001")
TARGET = AccountId("30001")
OTHER = AccountId("30002")
BOT_ID = AccountId("999")


def text_of(result):
    return result.messages[0][-1].text if result is not None and result.messages else ""


class FakeBot:
    self_id = BOT_ID

    def __init__(self):
        self.sent = []

    async def send_group_msg(self, *, group_id, message):
        self.sent.append((group_id, message))
        return {"message_id": 7000 + len(self.sent)}


class FakeDirectory:
    def __init__(self):
        self.calls = []
        self._account = uuid.uuid4()
        self._holder = uuid.uuid4()
        self._notes = {}

    def exact_card(self, user_id):
        return PersonCard(
            entity_id=self._holder,
            account_id=self._account,
            user_id=user_id,
            display=f"账号-{user_id}",
            accounts=(user_id,),
            messages=2,
            names=(NameCard("测试名", "nickname", 1.0),),
            facts=(fact_card(1, uuid.uuid4(), "likes", "茶", "茶", 0.8, account_id=self._account),),
        )

    def holder_card_value(self, user_id):
        return PersonCard(
            entity_id=self._holder,
            account_id=None,
            user_id=user_id,
            display=f"集合-{user_id}",
            accounts=(user_id, "linked-alt"),
            messages=5,
        )

    async def display_name(self, group_id, user_id):
        self.calls.append(("display_name", group_id, user_id))
        return f"账号-{user_id}"

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

    async def notes(self, group_id, user_id, *, all_linked=False, page=1):
        self.calls.append(("notes", group_id, user_id, all_linked, page))
        rows = self._notes.get((group_id, user_id, all_linked), [])
        offset = (page - 1) * 5
        return tuple(rows[offset : offset + 5]), len(rows) > offset + 5

    async def add_note(self, group_id, user_id, text, *, all_linked=False):
        self.calls.append(("add_note", group_id, user_id, text, all_linked))
        note = types.SimpleNamespace(object_value=text, object_key=str(uuid.uuid4()))
        self._notes.setdefault((group_id, user_id, all_linked), []).append(note)
        return note

    async def edit_note(self, group_id, user_id, index, text, *, all_linked=False):
        self.calls.append(("edit_note", group_id, user_id, index, text, all_linked))
        rows = self._notes.get((group_id, user_id, all_linked), [])
        if not 1 <= index <= len(rows):
            return None
        rows[index - 1].object_value = text
        return rows[index - 1]

    async def remove_note(self, group_id, user_id, index, *, all_linked=False):
        self.calls.append(("remove_note", group_id, user_id, index, all_linked))
        rows = self._notes.get((group_id, user_id, all_linked), [])
        return rows.pop(index - 1) if 1 <= index <= len(rows) else None

    async def clear_notes(self, group_id, user_id, *, all_linked=False):
        self.calls.append(("clear_notes", group_id, user_id, all_linked))
        return len(self._notes.pop((group_id, user_id, all_linked), []))

    async def name(self, group_id, user_id, text, *, all_linked=False):
        self.calls.append(("name", group_id, user_id, text, all_linked))
        return NameCard(text, "nickname", 1.0)

    async def unname(self, group_id, user_id, text, *, all_linked=False):
        self.calls.append(("unname", group_id, user_id, text, all_linked))
        return True

    async def set_confidence(self, group_id, user_id, text, confidence, *, all_linked=False):
        self.calls.append(("confidence", group_id, user_id, text, confidence, all_linked))
        return NameCard(text, "nickname", confidence)

    async def forget(self, group_id, user_id, index, *, all_linked=False):
        self.calls.append(("forget", group_id, user_id, index, all_linked))
        return fact_card(index, uuid.uuid4(), "likes", "茶", "茶", 0.8)

    async def split(self, user_id):
        self.calls.append(("split", user_id))
        return uuid.uuid4()

    async def merge(self, left, right):
        self.calls.append(("merge", left, right))
        return True


class FakeLinks:
    def __init__(self):
        self.calls = []

    async def issue(self, **kwargs):
        self.calls.append(("issue", kwargs))
        return object()

    async def confirm(self, **kwargs):
        self.calls.append(("confirm", kwargs))
        return object()

    async def cancel(self, **kwargs):
        self.calls.append(("cancel", kwargs))
        return True


def request(name, *, user=OWNER, text=None, mentions=()):
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


@pytest.fixture
async def router_case():
    members = MemberDirectory()
    bundle = fixture_bundle()
    assert str(OWNER) in bundle.default.bot.owners
    directory, links = FakeDirectory(), FakeLinks()
    providers = types.SimpleNamespace(search=types.SimpleNamespace(name="test-search"))
    router = CommandRouter(
        GroupDelivery(),
        Registry(groups=_test_db.groups, archive=_test_db.archive, bundle=bundle, clock=clock),
        directory,
        links,
        providers,
        tasks=fresh_tasks(),
        budget=fake_budget(),
        members=members,
        groups=_test_db.groups,
        database=pool,
        bundle=bundle,
        clock=clock,
        diagnostics=types.SimpleNamespace(count=lambda: 0),
    )
    try:
        yield router, FakeBot(), directory, links
    finally:
        await members.close()


def test_catalog_is_complete_and_role_independent():
    listing = help_text()
    assert registered_commands() == PREFIXES
    assert all(item.name in listing for item in CATALOG)
    assert "仅 owner" in listing and "详细用法" in listing
    assert {item.name for item in CATALOG if item.access is Access.MEMBER} == {
        "/help",
        "/who",
        "/note",
        "/alias",
        "/forget",
        "/link",
        "/unlink",
        "/card",
        "/stats",
        "/top",
    }
    assert "/agree" not in PREFIXES and "/terms" not in PREFIXES
    assert all(
        "仅 bot owner" in detail_text(item) for item in CATALOG if item.access is Access.OWNER
    )
    assert "默认查看当前精确账号" in detail_text(find("who"))
    assert {"/who", "/members"} <= set(PREFIXES)
    assert not ({"/unblock", "/unmute", "/groupstats"} & set(PREFIXES))
    assert all(find(item.name) is item and find(item.name[1:]) is item for item in CATALOG)
    assert find("nope") is None


def test_owner_and_member_decisions_use_single_access_policy():
    owners = ["owner-a"]
    assert perms.is_owner("owner-a", owners)
    assert perms.decide("m", owners=owners, access=Access.MEMBER) is perms.Verdict.MEMBER
    assert perms.decide("m", owners=owners, access=Access.OWNER) is perms.Verdict.DENIED


@pytest.mark.asyncio
async def test_help_who_and_members_authorization(router_case):
    router, bot, directory, _ = router_case
    owner_help = await router.handle(bot, request("/help", user=OWNER))
    member_help = await router.handle(bot, request("/help", user=MEMBER))
    assert text_of(owner_help) == text_of(member_help)
    assert await router.handle(bot, request("/nope")) is None
    await router.handle(bot, request("/who", user=OWNER))
    member_who = await router.handle(bot, request("/who", user=MEMBER))
    assert "账号-" in text_of(member_who)
    card_calls = [call[0] for call in directory.calls if call[0].endswith("_card")]
    assert card_calls[-2:] == ["account_card", "account_card"]
    await router.handle(bot, request("/who", user=MEMBER, text="/who --all"))
    assert directory.calls[-1][0] == "holder_card"
    assert "bot owner" in text_of(await router.handle(bot, request("/members", user=MEMBER)))
    assert "本群 1 人" in text_of(await router.handle(bot, request("/members", user=OWNER)))


@pytest.mark.asyncio
async def test_note_scope_is_identical_for_owner_and_member(router_case):
    router, bot, directory, _ = router_case
    for user in (OWNER, MEMBER):
        await router.handle(
            bot, request("/note", user=user, text="/note add -- 同一段备注", mentions=(TARGET,))
        )
    note_calls = [call for call in directory.calls if call[0] == "add_note"]
    assert note_calls == [("add_note", GROUP, str(TARGET), "同一段备注", False)] * 2
    directory.calls.clear()
    await router.handle(
        bot,
        request("/note", user=MEMBER, text="/note clear --all", mentions=(TARGET,)),
    )
    assert ("clear_notes", GROUP, str(TARGET), True) in directory.calls


@pytest.mark.asyncio
async def test_retired_sentinels_and_unknown_or_duplicate_flags_are_rejected(router_case):
    router, bot, directory, _ = router_case
    old_note = await router.handle(bot, request("/note", user=MEMBER, text="/note -"))
    assert "用法" in text_of(old_note)
    assert not any(call[0] == "note" for call in directory.calls)
    old_alias = await router.handle(bot, request("/alias", user=MEMBER, text="/alias -旧称"))
    assert "用法" in text_of(old_alias)
    assert not any(call[0] == "unname" for call in directory.calls)
    bad_flag = await router.handle(bot, request("/who", user=MEMBER, text="/who --every"))
    assert "未知选项" in text_of(bad_flag)
    duplicate = await router.handle(bot, request("/who", user=MEMBER, text="/who --all --all"))
    assert "只能写一次" in text_of(duplicate)


@pytest.mark.asyncio
async def test_alias_confidence_and_listing(router_case):
    router, bot, directory, _ = router_case
    result = await router.handle(
        bot,
        request("/alias", user=MEMBER, text="/alias confidence 0.6 新称呼"),
    )
    assert ("confidence", GROUP, str(MEMBER), "新称呼", 0.6, False) in directory.calls
    assert "仅作待确认线索" in text_of(result)
    assert "已确认" in text_of(await router.handle(bot, request("/alias", user=MEMBER)))


@pytest.mark.asyncio
async def test_link_confirmation_preserves_actor_and_event(router_case):
    router, bot, _, links = router_case
    await router.handle(bot, request("/link", user=MEMBER, mentions=(OTHER,)))
    issue = next(call for call in links.calls if call[0] == "issue")[1]
    assert issue["initiator_user_id"] == str(MEMBER)
    assert issue["target_user_id"] == str(OTHER)
    assert isinstance(issue["created_event_id"], uuid.UUID)
    await router.handle(bot, request("/link", user=OTHER, text="/link confirm"))
    confirm = next(call for call in links.calls if call[0] == "confirm")[1]
    assert confirm["actor_user_id"] == str(OTHER)
    assert isinstance(confirm["confirmed_event_id"], uuid.UUID)


@pytest.mark.asyncio
async def test_unlink_split_and_merge_use_exact_account_authorization(router_case):
    router, bot, directory, _ = router_case
    await router.handle(bot, request("/unlink", user=MEMBER))
    assert directory.calls == [("split", str(MEMBER))]
    directory.calls.clear()
    await router.handle(bot, request("/split", user=OWNER, mentions=(OTHER,)))
    assert directory.calls == [("split", str(OTHER))]
    denied = await router.handle(bot, request("/merge", user=MEMBER, mentions=(MEMBER, OTHER)))
    assert "bot owner" in text_of(denied)


@pytest.mark.asyncio
async def test_dispatch_uses_typed_group_delivery(router_case):
    router, bot, _, _ = router_case
    delivered = await router.dispatch(bot, request("/help", user=MEMBER))
    assert delivered
    assert len(bot.sent) == 1
    assert [item["type"] for item in bot.sent[0][1]] == ["reply", "at", "text"]
