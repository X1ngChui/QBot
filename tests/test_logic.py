"""Offline smoke test: exercises the pure logic without DB, model or network."""
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
os.environ.setdefault("DATABASE_URL", "postgresql://qqbot@127.0.0.1:15432/qqbot")
os.environ.setdefault("DATABASE_PASSWORD", "testpw")


from qqbot.settings import config
from qqbot.core.output import strip_markdown
from qqbot.core import nickname, prompt, trigger
from qqbot.core.state import ChatMsg, GroupState
import json

from qqbot.core.segments import AtRef, AudioRef, ForwardRef, parse_segments
from qqbot.util import now_local

fails = []


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


# ---- config
b = config()
check("config loads", b.default.gateway.max_msg_len == 2000,
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
cases = [
    ("**加粗**测试", "加粗测试"),
    ("# 标题\n正文", "标题\n正文"),
    ("```python\nprint(1)\n```", "print(1)"),
    ("- 项目一\n- 项目二", "项目一\n项目二"),
    ("看这个[链接](https://x.com)", "看这个链接 https://x.com"),
    ("`code`和***粗斜***", "code和粗斜"),
    ("正常文本不动", "正常文本不动"),
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


class _Res:
    text = "好的"
    tool_calls = []
    model = "fake"


_dbg.capture("777", 0, [{"role": "user", "content": "喂"}], _Res())
check("a captured round decrements the tap", _dbg.armed() == 1)
_caps = list(pathlib.Path(_dbg_dir, "debug").glob("reply-777-*.json"))
check("and writes one JSON file per round", len(_caps) == 1, str(_caps))
_cap = _json.loads(_caps[0].read_text(encoding="utf-8"))
check("with the exact request and response inside",
      _cap["messages"][0]["content"] == "喂" and _cap["text"] == "好的", str(_cap))
_dbg.capture("777", 1, [object()], object())   # unserializable-ish: must not raise
check("a capture failure never raises and still disarms", _dbg.armed() == 0)
_dbg.capture("777", 2, [], _Res())
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
pm = parse_segments(segs, "999")
check("at_bot detected", pm.at_bot)
check("reply_to captured", pm.reply_to == "555")
check("md5 key extracted", pm.refs[0].key == "a1b2c3d4e5f60718293a4b5c6d7e8f90", pm.refs[0].key)
check("mface summary kept", pm.refs[1].summary == "开心")
# The reply segment contributes its id and no text. What it quotes is put in front of the
# model as a pointer to a numbered line (prompt.numbered), not as an excerpt pasted
# here - an excerpt says what was said but not which line said it.
check("a quote adds no text of its own",
      pm.render() == "@我 看看这个 ⟦图片⟧ ⟦图片⟧", repr(pm.render()))
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
check("voice keeps the path napcat gave us", real_rec.refs[0].path.endswith(".amr"))
check("voice is an audio ref", isinstance(real_rec.refs[0], AudioRef))

# reply and forward segments carry content and must parse, not drop
quoted = parse_segments([{"type": "reply", "data": {"id": "123"}},
                         {"type": "text", "data": {"text": "这个"}}], "999")
check("reply records the quoted id", quoted.reply_to == "123")
# It costs no fetch and adds no text: which message this quotes is answered against the
# lines the model is already being shown, by number. See prompt.numbered.
check("and adds nothing to the text", quoted.render() == "这个", repr(quoted.render()))
check("and asks for no lookup", not quoted.refs, str(quoted.refs))

fwd = parse_segments([{"type": "forward", "data": {"id": "abc"}}], "999")
check("forward becomes a ref", isinstance(fwd.refs[0], ForwardRef) and fwd.refs[0].ident == "abc")

at_other = parse_segments([{"type": "at", "data": {"qq": "12345"}}], "999")
check("a bare @qq becomes a ref to resolve", isinstance(at_other.refs[0], AtRef))
check("unresolved @ falls back to the number", "@12345" in at_other.render())
at_named = parse_segments([{"type": "at", "data": {"qq": "12345", "name": "阿强"}}], "999")
check("@ with a name needs no lookup", not at_named.refs and "@阿强" in at_named.render())
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

# The quote mark renders on members' lines only. Assistant-role content is the
# model's strongest example of its own output format, and the day bot lines
# started opening with the mark, real replies started carrying it verbatim.
_qa = ChatMsg(msg_id="q1", user_id="u1", nickname="阿强", text="在吗", ts=now_local())
_qb = ChatMsg(msg_id="q2", user_id="999", nickname="小X", text="在的", ts=now_local(),
              is_bot=True, reply_to="q1")
_qc = ChatMsg(msg_id="q3", user_id="u2", nickname="阿花", text="哦哦", ts=now_local(),
              reply_to="q1")
_qn, _qm = prompt.numbered([_qa, _qb, _qc])
_qh = prompt.render_history([_qa, _qb, _qc], _qn, _qm)
check("the bot's own line renders without the quote mark",
      "⟦回复" not in _qh[1]["content"], repr(_qh[1]["content"]))
check("a member's line keeps its quote mark",
      "⟦回复 #1⟧" in _qh[2]["content"], repr(_qh[2]["content"]))

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
    st.add(ChatMsg(msg_id=f"m{i}", user_id="u1", nickname="阿强", text=f"第{i}条消息", ts=now_local()))
batch = [ChatMsg(msg_id="m99", user_id="u2", nickname="阿花", text="小X你在吗", ts=now_local())]
st.add(batch[0])
msgs = prompt.assemble(
    persona=persona, cfg=cfg, st=st, batch=batch,
    profiles=[{"user_id": "u1", "nickname": "阿强", "persona_card": "爱打游戏"}],
)
check("system first", msgs[0]["role"] == "system")
check("persona in system", "小X" in msgs[0]["content"])
check("profile in system", "爱打游戏" in msgs[0]["content"])
check("history in the middle", all(m["role"] in ("user", "assistant") for m in msgs[1:-1]))
tail = msgs[-1]["content"]
check("tail is last user msg", msgs[-1]["role"] == "user")
check("current msg in tail", "小X你在吗" in tail)

# The clock. A model has no time of its own, so it has to be told - but it must sit past
# the cache boundary: in the system block it would change every minute and cost the
# prefix cache on every call.
from qqbot import util as _util
check("current time is in the prompt", "当前时间：" in tail, tail[:40])
check("clock is NOT in the cached system block", "当前时间：" not in msgs[0]["content"])
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

check("history anchor set", st.history_anchor is not None, str(st.history_anchor))

# original pictures ride inside the messages that posted them: text first, then the
# file blocks; stale ids are left out (the vendor expires them, and a dead id fails
# the request it rides in); the sanity rail keeps the newest when it binds; and with
# no ids at all every message stays a plain string.
from datetime import timedelta as _td2
_sti = GroupState(group_id="77")
_old_ts = now_local() - prompt.PROMPT_IMAGE_MAX_AGE - _td2(hours=1)
_sti.add(ChatMsg(msg_id="i0", user_id="u", nickname="a", text="[图片:旧图]",
                 ts=_old_ts, images=["file-old"]))
for i in range(8):
    _sti.add(ChatMsg(msg_id=f"i{i+1}", user_id="u", nickname="a", text=f"[图片:第{i}张]",
                     ts=now_local(), images=[f"file-{i}"]))
_bi = [ChatMsg(msg_id="iq", user_id="u2", nickname="b", text="小X 看这张", ts=now_local(),
               images=["file-batch"])]
_rail = prompt.MAX_PROMPT_IMAGES
prompt.MAX_PROMPT_IMAGES = 99          # rail out of the way: freshness alone decides
_att = prompt.attached_images(list(_sti.recent), _bi, cfg)
check("every fresh picture is attached to its own message",
      all(_att.get(f"i{i+1}") == [f"file-{i}"] for i in range(8))
      and _att.get("iq") == ["file-batch"], str(_att))
check("a picture past the freshness cutoff is left out", "i0" not in _att)
_noimg = cfg.model_copy(deep=True)
_noimg.llm.text.reads_images = False
check("a text-only reply model gets no file blocks at all",
      prompt.attached_images(list(_sti.recent), _bi, _noimg) == {})
prompt.MAX_PROMPT_IMAGES = 3
_att2 = prompt.attached_images(list(_sti.recent), _bi, cfg)
check("when the rail binds it keeps the newest messages' pictures",
      set(_att2) == {"iq", "i8", "i7"}, str(_att2))
prompt.MAX_PROMPT_IMAGES = _rail
_mi = prompt.assemble(persona=persona, cfg=cfg, st=_sti, batch=_bi, profiles=[])
_hist_msgs = _mi[1:-1]
_with_files = [m for m in _hist_msgs if isinstance(m["content"], list)]
# Nine fresh pictures against the default rail of eight: the batch plus the newest
# seven history messages carry originals, and the oldest history picture falls back
# to its description line like any other unattached one.
check("history messages with pictures become text-then-file blocks",
      len(_with_files) == prompt.MAX_PROMPT_IMAGES - 1
      and all(m["content"][0]["type"] == "text"
              and all(b["type"] == "file" for b in m["content"][1:])
              for m in _with_files), str(_with_files[:1])[:120])
check("the rail-dropped picture keeps its description line",
      any(isinstance(m["content"], str) and "第0张" in m["content"] for m in _hist_msgs))
check("the old picture's line stays a plain string",
      any(isinstance(m["content"], str) and "旧图" in m["content"] for m in _hist_msgs))
check("the batch picture rides the tail, text first",
      isinstance(_mi[-1]["content"], list)
      and _mi[-1]["content"][0]["type"] == "text"
      and _mi[-1]["content"][-1] == {"type": "file", "file_id": "file-batch"},
      str(_mi[-1]["content"])[:120])
check("the legend explains what a block behind a marker is",
      "原图" in _mi[0]["content"])
_mn = prompt.assemble(persona=persona, cfg=cfg, st=st, batch=batch, profiles=[])
check("with no pictures every message is a plain string",
      all(isinstance(m["content"], str) for m in _mn))

# Prompts are data: <prompts_dir>/<key>.txt is the source of truth and the manifest in
# settings.py is the only list of keys. The filename IS the key, so there is no mapping
# to drift: a misspelled name is a missing file, and a missing file fails the load
# rather than silently blanking an instruction.
import shutil as _sh
import tempfile as _tf
from qqbot.settings import PROMPT_KEYS as _PK, load_bundle as _lb
with _tf.TemporaryDirectory() as _td:
    _cd = pathlib.Path(_td) / "config"
    _sh.copytree(ROOT / "tests" / "fixtures" / "config", _cd)
    _sh.copytree(ROOT / "config" / "prompts", _cd / "prompts")
    for _f in (_cd / "prompts").glob("*.md"):
        _f.unlink()
    # The fixture points prompts_dir at the real texts by a relative path, which
    # no longer resolves from the copy's location - point the copy at its own.
    _sy = _cd / "settings.yaml"
    _sy.write_text(_sy.read_text(encoding="utf-8").replace(
        "prompts_dir: ../../../config/prompts", "prompts_dir: prompts"),
        encoding="utf-8")
    (_cd / "prompts" / "describe_image.txt").write_text("换一种描述方式。", encoding="utf-8")
    _bo = _lb(config_dir=_cd)
    check("editing a prompt file changes what the bundle serves",
          _bo.prompts["describe_image"] == "换一种描述方式。")
    check("every manifest key was loaded from disk", set(_bo.prompts) == set(_PK))
    # A file nothing asks for is simply never read - it cannot ship a prompt the
    # code does not know about, the way a stray mapping key once could.
    (_cd / "prompts" / "no_such_key.txt").write_text("不该被读到。", encoding="utf-8")
    check("a stray prompt file is ignored, not loaded",
          set(_lb(config_dir=_cd).prompts) == set(_PK))
    (_cd / "prompts" / "no_such_key.txt").unlink()
    # Per-group overrides validate at load time too: for_group merges lazily,
    # so a typo in one group's overrides allowed through /reload would fail on
    # that group's every message - no reply, no archive - until the file was
    # fixed.
    _pd = _cd / "personas"
    _pd.mkdir(exist_ok=True)
    (_pd / "group_777.yaml").write_text(
        "system_prompt: 测试人设\noverrides:\n  triger:\n    nicknames: [x]\n",
        encoding="utf-8")
    try:
        _lb(config_dir=_cd)
        check("a bad per-group override fails the load", False, "it loaded")
    except Exception as e:
        check("a bad per-group override fails the load",
              "triger" in str(e), str(e)[:160])
    (_pd / "group_777.yaml").unlink()
    (_cd / "prompts" / "legend.txt").unlink()
    try:
        _lb(config_dir=_cd)
        check("a missing prompt file fails the load", False, "it loaded")
    except ValueError as e:
        check("a missing prompt file fails the load", "legend" in str(e))
check("the live bundle serves the shipped texts",
      b.prompts["legend"].startswith("【系统括号原则】"))

# Config numbers with a blast radius validate at load, not at detonation time:
# backup_keep=0 deletes the backup just written, nightly; a 4-field cron used to
# pass /reload and fail the *next boot*, days later.
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
# turns so the prefix cache keeps hitting, and when it moves it moves by EVICT_CHUNK.
# There is no token budget to test - money bounds spending, and nothing trims blocks.
_saved_win = prompt.HISTORY_MSGS, prompt.EVICT_CHUNK
prompt.HISTORY_MSGS, prompt.EVICT_CHUNK = 20, 5
st2 = GroupState(group_id="9")
for i in range(22):
    st2.add(ChatMsg(msg_id=f"x{i}", user_id="u", nickname="a", text="消息", ts=now_local()))
h1 = prompt.history_window(st2, [])
a1 = st2.history_anchor
check("an over-full window is cut back by whole chunks",
      len(h1) == 17 and a1 == "x5", f"{len(h1)} msgs, anchor {a1}")
anchors = []
for i in range(22, 25):
    st2.add(ChatMsg(msg_id=f"x{i}", user_id="u", nickname="a", text="短消息", ts=now_local()))
    prompt.history_window(st2, [])
    anchors.append(st2.history_anchor)
check("history anchor is stable across turns", all(a == a1 for a in anchors), f"{a1} -> {anchors}")
st2.add(ChatMsg(msg_id="x25", user_id="u", nickname="a", text="压过线", ts=now_local()))
prompt.history_window(st2, [])
check("and moves by a whole chunk when the window fills again",
      st2.history_anchor == "x10", str(st2.history_anchor))
prompt.HISTORY_MSGS, prompt.EVICT_CHUNK = _saved_win

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

# Every handler has to decide who may run it. A handler that simply forgets to ask is
# indistinguishable from one open on purpose, and that is how /who came to hand any
# member every impression in the group while /who all was carefully gated. The decision
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
from qqbot.providers import Kind as _Kind  # noqa: E402

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
import io as _io  # noqa: E402
import re as _re  # noqa: E402
import tokenize as _tok  # noqa: E402

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
        if isinstance(_n, _ast.Call) and isinstance(_n.func, _ast.Attribute)                 and isinstance(_n.func.value, _ast.Name) and _n.func.value.id == "log":
            for _a in _n.args:
                if isinstance(_a, _ast.Constant) and isinstance(_a.value, str)                         and _CJK.search(_a.value):
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
