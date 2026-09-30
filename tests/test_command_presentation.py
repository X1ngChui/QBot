"""Command output and input contracts use only fictional data and owned fakes."""

from dataclasses import replace
from types import SimpleNamespace
from unittest.mock import AsyncMock
import uuid

import pytest

from test_commands import (
    GROUP,
    MEMBER,
    OTHER,
    request,
    router_case,
)
from qqbot.commands.catalog import CATALOG
from qqbot.domain.ids import GroupId
from qqbot.repositories.scheduled_task import ScheduledTask, TaskStatus
from qqbot.services.scheduled_tasks import ScheduledTaskService
from scripts._task_simulation import FictionalTaskStorage

__all__ = ["router_case"]


def text(result):
    return "".join(message[-1].text.lstrip(" ") for message in result.messages)


def task_service(router):
    store = FictionalTaskStorage(clock=router._clock, namespace="command-contract")
    router._tasks = ScheduledTaskService(store, clock=router._clock)
    return store


@pytest.mark.parametrize("command", [item.name for item in CATALOG])
async def test_every_command_has_complete_current_help(router_case, command):
    router, bot, _directory, _links = router_case
    result = await router.handle(bot, request("/help", user=MEMBER, text="/help " + command))
    output = text(result)
    assert command in output and "用法" not in output
    assert "不需要验证码" not in output and "/note set" not in output
    assert all(
        len(message[-1].text) <= router._bundle.default.conversation.max_text_chars_per_message
        for message in result.messages
    )


@pytest.mark.parametrize("subcommand", ["", "list", "show", "add", "edit", "cancel"])
async def test_every_task_command_is_owner_only_before_storage(router_case, subcommand):
    router, bot, _directory, _links = router_case
    store = task_service(router)
    result = await router.handle(bot, request("/tasks", user=MEMBER, text="/tasks " + subcommand))
    assert "bot owner" in text(result) and store.operations == []


async def test_task_command_roundtrip_preserves_content_id_scope_and_output(router_case):
    router, bot, _directory, _links = router_case
    store = task_service(router)
    added = await router.handle(
        bot, request("/tasks", text="/tasks add --in 10m -- first  line\nsecond")
    )
    task = next(iter(store.tasks.values()))
    assert task.intent == "first  line\nsecond" and task.group_id == GROUP
    assert str(task.id) in text(added) and "预约时间" in text(added)
    modified = await router.handle(
        bot,
        request(
            "/tasks",
            text=f"/tasks edit {task.id} -- revised  content",
        ),
    )
    assert "已修改" in text(modified) and store.creations == 1
    assert store.tasks[task.id].due_at == task.due_at
    listed = await router.handle(bot, request("/tasks"))
    assert "待执行" in text(listed) and str(task.id) in text(listed)
    removed = await router.handle(bot, request("/tasks", text=f"/tasks cancel {task.id}"))
    assert "已取消" in text(removed)
    detail = await router.handle(bot, request("/tasks", text=f"/tasks show {task.id}"))
    assert "已取消" in text(detail) and "revised  content" in text(detail)
    other = replace(request("/tasks", text=f"/tasks show {task.id}"), group_id=GroupId("8002"))
    assert "本群没有" in text(await router.handle(bot, other))


@pytest.mark.parametrize(
    "suffix",
    [
        "add --in 2m -- too soon",
        "add --in 600 -- unit required",
        "add --at 2030-01-02T09:00:00 -- naive",
        "add --in 10m --at 2030-01-02T09:00:00Z -- both",
        "add --in 10m --in 20m -- duplicate",
        "add --unknown 10m -- extra",
        "add --in 10m",
        "add --in 10m --",
        "list 0",
        "list 10001",
        "show guessed-id",
        "edit 00000000-0000-0000-0000-000000000001",
        "cancel",
        "unknown",
    ],
)
async def test_invalid_task_command_has_no_storage_mutation(router_case, suffix):
    router, bot, _directory, _links = router_case
    store = task_service(router)
    result = await router.handle(bot, request("/tasks", text="/tasks " + suffix))
    assert text(result) and not store.tasks
    assert not any(name in store.operations for name in ("create", "update", "cancel"))


async def test_note_independent_crud_and_shared_scope(router_case):
    router, bot, directory, _links = router_case
    for value in ("exact one", "exact  two"):
        assert "新增" in text(
            await router.handle(
                bot,
                request(
                    "/note",
                    user=MEMBER,
                    text="/note add -- " + value,
                ),
            )
        )
    await router.handle(bot, request("/note", user=MEMBER, text="/note add --all -- shared"))
    output = text(await router.handle(bot, request("/note", user=MEMBER)))
    assert "1. exact one" in output and "2. exact  two" in output and "shared" not in output
    await router.handle(bot, request("/note", user=MEMBER, text="/note edit 1 -- revised"))
    assert "revised" in text(await router.handle(bot, request("/note", user=MEMBER)))
    await router.handle(bot, request("/note", user=MEMBER, text="/note remove 2"))
    cleared = await router.handle(bot, request("/note", user=MEMBER, text="/note clear --all"))
    assert "1 条" in text(cleared)
    assert len(directory._notes[(GROUP, str(MEMBER), False)]) == 1
    assert "无需清除" in text(
        await router.handle(
            bot,
            request(
                "/note",
                user=MEMBER,
                text="/note clear --all",
            ),
        )
    )


@pytest.mark.parametrize("action", ["confirm", "cancel"])
async def test_link_action_targets_only_its_authenticated_actor_and_event(router_case, action):
    router, bot, _directory, links = router_case
    call = request("/link", user=MEMBER, text="/link " + action)
    result = await router.handle(bot, call)
    assert "已" in text(result)
    operation, arguments = links.calls[-1]
    assert operation == action and arguments["actor_user_id"] == str(MEMBER)
    assert arguments["group_id"] == GROUP
    assert (
        arguments[("confirmed" if action == "confirm" else "cancelled") + "_event_id"]
        == call.raw_event_id
    )
    assert set(arguments) == {
        "group_id",
        "actor_user_id",
        ("confirmed" if action == "confirm" else "cancelled") + "_event_id",
    }
    links.calls.clear()
    assert "用法" in text(
        await router.handle(
            bot,
            request(
                "/link",
                user=MEMBER,
                text="/link " + action,
                mentions=(OTHER,),
            ),
        )
    )
    assert "用法" in text(
        await router.handle(
            bot,
            request(
                "/link",
                user=MEMBER,
                text="/link " + action + " extra",
            ),
        )
    )
    assert not links.calls


async def test_long_managed_detail_and_help_preserve_ids_and_full_body(router_case):
    router, bot, _directory, _links = router_case
    cfg = router._bundle.default.model_copy(
        update={
            "conversation": router._bundle.default.conversation.model_copy(
                update={"max_text_chars_per_message": 400}
            ),
        }
    )
    router._bundle = SimpleNamespace(default=cfg, for_group=lambda _: (cfg, None))
    store = task_service(router)
    identifier = uuid.uuid4()
    store.tasks[identifier] = ScheduledTask(
        identifier,
        GROUP,
        "a" * 500,
        router._clock.now(),
        identifier,
        0,
        status=TaskStatus.PENDING,
    )
    result = await router.handle(bot, request("/tasks", text=f"/tasks show {identifier}"))
    assert str(identifier) in text(result) and "a" * 500 in text(result)
    assert all(len(message[-1].text) <= 400 for message in result.messages)
    help_result = text(await router.handle(bot, request("/help", text="/help note")))
    assert "/note clear" in help_result and "/note remove" in help_result


async def test_mute_reports_unchanged_and_changed_state(router_case):
    router, bot, _directory, _links = router_case
    state = SimpleNamespace(muted=False, persist=AsyncMock())
    router._registry = SimpleNamespace(get=AsyncMock(return_value=state))
    assert "未静音" in text(await router.handle(bot, request("/mute")))
    assert "启用回复状态" in text(await router.handle(bot, request("/mute", text="/mute off")))
    state.persist.assert_awaited_once()
    assert "设为静音" in text(await router.handle(bot, request("/mute", text="/mute on")))
    assert state.persist.await_count == 2
    assert "已处于静音" in text(await router.handle(bot, request("/mute", text="/mute on")))


async def test_storage_failure_never_claims_success(router_case):
    router, bot, _directory, _links = router_case
    router._tasks = SimpleNamespace(active=AsyncMock(side_effect=TimeoutError))
    assert "未能确认" in text(await router.handle(bot, request("/tasks")))


async def test_mute_retry_persists_after_uncertain_first_write(router_case):
    router, bot, _directory, _links = router_case
    state = SimpleNamespace(muted=False, persist=AsyncMock(side_effect=[TimeoutError, None]))
    router._registry = SimpleNamespace(get=AsyncMock(return_value=state))
    first = await router.handle(bot, request("/mute", text="/mute on"))
    assert "未能确认" in text(first)
    second = await router.handle(bot, request("/mute", text="/mute on"))
    assert "静音" in text(second) and state.persist.await_count == 2
