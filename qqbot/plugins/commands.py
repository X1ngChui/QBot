"""The ops commands: the owner's console, with a member-facing edge.

Human intervention is meant to be one thing only: read the daily report, change what is
wrong. These commands exist to make that loop short. Members hold the slice the catalog
flags for them - their own record and names, the read-only surfaces, /agree - once they
have accepted the user agreement; everything else answers them with silence.

Almost nothing is decided here. Whether a person may run a command is decided in
core.perms, what each command is is declared in core.command_catalog, and what it does to
memory is done by services.Directory. What is left is parsing an argument and formatting
an answer - because this module cannot be imported without a live NoneBot runtime
(on_command runs at import time) and therefore cannot be tested, anything worth testing
is deliberately somewhere else.

One rule runs through all of it: a person is named by @ and no other way. A typed name is
a guess at an account - two members can display one nickname, a nickname changes, and a
name that matched yesterday quietly matches somebody else today. An @ segment carries the
account id outright.
"""

from __future__ import annotations

import logging
import os
from dataclasses import replace
from datetime import datetime
from pathlib import Path

from nonebot import on_command
from nonebot.adapters.onebot.v11 import GroupMessageEvent
from nonebot.matcher import Matcher, current_bot
from nonebot.rule import Rule
from pydantic import ValidationError

from ..core import agreement, command_catalog, debug, errors, perms
from ..core.budget import BUDGET, hit_split
from ..core.members import MEMBERS
from ..core.nickname import register as register_nicknames
from ..core.retrieval import directory
from ..core.state import REGISTRY
from ..db import repo
from ..domain.identity.alias import CONFIRM_THRESHOLD
from ..providers import Kind, providers
from ..repositories.event import EventRepository
from ..services import NameTaken, NotMerged, PersonCard, UnknownAccount
from ..settings import config, reload_config
from ..util import defang, fmt_when, now_local, parse_duration, today_local, why

log = logging.getLogger("qqbot.cmd")

#: Command output has to survive the same send-side truncation as a reply, and a silently
#: cut-off answer is worse than an explicitly shortened one.
OUT_MARGIN = 200
LOG_LINES_DEFAULT = 15
LOG_LINES_MAX = 60
#: Enough tail to hold LOG_LINES_MAX lines of any plausible length.
LOG_TAIL_BYTES = 64 * 1024
#: How many people the bare /who roster lists in full before it is trimmed to fit.
ROSTER_MAX = 60

#: Prefix that marks an argument as a removal rather than an addition.
DROP = "-"


def _fit(text: str, *, head: bool = True, gid: str | None = None) -> str:
    """Trim to what a single QQ message can carry, saying so rather than just stopping.

    `gid` selects the group's send-tool text bound; without it the global default applies.
    Sizing by the wrong group's limit re-creates the silent send-side truncation this
    function exists to prevent.
    """
    cfg = config().for_group(gid)[0] if gid else config().default
    # Never below one: at zero the tail slice text[-0:] is the whole text, and a
    # negative limit silently drops the end instead of marking it.
    limit = max(1, cfg.tools.send_messages.max_text_chars_per_message - OUT_MARGIN)
    if len(text) <= limit:
        return text
    kept = text[:limit] if head else text[-limit:]
    return (kept + "\n…（已截断）") if head else ("…（已截断）\n" + kept)


async def _gate(matcher: Matcher, event: GroupMessageEvent,
                *, global_only: bool = False, self_serve: bool = False,
                open_to_members: bool = False,
                pre_agreement: bool = False) -> bool:
    """Who is calling: True for the owner, False for a member allowed through.

    A member gets through on `self_serve` - the catalog-flagged commands whose
    handlers then narrow every operation to the caller's own person (the
    handler's job, via `_own_accounts`) - or on `open_to_members`, the
    read-only surfaces a member sees whole. Everyone
    else is stopped here with silence rather than a refusal, deliberately: to
    anyone who cannot run a command it does not exist, and answering "you are
    not allowed" is how they find out that it does.

    Reads the one global owner list. `global_only` marks commands whose effects span
    every group; members stay barred from them even if another catalogue flag is set.

    event.sender.role is deliberately not consulted. Running the QQ group is not running
    the bot.
    """
    gid, uid = str(event.group_id), str(event.user_id)
    # The decision table is perms.decide, a pure function tested on its own; this
    # wrapper only supplies the owner lists and, when the verdict asks for it,
    # whether the member has accepted the user agreement.
    verdict = perms.decide(
        uid, owners=config().default.owners, global_only=global_only,
        self_serve=self_serve, open_to_members=open_to_members,
        pre_agreement=pre_agreement)
    if verdict is perms.Verdict.OWNER:
        return True
    if verdict is perms.Verdict.MEMBER or (
            verdict is perms.Verdict.MEMBER_IF_AGREED and await agreement.ok(gid, uid)):
        return False
    await matcher.finish()
    return False  # unreachable; finish() raises


async def _name(gid: str, user_id: str) -> str:
    """How an answer names an account: the current group card, else the name the
    archive knows them by, else the account number said as such. Answers name
    people the way the group does; a bare number is what nobody recognises."""
    bot = current_bot.get()
    if name := await MEMBERS.name_of(bot, gid, user_id):
        return name
    try:
        card = await directory().person(int(gid), user_id)
        if card.display and card.display not in card.accounts:
            return card.display
    except UnknownAccount:
        pass
    return f"账号 {user_id}"


async def _own_accounts(event: GroupMessageEvent) -> list[str]:
    """Every account of the person speaking - "yourself" means the person, so
    a merged alt operates its main's record, same as /block treats them."""
    return await directory().accounts_of_person(str(event.user_id))


async def _finish(matcher: Matcher, message: str) -> None:
    """Answer a command by quoting and addressing its sender.

    The resulting bot-authored event returns through the ordinary gateway, which
    records exactly what QQ displayed rather than reconstructing it here.
    """
    await matcher.send(message, at_sender=True, reply_message=True)
    await matcher.finish()


async def _not_self(event: GroupMessageEvent) -> bool:
    """Let self-authored command-shaped text continue to the observation path."""

    return str(event.user_id) != str(event.self_id)


# Every name is registered in its own right, and every registration demands a break
# after the name. NoneBot resolves a message against the longest registered prefix,
# so a name nobody registered would otherwise arrive as the shorter command it
# starts with, carrying the rest as its argument - /topology as /top, /cards as
# /card, /whoami as /who - and block=True would keep it from the chat path as well.
# With force_whitespace such a message matches no command at all.
_CMD = {
    "block": True,
    "priority": 1,
    "force_whitespace": True,
    "rule": Rule(_not_self),
}

agree_cmd = on_command("agree", **_CMD)
terms_cmd = on_command("terms", **_CMD)
reload_cmd = on_command("reload", **_CMD)
mute_cmd = on_command("mute", **_CMD)
block_cmd = on_command("block", **_CMD)
unblock_cmd = on_command("unblock", **_CMD)
unmute_cmd = on_command("unmute", **_CMD)
stats_cmd = on_command("stats", **_CMD)
groupstats_cmd = on_command("groupstats", **_CMD)
top_cmd = on_command("top", **_CMD)
card_cmd = on_command("card", **_CMD)
who_cmd = on_command("who", **_CMD)
note_cmd = on_command("note", **_CMD)
alias_cmd = on_command("alias", **_CMD)
forget_cmd = on_command("forget", **_CMD)
merge_cmd = on_command("merge", **_CMD)
split_cmd = on_command("split", **_CMD)
log_cmd = on_command("log", **_CMD)
debug_cmd = on_command("debug", **_CMD)
help_cmd = on_command("help", **_CMD)


# -- argument parsing -------------------------------------------------------


def _strip_cmd(text: str, name: str) -> str:
    """The typed argument after the command name, defanged: it is member text,
    and what a note or an alias says is stored, echoed into the window and
    rendered into the prompt - a reserved bracket typed here would otherwise
    read as a system marker on every later turn."""
    text = defang(text.strip())
    for cmd in (f"/{name}", name):
        if text.startswith(cmd):
            return text[len(cmd):].strip()
    return text


def _mentioned(event: GroupMessageEvent) -> list[str]:
    """Accounts @-ed in this command, in order.

    An @ segment carries the account id outright, so it names somebody exactly - no
    matching, no ambiguity when two people display the same name, and it works for a
    nickname nobody can type. get_plaintext() drops these, so they have to be read off
    the raw message.
    """
    return [
        str(seg.data.get("qq"))
        for seg in event.message
        if seg.type == "at" and str(seg.data.get("qq", "")).isdigit()
    ]


# -- rendering --------------------------------------------------------------


def _one_line(card: PersonCard) -> str:
    """One person on one line, for the whole-group listing."""
    bits = f"{card.display}（{card.messages} 条）"
    tags = []
    if card.merged:
        tags.append(f"{len(card.accounts)} 个账号")
    if others := card.other_names:
        tags.append("称呼 " + "、".join(others[:3]))
    if tags:
        bits += "｜" + "；".join(tags)
    summary = card.summary
    if summary:
        bits += "｜" + (summary[:24] + "…" if len(summary) > 24 else summary)
    return bits


def _one_person(card: PersonCard) -> str:
    """One person in full, with the numbers /forget takes and what each entry rests on."""
    lines = [f"{card.display}（{card.messages} 条发言）"]
    if card.merged:
        lines.append(f"　账号：{len(card.accounts)} 个，已合并")
    if named := [n for n in card.names if n.text != card.display]:
        lines.append("　称呼：" + "、".join(
            f"{n.text}（{n.confidence:.2f}）" for n in named))
    if card.candidates:
        lines.append("　未确认：" + "、".join(
            f"{n.text}（{n.confidence:.2f}）" for n in card.candidates))
    if not card.facts:
        lines.append("　记录：（暂无）")
        return "\n".join(lines)

    lines.append("　记录：")
    for f in card.facts:
        if not f.text:
            continue
        tail = "（人工）" if f.manual else f"（{f.confidence:.2f}）"
        lines.append(f"　　{f.index}. {f.text}{tail}")
    return "\n".join(lines)


async def _card_of(matcher: Matcher, group_id: int, user_id: str) -> PersonCard:
    """The person behind an @, or a refusal saying which of the two things went wrong.

    A card whose display is a bare account number (an account only ever @-ed
    here, so no group card was ever filed) is renamed the way every other
    answer names people.
    """
    try:
        card = await directory().person(group_id, user_id)
    except UnknownAccount:
        await _finish(matcher, "本群还没有该成员的记录。")
    if not card.display or card.display in card.accounts:
        card = replace(card, display=await _name(str(group_id), user_id))
    return card


# -- configuration ----------------------------------------------------------


@agree_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Record that the speaker accepts the user agreement.

    The one command whose subject is always the speaker themselves - there is
    no target to narrow - and the one that must work before consent, or nobody
    could ever consent.
    """
    await _gate(matcher, event, self_serve=True, pre_agreement=True)
    if await agreement.accept(str(event.group_id), str(event.user_id)):
        await _finish(matcher, "已记录你在本群同意用户协议。")
    await _finish(matcher, "你已在本群同意过用户协议，无需重复发送。")


@terms_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Show the user agreement in full.

    Answers before consent, necessarily: the gate points the unconsenting
    here, and a viewer that itself required consent would be a locked door
    in front of the thing to be read.
    """
    await _gate(matcher, event, self_serve=True, pre_agreement=True)
    await _finish(matcher, _fit(agreement.text(), gid=str(event.group_id)))


def _validation_summary(e: ValidationError) -> str:
    """One line per failing key - the key's path and what is wrong with it, never
    the value that failed.

    pydantic's own rendering prints the offending input next to each error, and for
    a missing or unknown key that input is the whole parent block: a proxy URL with
    its credentials, the owner list, every endpoint. This text is posted to the
    group, so it carries what the owner needs to fix the file and nothing else.
    """
    lines = []
    for err in e.errors(include_input=False, include_url=False):
        loc = ".".join(str(p) for p in err["loc"]) or "(root)"
        lines.append(f"{loc}: {err['msg']}")
    return "\n".join(lines)


@reload_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    # The reload lands on every group at once. The same scope marker is used for
    # /debug and /log, whose tap and log tail are also process-global.
    await _gate(matcher, event, global_only=True)
    try:
        bundle = reload_config()
    except ValidationError as e:
        log.warning("config reload rejected: %s", why(e))
        # A multi-error summary can outgrow a QQ message; oversize is refused
        # whole, and the one command that reports what broke must not answer
        # with silence.
        await _finish(matcher, _fit("配置未通过校验，本次重载未生效：\n"
                                  + _validation_summary(e),
                                  gid=str(event.group_id)))
        return
    except Exception as e:
        # Anything short of validation - an unreadable file, malformed YAML - has
        # no input to leak and is small enough to quote as is.
        log.warning("config reload rejected: %s", why(e))
        await _finish(matcher, _fit(f"配置读取失败，本次重载未生效：{why(e)}",
                                  gid=str(event.group_id)))
        return
    for gid in list(bundle.personas):
        cfg, _ = bundle.for_group(gid)
        register_nicknames(cfg.trigger.nicknames)
    register_nicknames(bundle.default.trigger.nicknames)
    # A successful reload means every accepted field is live. Restart-scoped
    # changes are rejected above with the exact paths that require a restart.
    await _finish(matcher, f"配置已重载：{len(bundle.personas)} 份人设。")


@block_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Withhold replies from an account in this group.

    Only replies: the account's messages still arrive, archive and feed memory,
    so the window stays coherent around them - by owner decision, context
    continuity outweighs keeping a nuisance out of the bot's memory.
    """
    await _gate(matcher, event)
    gid = str(event.group_id)
    cfg, _ = config().for_group(gid)
    st = await REGISTRY.get(gid)
    at = _mentioned(event)
    if not at:
        # Filtered at display time: a lapsed timed entry waits for its account's
        # next message to be swept, and a listing must not show it as blocked.
        live = {u: t for u, t in st.blocked.items()
                if t is None or t > now_local()}
        if not live:
            await _finish(matcher, "本群没有屏蔽任何人。用法：/block @某人")
        named = [(await _name(gid, uid), t) for uid, t in live.items()]
        shown = "、".join(
            name + (f"（至 {fmt_when(t)}）" if t else "")
            for name, t in sorted(named, key=lambda p: p[0]))
        await _finish(matcher, _fit(f"本群屏蔽名单（{len(live)} 个账号）：{shown}",
                                  gid=gid))
    target = at[0]
    arg = _strip_cmd(event.get_plaintext(), "block").strip()
    until = None
    if arg:
        span = parse_duration(arg)
        if span is None:
            await _finish(matcher,
                          "时长格式无效。示例：/block @某人 3d（单位：m 分钟、h 小时、d 天）")
        until = now_local() + span
    if perms.is_owner(target, cfg.owners):
        await _finish(matcher, "不能屏蔽拥有者。")
    if str(event.self_id) == target:
        await _finish(matcher, "不能屏蔽机器人自己。")
    # A merge made several accounts one person, and blocking the account that was
    # @-ed while the alt keeps talking is not blocking anybody.
    accounts = await directory().accounts_of_person(target)
    if any(perms.is_owner(a, cfg.owners) for a in accounts):
        await _finish(matcher, "不能屏蔽拥有者。")
    st.blocked.update(dict.fromkeys(accounts, until))
    await repo.block(int(gid), accounts, until=until)
    log.info("group %s: person %s blocked by owner (%d account(s)%s)",
             gid, target, len(accounts),
             f", until {until:%m-%d %H:%M}" if until else "")
    extra = f"（同一人的 {len(accounts)} 个账号）" if len(accounts) > 1 else ""
    lapse = f"，{fmt_when(until)} 自动解除" if until else ""
    await _finish(matcher, f"已屏蔽 {await _name(gid, target)}{extra}{lapse}。")


@unblock_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    await _gate(matcher, event)
    gid = str(event.group_id)
    st = await REGISTRY.get(gid)
    at = _mentioned(event)
    if not at:
        await _finish(matcher, "请 @ 要解除屏蔽的成员。用法：/unblock @某人")
    target = at[0]
    accounts = await directory().accounts_of_person(target)
    if not any(a in st.blocked for a in accounts):
        await _finish(matcher, f"{await _name(gid, target)} 不在本群的屏蔽名单里。")
    # Lifted for the whole person, like it was applied: leaving one account of a
    # merged pair blocked would look like the command silently failed.
    for a in accounts:
        st.blocked.pop(a, None)
    await repo.unblock(int(gid), accounts)
    log.info("group %s: person %s unblocked by owner (%d account(s))",
             gid, target, len(accounts))
    extra = f"（同一人的 {len(accounts)} 个账号）" if len(accounts) > 1 else ""
    await _finish(matcher, f"已解除对 {await _name(gid, target)}{extra} 的屏蔽。")


@mute_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    await _gate(matcher, event)
    st = await REGISTRY.get(str(event.group_id))
    st.muted = True
    await st.persist()
    await _finish(matcher, "已静音。")


@unmute_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    await _gate(matcher, event)
    st = await REGISTRY.get(str(event.group_id))
    st.muted = False
    await st.persist()
    await _finish(matcher, "已解除静音。")


# -- usage ------------------------------------------------------------------


def _calls(rows: list[dict], kind: str) -> int:
    return sum(int(r["calls"]) for r in rows if r["kind"] == kind)


@stats_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Everything the groups share: one budget, one search quota, one cache.

    Deliberately carries no per-group number - a global call count reads as this group's
    the moment it sits next to a per-group cap, so those live in /groupstats instead.
    """
    await _gate(matcher, event, open_to_members=True)
    day = today_local()
    cfg = config().default

    rows = await repo.day_breakdown(day)
    spent = await BUDGET.spent_today()
    # The month is the search allowance's period, so the day count alone reads as
    # "plenty left" right up until the refusal. Metered per backend name, the same way
    # the backend meters itself.
    month_search = await repo.month_calls(Kind.SEARCH, providers().search.name)

    # Extraction is one nightly drain now, so a daytime call count reads zero and
    # says nothing. What an owner can act on is the backlog waiting for tonight -
    # the number that climbs when the group outruns the drain, or when the worker
    # is stuck (the daily report watches the queue side of that).
    backlog = 0
    for g in await repo.groups_with_state():
        n, _newest = await EventRepository().unread_since_extract(int(g))
        backlog += n
    lines = [
        "全局用量（所有群合计）",
        f"预算　　¥{spent:.3f} / ¥{cfg.budget.daily_cny_cap:.2f}",
        f"回复　　{_calls(rows, Kind.REPLY)} 次",
        f"搜索　　今日 {_calls(rows, Kind.SEARCH)} 次　本月 "
        f"{month_search}/{cfg.capabilities.search.monthly_quota}",
        f"记忆　　待归纳 {backlog} 条",
        f"媒体　　识图 {_calls(rows, Kind.VISION)} 次　转写 {_calls(rows, Kind.ASR)} 次",
    ]
    if cache := hit_split(rows):
        lines.append(f"缓存　　命中率 {cache}")
    if errors.count():
        lines.append(f"异常　　{errors.count()} 条，详见每日报告")
    await _finish(matcher, "\n".join(lines))


@top_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """This month's costliest people in this group, merged accounts counted as one.

    Person-level by construction: the ledger stores the causing account, and
    repo.top_spenders aggregates through identity_account at query time - so a
    /merge issued after the spending still pulls the history together.
    """
    await _gate(matcher, event, open_to_members=True)
    gid = str(event.group_id)
    arg = _strip_cmd(event.get_plaintext(), "top")
    k = min(int(arg), 20) if arg.isdecimal() and int(arg) > 0 else 5
    rows = await repo.top_spenders(int(gid), k=k)
    if not rows:
        await _finish(matcher, "本群本月尚无可归因的花费。")
    lines = ["本群本月花费排行"]
    for i, r in enumerate(rows, 1):
        accounts = list(r["accounts"])
        name = await _name(gid, accounts[0])
        tag = f"（{len(accounts)} 个账号）" if len(accounts) > 1 else ""
        lines.append(f"{i}. {name}{tag}　¥{float(r['cny']):.3f}　{int(r['calls'])} 次")
    await _finish(matcher, _fit("\n".join(lines), gid=gid))


@groupstats_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """This group's own numbers, including the two caps that are genuinely per-group."""
    await _gate(matcher, event, open_to_members=True)
    day = today_local()
    gid = str(event.group_id)
    _cfg, persona = config().for_group(gid)
    st = await REGISTRY.get(gid)

    rows = await repo.day_breakdown(day, gid)
    spent = sum(float(r["cny"]) for r in rows)

    unread, _newest = await EventRepository().unread_since_extract(int(gid))
    lines = [
        f"本群用量（{gid}）",
        f"人设　　{persona.name}",
        f"花费　　¥{spent:.3f}",
        f"回复　　{_calls(rows, Kind.REPLY)} 次",
        f"记忆　　待归纳 {unread} 条",
        f"状态　　{'已静音' if st.muted else '正常'}",
    ]
    blocked_live = sum(1 for t in st.blocked.values()
                       if t is None or t > now_local())
    if blocked_live:
        lines.append(f"屏蔽　　{blocked_live} 个账号")
    await _finish(matcher, "\n".join(lines))


# -- group knowledge --------------------------------------------------------


@card_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """What the bot has worked out about the group itself.

    These are facts like any other - the group is an entity, and what it is for and what
    its words mean are facts about it. That is why they are numbered here and deleted
    with the same /forget that deletes a fact about a person, and why the extraction pass
    that learns everything else is what refreshes them.
    """
    await _gate(matcher, event, open_to_members=True)
    gid = str(event.group_id)
    _cfg, persona = config().for_group(gid)
    learned = await directory().group_facts(int(gid))
    fixed = persona.group_knowledge.strip()

    parts = []
    if fixed:
        parts.append("固定资料（写在人设文件里）：\n" + fixed)
    if learned:
        parts.append("自动归纳：\n" + "\n".join(
            f"　{f.index}. {f.text}（{f.confidence:.2f}）" for f in learned))
    else:
        parts.append("自动归纳：（暂无）")
    await _finish(matcher, _fit("\n\n".join(parts), gid=str(gid)))


# -- what is known about people ---------------------------------------------


@who_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """The whole group, or one person by @.

    A person is named by @ and no other way. A typed name is a guess at an account: two
    members can display one nickname, a nickname changes, and a name that matched
    yesterday quietly matches somebody else today - so a lookup could hand back the wrong
    person's record and read as though it were right.

    A member gets exactly one shape of it: their own record, bare or @-ing one
    of their own accounts. Anything else - another person, a typed name, the
    roster - stays silent, the same silence an ungranted command gives.
    """
    owner = await _gate(matcher, event, self_serve=True)
    gid = int(event.group_id)

    if not owner:
        mine = await _own_accounts(event)
        if (any(q not in mine for q in _mentioned(event))
                or _strip_cmd(event.get_plaintext(), "who").strip()):
            await matcher.finish()
        card = await _card_of(matcher, gid, str(event.user_id))
        await _finish(matcher, _fit(_one_person(card), gid=str(gid)))

    if at := _mentioned(event):
        cards = []
        for q in at[:3]:
            try:
                cards.append(await directory().person(gid, q))
            except UnknownAccount:
                continue
        if not cards:
            await _finish(matcher, "这些成员在本群还没有记录。")
        await _finish(matcher, _fit("\n".join(_one_person(c) for c in cards),
                                  gid=str(gid)))

    if _strip_cmd(event.get_plaintext(), "who"):
        await _finish(matcher,
            "请用 @ 指定成员：/who @某人。\n"
            "昵称可能重复或更改，@ 才能准确指向账号。"
        )

    rows = await directory().roster(gid)
    if not rows:
        await _finish(matcher, "本群还没有任何成员记录。")
    head = f"本群 {len(rows)} 人有记录（发言数｜记录节选）："
    body = "\n".join("· " + _one_line(c) for c in rows[:ROSTER_MAX])
    await _finish(matcher, _fit(head + "\n" + body, gid=str(gid)))


@note_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Write the hand-written half of a record.

    Stored as an ordinary fact under its own predicate, so extraction cannot overwrite it
    and the prompt reads it through the same path as everything else.

    What goes in here is handed to the model as established fact, which is why writing
    about *somebody else* stays the owner's alone: it is a way to put words in the bot's
    mouth about them. A member writes only their own note - by owner decision it carries
    the same established-fact weight as the owner's hand.
    """
    owner = await _gate(matcher, event, self_serve=True)
    gid = int(event.group_id)
    at = _mentioned(event)
    if not at:
        await _finish(matcher, "请 @ 指定成员。用法：/note @某人 内容")
    if not owner:
        if at[0] not in await _own_accounts(event):
            await matcher.finish()
        # The one trace of a member rewriting their own note, for /log.
        log.info("group %s: member %s self-serves /note", gid, event.user_id)

    target = at[0]
    text = _strip_cmd(event.get_plaintext(), "note").strip()
    card = await _card_of(matcher, gid, target)

    if not text:
        if not card.note:
            await _finish(matcher,
                f"{card.display} 暂无备注。\n"
                "用法：/note @某人 内容（写入）；/note @某人 -（清除）"
            )
        await _finish(matcher, f"{card.display} 的备注：\n{card.note}")

    note = "" if text == DROP else text
    await directory().note(gid, target, note)
    if not note:
        await _finish(matcher, f"已清除 {card.display} 的备注。")
    await _finish(matcher, f"已写入 {card.display} 的备注：\n{note}")


@alias_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Bind or retire a name by hand.

    The escape hatch for names a group says but never types: the extractor only ever sees
    what was written down, so a name that lives entirely in speech is unlearnable unless
    somebody happens to write the sentence that coins it. A name bound here counts as
    certain, which is what lets it settle an argument the model would otherwise keep
    having with itself.

    A member may do all of it to themselves - they are the best source for what
    they are called, and their entry binds at the same full trust as the
    owner's. Anyone else's name draws silence.
    """
    owner = await _gate(matcher, event, self_serve=True)
    gid = int(event.group_id)
    at = _mentioned(event)
    if not at:
        await _finish(matcher, "请 @ 指定成员。用法：/alias @某人 称呼")
    if not owner:
        if at[0] not in await _own_accounts(event):
            await matcher.finish()
        # The one trace of a member rewriting their own names, for /log.
        log.info("group %s: member %s self-serves /alias", gid, event.user_id)

    target = at[0]
    arg = _strip_cmd(event.get_plaintext(), "alias").strip()
    card = await _card_of(matcher, gid, target)

    if not arg:
        if not card.names and not card.candidates:
            await _finish(matcher, f"{card.display} 暂无记录在案的称呼。")
        lines = [f"· {n.text}（{n.confidence:.2f}"
                 + ("，平台" if n.platform_given else "")
                 + ("，全局" if n.is_global else "") + "）"
                 for n in card.names]
        if card.candidates:
            lines.append("未确认：")
            lines += [f"· {n.text}（{n.confidence:.2f}）" for n in card.candidates]
        await _finish(matcher, _fit(f"{card.display} 的称呼：\n" + "\n".join(lines),
                                  gid=str(gid)))

    if arg.startswith(DROP):
        name = arg[len(DROP):].strip()
        if not name:
            await _finish(matcher, "请写明要撤销的称呼。用法：/alias @某人 -称呼")
        if await directory().unname(gid, target, name):
            await _finish(matcher, f"已撤销 {card.display} 的称呼「{name}」。")
        await _finish(matcher, f"{card.display} 名下没有「{name}」这个称呼。")

    # name=0.4 sets how much the name is trusted; a bare name binds at full trust.
    if "=" in arg:
        name, _, value = arg.rpartition("=")
        name = name.strip()
        try:
            conf = float(value.strip())
        except ValueError:
            conf = -1.0
        if not 0.0 <= conf <= 1.0:
            await _finish(matcher, "置信度需为 0 到 1 的数字，例如：/alias @某人 阿明=0.6")
            return
        if not name:
            await _finish(matcher, "请写明称呼。用法：/alias @某人 称呼=0.6")
        try:
            n = await directory().set_confidence(gid, target, name, conf)
        except ValueError as e:
            await _finish(matcher, str(e))
            return
        except NameTaken as e:
            await _finish(matcher,
                f"「{e.text}」在本群已经指向 {e.holder}，一个称呼只能指一个人。")
            return
        state = ("可以使用" if n.confidence >= CONFIRM_THRESHOLD
                 else "已保留记录，暂不使用")
        await _finish(matcher,
            f"已设置：{card.display} 的「{n.text}」置信度 {n.confidence:.2f}（{state}）。")

    try:
        await directory().name(gid, target, arg)
    except ValueError as e:
        await _finish(matcher, str(e))
        return
    except NameTaken as e:
        await _finish(matcher,
            f"「{e.text}」在本群已经指向 {e.holder}，一个称呼只能指一个人。\n"
            f"如需改为指向本人，请先在对方名下撤销：/alias @对方 -{e.text}")
        return
    await _finish(matcher, f"已登记：{card.display} 也叫「{arg}」。")


@forget_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Retract one entry by the number that listed it.

    By number rather than by text: the alternative is matching on what the owner retypes,
    and a near-miss there deletes the wrong entry while reporting success.

    With an @ it is that person's record; without one it is the group's own, because the
    group is an entity too and its facts are numbered by /card.

    A member may delete from their own record only - being the subject is the
    licence. The bare group-fact form and other people's entries stay the
    owner's, answered with silence.
    """
    owner = await _gate(matcher, event, self_serve=True)
    gid = int(event.group_id)
    arg = _strip_cmd(event.get_plaintext(), "forget").strip()
    digits = next((w for w in arg.split() if w.isdecimal()), "")
    if not digits:
        await _finish(matcher, "请写明要删除的编号。编号见 /who @某人 或 /card。")

    at = _mentioned(event)
    if not owner:
        if not at or at[0] not in await _own_accounts(event):
            await matcher.finish()
        # The one trace of a member pruning their own record, for /log.
        log.info("group %s: member %s self-serves /forget", gid, event.user_id)
    dropped = (await directory().forget(gid, at[0], int(digits)) if at
               else await directory().forget_group_fact(gid, int(digits)))
    if dropped is None:
        await _finish(matcher,
            f"没有编号为 {digits} 的记录。编号见 /who @某人 或 /card。")
    await _finish(matcher, f"已删除：{dropped.text}")


# -- identity ---------------------------------------------------------------


@merge_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Declare two accounts to be the same person.

    Owner only; the model may only ever propose it. A wrong merge puts two people's
    histories under one name, and nothing downstream can tell which half came from
    where - so the one operation that can cause it stays in human hands.
    """
    await _gate(matcher, event, global_only=True)
    at = _mentioned(event)
    if len(at) < 2:
        await _finish(matcher, "请 @ 两个账号。用法：/merge @小号 @大号")
    loser, winner = at[0], at[1]
    if loser == winner:
        await _finish(matcher, "这是同一个账号。")
    if str(event.self_id) in (loser, winner):
        # The bot has no person of its own to fold anyone into: merged with a
        # member, its id would follow that person into every block and roster.
        await _finish(matcher, "不能合并机器人自己的账号。")
    try:
        changed = await directory().merge(loser, winner)
    except UnknownAccount as e:
        await _finish(matcher,
                      f"{await _name(str(event.group_id), e.user_id)} 没有任何记录，无法合并。")
        return
    # Being blocked is a decision about a person, so it follows them across the
    # merge: an alt that stayed unblocked would keep talking, keep being archived
    # and keep feeding memory, which is exactly what the block refused. Runs even
    # when the merge itself was a no-op: a pair merged before blocks became
    # person-wide can be half-blocked, and re-issuing /merge is the one natural
    # repair the owner will actually try.
    accounts = await directory().accounts_of_person(winner)
    spread: list[tuple[int, datetime | None]] = []
    if str(event.self_id) not in accounts:
        def shielded(_gid: int) -> bool:
            # The same two refusals /block makes, per group: never the owner.
            return any(perms.is_owner(a, config().default.owners) for a in accounts)
        spread = await directory().blocks_after_merge(winner, shielded=shielded)
    for gid, until in spread:
        if (st := REGISTRY.loaded(str(gid))) is not None:
            st.blocked.update(dict.fromkeys(accounts, until))
    tail = f"两者在 {len(spread)} 个群的屏蔽状态已统一。" if spread else ""
    if not changed:
        await _finish(matcher, "这两个账号本来就属于同一个人。" + tail)
    await _finish(matcher, "已合并。" + tail)


@split_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Undo a merge for one account. The counterpart to /merge - possible only because
    every record keeps which account produced it, so the split knows what to take."""
    await _gate(matcher, event, global_only=True)
    at = _mentioned(event)
    if not at:
        await _finish(matcher, "请 @ 要拆分的账号。用法：/split @某人")
    if at[0] == str(event.self_id):
        await _finish(matcher, "不能拆分机器人自己的账号。")
    try:
        await directory().split(at[0])
    except UnknownAccount as e:
        await _finish(matcher,
                      f"{await _name(str(event.group_id), e.user_id)} 没有任何记录，无法拆分。")
        return
    except NotMerged as e:
        # A lone account has nothing to split from; the service refuses rather
        # than strand every fact under an emptied person.
        await _finish(matcher, e.message)
        return
    await _finish(matcher, "已拆分。")


# -- diagnostics ------------------------------------------------------------


@debug_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """Arm the model-round capture tap. The files land server-side; this only turns
    the tap on and says how many rounds it took."""
    await _gate(matcher, event, global_only=True)
    arg = _strip_cmd(event.get_plaintext(), "debug").strip().lower()
    if not arg:
        n = debug.armed()
        await _finish(matcher, f"捕获中：还剩 {n} 轮。" if n else "未在捕获。")
    if arg in ("off", "0"):
        debug.arm(0)
        await _finish(matcher, "已关闭捕获。")
    if not arg.isdecimal():
        await _finish(matcher, "用法：/debug 轮数（最多 50），/debug off 关闭。")
    took = debug.arm(int(arg))
    await _finish(matcher, f"已开启：接下来 {took} 轮模型调用写入 logs/debug/。")


@log_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """The tail of the log file, for when something looks wrong from inside the group."""
    await _gate(matcher, event, global_only=True)
    arg = event.get_plaintext().strip().split()
    n = LOG_LINES_DEFAULT
    if len(arg) > 1 and arg[-1].isdecimal():
        n = max(1, min(int(arg[-1]), LOG_LINES_MAX))
    path = Path(os.getenv("LOG_DIR", "/app/logs")) / "qqbot.log"
    try:
        # Read the tail rather than the file: it grows without bound and only the end
        # is ever wanted.
        with path.open("rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(max(0, size - LOG_TAIL_BYTES))
            tail = f.read().decode("utf-8", "replace")
    except OSError as e:
        await _finish(matcher, f"无法读取日志文件：{e}")
        return
    lines = [ln for ln in tail.splitlines() if ln.strip()][-n:]
    if not lines:
        await _finish(matcher, "日志为空。")
        return
    await _finish(matcher, _fit("\n".join(lines), head=False, gid=str(event.group_id)))


@help_cmd.handle()
async def _(matcher: Matcher, event: GroupMessageEvent) -> None:
    """The listing, or one command in full.

    Splitting the two is what lets the listing stay one line per command: usage, caveats
    and what a number means all live in the detail text, which is only ever read by
    someone who asked for it.

    A member's listing carries only the self-serve commands, and asking for an
    owner command's detail answers exactly like asking for one that does not
    exist - the two must be indistinguishable, or the error message becomes a
    directory of what is being hidden.
    """
    owner = await _gate(matcher, event, self_serve=True)
    wanted = _strip_cmd(event.get_plaintext(), "help")
    if not wanted:
        await _finish(matcher, _fit(command_catalog.help_text(owner=owner),
                                    gid=str(event.group_id)))

    cmd = command_catalog.find(wanted)
    if cmd is None or not (owner or cmd.self_serve or cmd.member) or (
            cmd.global_only and not owner):
        await _finish(matcher, f"没有「{wanted}」这条指令。发送 /help 查看全部。")
    await _finish(matcher, _fit(command_catalog.detail_text(cmd), gid=str(event.group_id)))
