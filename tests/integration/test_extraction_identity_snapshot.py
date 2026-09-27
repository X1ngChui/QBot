"""Extraction keeps exact accounts while using a fixed number of snapshot reads."""

from dataclasses import replace
import uuid

import pytest

from _fixtures import clock
from test_roster_snapshot import CountedPool, seed
from qqbot.domain.archive import ArchivedMessage, ArchivedSender, AuthorKind
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.repositories.identity import IdentityRepository
from qqbot.workers.memory import MemoryWorker

pytestmark = pytest.mark.database


def message(user, text="Fictional message"):
    return ArchivedMessage(
        raw_event_id=uuid.uuid4(),
        message_id=MessageId(uuid.uuid4().hex),
        group_id=GroupId("311"),
        event_type="message",
        notice_type="",
        author_kind=AuthorKind.MEMBER,
        sender=ArchivedSender(AccountId(user), user, "", "member"),
        self_id=AccountId("999"),
        occurred_at=clock.now(),
        created_at=clock.now(),
        text=text,
        typed_text=text,
        reply_to=None,
        to_me=False,
        segments=({"type": "text", "data": {"text": text}},),
    )


async def alias(database, account, text, *, group=311, status="confirmed"):
    return await database.pool.fetchval(
        """INSERT INTO alias(alias_text,normalized_text,target_account_id,group_id,
                             alias_type,confidence,status)
           VALUES ($1,$1,$2,$3,'nickname',0.9,$4) RETURNING id""",
        text,
        account,
        group,
        status,
    )


def worker(database):
    counted = CountedPool(database.pool)
    instance = MemoryWorker.__new__(MemoryWorker)
    instance._clock = clock
    instance._ids = IdentityRepository(database=lambda: counted, clock=clock)
    return counted, instance


@pytest.mark.parametrize("size", [1, 50])
async def test_render_query_count_is_independent_of_exact_account_count(database, size):
    rows = []
    for index in range(size):
        user = f"fictional-{index}"
        _, account = await seed(database, user)
        await alias(database, account, f"Alias-{index}")
        rows.append(message(user))
    counted, instance = worker(database)
    codes, roster, lines = await instance._render(GroupId("311"), rows)
    assert counted.queries == 3
    assert len(codes) == len(lines) == size
    assert all(f"Alias-{index}" in roster for index in range(size))


async def test_linked_accounts_keep_separate_codes_and_exact_alias_targets(database):
    holder, first = await seed(database, "fictional-a")
    _, second = await seed(database, "fictional-b")
    await database.pool.execute(
        "UPDATE identity_account SET entity_id=$1 WHERE id=$2", holder, second
    )
    await alias(database, first, "ConfirmedA")
    await alias(database, second, "CandidateB", status="candidate")
    await alias(database, second, "OtherGroup", group=312)
    await alias(database, first, "Ambiguous")
    await alias(database, second, "Ambiguous")
    _, instance = worker(database)
    codes, roster, lines = await instance._render(
        GroupId("311"),
        [
            message("fictional-a"),
            message("fictional-b", "ConfirmedA Ambiguous CandidateB OtherGroup"),
        ],
    )
    assert codes == {1: first, 2: second}
    assert "CandidateB" not in roster and "OtherGroup" not in roster
    targets = lines[1].targets
    assert {(target.account_id, target.reason, target.marker) for target in targets} == {
        (second, "author", ""),
        (first, "alias", "ConfirmedA"),
    }


async def test_alias_edits_between_queries_cannot_mix_snapshot_versions(database):
    _, account = await seed(database, "fictional-a")
    identifier = await alias(database, account, "ConfirmedA")
    counted, instance = worker(database)

    async def change_alias():
        await database.pool.execute("UPDATE alias SET status='inactive' WHERE id=$1", identifier)

    counted.after_accounts = change_alias
    _, roster, lines = await instance._render(
        GroupId("311"), [message("fictional-a", "ConfirmedA")]
    )
    assert "ConfirmedA" in roster
    assert any(
        target.reason == "alias" and target.marker == "ConfirmedA" for target in lines[0].targets
    )
    assert (
        await database.pool.fetchval("SELECT status FROM alias WHERE id=$1", identifier)
        == "inactive"
    )


async def test_unknown_and_self_accounts_do_not_gain_evidence_codes(database):
    _, account = await seed(database, "fictional-a")
    _, instance = worker(database)
    own = replace(message("999"), author_kind=AuthorKind.BOT)
    codes, _, lines = await instance._render(
        GroupId("311"), [message("absent"), own, message("fictional-a")]
    )
    assert codes == {1: account}
    assert lines[0].author_account_id is None
    assert lines[1].own and not lines[1].evidence_text and not lines[1].targets
