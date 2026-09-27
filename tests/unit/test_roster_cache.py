"""Roster retention is bounded, detached, namespaced and owned by one directory."""

from datetime import UTC, datetime
from types import SimpleNamespace
from unittest.mock import AsyncMock
import uuid

import pytest

from qqbot.domain.ids import GroupId
from qqbot.services.directory import PersonCard
from qqbot.services.retrieval import gather
from qqbot.services.roster_cache import RosterCache


GROUP = GroupId("311")
KEY = ("bot", GROUP)


def row(note="Fictional note"):
    return {
        "user_id": "101",
        "entity_id": uuid.uuid4(),
        "accounts": ["101"],
        "nickname": "Fictional",
        "former_names": [],
        "aliases": [],
        "memory_hints": (),
        "manual_note": note,
        "msg_count": 1,
    }


def test_cache_evicts_by_lru_and_total_rows():
    cache = RosterCache(capacity=3, max_rows=3)
    keys = [(str(i), GROUP) for i in range(4)]
    for key in keys[:3]:
        cache.put(key, (1,), {}, [row()])
    assert cache.get(keys[0])
    cache.put(keys[3], (1,), {}, [row()])
    assert cache.get(keys[1]) is None
    assert cache.size == cache.rows == 3
    cache.put(keys[0], (2,), {}, [row(), row()])
    assert cache.size == 2 and cache.rows == 3


def test_oversized_roster_is_not_retained_or_truncated():
    cache = RosterCache(max_text_bytes=1024)
    large = [row("x" * 2000)]
    cache.put(KEY, (1,), {}, large)
    assert cache.size == cache.rows == cache.text_bytes == 0
    assert len(large[0]["manual_note"]) == 2000
    cache.put(KEY, (2,), {}, [row()])
    assert 0 < cache.text_bytes <= 1024
    cache.put(("other", GROUP), (2,), {}, [row()])
    assert cache.text_bytes <= 1024


def test_snapshot_does_not_share_mutable_input_or_output():
    cache = RosterCache()
    source = row()
    cache.put(KEY, (1,), {}, [source])
    source["accounts"].append("102")
    entry = cache.get(KEY)
    assert entry is not None
    first = entry.project()
    first[0]["accounts"].append("103")
    first[0]["nickname"] = "Changed"
    assert entry.project()[0]["accounts"] == ["101"]
    assert entry.project()[0]["nickname"] == "Fictional"
    assert entry.project()[0]["entity_id"] == source["entity_id"]


def test_expiry_and_close_release_retention_and_prevent_late_refill():
    now = 0
    cache = RosterCache(ttl=5, clock=lambda: now)
    cache.put(KEY, (1,), {}, [row()])
    now = 5
    assert cache.get(KEY) is None
    assert cache.text_bytes == cache.rows == cache.size == 0
    cache.close()
    cache.put(KEY, (2,), {}, [row()])
    assert cache.size == 0


def directory():
    cards = [
        PersonCard(
            uuid.uuid4(),
            "101",
            "101",
            accounts=("101",),
            messages=1,
            first_seen=datetime(2026, 1, 1, tzinfo=UTC),
        )
    ]
    return SimpleNamespace(
        roster_cache=RosterCache(),
        roster=AsyncMock(return_value=cards),
        roster_revision=AsyncMock(return_value=(1, 1, 1)),
    )


async def test_gather_namespaces_bot_identity_and_detaches_cache_hits():
    owner = directory()
    members = SimpleNamespace(names_of=AsyncMock(return_value={"101": "Fictional"}))
    bot = SimpleNamespace(self_id="999")
    first = await gather(group_id=GROUP, directory=owner, members=members, bot=bot)
    second = await gather(group_id=GROUP, directory=owner, members=members, bot=bot)
    assert first == second
    assert owner.roster.await_count == 1
    await gather(group_id=GROUP, directory=owner, members=members)
    assert owner.roster.await_count == 2
    other = directory()
    await gather(group_id=GROUP, directory=other, members=members, bot=bot)
    assert other.roster.await_count == 1


async def test_revision_change_during_read_cannot_publish_a_stale_cache_entry():
    owner = directory()
    owner.roster_revision.side_effect = [(1,), (2,), (2,), (2,)]
    members = object()
    await gather(group_id=GROUP, directory=owner, members=members)
    assert owner.roster_cache.size == 0
    await gather(group_id=GROUP, directory=owner, members=members)
    assert owner.roster.await_count == 2
    assert owner.roster_cache.size == 1


async def test_first_appearance_order_is_part_of_the_same_read_model():
    owner = directory()
    older = PersonCard(
        uuid.uuid4(),
        "102",
        "102",
        accounts=("102",),
        messages=100,
        first_seen=datetime(2025, 1, 1, tzinfo=UTC),
    )
    owner.roster.return_value.append(older)
    result = await gather(group_id=GROUP, directory=owner, members=object())
    assert [item["user_id"] for item in result] == ["102", "101"]


@pytest.mark.parametrize("option", ["capacity", "max_rows", "max_text_bytes", "ttl"])
def test_retention_cannot_be_unbounded(option):
    with pytest.raises(ValueError):
        RosterCache(**{option: 0})
