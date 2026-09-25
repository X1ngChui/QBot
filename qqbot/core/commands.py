"""Importable, strictly parsed command routing and group delivery."""

from __future__ import annotations

import logging
import os
import uuid
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, replace
from pathlib import Path

from ..db import repo
from ..domain.ids import AccountId, GroupId, MessageId
from ..domain.identity.alias import CONFIRM_THRESHOLD
from ..providers import Kind
from ..providers.base import Providers
from ..repositories.extraction import ExtractionRepository
from ..repositories.identity_link import LinkChallengeError
from ..services import (
    Directory,
    IdentityLinkService,
    NameTaken,
    NotMerged,
    PersonCard,
    UnknownAccount,
)
from ..settings import config
from ..util import defang, fmt_when, now_local, parse_duration, today_local
from . import agreement, command_catalog, debug, errors, perms
from .botapi import BotApi
from .budget import BUDGET, hit_split
from .delivery import GroupDelivery
from .members import MEMBERS
from .outbound import AtSegment, ReplySegment, SendSegment, TextSegment
from .state import Registry

log = logging.getLogger("qqbot.cmd")


@dataclass(frozen=True, slots=True)
class CommandRequest:
    """One admitted group command, detached from the framework event type."""

    name: str
    message_id: MessageId
    raw_event_id: uuid.UUID
    group_id: GroupId
    user_id: AccountId
    self_id: AccountId
    text: str
    mentions: tuple[AccountId, ...] = ()

    def get_plaintext(self) -> str:
        return self.text


@dataclass(frozen=True, slots=True)
class CommandResult:
    messages: tuple[tuple[SendSegment, ...], ...]

    @classmethod
    def reply(cls, request: CommandRequest, text: str) -> CommandResult:
        return cls(
            (
                (
                    ReplySegment(request.message_id),
                    AtSegment(request.user_id),
                    TextSegment(" " + text),
                ),
            )
        )


@dataclass(frozen=True, slots=True)
class CommandContext:
    request: CommandRequest
    bot: BotApi
    owner: bool
    registry: Registry
    directory: Directory
    links: IdentityLinkService
    providers: Providers


class _Finished(Exception):
    def __init__(self, result: CommandResult | None) -> None:
        super().__init__()
        self.result = result


class UsageError(ValueError):
    pass


async def _finish(ctx: CommandContext, message: str) -> None:
    raise _Finished(CommandResult.reply(ctx.request, message))


type CommandHandler = Callable[[CommandContext], Awaitable[None]]
type CommandBody = Callable[[CommandContext, CommandRequest], Awaitable[None]]
_HANDLERS: dict[str, CommandHandler] = {}


def _handler(name: str) -> Callable[[CommandBody], CommandBody]:
    spec = command_catalog.find(name)
    if spec is None or spec.name in _HANDLERS:
        raise RuntimeError(f"invalid command handler registration: {name}")

    def register(body: CommandBody) -> CommandBody:
        async def run(ctx: CommandContext) -> None:
            await body(ctx, ctx.request)

        _HANDLERS[spec.name] = run
        return body

    return register


class CommandRouter:
    """Authorize one stable command meaning, execute it, then deliver its result."""

    def __init__(
        self,
        delivery: GroupDelivery,
        registry: Registry,
        directory: Directory,
        links: IdentityLinkService,
        providers: Providers,
    ) -> None:
        self._delivery = delivery
        self._registry = registry
        self._directory = directory
        self._links = links
        self._providers = providers

    async def handle(self, bot: BotApi, request: CommandRequest) -> CommandResult | None:
        spec = command_catalog.find(request.name)
        if spec is None:
            return None
        verdict = perms.decide(
            request.user_id,
            owners=config().default.owners,
            access=spec.access,
        )
        if verdict is perms.Verdict.MEMBER_IF_AGREED:
            allowed = await agreement.ok(request.group_id, request.user_id)
            denial = agreement.POINTER
        else:
            allowed = verdict in (perms.Verdict.OWNER, perms.Verdict.MEMBER)
            denial = "该操作需要 bot owner 权限。"
        if not allowed:
            return CommandResult.reply(request, denial)

        handler = _HANDLERS.get(spec.name)
        if handler is None:
            raise RuntimeError(f"command has no handler: {spec.name}")
        ctx = CommandContext(
            request=request,
            bot=bot,
            owner=verdict is perms.Verdict.OWNER,
            registry=self._registry,
            directory=self._directory,
            links=self._links,
            providers=self._providers,
        )
        try:
            await handler(ctx)
        except UsageError as exc:
            return CommandResult.reply(request, str(exc))
        except _Finished as done:
            return done.result
        raise RuntimeError(f"command handler returned without a result: {spec.name}")

    async def dispatch(self, bot: BotApi, request: CommandRequest) -> bool:
        result = await self.handle(bot, request)
        if result is None:
            return False
        delivered = await self._delivery.deliver(
            bot,
            group_id=request.group_id,
            messages=result.messages,
        )
        return bool(delivered)


def _fit(text: str, *, head: bool = True, gid: GroupId | None = None) -> str:
    cfg = config().for_group(gid)[0] if gid is not None else config().default
    marker = "…（已截断）"
    limit = max(1, cfg.tools.send_messages.max_text_chars_per_message - len(marker) - 2)
    if len(text) <= limit:
        return text
    kept = text[:limit] if head else text[-limit:]
    return (kept + "\n" + marker) if head else (marker + "\n" + kept)


def _strip_cmd(text: str, name: str) -> str:
    text = defang(text.strip())
    for command in (f"/{name}", name):
        if text.startswith(command):
            return text[len(command) :].strip()
    return text


def _args(event: CommandRequest, name: str) -> list[str]:
    return _strip_cmd(event.text, name).split()


def _mentions(event: CommandRequest, *, exact: int | None = None, maximum: int = 1) -> list[str]:
    mentions = [str(account) for account in event.mentions]
    if exact is not None and len(mentions) != exact:
        raise UsageError(f"需要准确 @ {exact} 个账号。")
    if exact is None and len(mentions) > maximum:
        raise UsageError(f"最多只能 @ {maximum} 个账号。")
    return mentions


def _scope(tokens: list[str]) -> tuple[bool, list[str]]:
    count = tokens.count("--all")
    if count > 1:
        raise UsageError("--all 只能写一次。")
    if any(token.startswith("--") and token != "--all" for token in tokens):
        raise UsageError("存在未知选项。发送 /help 查看当前语法。")
    return bool(count), [token for token in tokens if token != "--all"]


async def _target(
    ctx: CommandContext,
    event: CommandRequest,
    *,
    all_linked: bool,
) -> str:
    mentioned = _mentions(event)
    target = mentioned[0] if mentioned else str(event.user_id)
    if not ctx.owner:
        own = await ctx.directory.linked_account_ids(str(event.user_id))
        if target not in own:
            await _finish(ctx, "普通成员只能操作自己的账号记录。")
    return target


async def _owner_action(ctx: CommandContext) -> None:
    if not ctx.owner:
        await _finish(ctx, "该操作需要 bot owner 权限。")


async def _name(ctx: CommandContext, group_id: GroupId, user_id: str) -> str:
    if name := await MEMBERS.name_of(ctx.bot, group_id, user_id):
        return name
    try:
        card = await ctx.directory.account_card(group_id, user_id)
        if card.display and card.display not in card.accounts:
            return card.display
    except UnknownAccount:
        pass
    return f"账号 {user_id}"


def _one_line(card: PersonCard) -> str:
    bits = f"{card.display}（{card.messages} 条）"
    tags = []
    if card.merged:
        tags.append(f"{len(card.accounts)} 个账号")
    if others := card.other_names:
        tags.append("称呼 " + "、".join(others[:3]))
    if tags:
        bits += "｜" + "；".join(tags)
    if card.summary:
        summary = card.summary
        bits += "｜" + (summary[:24] + "…" if len(summary) > 24 else summary)
    return bits


def _one_person(card: PersonCard) -> str:
    lines = [f"{card.display}（{card.messages} 条发言）"]
    if card.merged:
        lines.append(f"　账号：{len(card.accounts)} 个，已关联")
    if named := [name for name in card.names if name.text != card.display]:
        lines.append(
            "　称呼：" + "、".join(f"{name.text}（{name.confidence:.2f}）" for name in named)
        )
    if card.candidates:
        lines.append(
            "　未确认："
            + "、".join(f"{name.text}（{name.confidence:.2f}）" for name in card.candidates)
        )
    lines.append("　记录：")
    if not card.facts:
        lines.append("　　（暂无）")
    for fact in card.facts:
        if not fact.text:
            continue
        tail = "（人工）" if fact.manual else f"（{fact.confidence:.2f}）"
        lines.append(f"　　{fact.index}. {fact.text}{tail}")
    return "\n".join(lines)


async def _card_of(
    ctx: CommandContext,
    group_id: GroupId,
    user_id: str,
    *,
    all_linked: bool,
) -> PersonCard:
    try:
        card = (
            await ctx.directory.holder_card(group_id, user_id)
            if all_linked
            else await ctx.directory.account_card(group_id, user_id)
        )
    except UnknownAccount:
        await _finish(ctx, "本群还没有该账号的记录。")
    if not card.display or card.display in card.accounts:
        card = replace(card, display=await _name(ctx, group_id, user_id))
    return card


@_handler("/agree")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if _args(event, "agree") or event.mentions:
        raise UsageError("用法：/agree")
    if await agreement.accept(event.group_id, event.user_id):
        await _finish(ctx, "已记录你在本群同意用户协议。")
    await _finish(ctx, "你已在本群同意过用户协议。")


@_handler("/terms")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if _args(event, "terms") or event.mentions:
        raise UsageError("用法：/terms")
    await _finish(ctx, _fit(agreement.text(), gid=event.group_id))


@_handler("/help")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if event.mentions:
        raise UsageError("用法：/help [指令名]")
    tokens = _args(event, "help")
    if not tokens:
        await _finish(ctx, _fit(command_catalog.help_text(), gid=event.group_id))
    if len(tokens) != 1:
        raise UsageError("用法：/help [指令名]")
    command = command_catalog.find(tokens[0])
    if command is None:
        await _finish(ctx, f"没有「{tokens[0]}」这条指令。")
    await _finish(ctx, _fit(command_catalog.detail_text(command), gid=event.group_id))


@_handler("/who")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    all_linked, rest = _scope(_args(event, "who"))
    if rest:
        raise UsageError("用法：/who [--all] [@账号]")
    target = await _target(ctx, event, all_linked=all_linked)
    card = await _card_of(ctx, event.group_id, target, all_linked=all_linked)
    await _finish(ctx, _fit(_one_person(card), gid=event.group_id))


@_handler("/members")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if _args(event, "members") or event.mentions:
        raise UsageError("用法：/members")
    rows = await ctx.directory.roster(event.group_id)
    if not rows:
        await _finish(ctx, "本群还没有任何成员记录。")
    limit = config().default.commands.roster_max_entries
    head = f"本群 {len(rows)} 人有记录（发言数｜记录节选）："
    body = "\n".join("· " + _one_line(card) for card in rows[:limit])
    await _finish(ctx, _fit(head + "\n" + body, gid=event.group_id))


@_handler("/note")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    tokens = _args(event, "note")
    action = tokens.pop(0) if tokens and tokens[0] in {"set", "clear"} else "show"
    all_linked, rest = _scope(tokens)
    target = await _target(ctx, event, all_linked=all_linked)
    card = await _card_of(ctx, event.group_id, target, all_linked=all_linked)
    if action == "show":
        if rest:
            raise UsageError("用法：/note [--all] [@账号]")
        await _finish(
            ctx,
            f"{card.display} 的备注：\n{card.note}" if card.note else f"{card.display} 暂无备注。",
        )
    if action == "clear":
        if rest:
            raise UsageError("用法：/note clear [--all] [@账号]")
        await ctx.directory.note(event.group_id, target, "", all_linked=all_linked)
        await _finish(ctx, f"已清除 {card.display} 的备注。")
    if not rest:
        raise UsageError("用法：/note set [--all] [@账号] 内容")
    text = " ".join(rest)
    await ctx.directory.note(event.group_id, target, text, all_linked=all_linked)
    await _finish(ctx, f"已写入 {card.display} 的备注：\n{text}")


@_handler("/alias")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    tokens = _args(event, "alias")
    action = tokens.pop(0) if tokens and tokens[0] in {"add", "remove", "confidence"} else "list"
    all_linked, rest = _scope(tokens)
    target = await _target(ctx, event, all_linked=all_linked)
    card = await _card_of(ctx, event.group_id, target, all_linked=all_linked)
    if action == "list":
        if rest:
            raise UsageError("用法：/alias [--all] [@账号]")
        names = list(card.names) + list(card.candidates)
        if not names:
            await _finish(ctx, f"{card.display} 暂无记录在案的称呼。")
        lines = [
            f"· {name.text}（{name.confidence:.2f}，已确认）" for name in card.names
        ] + [
            f"· {name.text}（{name.confidence:.2f}，待确认线索）" for name in card.candidates
        ]
        await _finish(
            ctx, _fit(f"{card.display} 的称呼：\n" + "\n".join(lines), gid=event.group_id)
        )
    if action in {"add", "remove"}:
        if not rest:
            raise UsageError(f"用法：/alias {action} [--all] [@账号] 称呼")
        name = " ".join(rest)
        if action == "remove":
            if await ctx.directory.unname(event.group_id, target, name, all_linked=all_linked):
                await _finish(ctx, f"已撤销 {card.display} 的称呼「{name}」。")
            await _finish(ctx, f"{card.display} 名下没有「{name}」这个称呼。")
        try:
            await ctx.directory.name(event.group_id, target, name, all_linked=all_linked)
        except NameTaken as exc:
            await _finish(ctx, f"「{exc.text}」已经指向 {exc.holder}。")
        except ValueError as exc:
            await _finish(ctx, str(exc))
        await _finish(ctx, f"已登记：{card.display} 也叫「{name}」。")
    if len(rest) < 2:
        raise UsageError("用法：/alias confidence [--all] [@账号] 0到1 称呼")
    try:
        confidence = float(rest[0])
    except ValueError as exc:
        raise UsageError("置信度需为 0 到 1 的数字。") from exc
    if not 0 <= confidence <= 1:
        raise UsageError("置信度需为 0 到 1 的数字。")
    name = " ".join(rest[1:])
    try:
        result = await ctx.directory.set_confidence(
            event.group_id,
            target,
            name,
            confidence,
            all_linked=all_linked,
        )
    except NameTaken as exc:
        await _finish(ctx, f"「{exc.text}」已经指向 {exc.holder}。")
    state = (
        "已确认，可用于称呼和指认" if result.confidence >= CONFIRM_THRESHOLD else "仅作待确认线索"
    )
    await _finish(ctx, f"已设置「{result.text}」为 {result.confidence:.2f}（{state}）。")


@_handler("/forget")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    all_linked, rest = _scope(_args(event, "forget"))
    if len(rest) != 1 or not rest[0].isdecimal() or int(rest[0]) < 1:
        raise UsageError("用法：/forget [--all] [@账号] 编号")
    target = await _target(ctx, event, all_linked=all_linked)
    dropped = await ctx.directory.forget(
        event.group_id,
        target,
        int(rest[0]),
        all_linked=all_linked,
    )
    if dropped is None:
        await _finish(ctx, f"没有编号为 {rest[0]} 的记录。")
    await _finish(ctx, f"已删除：{dropped.text}")


@_handler("/card")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if event.mentions:
        raise UsageError("用法：/card 或 /card forget 编号")
    tokens = _args(event, "card")
    if tokens:
        if len(tokens) != 2 or tokens[0] != "forget" or not tokens[1].isdecimal():
            raise UsageError("用法：/card forget 编号")
        await _owner_action(ctx)
        dropped = await ctx.directory.forget_group_fact(event.group_id, int(tokens[1]))
        if dropped is None:
            await _finish(ctx, f"没有编号为 {tokens[1]} 的群记录。")
        await _finish(ctx, f"已删除：{dropped.text}")
    _cfg, persona = config().for_group(event.group_id)
    learned = await ctx.directory.group_facts(event.group_id)
    parts = []
    if fixed := persona.group_knowledge.strip():
        parts.append("固定资料（写在人设文件里）：\n" + fixed)
    parts.append(
        "自动归纳：\n"
        + "\n".join(f"　{fact.index}. {fact.text}（{fact.confidence:.2f}）" for fact in learned)
        if learned
        else "自动归纳：（暂无）"
    )
    await _finish(ctx, _fit("\n\n".join(parts), gid=event.group_id))


@_handler("/link")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    tokens = _args(event, "link")
    mentions = [str(account) for account in event.mentions]
    if tokens and tokens[0] in {"confirm", "cancel"}:
        if len(tokens) != 2 or mentions:
            raise UsageError(f"用法：/link {tokens[0]} 验证码")
        code = tokens[1]
        try:
            if tokens[0] == "confirm":
                await ctx.links.confirm(
                    group_id=event.group_id,
                    actor_user_id=str(event.user_id),
                    code=code,
                    confirmed_event_id=event.raw_event_id,
                )
                await _finish(ctx, "已确认，两个账号集合现已关联。")
            cancelled = await ctx.links.cancel(
                group_id=event.group_id,
                actor_user_id=str(event.user_id),
                code=code,
            )
            await _finish(ctx, "已取消。" if cancelled else "没有可取消的验证。")
        except LinkChallengeError as exc:
            await _finish(ctx, str(exc))
    if tokens or len(mentions) != 1:
        raise UsageError("用法：/link @另一个账号")
    target = mentions[0]
    if target == str(event.self_id):
        await _finish(ctx, "不能关联机器人账号。")
    if not await agreement.ok(event.group_id, AccountId(target)):
        await _finish(ctx, "目标账号需要先在本群发送 /agree。")
    try:
        _challenge, code = await ctx.links.issue(
            group_id=event.group_id,
            initiator_user_id=str(event.user_id),
            target_user_id=target,
            created_event_id=event.raw_event_id,
        )
    except (LinkChallengeError, UnknownAccount) as exc:
        await _finish(ctx, str(exc))
    ttl = config().default.identity_link.challenge_ttl_sec
    await _finish(ctx, f"请目标账号在 {ttl} 秒内发送：/link confirm {code}")


@_handler("/unlink")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if _args(event, "unlink") or event.mentions:
        raise UsageError("用法：/unlink")
    try:
        await ctx.directory.split(str(event.user_id))
    except NotMerged as exc:
        await _finish(ctx, exc.message)
    await _finish(ctx, "已将当前账号从关联集合中剥离。")


@_handler("/merge")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if _args(event, "merge"):
        raise UsageError("用法：/merge @账号A @账号B")
    left, right = _mentions(event, exact=2)
    if str(event.self_id) in {left, right}:
        await _finish(ctx, "不能合并机器人账号。")
    try:
        changed = await ctx.directory.merge(left, right)
    except UnknownAccount as exc:
        await _finish(ctx, f"账号 {exc.user_id} 没有记录，无法合并。")
    await _finish(ctx, "已合并。" if changed else "这两个账号已经关联。")


@_handler("/split")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if _args(event, "split"):
        raise UsageError("用法：/split @账号")
    [target] = _mentions(event, exact=1)
    if target == str(event.self_id):
        await _finish(ctx, "不能拆分机器人账号。")
    try:
        await ctx.directory.split(target)
    except UnknownAccount as exc:
        await _finish(ctx, f"账号 {exc.user_id} 没有记录，无法拆分。")
    except NotMerged as exc:
        await _finish(ctx, exc.message)
    await _finish(ctx, "已剥离该账号，其余关联账号保持不变。")


@_handler("/block")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    tokens = _args(event, "block")
    mentions = _mentions(event)
    if not tokens and not mentions:
        rules = await repo.block_rules(event.group_id)
        if not rules:
            await _finish(ctx, "本群没有屏蔽规则。")
        lines = ["本群屏蔽规则："]
        for rule in rules:
            if rule["user_id"]:
                label = await _name(ctx, event.group_id, rule["user_id"])
            else:
                accounts = await ctx.directory.accounts_of_holder(rule["entity_id"])
                label = "关联账号 " + "、".join(account.platform_user_id for account in accounts)
            until = f"（至 {fmt_when(rule['blocked_until'])}）" if rule["blocked_until"] else ""
            lines.append(f"· {label}{until}")
        await _finish(ctx, _fit("\n".join(lines), gid=event.group_id))
    if not tokens or tokens[0] not in {"add", "remove"}:
        raise UsageError("用法：/block add|remove [--all] @账号 [时长]")
    action = tokens.pop(0)
    all_linked, rest = _scope(tokens)
    if len(mentions) != 1:
        raise UsageError("需要准确 @ 1 个账号。")
    target = mentions[0]
    if action == "remove":
        if rest:
            raise UsageError("用法：/block remove [--all] @账号")
        account = await ctx.directory.account(target)
        changed = (
            await repo.unblock_holder(event.group_id, account.entity_id)
            if all_linked
            else await repo.unblock(event.group_id, target)
        )
        await _finish(ctx, "已解除屏蔽。" if changed else "没有对应的屏蔽规则。")
    if len(rest) > 1:
        raise UsageError("用法：/block add [--all] @账号 [30m|12h|3d]")
    cfg = config().default
    accounts = await ctx.directory.linked_account_ids(target)
    if target == str(event.self_id) or any(
        perms.is_owner(linked, cfg.owners) for linked in accounts
    ):
        await _finish(ctx, "不能屏蔽机器人或 owner。")
    account = await ctx.directory.account(target)
    until = None
    if rest:
        span = parse_duration(rest[0])
        if span is None:
            raise UsageError("时长格式无效，支持 m、h、d。")
        until = now_local() + span
    if all_linked:
        await repo.block_holder(event.group_id, account.entity_id, until=until)
    else:
        await repo.block(event.group_id, target, until=until)
    lapse = f"，{fmt_when(until)} 自动解除" if until else ""
    await _finish(ctx, f"已屏蔽{lapse}。")


@_handler("/mute")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if event.mentions:
        raise UsageError("用法：/mute [status|on|off]")
    tokens = _args(event, "mute")
    action = tokens[0] if tokens else "status"
    if len(tokens) > 1 or action not in {"status", "on", "off"}:
        raise UsageError("用法：/mute [status|on|off]")
    state = await ctx.registry.get(event.group_id)
    if action == "status":
        await _finish(ctx, "本群已静音。" if state.muted else "本群未静音。")
    state.muted = action == "on"
    await state.persist()
    await _finish(ctx, "已静音。" if state.muted else "已解除静音。")


def _calls(rows: list[dict], kind: str) -> int:
    return sum(int(row["calls"]) for row in rows if row["kind"] == kind)


@_handler("/stats")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if event.mentions:
        raise UsageError("用法：/stats [global]")
    tokens = _args(event, "stats")
    if tokens == ["global"]:
        await _owner_action(ctx)
        cfg = config().default
        rows = await repo.day_breakdown(today_local())
        backlog = sum(
            [
                await ExtractionRepository().unconsumed_count(group_id)
                for group_id in await repo.groups_with_state()
            ],
            0,
        )
        month_search = await repo.month_calls(Kind.SEARCH, ctx.providers.search.name)
        lines = [
            "全局用量（所有群合计）",
            f"预算　　¥{await BUDGET.spent_today():.3f} / ¥{cfg.budget.daily_cny_cap:.2f}",
            f"回复　　{_calls(rows, Kind.REPLY)} 次",
            (
                f"搜索　　今日 {_calls(rows, Kind.SEARCH)} 次　本月 "
                f"{month_search}/{cfg.capabilities.search.monthly_quota}"
            ),
            f"记忆　　待归纳 {backlog} 条",
            f"媒体　　识图 {_calls(rows, Kind.VISION)} 次　转写 {_calls(rows, Kind.ASR)} 次",
        ]
        if cache := hit_split(rows):
            lines.append(f"缓存　　命中率 {cache}")
        if errors.count():
            lines.append(f"异常　　{errors.count()} 条")
        await _finish(ctx, "\n".join(lines))
    if tokens:
        raise UsageError("用法：/stats [global]")
    rows = await repo.day_breakdown(today_local(), event.group_id)
    _cfg, persona = config().for_group(event.group_id)
    state = await ctx.registry.get(event.group_id)
    rules = await repo.block_rules(event.group_id)
    lines = [
        f"本群用量（{event.group_id}）",
        f"人设　　{persona.name}",
        f"花费　　¥{sum(float(row['cny']) for row in rows):.3f}",
        f"回复　　{_calls(rows, Kind.REPLY)} 次",
        f"记忆　　待归纳 {await ExtractionRepository().unconsumed_count(event.group_id)} 条",
        f"状态　　{'已静音' if state.muted else '正常'}",
    ]
    if rules:
        lines.append(f"屏蔽　　{len(rules)} 条规则")
    await _finish(ctx, "\n".join(lines))


@_handler("/top")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if event.mentions:
        raise UsageError("用法：/top [--all] [数量]")
    all_linked, rest = _scope(_args(event, "top"))
    if len(rest) > 1 or (rest and (not rest[0].isdecimal() or int(rest[0]) < 1)):
        raise UsageError("用法：/top [--all] [数量]")
    cfg = config().default.commands
    count = min(int(rest[0]), cfg.top_max_entries) if rest else cfg.top_default_entries
    rows = await repo.top_spenders(event.group_id, k=count, all_linked=all_linked)
    if not rows:
        await _finish(ctx, "本群本月尚无可归因的花费。")
    lines = ["本群本月花费排行"]
    for index, row in enumerate(rows, 1):
        accounts = list(row["accounts"])
        name = await _name(ctx, event.group_id, accounts[0])
        tag = f"（{len(accounts)} 个账号）" if all_linked and len(accounts) > 1 else ""
        lines.append(f"{index}. {name}{tag}　¥{float(row['cny']):.3f}　{int(row['calls'])} 次")
    await _finish(ctx, _fit("\n".join(lines), gid=event.group_id))


@_handler("/debug")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if event.mentions:
        raise UsageError("用法：/debug [status|start 轮数|stop]")
    tokens = _args(event, "debug")
    action = tokens[0] if tokens else "status"
    maximum = config().default.diagnostics.debug_max_rounds
    if action == "status" and len(tokens) <= 1:
        left = debug.armed()
        await _finish(ctx, f"捕获中：还剩 {left} 轮。" if left else "未在捕获。")
    if action == "stop" and len(tokens) == 1:
        debug.arm(0, max_rounds=maximum)
        await _finish(ctx, "已关闭捕获。")
    if action != "start" or len(tokens) != 2 or not tokens[1].isdecimal():
        raise UsageError(f"用法：/debug start 轮数（最多 {maximum}），或 /debug stop")
    took = debug.arm(int(tokens[1]), max_rounds=maximum)
    await _finish(ctx, f"已开启：接下来 {took} 轮写入 logs/debug/。")


@_handler("/log")
async def _(ctx: CommandContext, event: CommandRequest) -> None:
    if event.mentions:
        raise UsageError("用法：/log [行数]")
    tokens = _args(event, "log")
    cfg = config().default.diagnostics
    if len(tokens) > 1 or (tokens and not tokens[0].isdecimal()):
        raise UsageError(f"用法：/log [行数]，最多 {cfg.log_tail_max_lines}")
    count = int(tokens[0]) if tokens else cfg.log_tail_default_lines
    if not 1 <= count <= cfg.log_tail_max_lines:
        raise UsageError(f"行数需为 1 到 {cfg.log_tail_max_lines}。")
    path = Path(os.getenv("LOG_DIR", "/app/logs")) / "qqbot.log"
    try:
        with path.open("rb") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            handle.seek(max(0, size - cfg.log_tail_scan_bytes))
            tail = handle.read().decode("utf-8", "replace")
    except OSError as exc:
        await _finish(ctx, f"无法读取日志文件：{exc}")
    lines = [line for line in tail.splitlines() if line.strip()][-count:]
    await _finish(
        ctx,
        _fit("\n".join(lines) if lines else "日志为空。", head=False, gid=event.group_id),
    )


if set(_HANDLERS) != set(command_catalog.PREFIXES):
    missing = sorted(set(command_catalog.PREFIXES) - set(_HANDLERS))
    extra = sorted(set(_HANDLERS) - set(command_catalog.PREFIXES))
    raise RuntimeError(f"command handler registry mismatch: missing={missing}, extra={extra}")


def registered_commands() -> tuple[str, ...]:
    return tuple(spec.name for spec in command_catalog.CATALOG if spec.name in _HANDLERS)
