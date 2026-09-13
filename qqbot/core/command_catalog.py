"""The ops commands, as data.

One list serving two readers: the gateway takes the prefixes so a command does not also
draw a chat reply, and /help takes the descriptions. Kept together because two
hand-maintained lists drift - a command present in one and missing from the other either
goes undocumented or gets answered twice.

Data only, so it imports without a NoneBot runtime and can therefore be tested. The
handlers in plugins/commands.py cannot: on_command() runs at import time.

Most commands here are the owner's. Two kinds of exception, both catalog data:
`self_serve` commands any member may run against themselves - their own record,
names, note - because the data subject is the member; `member` commands any
member may run whole, because they only read shared state. The command path
answers for free where the chat path costs a model call. See core.perms for
the boundary.
"""

from __future__ import annotations

from dataclasses import dataclass

from ..domain.identity.alias import CONFIRM_THRESHOLD


@dataclass(frozen=True)
class Command:
    #: One line for the list. Says what it does, not how to call it.
    name: str
    what: str
    #: Shown by `/help <name>`: how to call it, and what to know before doing so. The
    #: listing has one line per command and has to stay readable in a chat window, so
    #: anything longer than a clause belongs here instead of in `what`.
    detail: str = ""
    #: Whether a non-owner may run this against themselves. The handler must
    #: implement the self-only narrowing; this flag is what /help's filtered
    #: listing and the gate read. Everything else stays owner-only.
    self_serve: bool = False
    #: Whether a non-owner may run this whole - the read-only surfaces with
    #: nothing to narrow.
    member: bool = False

    @property
    def global_only(self) -> bool:
        """Whether only the default (all-group) owner list holds this command -
        the ones whose blast radius is every group at once. The gate and the
        filtered listing both read this, so the two cannot disagree."""
        return self.name in GLOBAL_ONLY


#: The commands that rewrite the platform-global identity graph, swap every
#: group's config, capture every group's model rounds or read the global log.
GLOBAL_ONLY = frozenset({"/merge", "/split", "/reload", "/debug", "/log"})


#: The order is the listing's only structure - there are no headings - so what a reader
#: can tell about a command before opening it is whatever its neighbours suggest.
#:
#: People first, and within them: read the record, then the three ways to correct it, then
#: the two that say who is who. Then the group's own record. Then the two commands that
#: act on both, which is why they sit between the halves rather than inside either. Then
#: what it costs, the switch, and the machine.
CATALOG: tuple[Command, ...] = (
    Command("/help", "显示指令列表", """/help　　　　　显示指令列表
/help 指令名　显示该指令的详细用法

示例：/help who""", self_serve=True),
    Command("/agree", "同意用户协议", """/agree

发送本指令，即视为你已阅读并同意用户协议；协议全文可通过 /terms 查看。
同意之前，机器人不回复你的消息、只提示你查看协议，可用指令仅 /agree
和 /terms；你发的消息仍会照常接收。
同意按群、按协议版本记录：在本群同意一次即持续有效，其他群需分别同意；
协议更新版本后需重新同意。普通成员也可使用。

示例：/agree""", self_serve=True),
    Command("/terms", "查看用户协议", """/terms

显示用户协议全文。未同意用户协议时也可使用——先看协议，
再决定是否发送 /agree。普通成员也可使用。

示例：/terms""", self_serve=True),
    # This group exists because the memory system is otherwise unobservable: what it
    # learned goes into a prompt nobody sees, and a wrong fact looks exactly like a right
    # one until the bot says something odd.
    Command("/who", "查看成员记录", """/who　　　　列出全群成员
/who @某人　查看该成员的记录

记录包含两类信息：称呼来自群名片、/alias 与聊天中观察到的叫法；条目由模型从聊天记录归纳，可能有误。
括号内的数字是置信度（0 到 1）：由不同消息反复确认的次数算出，说得越多越高。
条目前的编号用于 /forget。

普通成员也可使用，但只能查自己：/who 或 /who @自己。

示例：/who @小明""", self_serve=True),
    Command("/note", "补充或更正成员记录", """/note @某人　　　　查看该成员的备注
/note @某人 内容　写入，覆盖原有内容
/note @某人 -　　　清除

备注不会被自动改写，并作为确定信息供后续归纳使用。

普通成员也可使用，但只能 @自己，查看、写入自己的备注；
写入的内容同样视作确定信息。

示例：/note @小明 只在周末上线""", self_serve=True),
    # Names a group says out loud but never types at anyone. The extractor only ever
    # sees what was written down, so a name that lives entirely in speech cannot be
    # learned unless somebody happens to write the sentence that coins it.
    Command("/alias", "登记、撤销或调整称呼", f"""/alias @某人　　　　　列出该成员的全部称呼及置信度
/alias @某人 称呼　　　登记一个称呼（置信度 1.0）
/alias @某人 称呼=0.6　设置该称呼的置信度，0 到 1
/alias @某人 -称呼　　 撤销一个称呼

置信度达到 {CONFIRM_THRESHOLD} 才会启用该称呼。手动设置的数值为最终决定，
后续自动观察不会覆盖它。撤销仅标记为不再使用，历史消息仍可识别。

普通成员也可使用，但只能 @自己：登记、撤销自己的称呼或设置其置信度，效力与拥有者录入的相同。

示例：/alias @小明 阿明""", self_serve=True),
    # Two accounts, one person. The identity layer exists to make this expressible; these
    # two commands are the only way to state it, because the model may only propose.
    Command("/merge", "合并两个账号", """/merge @小号 @大号

将前者的记录并入后者，此后两个账号视为同一人。
记录不会被改写，只是合并在一起。
若其中一方在某些群被屏蔽，合并后屏蔽会覆盖该人的全部账号。
此指令改写跨群的身份记录，仅默认配置里的拥有者可用。

示例：/merge @小明的小号 @小明"""),
    Command("/split", "拆分一个账号", """/split @某人

将该账号从当前身份中分出，恢复为独立的人。
该账号自身产生的称呼随其转移，其余记录保留在原账号。
此指令改写跨群的身份记录，仅默认配置里的拥有者可用。

示例：/split @小明的小号"""),
    Command("/card", "查看本群记录", """/card

显示关于本群本身的记录：群的主题，以及群内术语的含义。
与成员记录同源，同样由模型从聊天记录归纳，同样会因长期无人再提而淡出。
括号内的数字是置信度，与 /who 相同。条目前的编号用于 /forget（不带 @）。
普通成员也可查看。

示例：/card""", member=True),
    Command("/forget", "删除一条记录", """/forget @某人 编号　删除该成员的一条记录
/forget 编号　　　　删除本群的一条记录

编号取自 /who @某人 或 /card 的列表。删除后不再用于回复。
称呼请用 /alias 撤销。

普通成员也可使用，但只能 @自己 删除自己记录里的条目；
不带 @ 的群条目操作仍仅限拥有者。

示例：/forget @小明 2""", self_serve=True),
    Command("/relearn", "立即重新归纳",
            "归纳通常在每天凌晨集中进行；此指令不等今晚，立即重读本群最近的\n"
            "聊天记录（至多一窗）归纳一次。消耗模型调用，结果稍后生效，不即时返回。\n\n"
            "示例：/relearn"),
    # Split because one screen mixing the two reads as if every number were this group's:
    # the budget and the call counts are shared across every group, while the spend and the
    # mute switch belong to this one alone.
    Command("/stats", "查看全局用量",
            "所有群合计的数据，除搜索额度按月外均为当日：预算用量、回复次数、"
            "媒体调用次数、搜索额度、待归纳积压、缓存命中率、异常条数。\n"
            "待归纳的内容在每天凌晨集中处理。本群单独的数据用 /groupstats。\n"
            "普通成员也可查看。\n\n"
            "示例：/stats", member=True),
    Command("/top", "查看本群花费排行", """/top　　　默认列出本月花费最高的 5 人
/top 数量　最多 20

按人排名：已用 /merge 合并的账号计为一人，名下花费合并计算。
回复只在被 @ 或叫到名字时发生，一次回复的全部开销（含其间的
语音转写、看图与搜索）都记在发起的人头上；图片的入档描述记在
发图的人头上。归纳等大家共同引发的开销不计入。
普通成员也可查看。

示例：/top 10""", member=True),
    Command("/groupstats", "查看本群用量",
            "本群当日数据：人设、花费、回复次数、待归纳条数、静音状态、屏蔽人数。\n"
            "普通成员也可查看。\n\n"
            "示例：/groupstats", member=True),
    Command("/block", "屏蔽某个成员", """/block　　　　　　　列出本群的屏蔽名单
/block @某人　　　　屏蔽该成员，直到手动解除
/block @某人 时长　到期自动解除，如 30m、12h、3d
/unblock @某人　　 解除屏蔽

被屏蔽的成员只是得不到回复；其消息照常接收、存档并纳入记忆，
以保持上下文连贯。
若该成员有多个账号已用 /merge 合并，屏蔽与解除都对其全部账号生效。
再次 /block 会覆盖时长：不带时长转为永久，带时长重新计时。
名单随群保存，重启后仍有效。不能屏蔽拥有者。

示例：/block @某人 3d"""),
    Command("/unblock", "解除屏蔽", """/unblock @某人

解除后恢复回复。与 /block 一样，作用于该成员的全部账号。

示例：/unblock @某人"""),
    Command("/mute", "本群静音",
            "静音后本群不再发言，被 @ 时也不回复。重启后仍有效。\n"
            "解除用 /unmute。\n\n示例：/mute"),
    Command("/unmute", "解除静音", "解除静音，本群恢复发言。\n\n示例：/unmute"),
    Command("/log", "查看运行日志",
            "/log　　　　默认读取日志文件末尾 15 行\n/log 行数　指定行数，最多 60 行\n\n"
            "示例：/log 30"),
    Command("/debug", "捕获模型调用现场", """/debug　　　查看捕获状态
/debug 轮数　捕获接下来 N 轮模型调用（最多 50）
/debug off　 关闭

把每轮发给模型的完整消息与其原始回复写入服务器 logs/debug/ 目录，
一轮一个 JSON 文件，捕获满即自动关闭；重启也会关闭。
用于排查模型行为异常：直接看模型当时读到了什么，而不是事后推断。
捕获覆盖所有群，仅默认配置里的拥有者可用；/log、/reload 同此。

示例：/debug 5"""),
    Command("/reload", "重载配置与人设",
            "重新读取配置、人设、提示词、谓词表与用户协议，无需重启。若新配置有误，则继续使用原配置。\n"
            "定时任务的改动（含其时区）需重启生效；改动代码不能靠它生效，需重建镜像。\n\n"
            "示例：/reload"),
)

#: What the gateway routes away from the reply pipeline.
PREFIXES: tuple[str, ...] = tuple(c.name for c in CATALOG)

_BY_NAME = {c.name.lstrip("/"): c for c in CATALOG}


def find(name: str) -> Command | None:
    """Look up one command by name, with or without the leading slash."""
    return _BY_NAME.get((name or "").strip().lstrip("/").lower())


def help_text(*, owner: bool = True, global_owner: bool | None = None) -> str:
    """The listing: one line each, and how to get more.

    A reader's listing shows only what that reader can run - the rest must not
    be advertised to whoever cannot run it. `global_owner` is whether the reader
    is on the default owner list (the group-override owners are not); it
    defaults to `owner`.
    """
    if global_owner is None:
        global_owner = owner
    shown = [c for c in CATALOG
             if c.self_serve or c.member
             or (global_owner if c.global_only else owner)]
    width = max(len(c.name) for c in shown)
    lines = ["可用指令："]
    lines.extend(f"{c.name.ljust(width)}  {c.what}" for c in shown)
    lines.append("详细用法：/help 指令名")
    return "\n".join(lines)


def detail_text(cmd: Command) -> str:
    """Everything known about one command."""
    return f"{cmd.name}　{cmd.what}\n\n{cmd.detail.strip()}"
