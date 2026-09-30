"""One execution path for independently addressed messages and durable wakeups."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass

from qqbot.domain.ids import AccountId
import asyncpg

from qqbot.clock import Clock
from qqbot.configuration import ConfigBundle
from qqbot.conversation import engine, prompt
from qqbot.conversation.scheduler import ReplyWork
from qqbot.conversation.state import ChatMsg, Registry
from qqbot.delivery.service import GroupDelivery
from qqbot.domain.ids import GroupId
from qqbot.domain.reply import ReplyEnd, ReplyOutcome
from qqbot.gateway.botapi import BotApi
from qqbot.media.coordinator import MediaCoordinator
from qqbot.media.limits import MEDIA_IO
from qqbot.providers.base import Providers
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.evidence import EvidenceRepository
from qqbot.repositories.archive import ArchiveRepository
from qqbot.services.scheduled_tasks import ScheduledTaskService
from qqbot.repositories.scheduled_task import ScheduledTask
from qqbot.services import Directory
from qqbot.services.budget import Budget, BudgetExceeded, BudgetUnavailable
from qqbot.services.members import MemberDirectory


@dataclass(frozen=True, slots=True)
class AddressedMessage:
    message: ChatMsg
    initiator: AccountId
    window: tuple[ChatMsg, ...]


@dataclass(frozen=True, slots=True)
class DueTask:
    task: ScheduledTask


type ReplyCause = AddressedMessage | DueTask


@dataclass(frozen=True, slots=True)
class ReplyRequest:
    bot: BotApi
    group_id: GroupId
    cause: ReplyCause


class ReplyExecutor:
    def __init__(
        self,
        *,
        bundle: ConfigBundle,
        clock: Clock,
        database: Callable[[], asyncpg.Pool],
        identities: IdentityRepository,
        evidence_store: EvidenceRepository,
        archive: ArchiveRepository,
        budget: Budget,
        members: MemberDirectory,
        registry: Registry,
        delivery: GroupDelivery,
        media: MediaCoordinator,
        providers: Providers,
        directory: Directory,
        tasks: ScheduledTaskService,
    ) -> None:
        self.tasks = tasks
        self.bundle = bundle
        self.clock = clock
        self.database = database
        self.identities = identities
        self.evidence_store = evidence_store
        self.archive = archive
        self.budget = budget
        self.members = members
        self.registry = registry
        self.delivery = delivery
        self.media = media
        self.providers = providers
        self.directory = directory

    async def __call__(self, work: ReplyWork[ReplyRequest]) -> ReplyOutcome:
        return await ReplySession(self, work).run()


@dataclass(slots=True)
class ReplySession:
    executor: ReplyExecutor
    work: ReplyWork[ReplyRequest]

    async def run(self) -> ReplyOutcome:
        owner, work = self.executor, self.work
        request = work.value
        cfg, persona = owner.bundle.for_group(request.group_id)
        state = await owner.registry.get(request.group_id)
        match request.cause:
            case AddressedMessage(message, initiator, history):
                current, window, scheduled = message, list(history), None
            case DueTask(task):
                await state.load_history(self_id=request.bot.self_id, owners=cfg.bot.owners)
                current, window, scheduled = None, prompt.history_window(state, None), task
                initiator = None
        if state.muted:
            return work.progress.finish(ReplyEnd.MUTED)
        if (
            initiator is not None
            and initiator not in cfg.bot.owners
            and await state.blocked_now(initiator)
        ):
            return work.progress.finish(ReplyEnd.BLOCKED)
        try:
            if await owner.budget.exceeded():
                return work.progress.finish(ReplyEnd.BUDGET)
            with owner.budget.attribute(initiator), owner.budget.scope(cfg.budget.per_reply_cny):
                await owner.media.settle(
                    window + ([current] if current is not None else []),
                    wait_sec=MEDIA_IO.wait_sec,
                    who=initiator,
                    cfg=cfg,
                )
                return await engine.respond(
                    bot=request.bot,
                    st=state,
                    cfg=cfg,
                    clock=owner.clock,
                    prompts=owner.bundle.prompts,
                    database=owner.database,
                    identities=owner.identities,
                    evidence_store=owner.evidence_store,
                    archive=owner.archive,
                    budget=owner.budget,
                    members=owner.members,
                    persona=persona,
                    msg=current,
                    providers=owner.providers,
                    media=owner.media.processor,
                    directory=owner.directory,
                    tasks=owner.tasks,
                    delivery=owner.delivery,
                    progress=work.progress,
                    window=window,
                    scheduled=scheduled,
                    deadline=work.deadline,
                )
        except (BudgetExceeded, BudgetUnavailable):
            return work.progress.finish(ReplyEnd.BUDGET)
