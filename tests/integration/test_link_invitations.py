"""Invitation participation is exclusive per group across both endpoint roles."""

import asyncio
import uuid

import pytest

import _db as owners
from qqbot.domain.ids import GroupId
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.identity_link import IdentityLinkRepository, LinkChallengeError
from qqbot.services.identity_link import IdentityLinkService
from qqbot.services.identity_limits import CHALLENGE_LIMITS
from qqbot.services.identity_resolver import IdentityResolver

pytestmark = pytest.mark.database
GROUP = GroupId("311")
OTHER = GroupId("312")


async def setup(database):
    identities = IdentityRepository(database=lambda: database.pool, clock=owners.clock)
    for user in ("fictional-a", "fictional-b", "fictional-c", "fictional-d"):
        await identities.ensure_account("qq", user, seen_at=owners.clock.now())
    service = IdentityLinkService(
        CHALLENGE_LIMITS,
        IdentityResolver(identities),
        identities,
        IdentityLinkRepository(database=lambda: database.pool),
        clock=owners.clock,
    )
    return service, identities


async def event(database, user, group=GROUP):
    return await database.pool.fetchval(
        """INSERT INTO raw_event(platform,event_type,group_id,platform_user_id,occurred_at,payload)
           VALUES ('qq','message',$1,$2,clock_timestamp(),'{}') RETURNING id""",
        group.to_db(),
        user,
    )


async def issue(database, service, left="fictional-a", right="fictional-b", group=GROUP):
    return await service.issue(
        group_id=group,
        initiator_user_id=left,
        target_user_id=right,
        created_event_id=await event(database, left, group),
    )


@pytest.mark.parametrize(
    "left,right",
    [
        ("fictional-a", "fictional-c"),
        ("fictional-c", "fictional-a"),
        ("fictional-b", "fictional-c"),
        ("fictional-c", "fictional-b"),
        ("fictional-b", "fictional-a"),
    ],
)
async def test_either_endpoint_cannot_join_another_invitation_in_same_group(database, left, right):
    service, _identities = await setup(database)
    await issue(database, service)
    with pytest.raises(LinkChallengeError, match="本群已有"):
        await issue(database, service, left, right)
    other = await issue(database, service, left, right, OTHER)
    assert other.group_id == OTHER
    assert (
        await database.pool.fetchval(
            "SELECT count(*) FROM account_link_challenge WHERE status='pending'"
        )
        == 2
    )


async def test_concurrent_cross_role_offers_have_one_winner(database):
    service, _identities = await setup(database)
    results = await asyncio.wait_for(
        asyncio.gather(
            issue(database, service, "fictional-a", "fictional-b"),
            issue(database, service, "fictional-c", "fictional-a"),
            return_exceptions=True,
        ),
        timeout=4,
    )
    assert sum(isinstance(result, LinkChallengeError) for result in results) == 1
    assert (
        await database.pool.fetchval(
            "SELECT count(*) FROM account_link_challenge WHERE status='pending'"
        )
        == 1
    )


@pytest.mark.parametrize("actor", ["fictional-a", "fictional-b"])
async def test_either_party_can_cancel_only_their_current_group_invitation(database, actor):
    service, _identities = await setup(database)
    task = await issue(database, service)
    elsewhere = await issue(database, service, group=OTHER)
    assert not await service.cancel(
        group_id=GROUP,
        actor_user_id="fictional-c",
        cancelled_event_id=await event(database, "fictional-c"),
    )
    cancelled = await event(database, actor)
    assert await service.cancel(group_id=GROUP, actor_user_id=actor, cancelled_event_id=cancelled)
    assert not await service.cancel(
        group_id=GROUP, actor_user_id=actor, cancelled_event_id=cancelled
    )
    assert (
        await database.pool.fetchval(
            "SELECT status FROM account_link_challenge WHERE id=$1",
            task.id,
        )
        == "cancelled"
    )
    assert (
        await database.pool.fetchval(
            "SELECT status FROM account_link_challenge WHERE id=$1",
            elsewhere.id,
        )
        == "pending"
    )
    replacement = await issue(database, service)
    assert not await service.cancel(
        group_id=GROUP, actor_user_id=actor, cancelled_event_id=cancelled
    )
    assert (
        await database.pool.fetchval(
            "SELECT status FROM account_link_challenge WHERE id=$1",
            replacement.id,
        )
        == "pending"
    )


async def test_confirmation_binds_account_group_event_and_arrival_before_invitation(database):
    service, identities = await setup(database)
    early = await event(database, "fictional-b")
    invitation = await issue(database, service)
    for actor, group, event_id in (
        ("fictional-a", GROUP, await event(database, "fictional-a")),
        ("fictional-c", GROUP, await event(database, "fictional-c")),
        ("fictional-b", OTHER, await event(database, "fictional-b", OTHER)),
        ("fictional-b", GROUP, early),
        ("fictional-b", GROUP, await event(database, "fictional-c")),
    ):
        with pytest.raises(LinkChallengeError):
            await service.confirm(group_id=group, actor_user_id=actor, confirmed_event_id=event_id)
    confirmation = await event(database, "fictional-b")
    result = await service.confirm(
        group_id=GROUP, actor_user_id="fictional-b", confirmed_event_id=confirmation
    )
    assert result.id == invitation.id and result.status.value == "applied"
    assert (
        await service.confirm(
            group_id=GROUP, actor_user_id="fictional-b", confirmed_event_id=confirmation
        )
    ).id == invitation.id
    with pytest.raises(LinkChallengeError):
        await service.confirm(
            group_id=GROUP,
            actor_user_id="fictional-b",
            confirmed_event_id=await event(database, "fictional-b"),
        )
    first, second = await identities.accounts_by_users("qq", ["fictional-a", "fictional-b"])
    assert first.entity_id == second.entity_id


async def test_expired_invitation_releases_both_roles(database):
    service, _identities = await setup(database)
    expired = await issue(database, service)
    await database.pool.execute(
        "UPDATE account_link_challenge SET expires_at=clock_timestamp()-interval '1 second' "
        "WHERE id=$1",
        expired.id,
    )
    await issue(database, service, "fictional-b", "fictional-c")
    assert (
        await database.pool.fetchval(
            "SELECT status FROM account_link_challenge WHERE id=$1",
            expired.id,
        )
        == "expired"
    )


async def test_unknown_confirmation_event_cannot_apply_union(database):
    service, _identities = await setup(database)
    await issue(database, service)
    with pytest.raises(LinkChallengeError):
        await service.confirm(
            group_id=GROUP, actor_user_id="fictional-b", confirmed_event_id=uuid.uuid4()
        )
