"""One role-independent command catalogue shared by routing, permission, and help."""

# Help text stays readable in chat; splitting these literals would hide the rendered shape.
# ruff: noqa: E501

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum


class Access(StrEnum):
    MEMBER = "member"
    OWNER = "owner"


@dataclass(frozen=True, slots=True)
class Command:
    name: str
    what: str
    detail: str
    category: str
    access: Access


CATALOG: tuple[Command, ...] = (
    Command(
        "/who",
        "查看账号记录",
        "/who [--all] [@账号]\n默认查看当前精确账号；--all 查看关联账号的聚合记录。",
        "我的资料",
        Access.MEMBER,
    ),
    Command(
        "/note",
        "管理多条人工备注",
        "/note [--all] [@账号]\n/note list [--all] [@账号] [页码]\n/note add [--all] [@账号] -- 内容\n/note edit [--all] [@账号] 备注编号 -- 内容\n/note remove [--all] [@账号] 备注编号\n/note clear [--all] [@账号]\n默认管理精确账号备注；--all 只管理关联身份共享备注，不清除各账号备注。编号来自同范围的 /note list，不用于 /forget。",
        "我的资料",
        Access.MEMBER,
    ),
    Command(
        "/alias",
        "管理称呼",
        "/alias [--all] [@账号]\n/alias add [--all] [@账号] 称呼\n/alias remove [--all] [@账号] 称呼\n/alias confidence [--all] [@账号] 0到1 称呼",
        "我的资料",
        Access.MEMBER,
    ),
    Command(
        "/forget",
        "删除自动归纳事实",
        "/forget [--all] [@账号] 编号\n编号来自相同范围的 /who 自动归纳区；不删除人工备注或称呼。编号是当前列表的位置，资料变动后请重新查询。",
        "我的资料",
        Access.MEMBER,
    ),
    Command(
        "/link",
        "确认自己的关联账号",
        "/link @另一个账号\n/link confirm\n/link cancel\n向对方账号发出关联邀请；受邀账号在本群确认，任一方可在本群取消。每个账号在本群同时至多参与一份待确认邀请。",
        "身份",
        Access.MEMBER,
    ),
    Command(
        "/unlink",
        "解除当前账号关联",
        "/unlink\n只剥离发送指令的账号，其余账号保持关联。",
        "身份",
        Access.MEMBER,
    ),
    Command(
        "/card", "查看或修正本群记录", "/card\n/card forget 编号（仅 owner）", "本群", Access.MEMBER
    ),
    Command("/stats", "查看用量", "/stats\n/stats global（仅 owner）", "本群", Access.MEMBER),
    Command(
        "/top",
        "查看本群花费排行",
        "/top [--all] [数量]\n默认按账号；--all 按关联账号聚合。",
        "本群",
        Access.MEMBER,
    ),
    Command(
        "/tasks",
        "管理本群定时任务",
        "/tasks\n/tasks list [页码]\n/tasks show UUID\n/tasks add (--at 带时区ISO8601 | --in 30m|12h|3d) -- 内容\n/tasks edit UUID [--at 时间 | --in 时长] [-- 内容]\n/tasks cancel UUID\n列表包含待执行和执行中的任务；只能修改或取消待执行项。预约时间不保证准点送达。",
        "管理",
        Access.OWNER,
    ),
    Command("/members", "查看全群成员目录", "/members（仅 owner）", "管理", Access.OWNER),
    Command(
        "/block",
        "管理回复屏蔽",
        "/block\n/block add [--all] @账号 [30m|12h|3d]\n/block remove [--all] @账号",
        "管理",
        Access.OWNER,
    ),
    Command("/mute", "管理本群静音", "/mute [status|on|off]", "管理", Access.OWNER),
    Command(
        "/merge", "强制合并两个账号集合", "/merge @账号A @账号B（仅 owner）", "管理", Access.OWNER
    ),
    Command("/split", "强制剥离一个账号", "/split @账号（仅 owner）", "管理", Access.OWNER),
    Command(
        "/debug",
        "捕获模型调用现场",
        "/debug [status|start 轮数|stop]（仅 owner）",
        "维护",
        Access.OWNER,
    ),
    Command("/log", "查看运行日志", "/log [行数]（仅 owner）", "维护", Access.OWNER),
    Command("/help", "显示指令列表", "/help [指令名]", "帮助", Access.MEMBER),
)

PREFIXES: tuple[str, ...] = tuple(command.name for command in CATALOG)
_BY_NAME = {command.name.lstrip("/"): command for command in CATALOG}


def find(name: str) -> Command | None:
    return _BY_NAME.get((name or "").strip().lstrip("/").lower())


def help_text() -> str:
    lines = ["可用指令："]
    current = ""
    for command in CATALOG:
        if command.category != current:
            current = command.category
            lines.append(f"\n【{current}】")
        tag = "（仅 owner）" if command.access is Access.OWNER else ""
        lines.append(f"{command.name}　{command.what}{tag}")
    lines.append("\n详细用法：/help 指令名")
    return "\n".join(lines)


def detail_text(command: Command) -> str:
    access = {
        Access.MEMBER: "所有成员可用",
        Access.OWNER: "仅 bot owner",
    }[command.access]
    return f"{command.name}　{command.what}\n{access}\n\n{command.detail.strip()}"
