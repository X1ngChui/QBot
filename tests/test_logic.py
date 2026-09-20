"""Offline smoke test: exercises the pure logic without DB, model or network."""
import os
import pathlib
import sys
from types import SimpleNamespace

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
from _db import configure_test_database

configure_test_database()


from qqbot.settings import config
from qqbot.core.output import strip_markdown
from qqbot.core import nickname, prompt, trigger
from qqbot.core.state import ChatMsg, GroupState
import json

from qqbot.core.segments import (
    AtRef,
    AudioRef,
    at_mentions,
    number_at_mentions,
    parse_segments,
)
from qqbot.util import describe_now, now_local

fails = []


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


# ---- config
b = config()
check("config loads", b.default.tools.send_messages.max_text_chars_per_message == 2000,
      f"nicknames={b.default.trigger.nicknames}")
check("persona loaded", "default" in b.personas, str(list(b.personas)))
cfg, persona = b.for_group("12345")
check("for_group falls back to default persona", persona.name == "小X")

# owners is a list: several people must be able to run ops commands, and a bot that
# outlives one person's attention needs more than one pair of hands
owners = b.default.owners
check("owners is a list of ids", isinstance(owners, list) and len(owners) == 2, str(owners))
check("a listed owner is recognised", "10001" in owners)
check("a stranger is not", "999999" not in owners)
# An id typed without quotes is an int to YAML; it must land as the string the
# permission checks compare against, not fail the whole config over a quote.
from qqbot.settings import Settings as _OwnerSettings
check("an unquoted owner id validates as a string",
      _OwnerSettings.model_validate({**b.default.model_dump(), "owners": [10001, "10002"]}).owners
      == ["10001", "10002"])

# Persona inheritance: a group file states only its differences. Without this the shared
# blocks are copied into every group file, and they drift - which is exactly what had
# happened to the real ones before this was added.
base_cfg, base_p = b.for_group("nosuchgroup")
grp_cfg, grp_p = b.for_group("555")
check("group without a file gets the default persona", base_p.name == "小X")
check("group persona inherits the name", grp_p.name == "小X", grp_p.name)
check("group persona inherits the base prompt",
      "不要 Markdown" in grp_p.system_prompt, grp_p.system_prompt[:40])
check("group persona appends its own part",
      "这个群专门聊测试" in grp_p.system_prompt)
check("base prompt comes before the group's part",
      grp_p.system_prompt.index("不要 Markdown") < grp_p.system_prompt.index("这个群专门聊测试"))
check("a field the group sets overrides rather than appends",
      grp_p.group_knowledge.strip() == "这个群的固定资料只有这一句。",
      grp_p.group_knowledge.strip()[:30])
check("a field the group omits is inherited", grp_p.name == base_p.name)
check("and the default is not overwritten by the group's value",
      base_p.group_knowledge.strip() == "测试群。", base_p.group_knowledge.strip())
check("the default persona is not polluted by the group's extra",
      "这个群专门聊测试" not in base_p.system_prompt)

# ---- strip_markdown
# The line is what QQ can show. Markup it does not render arrives as the characters
# themselves and has to go; anything that survives being sent as plain text stays,
# because a list somebody asked for reads better as a list.
cases = [
    ("**加粗**测试", "加粗测试"),
    ("# 标题\n正文", "标题\n正文"),
    ("```python\nprint(1)\n```", "print(1)"),
    ("- 项目一\n- 项目二", "- 项目一\n- 项目二"),
    ("* 项目一\n+ 项目二", "- 项目一\n- 项目二"),
    ("- 一级\n  * 二级", "- 一级\n  - 二级"),
    ("1. 第一步\n2. 第二步", "1. 第一步\n2. 第二步"),
    ("- **CPU**：某型号\n- 内存：32G", "- CPU：某型号\n- 内存：32G"),
    ("零下 -5 度", "零下 -5 度"),
    ("---\n分割线上下", "分割线上下"),
    ("看这个[链接](https://x.com)", "看这个链接 https://x.com"),
    ("`code`和***粗斜***", "code和粗斜"),
    ("正常文本不动", "正常文本不动"),
    # A single asterisk is also multiplication; only a starred span with word
    # boundaries outside it is emphasis.
    ("算式 3*5*2 等于 30", "算式 3*5*2 等于 30"),
    ("a*b, c*d", "a*b, c*d"),
    ("*强调*，然后", "强调，然后"),
]
for src, want in cases:
    got = strip_markdown(src)
    check(f"strip_markdown {src[:14]!r}", got == want, f"-> {got!r}")

# ---- /block durations: English suffixes only, typo distinguishable from absence
from datetime import timedelta as _dtd
from qqbot.util import parse_duration
check("30m parses", parse_duration("30m") == _dtd(minutes=30))
check("12h parses", parse_duration("12H") == _dtd(hours=12))
check("3d parses", parse_duration(" 3 d ") == _dtd(days=3))
check("a Chinese suffix is a typo", parse_duration("3天") is None)
check("a bare number is a typo", parse_duration("30") is None)
check("zero is a typo, not a block", parse_duration("0d") is None)
check("absurd width is refused", parse_duration("123456d") is None)

# ---- the debug tap: armed rounds, capped, self-disarming, never raising
import json as _json
import tempfile as _tmpf
from qqbot.core import debug as _dbg
_dbg_dir = _tmpf.mkdtemp()
os.environ["LOG_DIR"] = _dbg_dir
check("arming caps at the maximum", _dbg.arm(999) == _dbg.MAX_ROUNDS)
check("arming zero disarms", _dbg.arm(0) == 0 and _dbg.armed() == 0)
_dbg.arm(2)


from qqbot.providers.contracts import (
    Message as _DbgMessage,
    ModelTurn as _DbgTurn,
    Role as _DbgRole,
)

_dbg.capture(
    "777",
    0,
    (_DbgMessage(_DbgRole.USER, "喂"),),
    _DbgTurn(text="好的", model="fake"),
)
check("a captured round decrements the tap", _dbg.armed() == 1)
_caps = list(pathlib.Path(_dbg_dir, "debug").glob("reply-777-*.json"))
check("and writes one JSON file per round", len(_caps) == 1, str(_caps))
_cap = _json.loads(_caps[0].read_text(encoding="utf-8"))
check("with the neutral request and response and no reasoning field",
      _cap["prompt"][0]["content"] == "喂" and _cap["turn"]["text"] == "好的"
      and "reasoning" not in _cap["turn"], str(_cap))
_dbg.capture("777", 1, (object(),), object())   # invalid contract values must not raise
check("a capture failure never raises and still disarms", _dbg.armed() == 0)
_dbg.capture("777", 2, (), _DbgTurn(text="ignored"))
check("an exhausted tap writes nothing",
      len(list(pathlib.Path(_dbg_dir, "debug").glob("*.json"))) <= 2)
del os.environ["LOG_DIR"]

# ---- nickname: word vs substring
nickname.initialize()
nicks = ["小夜", "小X", "X酱"]
nickname.register(nicks)
check("nickname word hit", nickname.word_hit("小夜 在吗", nicks) == "小夜")
check("nickname variant hit", nickname.word_hit("X酱你说呢", nicks) == "X酱")
check("nickname NOT hit inside 小夜曲", nickname.word_hit("今天听了首小夜曲", nicks) is None,
      str(nickname.word_hit("今天听了首小夜曲", nicks)))
# What tokenization buys, stated as the contrast: the near-miss really does contain the
# nickname, and is still not the bot being addressed. Written out rather than through a
# helper, because the helper existed in the production module for these two lines alone.
check("and it is a substring, which is exactly why substring matching would misfire",
      any(n in "今天听了首小夜曲" for n in nicks))
check("unrelated text no hit", nickname.word_hit("今天天气不错", nicks) is None)

# ---- when the bot answers
# One rule, and nothing to tune. What was here before - a logistic cooldown curve and the
# adaptive ceiling it was scaled by - decided whether to speak uninvited, and there is no
# such decision now.
_st = GroupState(group_id="555")
_tcfg = b.for_group("555")[0]


def _tm(uid, text, is_bot=False):
    return ChatMsg(msg_id=f"t-{uid}-{len(text)}", user_id=uid, nickname=uid,
                   text=text, ts=now_local(), is_bot=is_bot)


check("an @ is answered",
      trigger.decide(_tm("u1", "在吗"), True, st=_st, cfg=_tcfg).reply)
check("a nickname is answered",
      trigger.decide(_tm("u1", "小X 在吗"), False, st=_st, cfg=_tcfg).reply)
check("anything else is not",
      not trigger.decide(_tm("u1", "今天天气不错"), False, st=_st, cfg=_tcfg).reply)

# One message, one verdict: the initiator is the addressed message's own sender,
# settled by the same look that decides to reply, and travels on the Decision -
# the reply quotes their message, the spend is attributed to them. Nothing
# downstream re-derives it, and nobody else's message can steal it.
_m1 = _tm("u1", "小X 来评评理")
_d = trigger.decide(_m1, False, st=_st, cfg=_tcfg)
check("the decision names the addresser",
      _d.reply and _d.initiator == "u1" and _d.initiator_msg_id == _m1.msg_id,
      str(_d))
_da = trigger.decide(_tm("u1", "在吗"), True, st=_st, cfg=_tcfg)
_db = trigger.decide(_tm("u2", "小X 你说说"), False, st=_st, cfg=_tcfg)
check("two callers each earn their own decision",
      _da.initiator == "u1" and _db.initiator == "u2")
check("the bot's own line is never the initiator",
      not trigger.decide(_tm("999", "在的", True), False, st=_st, cfg=_tcfg).reply)
check("no addresser, no initiator",
      not trigger.decide(_tm("u2", "早啊"), False, st=_st, cfg=_tcfg).reply)

_st.muted = True
check("and muting outranks being addressed",
      not trigger.decide(_tm("u1", "在吗"), True, st=_st, cfg=_tcfg).reply)
_st.muted = False

# ---- segment parsing
segs = [
    {"type": "at", "data": {"qq": "999"}},
    {"type": "text", "data": {"text": " 看看这个 "}},
    {"type": "image", "data": {"file": "A1B2C3D4E5F60718293A4B5C6D7E8F90.image", "url": "http://x/y"}},
    {"type": "mface", "data": {"emoji_id": "e123", "summary": "[开心]"}},
    {"type": "reply", "data": {"id": "555"}},
]
pm = parse_segments(segs, "999", self_name="小X")
check("at_bot detected", pm.at_bot)
_plain_at_me = parse_segments(
    [{"type": "text", "data": {"text": "@我 只是普通文字"}}],
    "999",
    self_name="小X",
)
check("typed @me is not a structured bot mention",
      not _plain_at_me.at_bot and _plain_at_me.render() == "@我 只是普通文字")
check("reply_to captured", pm.reply_to == "555")
check("md5 key extracted", pm.refs[0].key == "a1b2c3d4e5f60718293a4b5c6d7e8f90", pm.refs[0].key)
check("mface summary kept", pm.refs[1].summary == "开心")
# The reply segment contributes its id and no text. What it quotes is put in front of the
# model as a pointer to a numbered line (prompt.numbered), not as an excerpt pasted
# here - an excerpt says what was said but not which line said it.
check("a quote adds no text of its own",
      pm.render() == "@小X 看看这个 ⟦图片⟧ ⟦图片⟧", repr(pm.render()))
check("resolved render", "⟦图片:猫⟧" in pm.render({0: "⟦图片:猫⟧"}))

# Shapes copied from what NapCat actually archived, not from the spec's examples.
real_img = parse_segments([{"type": "image", "data": {
    "url": "https://multimedia.nt.qq.com.cn/download?x=1",
    "file": "C3B1F2A15C93C44D65039EBB736E7DEA.png",
    "summary": "", "sub_type": 0, "file_size": "222164"}}], "999")
check("md5 read from a .png filename, not just .image",
      real_img.refs[0].key == "c3b1f2a15c93c44d65039ebb736e7dea", real_img.refs[0].key)
check("an empty summary does not masquerade as a sticker name",
      real_img.refs[0].summary is None, repr(real_img.refs[0].summary))

real_rec = parse_segments([{"type": "record", "data": {
    "url": "https://multimedia.nt.qq.com.cn/download?y=2",
    "file": "409756b4fccbfe3bb257d6ccfe6da742.amr",
    "path": "/app/.config/QQ/nt_qq_abc/nt_data/Ptt/2026-07/Ori/409756b4.amr",
    "file_size": "10726"}}], "999")
check("voice is an audio ref", isinstance(real_rec.refs[0], AudioRef))
# The path a clip arrives with is not kept: the file there is SILK, which no
# transcriber reads, and get_record's WAV is the only route ever taken.
check("a clip's local path is not carried", not hasattr(real_rec.refs[0], "path"))

# reply and forward segments carry content and must parse, not drop
quoted = parse_segments([{"type": "reply", "data": {"id": "123"}},
                         {"type": "text", "data": {"text": "这个"}}], "999")
check("reply records the quoted id", quoted.reply_to == "123")
# It costs no fetch and adds no text: which message this quotes is answered against the
# lines the model is already being shown, by number. See prompt.numbered.
check("and adds nothing to the text", quoted.render() == "这个", repr(quoted.render()))
check("and asks for no lookup", not quoted.refs, str(quoted.refs))

fwd = parse_segments([{"type": "forward", "data": {"id": "abc"}}], "999")
check("a forward without its content is a bare marker, not a fetch",
      fwd.render() == "⟦转发的聊天记录⟧" and not fwd.refs, fwd.render())

at_other = parse_segments([{"type": "at", "data": {"qq": "12345"}}], "999")
check("a bare @qq becomes a ref to resolve", isinstance(at_other.refs[0], AtRef))
check("unresolved @ falls back to the number", "@12345" in at_other.render())
at_named = parse_segments([{"type": "at", "data": {"qq": "12345", "name": "阿强"}}], "999")
check("@ with a name needs no lookup", not at_named.refs and "@阿强" in at_named.render())
_same_name_mentions = [("member-a", "张伟"), ("member-b", "张伟")]
_numbered_ats = number_at_mentions(
    "@张伟 和 @张伟 都看看",
    _same_name_mentions,
    {"member-a": 3, "member-b": 7}.get,
)
check("same-name @ targets keep distinct prompt-local numbers",
      _numbered_ats == "@张伟⟦3⟧ 和 @张伟⟦7⟧ 都看看", _numbered_ats)
_self_at = number_at_mentions("@小X 在吗", [("999", "小X")], lambda _account: 0)
check("structured bot mentions retain reserved display zero",
      _self_at == "@小X⟦0⟧ 在吗", _self_at)
_missing_label = number_at_mentions(
    "@我 @小X 在吗",
    [("999", "")],
    lambda _account: 0,
)
check("a missing at label never marks preceding member-typed text",
      _missing_label == "@我 @小X 在吗", _missing_label)
check("raw at segments retain ordered account identity",
      at_mentions([
          {"type": "at", "data": {"qq": "member-a", "name": "张伟"}},
          {"type": "at", "data": {"qq": "member-b", "name": "张伟"}},
      ]) == _same_name_mentions)
at_all = parse_segments([{"type": "at", "data": {"qq": "all"}}], "999")
check("@all is not looked up", not at_all.refs and "@全体成员" in at_all.render())

card = parse_segments([{"type": "json", "data": {"data": json.dumps(
    {"prompt": "[分享]", "meta": {"news": {"title": "标题党", "desc": "正文摘要"}}})}}], "999")
check("share card yields its title and description",
      "标题党" in card.render() and "正文摘要" in card.render(), card.render())
bad_card = parse_segments([{"type": "json", "data": {"data": "not json"}}], "999")
check("an unparseable card degrades quietly", bad_card.render() == "⟦卡片消息⟧", bad_card.render())

check("only media costs a model call",
      real_img.needs_model and not quoted.needs_model and not at_other.needs_model)

# Transient paid failures stay retryable: the describing path marks a rate-limited /
# cap-blocked / failed turn-away by returning the fallback as an Unsettled string,
# and MediaProcessor.settled is the reader - pipeline clears ChatMsg.pending on that
# verdict alone. A terminal answer is a plain str and settles.
from qqbot.core.media import MEDIA as _MD, Unsettled as _Un
check("a described slot settles", _MD.settled(real_img, {0: "[图片:猫]"}))
check("an Unsettled fallback does not settle",
      not _MD.settled(real_img, {0: _Un("[图片:猫]")}))
check("an absent paid slot does not settle", not _MD.settled(real_img, {}))
check("a free-only message settles trivially", _MD.settled(quoted, {}))
check("Unsettled renders as its own text", _Un("[图片:猫]") == "[图片:猫]")

# The bot's own lines render as the send call that sent them, followed by its
# result: what the model reads of its own output is the shape it should produce.
# Members' lines keep the transcript form, quote mark and member number included.
from qqbot.core.member_numbers import MemberNumbers as _MN
_qa = ChatMsg(msg_id="q1", user_id="u1", nickname="阿强", text="在吗", ts=now_local())
_qb = ChatMsg(msg_id="q2", user_id="999", nickname="小X",
              text="在的 ⟦依据:搜索“在不在”⟧", ts=now_local(),
              is_bot=True, reply_to="q1", at=[("u1", "阿强")])
_qc = ChatMsg(msg_id="q3", user_id="u2", nickname="阿花", text="哦哦", ts=now_local(),
              reply_to="q1")
_qn, _qm = prompt.numbered([_qa, _qb, _qc])
_qp = _MN(self_id="999")
check("member numbering distinguishes bot zero from unknown",
      _qp.number("999") == 0 and _qp.known("missing") is None
      and _qp.number("") is None)
check("bot zero is display-only and never addressable",
      _qp.account(0) is None and _qp.accounts(0) == [])
prompt.number_people(_qp, [], [_qa, _qb, _qc], None)
_qh = prompt.render_history([_qa, _qb, _qc], _qn, _qm, people=_qp)
from qqbot.providers.contracts import (
    Message as _PromptMessage,
    ToolCall as _PromptCall,
    ToolResult as _PromptResult,
)
_qcall = _qh[1] if isinstance(_qh[1], _PromptCall) else None
check("the bot's own line renders as its send call",
      _qcall is not None and _qcall.name == "send_messages"
      and json.loads(_qcall.arguments or "{}")
      == {"messages": [{"content": [
          {"type": "reply", "data": {"line": 1}},
          {"type": "at", "data": {"member": 1}},
          {"type": "text", "data": {"text": "在的"}},
      ]}]}, repr(_qh[1]))
_qresult = _qh[2] if isinstance(_qh[2], _PromptResult) else None
check("and its result carries only the line number and time",
      _qresult is not None and _qcall is not None
      and _qresult.call_id == _qcall.call_id
      and str(_qresult.output).startswith("已发送：#2 ⟦")
      and "依据" not in str(_qresult.output), repr(_qh[2]))
check("a member's line keeps its quote mark and wears its member number",
      isinstance(_qh[3], _PromptMessage)
      and "阿花⟦2⟧: ⟦回复 #1⟧" in _qh[3].content, repr(_qh[3]))

from qqbot.core.outbound import DiceSegment as _PromptDice
_qd = ChatMsg(msg_id="q4", user_id="999", nickname="小X",
              text="⟦骰子:4点⟧", ts=now_local(), is_bot=True,
              outbound=(_PromptDice(),))
_qd_items = prompt.own_line(_qd, nums={"q4": 4}, people=_qp)
_qd_call, _qd_result = _qd_items[-2:]
check("an observed random result stays out of the legal send arguments",
      isinstance(_qd_call, _PromptCall)
      and json.loads(_qd_call.arguments or "{}")
      == {"messages": [{"content": [{"type": "dice", "data": {}}]}]},
      repr(_qd_call))
check("and the platform result is visible on the historical tool result",
      isinstance(_qd_result, _PromptResult)
      and "平台显示：⟦骰子:4点⟧" in str(_qd_result.output),
      repr(_qd_result))

from qqbot.core.archive import (
    archive_author as _archive_author,
    archive_mentions as _archive_mentions,
    archive_sender as _archive_sender,
    archive_text as _archive_text,
)
from qqbot.domain.archive import AuthorKind as _AuthorKind
_archive_row = {
    "plain_text": "@阿花 已查到 ⟦依据:搜索“旧查询”⟧",
    "payload": {
        "author_kind": "bot",
        "sender": {"nickname": "小X"},
        "segments": [
            {"type": "at", "data": {"qq": "u2", "name": "阿花"}},
            {"type": "text", "data": {"text": " 已查到"}},
        ],
    },
}
check("archive helpers share explicit author, sender, text and mentions",
      _archive_author(_archive_row["payload"], "999") is _AuthorKind.BOT
      and _archive_sender(_archive_row["payload"]) == "小X"
      and _archive_text(_archive_row) == "@阿花 已查到"
      and _archive_mentions(_archive_row["payload"]) == [("u2", "阿花")])
check("explicit member authorship wins over legacy bot hints",
      _archive_author(
          {"author_kind": "member", "self_id": "999", "outbound_schema": 1},
          "999",
      ) is _AuthorKind.MEMBER)
check("legacy structured outbound still classifies as bot",
      _archive_author({"outbound_schema": 1}, "legacy-bot") is _AuthorKind.BOT)

# A message the adapter replays across a restart must not enter the window twice:
# the pipeline's dedup set is process-local, and load_history - triggered by that
# same replay - has already rebuilt the deque from the archive with the original.
_dupe = GroupState(group_id="777")
_dupe.add(ChatMsg(msg_id="r1", user_id="u1", nickname="阿强", text="重放的一句", ts=now_local()))
_dupe.add(ChatMsg(msg_id="r1", user_id="u1", nickname="阿强", text="重放的一句", ts=now_local()))
check("a replayed msg_id does not enter the window twice", len(_dupe.recent) == 1)

# ---- prompt assembly + ordering
st = GroupState(group_id="12345")
for i in range(25):
    st.add(ChatMsg(msg_id=f"m{i}", user_id="u1", nickname="阿强",
                   text=f"第{i}条消息", ts=now_local()))
asked = ChatMsg(msg_id="m99", user_id="u2", nickname="阿花", text="小X你在吗", ts=now_local())
st.add(asked)
msgs = prompt.assemble(
    persona=persona, cfg=cfg, st=st, msg=asked,
    profiles=[{"user_id": "u1", "nickname": "阿强", "persona_card": "爱打游戏"}],
)
check("system first", isinstance(msgs[0], _PromptMessage) and msgs[0].role is _DbgRole.SYSTEM)
check("global policy stays in system", "【怎样发言】" in msgs[0].content)
check("group context follows as developer",
      isinstance(msgs[1], _PromptMessage) and msgs[1].role is _DbgRole.DEVELOPER)
check("persona in developer", "小X" in msgs[1].content)
check("profile in developer", "爱打游戏" in msgs[1].content)
check(
    "history follows both authority layers",
    all(isinstance(item, _PromptMessage) and item.role in (_DbgRole.USER, _DbgRole.ASSISTANT)
        for item in msgs[2:-1]),
)
tail = msgs[-1].content
check("tail is last user msg", isinstance(msgs[-1], _PromptMessage)
      and msgs[-1].role is _DbgRole.USER)
check("current msg in tail", "小X你在吗" in tail)

# The clock. A model has no time of its own, so it has to be told - but it must sit past
# the cache boundary: in the system block it would change every minute and cost the
# prefix cache on every call.
from qqbot import util as _util
check("current time is in the prompt", describe_now() in tail, tail[:80])
check("clock is NOT in the cached system block", "当前时间：" not in msgs[0].content)
check("clock names the weekday", any(d in tail for d in _util.WEEKDAYS))

# timezone is configurable, and a typo must not take the bot down
_before = _util.now_local().utcoffset()
_util.set_timezone("America/New_York")
_ny = _util.now_local().utcoffset()
check("timezone is configurable", _ny != _before)
_util.set_timezone("Not/AZone")
check("an unknown zone keeps the last good one", _util.now_local().utcoffset() == _ny)
_util.set_timezone("Asia/Shanghai")

# The zone as SQL will read it. Bare offset strings are POSIX syntax, where the sign
# runs backwards: PostgreSQL reads "UTC+08:00" as eight hours WEST, shifting every
# derived date by sixteen hours - so the fixed-offset fallback must never reach SQL
# as its str() form.
from datetime import timezone as _tzc, timedelta as _tdc
check("a named zone goes to SQL as its own name", _util.tz_sql() == "Asia/Shanghai")
_saved_tz = _util._TZ
_util._TZ = _tzc(_tdc(hours=8))
check("a fixed eastern offset is inverted into POSIX form",
      _util.tz_sql() == "UTC-08:00", _util.tz_sql())
_util._TZ = _tzc(_tdc(hours=-5))
check("and a western one likewise", _util.tz_sql() == "UTC+05:00", _util.tz_sql())
_util._TZ = _saved_tz

# A NUL inside a message is refused by PostgreSQL in text and jsonb alike, so it has to
# leave at the envelope: the rendered text through defang, the verbatim segments
# through the event parser. Before this, one stray NUL cost the whole message its
# place in the archive (three times in a month).
from qqbot.gateway.onebot import (
    GroupMessage as _GM,
    NapCatGroupMessageSentEvent as _SentEvent,
)
check("defang drops NUL", _util.defang("a\x00b⟦c⟧") == "ab[c]", repr(_util.defang("a\x00b")))
check("scrub_nul walks a nested structure",
      _util.scrub_nul({"a": ["x\x00", {"b": "\x00y"}], "n": 3})
      == {"a": ["x", {"b": "y"}], "n": 3})


class _Ev:
    message_id = 1
    group_id = 12345
    user_id = 10001
    time = 0
    sub_type = "normal"
    sender = {"user_id": 10001, "nickname": "王\x00大锤", "card": ""}
    reply = SimpleNamespace(message_id=9)
    to_me = True
    calls = 0

    @classmethod
    def get_message(cls):
        cls.calls += 1
        return [SimpleNamespace(
            type="text",
            data={"text": "hi\x00there"},
        )]


_gm = _GM.from_event(_Ev(), "999")
_gm_with_mention = _gm.with_bot_mention("小X")
_pl_json = json.dumps(_gm_with_mention.as_payload(), ensure_ascii=False)
check("an event carrying NUL is archived without it",
      "\x00" not in _pl_json and _gm.sender.nickname == "王大锤", repr(_pl_json[:80]))
check("the live adapter message is captured exactly once",
      _Ev.calls == 1 and _gm.reply_to_message_id == "9", str(_Ev.calls))
check("an adapter-stripped self mention is restored on the detached envelope",
      _gm.segments[0]["type"] == "text"
      and _gm_with_mention.segments[0] == {
          "type": "at", "data": {"qq": "999", "name": "小X"}
      })
from qqbot.domain.archive import AuthorKind as _EnvelopeAuthor


class _SelfEv(_Ev):
    user_id = 999
    sender = {"user_id": 10001, "nickname": "小X", "card": ""}


_self_gm = _GM.from_event(_SelfEv(), "999")
check("top-level authorship classifies a reported self message",
      _self_gm.author_kind is _EnvelopeAuthor.BOT
      and _self_gm.outbound_schema == 1
      and _self_gm.sender.user_id == "999", repr(_self_gm))
check("nested sender metadata cannot fabricate self authorship",
      _gm.author_kind is _EnvelopeAuthor.MEMBER and _gm.sender.user_id == "10001")
_sent_event = _SentEvent.model_validate({
    "time": 1789923561,
    "self_id": 999,
    "post_type": "message_sent",
    "user_id": 999,
    "message_type": "group",
    "sub_type": "normal",
    "message_id": 625907631,
    "group_id": 12345,
    "message": [{"type": "dice", "data": {"result": "2"}}],
    "raw_message": "[CQ:dice,result=2]",
    "font": 14,
    "sender": {"user_id": 999, "nickname": "小X", "role": "member"},
})
_sent_gm = _GM.from_event(_sent_event, "999")
check("NapCat message_sent keeps the ordinary group-message interface",
      _sent_gm.author_kind is _EnvelopeAuthor.BOT
      and _sent_gm.segments == [{"type": "dice", "data": {"result": "2"}}])

# The per-line cut never leaves a marker half open: an unbalanced bracket in the
# window is the one thing defang rules out everywhere else.
check("cut_text keeps a whole marker", _util.cut_text("你看 ⟦图片:一只猫⟧", 8) == "你看 ",
      repr(_util.cut_text("你看 ⟦图片:一只猫⟧", 8)))
check("cut_text leaves short text alone", _util.cut_text("短", 8) == "短")
check("cut_text cuts plain text at the limit", _util.cut_text("一二三四五", 3) == "一二三")
check("cut_text steps back out of nested markers",
      _util.cut_text("a ⟦x ⟦y⟧⟧ tail", 6) == "a ", repr(_util.cut_text("a ⟦x ⟦y⟧⟧ tail", 6)))

# The parser is total: a forward entry with a stamp no calendar can hold, a
# numeric faceText and a dice result all still render.
_odd = parse_segments([
    {"type": "forward", "data": {"id": "f", "content": [
        {"sender": {"nickname": "王大锤"}, "time": 10 ** 14,
         "message": [{"type": "text", "data": {"text": "早"}}]}]}},
    {"type": "face", "data": {"id": "14", "raw": {"faceText": 5}}},
    {"type": "dice", "data": {"result": 6}},
], "999")
_odd_text = _odd.render()
check("an out-of-range forward stamp leaves the entry untimed",
      "王大锤: 早" in _odd_text and "⟦转发的聊天记录 1条⟧" in _odd_text, _odd_text)
check("a numeric faceText still renders", "⟦表情:5⟧" in _odd_text, _odd_text)
check("a dice result renders its number", "⟦骰子:6点⟧" in _odd_text, _odd_text)
from qqbot.core.segments import _markdown_text as _mdt
check("a hashtag is not a heading", _mdt("#话题 今天") == "#话题 今天", repr(_mdt("#话题 今天")))
_hd = _mdt("## 标题\n正文")
check("a real heading loses its hashes", _hd == "标题\n正文", repr(_hd))

# The parser is total: it runs under the message's dedup mark, so a card whose
# JSON is a list, or a size that is not a number, must degrade rather than raise.
_odd = parse_segments([
    {"type": "json", "data": {"data": "[1, 2]"}},
    {"type": "json", "data": {"data": '{"meta": {"x": {"title": 7}}, "prompt": 3}'}},
    {"type": "image", "data": {"file": "a.jpg", "file_size": "big"}},
    {"type": "text", "data": "not a dict"},
], "999")
check("a card that is not an object degrades to the bare marker",
      _odd.parts[0] == "⟦卡片消息⟧", _odd.parts[0])
check("a card with non-string fields still renders", _odd.parts[1] == "⟦分享:7⟧", _odd.parts[1])
check("a non-numeric size is no size", _odd.refs[0].size is None)
# The trigger reads what was typed, not the render: a share card whose title
# carries the nickname is not somebody addressing the bot.
_card_nick = parse_segments([
    {"type": "json", "data": {"data": '{"meta": {"x": {"title": "小X 教程"}}}'}},
    {"type": "text", "data": {"text": "看这个"}},
], "999")
check("typed text is the text segments alone", _card_nick.typed_text == "看这个",
      _card_nick.typed_text)
check("while the render carries the card", "小X" in _card_nick.render())

# A log line is one line: an HTTP client's "for more information see <link>" second
# line would otherwise appear in the log as a separate, unlabelled event.
check("why() keeps the first line of a multi-line message",
      _util.why(RuntimeError("bad request\nFor more information check: https://x")) ==
      "RuntimeError: bad request", _util.why(RuntimeError("a\nb")))
check("why() still names a message-less exception", _util.why(TimeoutError()) == "TimeoutError")

check("history anchor set", st.history_anchor is not None, str(st.history_anchor))

# No picture rides in the prompt: every marker carries a number and the model opens
# what it wants to see with open_images. Every picture in the prompt carries a
# number, and the number is what open_images resolves. It has to be one coordinate
# rather than two ("the second picture in message #12"), because two is a pair the
# model gets to miscount independently. Numbered oldest first, like the line
# numbers, so both count the same direction - and stickers share the run, so there
# is one rule rather than two.
from qqbot.core.segments import ImageRef as _IR
_p1 = ChatMsg(msg_id="p1", user_id="u", nickname="王大锤", text="看 ⟦图片:一只橘猫⟧",
              ts=now_local(), image_refs=[_IR(key="a" * 32)])
_p2 = ChatMsg(msg_id="p2", user_id="v", nickname="阿旺", text="没有图的一句",
              ts=now_local())
_p3 = ChatMsg(msg_id="p3", user_id="u", nickname="王大锤",
              text="还有 ⟦图片:一条狗⟧ 和 ⟦表情:笑到打滚⟧", ts=now_local(),
              image_refs=[_IR(key="b" * 32), _IR(key="c" * 32)])
_per, _by = prompt.numbered_images([_p1, _p2, _p3])
check("pictures are numbered oldest first, stickers in the same run",
      _per == {"p1": [1], "p3": [2, 3]}, str(_per))
check("and the number maps back to the picture it names",
      [(m.msg_id, i) for m, i in (_by[1], _by[2], _by[3])]
      == [("p1", 0), ("p3", 0), ("p3", 1)],
      str([(m.msg_id, i) for m, i in _by.values()]))
check("the number rides inside the marker, description untouched",
      _p3.render(seq=9, pic_nums=_per["p3"]).endswith(
          "还有 ⟦图片2:一条狗⟧ 和 ⟦表情3:笑到打滚⟧"),
      _p3.render(seq=9, pic_nums=_per["p3"]))
# A line whose markers outnumber its references (an old archived rendering, a
# record fetched by id after numbering) is left alone: a number that opened the
# wrong picture would be worse than none.
_pf = ChatMsg(msg_id="pf", user_id="u", nickname="小红",
              text="⟦图片:我的图⟧ ⟦转发的聊天记录：李芳: ⟦图片⟧⟧", ts=now_local(),
              image_refs=[_IR(key="d" * 32)])
check("a line whose markers outnumber its pictures stays unnumbered",
      "⟦图片1:" not in _pf.render(seq=1, pic_nums=[1]), _pf.render(seq=1, pic_nums=[1]))
_mn = prompt.assemble(persona=persona, cfg=cfg, st=st, msg=asked, profiles=[])
check("every prompt message is a plain string - pictures are opened, never pushed",
      all(isinstance(item, _PromptMessage) and isinstance(item.content, str) for item in _mn))
check("the legend tells the model to open pictures by number",
      isinstance(_mn[0], _PromptMessage) and "open_images" in _mn[0].content)

# ---- forwarded chat records: delivered inline, rendered as an indented block
# The protocol side sends a forwarded record's entries inside the segment, nested
# records included. Each entry renders as a stamped, named line under the message
# that carries the record, one indent level per nesting; the pictures inside are
# the carrying message's own references, numbered in render order and openable.
def _node(t, who, *segs):
    return {"time": t, "sender": {"nickname": who}, "message": list(segs)}


def _txt(s):
    return {"type": "text", "data": {"text": s}}


def _img(h):
    return {"type": "image", "data": {"file": h * 32 + ".jpg", "url": "http://x/" + h}}


_t0 = int(now_local().timestamp())
_inner = {"type": "forward", "data": {"id": "in", "content": [
    _node(_t0 - 86400, "王大锤", _txt("已经来啦")),
    _node(_t0 - 86000, "王大锤", {"type": "at", "data": {"qq": "999"}}, _txt("草")),
]}}
_outer = [
    _txt("看这个"),
    {"type": "forward", "data": {"id": "out", "content": [
        _node(_t0 - 3600, "李芳", _txt("苹果的下载榜一了")),
        _node(_t0 - 3500, "李芳", _img("a")),
        _node(_t0 - 3400, "李芳", _inner),
        _node(_t0 - 3300, "李芳", {"type": "record", "data": {"file": "v.amr"}}),
    ]}},
    _txt("怎么看"),
]
_fw = parse_segments(_outer, "999")
_fw_text = _fw.render()
_fw_lines = _fw_text.splitlines()
check("the record renders as a block under the carrying message",
      _fw_lines[0] == "看这个 ⟦转发的聊天记录 4条⟧" and _fw_lines[-1] == "怎么看", _fw_text)
check("entries are stamped and named, one indent level in",
      _fw_lines[1].startswith("  ⟦") and _fw_lines[1].endswith("李芳: 苹果的下载榜一了"),
      _fw_lines[1])
check("a record inside the record goes one level deeper",
      any(ln.startswith("    ⟦") and ln.endswith("王大锤: 已经来啦") for ln in _fw_lines),
      _fw_text)
check("a forwarded picture is the carrying message's own reference",
      len(_fw.pictures) == 1 and _fw.pictures[0].nested and _fw.pictures[0].free
      and _fw.pictures[0].key == "a" * 32, str(_fw.pictures))
check("and its marker takes a number like any other",
      "⟦图片7⟧" in ChatMsg(msg_id="fw", user_id="u", nickname="n", text=_fw_text,
                          ts=now_local(), image_refs=_fw.pictures).render(pic_nums=[7]))
check("a forwarded voice clip is a bare marker, never transcribed",
      "  ⟦" in _fw_text and _fw_text.count("⟦语音⟧") == 1
      and not any(not r.free for r in _fw.refs), _fw_text)
check("an @ inside the record names the person, never addresses the bot",
      not _fw.at_bot and any(isinstance(r, AtRef) and r.ident == "999" for r in _fw.refs)
      and _fw.mentions == [], str(_fw.refs))
check("what was typed excludes the record", _fw.typed_text == "看这个 怎么看", _fw.typed_text)
# The bounds: lines in all, depth, and characters - the rest is said as a count, and
# a picture in an entry that is not rendered is not registered.
_tight = cfg.prompt.model_copy(update={"forward_lines": 2})
_fw2 = parse_segments(_outer, "999", limits=_tight)
check("past the line bound the rest is counted, not rendered",
      "⟦其余2条未显示⟧" in _fw2.render() and "已经来啦" not in _fw2.render(), _fw2.render())
_shallow = cfg.prompt.model_copy(update={"forward_depth": 1})
_fw3 = parse_segments(_outer, "999", limits=_shallow)
check("past the depth bound a record shows only its header",
      "⟦转发的聊天记录 2条⟧" in _fw3.render() and "已经来啦" not in _fw3.render(), _fw3.render())
_short = cfg.prompt.model_copy(update={"forward_chars": 100})
_fw4 = parse_segments(_outer, "999", limits=_short)
check("past the character bound the rest is counted too",
      "未显示⟧" in _fw4.render(), _fw4.render())
_onlyfirst = cfg.prompt.model_copy(update={"forward_lines": 1})
_fw5 = parse_segments(_outer, "999", limits=_onlyfirst)
check("a picture in an unrendered entry is not registered",
      _fw5.pictures == [] and "⟦图片⟧" not in _fw5.render(), _fw5.render())

# ---- ordered outbound QQ segments -----------------------------------------
from qqbot.core.agent import parse_send as _parse_send
from qqbot.core.tools import send_def as _send_def
from qqbot.core.outbound import (
    AtSegment as _OutAt,
    ContactSegment as _OutContact,
    CustomMusicSegment as _OutCustomMusic,
    DiceSegment as _OutDice,
    FaceSegment as _OutFace,
    JsonCardSegment as _OutJson,
    MarketFaceSegment as _OutMarketFace,
    MusicSegment as _OutMusic,
    ReplySegment as _OutReply,
    RpsSegment as _OutRps,
    TextSegment as _OutText,
    from_onebot as _from_onebot,
    to_onebot as _to_onebot,
)
from qqbot.providers.contracts import ToolCallId as _OutCallId

_send_spec = _send_def()
_send_description = _send_spec.description
check("send tool expands the fixed QQ face catalog",
      "14=微笑" in _send_description and "326=生气" in _send_description
      and "{{FACE_CATALOG}}" not in _send_description,
      _send_description[-200:])
_send_contract = json.dumps(_send_spec.parameters, ensure_ascii=False)
_send_schemas = (
    _send_spec.parameters["properties"]["messages"]["items"]
    ["properties"]["content"]["items"]["anyOf"]
)
_send_types = {
    schema["properties"]["type"]["enum"][0]
    for schema in _send_schemas
}
check("send tool does not expose market faces to the model",
      "mface" not in _send_description and "商城表情" not in _send_description
      and "mface" not in _send_contract)
check("send tool hides rich card segments from the model",
      _send_types == {
          "text", "at", "reply", "face", "dice", "rps",
          "contact_member", "contact_group",
      }
      and all(name not in _send_description
              for name in ("music", "music_custom", "json")),
      str(sorted(_send_types)))

_out_people = _MN(self_id="bot")
_out_people.number("member-a", spoke=True)
_out_people.number("member-b", spoke=True)
_out_line = ChatMsg(
    msg_id="line-7",
    user_id="member-a",
    nickname="甲",
    text="问题",
    ts=now_local(),
)


def _out_call(content, *more):
    return _PromptCall(
        _OutCallId("send-test"),
        "send_messages",
        json.dumps(
            {"messages": [{"content": item} for item in (content, *more)]},
            ensure_ascii=False,
        ),
    )


_out_content = [
    {"type": "text", "data": {"text": "请"}},
    {"type": "at", "data": {"member": 2}},
    {"type": "text", "data": {"text": "看这里"}},
    {"type": "face", "data": {"id": 14}},
    {"type": "dice", "data": {}},
    {"type": "rps", "data": {}},
    {"type": "contact_member", "data": {"member": 1}},
    {"type": "contact_group", "data": {}},
    {"type": "music", "data": {"platform": "qq", "id": "42"}},
    {"type": "music_custom", "data": {
        "url": "https://example.invalid/song",
        "audio": "https://example.invalid/song.mp3",
        "title": "测试曲",
        "image": "https://example.invalid/cover.jpg",
        "singer": "测试歌手",
    }},
    {"type": "json", "data": {"payload": {"app": "test", "version": 1}}},
    {"type": "reply", "data": {"line": 7}},
]
_out_reply, _out_note = _parse_send(
    _out_call(_out_content),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("ordered send parses every supported scalar-only segment", _out_reply is not None,
      _out_note)
_out_types = tuple(type(segment) for segment in _out_reply.messages[0].segments)
check("an @ keeps its arbitrary position",
      _out_types[:3] == (_OutText, _OutAt, _OutText), str(_out_types))
check("special segments remain closed typed variants",
      all(kind in _out_types for kind in (
          _OutFace, _OutDice, _OutRps, _OutContact,
          _OutMusic, _OutCustomMusic, _OutJson, _OutReply,
      )), str(_out_types))
_out_wire = [_to_onebot(segment) for segment in _out_reply.messages[0].segments]
check("typed variants project to literal OneBot nested segments",
      [segment["type"] for segment in _out_wire]
      == ["text", "at", "text", "face", "dice", "rps", "contact",
          "contact", "music", "music", "json", "reply"], str(_out_wire))
check("member and message numbers resolve against this snapshot",
      _out_wire[1]["data"]["qq"] == "member-b"
      and _out_wire[-1]["data"]["id"] == "line-7")

_batch_reply, _batch_note = _parse_send(
    _out_call(
        [{"type": "text", "data": {"text": "先说明"}}],
        [{"type": "dice", "data": {}}],
    ),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check(
    "send parses an ordered batch of independent messages",
    _batch_reply is not None
    and len(_batch_reply.messages) == 2
    and isinstance(_batch_reply.messages[0].segments[0], _OutText)
    and isinstance(_batch_reply.messages[1].segments[0], _OutDice),
    _batch_note,
)
_too_many, _ = _parse_send(
    _out_call(*([
        {"type": "text", "data": {"text": str(index)}}
    ] for index in range(5))),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("the global message limit rejects an oversized batch", _too_many is None)
_invalid_batch, _ = _parse_send(
    _out_call(
        [{"type": "text", "data": {"text": "不能先发"}}],
        [{"type": "at", "data": {"member": 99}}],
    ),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("one invalid item rejects the entire batch", _invalid_batch is None)

_guessed_mface, _guessed_note = _parse_send(
    _out_call([{"type": "mface", "data": {
        "package_id": "pkg",
        "emoji_id": "emoji",
        "key": "key",
    }}]),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("a guessed market-face segment rejects the entire model send",
      _guessed_mface is None and "无效" in _guessed_note, _guessed_note)
_historical_mface = _from_onebot([{"type": "mface", "data": {
    "emoji_package_id": "pkg",
    "emoji_id": "emoji",
    "key": "key",
    "summary": "[历史表情]",
}}])
check("an archived market face remains readable",
      len(_historical_mface) == 1
      and isinstance(_historical_mface[0], _OutMarketFace)
      and _to_onebot(_historical_mface[0])["type"] == "mface",
      repr(_historical_mface))

_repeated, _ = _parse_send(
    _out_call([
        {"type": "at", "data": {"member": 1}},
        {"type": "text", "data": {"text": "和"}},
        {"type": "at", "data": {"member": 1}},
        {"type": "text", "data": {"text": "都来"}},
    ]),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("repeated mentions are preserved rather than deduplicated",
      _repeated is not None
      and [
          segment.account
          for segment in _repeated.messages[0].segments
          if isinstance(segment, _OutAt)
      ]
      == ["member-a", "member-a"])
_invalid_member, _ = _parse_send(
    _out_call([
        {"type": "at", "data": {"member": 99}},
        {"type": "text", "data": {"text": "不会偷发"}},
    ]),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("an unknown member number rejects the entire send", _invalid_member is None)
_zero_member, _ = _parse_send(
    _out_call([
        {"type": "at", "data": {"member": 0}},
        {"type": "text", "data": {"text": "不能给机器人自己发 at"}},
    ]),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("reserved bot zero is not an addressable send target", _zero_member is None)
_invalid_line, _ = _parse_send(
    _out_call([
        {"type": "reply", "data": {"line": 99}},
        {"type": "text", "data": {"text": "不会偷发"}},
    ]),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("an unknown line number rejects the entire send", _invalid_line is None)
_unsupported, _ = _parse_send(
    _out_call([{"type": "xml", "data": {"data": "<msg/>"}}]),
    people=_out_people,
    lines={7: _out_line},
    group_id="123",
    max_messages=4,
)
check("unsupported raw segment kinds cannot cross the closed schema", _unsupported is None)

_out_history = ChatMsg(
    msg_id="sent-1",
    user_id="bot",
    nickname="小X",
    text="请@乙看这里",
    ts=now_local(),
    is_bot=True,
    outbound=_out_reply.messages[0].segments,
)
_out_nums = {"line-7": 7, "sent-1": 8}
_out_first = prompt.own_line(
    _out_history,
    nums=_out_nums,
    people=_out_people,
)[0]
_out_second = prompt.own_line(
    _out_history,
    nums=_out_nums,
    people=_out_people,
)[0]
check("structured history replay is byte-stable",
      isinstance(_out_first, _PromptCall) and isinstance(_out_second, _PromptCall)
      and _out_first.arguments == _out_second.arguments)
_out_history_args = json.loads(_out_first.arguments)
check(
    "each archived bot line projects as a one-message batch",
    set(_out_history_args) == {"messages"}
    and len(_out_history_args["messages"]) == 1
    and _out_history_args["messages"][0]["content"],
)

# Runtime prompt wording is one YAML bundle with a closed code-owned key and slot
# contract. Any malformed template rejects the entire candidate configuration before it
# can replace the active bundle.
import shutil as _sh
import tempfile as _tf
import yaml as _yaml
from qqbot.prompting import (
    PROMPT_SPECS as _PS,
    PromptKey as _PromptKey,
    PromptTemplate as _PromptTemplate,
    TemplateValidationError as _TemplateError,
)
from qqbot.settings import load_bundle as _lb
with _tf.TemporaryDirectory() as _td:
    _cd = pathlib.Path(_td) / "config"
    _sh.copytree(ROOT / "tests" / "fixtures" / "config", _cd)
    _sh.copytree(ROOT / "config" / "prompts", _cd / "prompts")
    for _f in (_cd / "prompts").glob("*.md"):
        _f.unlink()
    _sh.copy(ROOT / "config" / "predicates.yaml", _cd / "predicates.yaml")
    # The fixture points prompts_dir and predicates_file at the real ones by relative
    # path, which no longer resolves from the copy's location - point the copy at its
    # own.
    _sy = _cd / "settings.yaml"
    _sy.write_text(_sy.read_text(encoding="utf-8")
                   .replace("prompts_dir: ../../../config/prompts",
                            "prompts_dir: prompts")
                   .replace("predicates_file: ../../../config/predicates.yaml",
                            "predicates_file: predicates.yaml"),
                   encoding="utf-8")
    _bundle_path = _cd / "prompts" / "prompts.yaml"
    _raw_bundle = _yaml.safe_load(_bundle_path.read_text(encoding="utf-8"))
    _raw_bundle["templates"]["vision_system"] = "换一种描述方式。"
    _bundle_path.write_text(
        _yaml.safe_dump(_raw_bundle, allow_unicode=True, sort_keys=False),
        encoding="utf-8",
    )
    _bo = _lb(config_dir=_cd)
    check("editing the single prompt bundle changes what the catalog serves",
          _bo.prompts.source(_PromptKey.VISION_SYSTEM) == "换一种描述方式。")
    check("every closed template key was loaded",
          set(_bo.prompts.templates) == set(_PS))
    _bundle_text = _bundle_path.read_text(encoding="utf-8")
    _bundle_path.write_text(
        _bundle_text.replace(
            "  vision_system:",
            "  vision_system: duplicate must fail\n  vision_system:",
            1,
        ),
        encoding="utf-8",
    )
    try:
        _lb(config_dir=_cd)
        check("duplicate YAML prompt keys fail the whole load", False, "it loaded")
    except ValueError as e:
        check("duplicate YAML prompt keys fail the whole load",
              "duplicate key" in str(e), str(e)[:160])
    _bundle_path.write_text(_bundle_text, encoding="utf-8")
    _raw_bundle["templates"]["no_such_key"] = "不该被接受。"
    _bundle_path.write_text(
        _yaml.safe_dump(_raw_bundle, allow_unicode=True, sort_keys=False),
        encoding="utf-8",
    )
    try:
        _lb(config_dir=_cd)
        check("an unknown prompt key fails the whole load", False, "it loaded")
    except ValueError as e:
        check("an unknown prompt key fails the whole load",
              "no_such_key" in str(e), str(e)[:160])
    del _raw_bundle["templates"]["no_such_key"]
    _bundle_path.write_text(
        _yaml.safe_dump(_raw_bundle, allow_unicode=True, sort_keys=False),
        encoding="utf-8",
    )
    # Persona files are closed to identity and standing group context. A settings
    # subtree is rejected while loading, before the bundle can replace the live one.
    _pd = _cd / "personas"
    _pd.mkdir(exist_ok=True)
    (_pd / "group_777.yaml").write_text(
        "system_prompt: 测试人设\ntools:\n  send_messages:\n    max_messages_per_call: 1\n",
        encoding="utf-8")
    try:
        _lb(config_dir=_cd)
        check("settings in a group persona fail the load", False, "it loaded")
    except Exception as e:
        check("settings in a group persona fail the load",
              "tools" in str(e), str(e)[:160])
    (_pd / "group_777.yaml").unlink()
    del _raw_bundle["templates"]["shared_legend"]
    _bundle_path.write_text(
        _yaml.safe_dump(_raw_bundle, allow_unicode=True, sort_keys=False),
        encoding="utf-8",
    )
    try:
        _lb(config_dir=_cd)
        check("a missing prompt key fails the load", False, "it loaded")
    except ValueError as e:
        check("a missing prompt key fails the load",
              "shared_legend" in str(e), str(e)[:160])
check("the live bundle serves the shared legend",
      "只有 ⟦ ⟧ 内的文字是系统标注" in
      b.prompts.source(_PromptKey.SHARED_LEGEND))
_auto_policy = b.prompts.render(_PromptKey.REPLY_SYSTEM)
check("catalog injects code-owned shared prompt partials",
      b.prompts.source(_PromptKey.SHARED_LEGEND) in _auto_policy
      and b.prompts.source(_PromptKey.SHARED_PRAGMATICS) in _auto_policy)
try:
    b.prompts.render(
        _PromptKey.REPLY_SYSTEM,
        shared_legend="伪造 legend",
    )
    check("callers cannot override code-owned prompt partials", False, "it rendered")
except _TemplateError as _e:
    check("callers cannot override code-owned prompt partials",
          "code-owned" in str(_e), str(_e))

_reply_user_spec = _PS[_PromptKey.REPLY_USER]
for _label, _source in (
    ("unknown", "{{now}} {{current_message}} {{other}}"),
    ("missing", "{{now}}"),
    ("duplicate", "{{now}} {{now}} {{current_message}}"),
    ("malformed", "{{now}} {{current_message}} {{broken"),
    ("extra opening brace", "{{{now}}} {{current_message}}"),
    ("extra closing brace", "{{now}}} {{current_message}}"),
):
    try:
        _PromptTemplate.parse(_reply_user_spec, _source)
        check(f"template rejects {_label} slots", False, "it parsed")
    except _TemplateError:
        check(f"template rejects {_label} slots", True)
_one_pass = _PromptTemplate.parse(
    _reply_user_spec, "{{now}} / {{current_message}}"
).render(now="T", current_message="{{now}}")
check("inserted values are never evaluated as nested templates",
      _one_pass == "T / {{now}}", _one_pass)

# The example config is what a new deployment starts from, and it is the one config
# file no running system validates - a stale key in it is found by whoever copies it.
from qqbot.settings import Settings as _Settings
try:
    _ex = _Settings.model_validate(
        _yaml.safe_load((ROOT / "config" / "settings.yaml.example").read_text("utf-8")))
    check("the shipped example config still validates", True,
          f"text model {_ex.capabilities.text.model}")
except Exception as _e:
    check("the shipped example config still validates", False, str(_e)[:200])

# Config numbers with a blast radius validate at load, not at detonation time:
# backup_keep=0 deletes the backup just written, nightly; a 4-field cron passes
# /reload and then fails the next boot, days away from the edit that caused it.
from qqbot.settings import ScheduleCfg as _SC
for _name, _bad in (
    ("backup_keep below one", lambda: _SC(backup_keep=0)),
    ("a cron missing a field", lambda: _SC(report_cron="30 4 * *")),
    ("a bad nightly cron", lambda: _SC(nightly_cron="bad")),
):
    try:
        _bad()
        check(f"{_name} is refused at load", False, "it validated")
    except ValueError:
        check(f"{_name} is refused at load", True)

# The cache hit rate is reported per use, not blended: the reply rate is the
# prompt-discipline signal, extraction's is structurally low (first-read text),
# and averaging the two made every nightly drain read as a regression.
from qqbot.core.budget import hit_split
_ledger = [
    {"kind": "reply", "in_hit": 800, "in_miss": 200},
    {"kind": "extract", "in_hit": 25, "in_miss": 75},
    {"kind": "vision", "in_hit": 50, "in_miss": 50},
]
check("the hit rate is split by use", hit_split(_ledger) == "回复 80%　归纳 25%　其他 50%",
      hit_split(_ledger))
check("a use with no prompt tokens is omitted",
      hit_split(_ledger[:1]) == "回复 80%", hit_split(_ledger[:1]))
check("no prompt tokens at all means no line", hit_split([]) == "")

# The window is a message count, evicted in chunks: the anchor must survive several
# turns so the prefix cache keeps hitting, and when it moves it moves by a whole chunk.
# There is no token budget to test - money bounds spending, and nothing trims blocks.
small = cfg.model_copy(deep=True)
small.prompt.evict_chunk, small.prompt.window_chunks = 5, 4
st2 = GroupState(group_id="9")
for i in range(22):
    st2.add(ChatMsg(msg_id=f"x{i}", user_id="u", nickname="a", text="消息", ts=now_local()))
h1 = prompt.history_window(st2, None, small)
a1 = st2.history_anchor
check("an over-full window is cut back by whole chunks",
      len(h1) == 17 and a1 == "x5", f"{len(h1)} msgs, anchor {a1}")
anchors = []
for i in range(22, 25):
    st2.add(ChatMsg(msg_id=f"x{i}", user_id="u", nickname="a", text="短消息", ts=now_local()))
    prompt.history_window(st2, None, small)
    anchors.append(st2.history_anchor)
check("history anchor is stable across turns", all(a == a1 for a in anchors), f"{a1} -> {anchors}")
st2.add(ChatMsg(msg_id="x25", user_id="u", nickname="a", text="压过线", ts=now_local()))
prompt.history_window(st2, None, small)
check("and moves by a whole chunk when the window fills again",
      st2.history_anchor == "x10", str(st2.history_anchor))

# ---- every source file parses, and the handler module's calls resolve
#
# plugins/commands.py cannot be imported by a test: on_command() runs at import time and
# needs a NoneBot runtime. That left it with no coverage of any kind, and a file with no
# coverage of any kind reaches production with a syntax error in it - which is exactly what
# happened. These two checks are what can be done without importing it.
import ast as _ast
import pathlib as _pl

_srcs = sorted(_pl.Path("qqbot").rglob("*.py"))
_broken = []
for _f in _srcs:
    try:
        _ast.parse(_f.read_text(encoding="utf-8"))
    except SyntaxError as e:
        _broken.append(f"{_f}:{e.lineno}: {e.msg}")
check("every source file parses", not _broken, "; ".join(_broken))

# A name that does not exist fails at call time, in a command nobody runs until they need
# it. Renaming a repo function and missing a caller here is the shape this catches.
from qqbot.db import repo as _repo

_mods = {"repo": _repo}
_missing = []
# tasks.py shares the blind spot: scheduled jobs import nonebot at module level, so a
# renamed repo function there also fails at fire time - 04:30, with nobody watching.
for _path in ("qqbot/plugins/commands.py", "qqbot/plugins/tasks.py"):
    for _n in _ast.walk(_ast.parse(_pl.Path(_path).read_text(encoding="utf-8"))):
        if (isinstance(_n, _ast.Attribute) and isinstance(_n.value, _ast.Name)
                and _n.value.id in _mods and not hasattr(_mods[_n.value.id], _n.attr)):
            _missing.append(f"{_path}: {_n.value.id}.{_n.attr} (line {_n.lineno})")
check("and every module attribute the handlers reach for exists",
      not _missing, "; ".join(_missing))
_tree = _ast.parse(_pl.Path("qqbot/plugins/commands.py").read_text(encoding="utf-8"))

# The gate's decision table, as the pure function the handlers call.
from qqbot.core.perms import Verdict as _V, decide as _decide
_own = ["10001", "20001"]


def _d(uid, **flags):
    return _decide(uid, owners=_own, **flags)


check("an owner holds an ordinary command", _d("20001") is _V.OWNER)
check("a member is denied an owner-only command", _d("30001") is _V.DENIED)
check("every owner holds a global-only command",
      _d("20001", global_only=True) is _V.OWNER)
check("a member reaches a self-serve command only past the agreement",
      _d("30001", self_serve=True) is _V.MEMBER_IF_AGREED)
check("and a member-readable surface the same way",
      _d("30001", open_to_members=True) is _V.MEMBER_IF_AGREED)
check("consenting itself is open before the agreement",
      _d("30001", self_serve=True, pre_agreement=True) is _V.MEMBER)
check("a global-only command stays closed to members whatever else it is flagged",
      _d("30001", global_only=True, self_serve=True, open_to_members=True) is _V.DENIED)

# Every handler has to decide who may run it. A handler that simply forgets to ask is
# indistinguishable from one open on purpose, and that is how /who came to hand any
# member every impression in the group while the owner-only commands were gated. The decision
# itself is tested in test_pipeline (qqbot/core/perms.py); what is checked here is only
# that each handler asks at all.
_ungated = []
for _n in _ast.walk(_tree):
    if not isinstance(_n, (_ast.AsyncFunctionDef, _ast.FunctionDef)):
        continue
    _handler = any(
        isinstance(_d, _ast.Call) and isinstance(_d.func, _ast.Attribute)
        and _d.func.attr == "handle" and isinstance(_d.func.value, _ast.Name)
        and _d.func.value.id.endswith("_cmd")
        for _d in _n.decorator_list
    )
    if not _handler:
        continue
    _asks = any(
        isinstance(_c, _ast.Call) and isinstance(_c.func, _ast.Name) and _c.func.id == "_gate"
        for _c in _ast.walk(_n)
    )
    if not _asks:
        _cmds = [
            _d.func.value.id for _d in _n.decorator_list
            if isinstance(_d, _ast.Call) and isinstance(_d.func, _ast.Attribute)
        ]
        _ungated.append(f"{_cmds} (line {_n.lineno})")
check("every command handler checks who is calling it",
      not _ungated, "; ".join(_ungated))

# The global-only set is catalogue data, and each handler's gate must agree with
# it: losing the scope marker could expose a cross-group command to members if its
# catalogue flags changed later.
from qqbot.core.command_catalog import CATALOG as _CAT
_gated_global = set()
for _n in _ast.walk(_tree):
    if not isinstance(_n, _ast.AsyncFunctionDef):
        continue
    _cmds = [_d.func.value.id for _d in _n.decorator_list
             if isinstance(_d, _ast.Call) and isinstance(_d.func, _ast.Attribute)
             and _d.func.attr == "handle" and isinstance(_d.func.value, _ast.Name)]
    for _c in _ast.walk(_n):
        if (isinstance(_c, _ast.Call) and isinstance(_c.func, _ast.Name)
                and _c.func.id == "_gate"
                and any(k.arg == "global_only" and isinstance(k.value, _ast.Constant)
                        and k.value.value is True for k in _c.keywords)):
            _gated_global.update("/" + c.removesuffix("_cmd") for c in _cmds)
_catalog_global = {c.name for c in _CAT if c.global_only}
check("the handlers gated global-only are exactly the catalogue's global-only set",
      _gated_global == _catalog_global, f"{_gated_global} vs {_catalog_global}")

# A name that does not exist anywhere in the file. Parsing catches a typo in the syntax;
# nothing caught `int(owner)` in a function whose list is called `owners`, so the 09:00
# report raised NameError on send and the report was never delivered - for three days,
# with the failure visible only in the log it would have been reporting.
#
# symtable does the scope analysis the interpreter would: a name read inside a function,
# bound in no enclosing scope, absent from module scope, and not a builtin, is a name that
# will raise the moment that line runs. Whole package, because plugins/ and any other
# module reached only by a scheduler or a matcher has no other coverage.
import builtins as _bi
import symtable as _sym

_undef = []
for _f in _srcs:
    _top = _sym.symtable(_f.read_text(encoding="utf-8"), str(_f), "exec")
    _bound = {s.get_name() for s in _top.get_symbols()
              if s.is_assigned() or s.is_imported() or s.is_namespace()}

    def _walk(table, top=_top, bound=_bound, path=_f):
        if table is not top:
            for s in table.get_symbols():
                n = s.get_name()
                if (s.is_global() and s.is_referenced()
                        and n not in bound and not hasattr(_bi, n)):
                    _undef.append(f"{path}: {table.get_name()}() uses undefined {n!r}")
        for c in table.get_children():
            _walk(c, top, bound, path)

    _walk(_top)
check("and no function reads a name that was never bound",
      not _undef, "; ".join(_undef))

# Every billed kind the reports count has to be one something actually writes. These were
# bare strings at both ends - two lists nobody could diff - and when the per-account
# profile rewrite became one batched call the writer's string changed while the reader's
# did not, so /stats showed zero of them for as long as the feature existed.
from qqbot.providers import Kind as _Kind

_used: set[str] = set()
for _f in _srcs:
    for _n in _ast.walk(_ast.parse(_f.read_text(encoding="utf-8"))):
        if (isinstance(_n, _ast.Attribute) and isinstance(_n.value, _ast.Name)
                and _n.value.id == "Kind"):
            _used.add(_n.attr)
check("every Kind named anywhere in the package exists",
      not (_unknown := sorted(n for n in _used if not hasattr(_Kind, n))), str(_unknown))
check("and both the writers and the readers name one",
      {"REPLY", "EXTRACT", "SEARCH", "VISION", "ASR"} <= _used,
      str(sorted(_used)))
# A Ref subclass carries no kind string at all now, so this looks only for
# the billed ones. A bare string here is one the reports cannot be checked against.
_bare = sorted(
    f"{_f.name}: {_k}" for _f in _srcs
    for _k in ("reply", "extract", "search", "vision", "asr")
    if f'kind="{_k}"' in _f.read_text(encoding="utf-8")
)
check("no billed kind is written as a bare string", not _bare, "; ".join(_bare))

# Comments, docstrings and log messages are written in English; prompts and anything the
# bot says in the group are not. The split is not stylistic - it is what keeps the two
# apart. Chinese in a comment reads like prompt text at a glance, and prompt text edited
# as though it were a comment is how a rule the model depends on gets casually reworded.
#
# What this checks is the code *about* the system. What the system says to a group is
# left alone: those strings are the product.
import io as _io
import re as _re
import tokenize as _tok

_CJK = _re.compile(r"[一-鿿]")
_cn_docs, _cn_coms, _cn_logs = [], [], []
# The tests are covered too. A suite is read by the same person as the code it guards,
# and a check whose label they cannot read tells them nothing when it fails.
for _f in _srcs + sorted(_pl.Path("tests").rglob("*.py")):
    _src = _f.read_text(encoding="utf-8")
    if not _CJK.search(_src):
        continue
    _tree = _ast.parse(_src)
    for _n in _ast.walk(_tree):
        if isinstance(_n, (_ast.Module, _ast.ClassDef, _ast.FunctionDef,
                           _ast.AsyncFunctionDef)):
            _d = _ast.get_docstring(_n, clean=False)
            if _d and _CJK.search(_d):
                _cn_docs.append(f"{_f.name}:{getattr(_n, 'name', '<module>')}")
        # A log line is read by whoever is debugging at 3am, which is the same audience
        # as a comment.
        if (isinstance(_n, _ast.Call) and isinstance(_n.func, _ast.Attribute)
                and isinstance(_n.func.value, _ast.Name)
                and _n.func.value.id == "log"):
            for _a in _n.args:
                if (isinstance(_a, _ast.Constant) and isinstance(_a.value, str)
                        and _CJK.search(_a.value)):
                    _cn_logs.append(f"{_f.name}:{_n.lineno}")
    for _t in _tok.generate_tokens(_io.StringIO(_src).readline):
        if _t.type == _tok.COMMENT and _CJK.search(_t.string):
            _cn_coms.append(f"{_f.name}:{_t.start[0]}")

check("no docstring is written in Chinese", not _cn_docs, "; ".join(_cn_docs))
check("no comment is written in Chinese", not _cn_coms, "; ".join(_cn_coms))
check("no log message is written in Chinese", not _cn_logs, "; ".join(_cn_logs))

# SQL is code about the system too, and it was the one file this guard did not read -
# which is exactly where the Chinese comments survived three sweeps.
_cn_sql = [
    f"{_p.name}:{_i}"
    for _p in sorted(_pl.Path("sql").glob("*.sql"))
    for _i, _line in enumerate(_p.read_text(encoding="utf-8").splitlines(), 1)
    if _line.lstrip().startswith("--") and _CJK.search(_line)
]
check("no SQL comment is written in Chinese", not _cn_sql, "; ".join(_cn_sql))

print()
print("FAILED:", fails if fails else "none")
sys.exit(1 if fails else 0)
