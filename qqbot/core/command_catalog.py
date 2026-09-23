"""One role-independent command catalogue shared by routing, permission, and help."""

# Help text stays readable in chat; splitting these literals would hide the rendered shape.
# ruff: noqa: E501

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum


class Access(StrEnum):
    OPEN = "open"
    AGREED = "agreed"
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
        Access.AGREED,
    ),
    Command(
        "/note",
        "查看或修改备注",
        "/note [--all] [@账号]\n/note set [--all] [@账号] 内容\n/note clear [--all] [@账号]",
        "我的资料",
        Access.AGREED,
    ),
    Command(
        "/alias",
        "管理称呼",
        "/alias [--all] [@账号]\n/alias add [--all] [@账号] 称呼\n/alias remove [--all] [@账号] 称呼\n/alias confidence [--all] [@账号] 0到1 称呼",
        "我的资料",
        Access.AGREED,
    ),
    Command(
        "/forget",
        "删除账号记录",
        "/forget [--all] [@账号] 编号\n编号来自相同范围的 /who。",
        "我的资料",
        Access.AGREED,
    ),
    Command(
        "/link",
        "确认自己的关联账号",
        "/link @另一个账号\n/link confirm 验证码\n/link cancel 验证码",
        "身份",
        Access.AGREED,
    ),
    Command(
        "/unlink",
        "解除当前账号关联",
        "/unlink\n只剥离发送指令的账号，其余账号保持关联。",
        "身份",
        Access.AGREED,
    ),
    Command(
        "/card", "查看或修正本群记录", "/card\n/card forget 编号（仅 owner）", "本群", Access.AGREED
    ),
    Command("/stats", "查看用量", "/stats\n/stats global（仅 owner）", "本群", Access.AGREED),
    Command(
        "/top",
        "查看本群花费排行",
        "/top [--all] [数量]\n默认按账号；--all 按关联账号聚合。",
        "本群",
        Access.AGREED,
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
    Command("/help", "显示指令列表", "/help [指令名]", "协议", Access.OPEN),
    Command("/terms", "查看用户协议", "/terms", "协议", Access.OPEN),
    Command("/agree", "同意用户协议", "/agree", "协议", Access.OPEN),
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
        Access.OPEN: "无需同意协议",
        Access.AGREED: "需在本群同意协议",
        Access.OWNER: "仅 bot owner",
    }[command.access]
    return f"{command.name}　{command.what}\n{access}\n\n{command.detail.strip()}"
