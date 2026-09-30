"""Multiple manual notes remain separate from learned facts in every scope."""

import asyncio

import pytest

import _db as owners
from _fixtures import example_bundle
from qqbot.domain.ids import GroupId
from qqbot.domain.memory import Fact
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.memory import MemoryRepository
from qqbot.services import IdentityResolver, retrieval

pytestmark = pytest.mark.database
GROUP = GroupId("311")


async def account(database, user):
    entity = await database.pool.fetchval(
        "INSERT INTO entity(entity_type,canonical_name) VALUES ('person','Fictional') RETURNING id"
    )
    exact = await database.pool.fetchval(
        "INSERT INTO identity_account(entity_id,platform,platform_user_id) VALUES ($1,'qq',$2) "
        "RETURNING id",
        entity,
        user,
    )
    return entity, exact


def services(database):
    identities = IdentityRepository(database=lambda: database.pool, clock=owners.clock)
    memory = MemoryRepository(database=lambda: database.pool, clock=owners.clock)
    directory = retrieval.build_directory(
        database=lambda: database.pool,
        identities=identities,
        resolver=IdentityResolver(identities),
        predicates=example_bundle().predicates,
        clock=owners.clock,
    )
    return directory, memory


async def test_independent_notes_edit_history_and_learned_numbering(database):
    _holder, exact = await account(database, "fictional-a")
    directory, memory = services(database)
    first = await directory.add_note(GROUP, "fictional-a", "Fictional manual detail")
    second = await directory.add_note(GROUP, "fictional-a", "Fictional manual detail")
    assert first.object_key != second.object_key
    automatic = await memory.supersede(
        Fact(
            subject_entity_id=None,
            subject_account_id=exact,
            group_id=GROUP,
            predicate="likes",
            object_key="fictional-item",
            object_value="Fictional preference",
            confidence=1.0,
        ),
        [],
        when=owners.clock.now(),
    )
    card = await directory.account_card(GROUP, "fictional-a")
    assert len(card.notes) == 2 and all(note.index is None for note in card.notes)
    assert [(fact.index, fact.id) for fact in card.learned] == [(1, automatic.id)]
    rows, _more = await directory.notes(GROUP, "fictional-a")
    before = rows[0]
    updated = await directory.edit_note(GROUP, "fictional-a", 1, "Revised fictional detail")
    assert updated.object_key == before.object_key and updated.id != before.id
    assert (
        await database.pool.fetchval(
            "SELECT status FROM memory_fact WHERE id=$1",
            before.id,
        )
        == "superseded"
    )
    assert (await directory.forget(GROUP, "fictional-a", 1)).id == automatic.id
    assert len((await directory.account_card(GROUP, "fictional-a")).notes) == 2
    assert await memory.retract_learned(GROUP, updated.id, subject=exact, holder=False) is False
    assert (await directory.remove_note(GROUP, "fictional-a", 1)).object_key == before.object_key
    assert len((await directory.notes(GROUP, "fictional-a"))[0]) == 1


async def test_shared_clear_keeps_exact_and_other_group_notes(database):
    holder, first = await account(database, "fictional-a")
    other, second = await account(database, "fictional-b")
    await database.pool.execute("UPDATE entity SET merged_into=$1 WHERE id=$2", holder, other)
    await database.pool.execute(
        "UPDATE identity_account SET entity_id=$1 WHERE id=$2",
        holder,
        second,
    )
    directory, _memory = services(database)
    await directory.add_note(GROUP, "fictional-a", "Exact A")
    await directory.add_note(GROUP, "fictional-b", "Exact B")
    await directory.add_note(GROUP, "fictional-a", "Shared A", all_linked=True)
    await directory.add_note(GROUP, "fictional-b", "Shared B", all_linked=True)
    await directory.add_note(GroupId("312"), "fictional-a", "Other group", all_linked=True)
    shared, _ = await directory.notes(GROUP, "fictional-b", all_linked=True)
    assert {note.object_value for note in shared} == {"Shared A", "Shared B"}
    assert len((await directory.holder_card(GROUP, "fictional-a")).notes) == 4
    assert await directory.clear_notes(GROUP, "fictional-b", all_linked=True) == 2
    assert (
        await database.pool.fetchval(
            "SELECT count(*) FROM memory_fact WHERE status='active' AND predicate='note' "
            "AND subject_account_id=ANY($1::uuid[])",
            [first, second],
        )
        == 2
    )
    assert len((await directory.notes(GroupId("312"), "fictional-a", all_linked=True))[0]) == 1


async def test_concurrent_note_adds_respect_scope_limit_and_paging(database):
    await account(database, "fictional-a")
    directory, _memory = services(database)
    results = await asyncio.gather(
        *(
            directory.add_note(GROUP, "fictional-a", f"Fictional detail {index}")
            for index in range(21)
        ),
        return_exceptions=True,
    )
    assert sum(isinstance(value, Fact) for value in results) == 20
    assert sum(isinstance(value, ValueError) for value in results) == 1
    first, more = await directory.notes(GROUP, "fictional-a")
    last, last_more = await directory.notes(GROUP, "fictional-a", page=4)
    assert len(first) == len(last) == 5 and more and not last_more
    assert {fact.id for fact in first}.isdisjoint(fact.id for fact in last)
    assert await directory.clear_notes(GROUP, "fictional-a") == 20
    assert await directory.clear_notes(GROUP, "fictional-a") == 0


async def test_learning_limit_cannot_hide_notes_or_leak_scope(database):
    holder, exact = await account(database, "fictional-a")
    directory, memory = services(database)
    await directory.add_note(GROUP, "fictional-a", "Exact manual")
    await directory.add_note(GROUP, "fictional-a", "Shared manual", all_linked=True)
    for index in range(3):
        await memory.supersede(
            Fact(
                subject_entity_id=None,
                subject_account_id=exact,
                group_id=GROUP,
                predicate="likes",
                object_key=str(index),
                object_value=str(index),
                confidence=1,
            ),
            [],
            when=owners.clock.now(),
        )
    facts = await memory.current_facts(GROUP, [holder], limit=1)
    assert len(facts) == 3 and sum(fact.predicate == "note" for fact in facts) == 2
    assert not await memory.retract_learned(
        GroupId("312"), facts[-1].id, subject=holder, holder=True
    )


@pytest.mark.parametrize("text", ["", " ", "x" * 501])
async def test_invalid_note_does_not_write(database, text):
    await account(database, "fictional-a")
    directory, _memory = services(database)
    with pytest.raises(ValueError):
        await directory.add_note(GROUP, "fictional-a", text)
    assert await database.pool.fetchval("SELECT count(*) FROM memory_fact") == 0


async def test_shared_note_write_uses_current_holder_after_account_detaches(database, monkeypatch):
    from unittest.mock import AsyncMock
    from qqbot.domain.ids import AccountId
    from qqbot.repositories.identity import IdentityRepository

    _holder_a, first = await account(database, "fictional-a")
    _holder_b, _second = await account(database, "fictional-b")
    directory, _memory = services(database)
    identities = IdentityRepository(database=lambda: database.pool, clock=owners.clock)
    await identities.merge_accounts(first, _second)
    stale = await identities.account_by_id(first)
    shared = await directory.add_note(
        GROUP, AccountId("fictional-a"), "Remaining shared note", all_linked=True
    )
    await identities.split(stale)
    monkeypatch.setattr(directory._identity, "account", AsyncMock(return_value=stale))
    assert await directory.clear_notes(GROUP, AccountId("fictional-a"), all_linked=True) == 0
    assert (
        await database.pool.fetchval(
            "SELECT status FROM memory_fact WHERE id=$1",
            shared.id,
        )
        == "active"
    )
