"""Offline, deterministic checks of configuration, messages, prompts, and source contracts."""

import ast as _ast
import builtins as _bi
import io as _io
import json
import json as _json
import pathlib
import re as _re
import shutil as _sh
import symtable as _sym
import tempfile as _tf
import tokenize as _tok
import uuid as _uuid
from copy import deepcopy
from dataclasses import replace
from datetime import timedelta as _dtd
from types import SimpleNamespace

import _db as _test_db
import pytest
import yaml as _yaml
from _fixtures import config, describe_now, now_local

from qqbot import util as _util
from qqbot.clock import WEEKDAYS
from qqbot.commands.catalog import PREFIXES as _COMMAND_PREFIXES
from qqbot.commands.catalog import Access as _Access
from qqbot.commands.router import registered_commands as _registered_commands
from qqbot.configuration import MaintenanceCfg as _SC
from qqbot.configuration import Settings as _OwnerSettings
from qqbot.configuration import Settings as _Settings
from qqbot.configuration import load_bundle as _lb
from qqbot.conversation import prompt
from qqbot.conversation.agent import parse_send as _parse_send
from qqbot.conversation.history import HistoryWindow
from qqbot.conversation.member_numbers import MemberNumbers as _MN
from qqbot.conversation.state import ChatMsg, GroupState
from qqbot.conversation.tools import send_def as _send_def
from qqbot.delivery.output import strip_markdown
from qqbot.delivery.segments import AtSegment as _OutAt
from qqbot.delivery.segments import ContactSegment as _OutContact
from qqbot.delivery.segments import CustomMusicSegment as _OutCustomMusic
from qqbot.delivery.segments import DiceSegment as _OutDice
from qqbot.delivery.segments import DiceSegment as _PromptDice
from qqbot.delivery.segments import FaceSegment as _OutFace
from qqbot.delivery.segments import JsonCardSegment as _OutJson
from qqbot.delivery.segments import MarketFaceSegment as _OutMarketFace
from qqbot.delivery.segments import MusicSegment as _OutMusic
from qqbot.delivery.segments import ReplySegment as _OutReply
from qqbot.delivery.segments import RpsSegment as _OutRps
from qqbot.delivery.segments import TextSegment as _OutText
from qqbot.delivery.segments import from_onebot as _from_onebot
from qqbot.delivery.segments import to_onebot as _to_onebot
from qqbot.domain.archive import ArchivedMessage as _ArchivedMessage
from qqbot.domain.archive import AuthorKind as _AuthorKind
from qqbot.domain.archive import AuthorKind as _EnvelopeAuthor
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.gateway import nickname, trigger
from qqbot.gateway.limits import PARSE_LIMITS
from qqbot.gateway.nonebot_adapter import NapCatGroupMessageSentEvent as _SentEvent
from qqbot.gateway.onebot import GroupMessage as _GM
from qqbot.gateway.segments import AtRef, AudioRef, at_mentions, number_at_mentions, parse_segments
from qqbot.gateway.segments import ImageRef as _IR
from qqbot.gateway.segments import _markdown_text as _mdt
from qqbot.media.result import Resolution as _Resolution
from qqbot.media.service import MediaProcessor as _MD
from qqbot.operations import debug as _dbg
from qqbot.operations.limits import DIAGNOSTIC_LIMITS
from qqbot.prompting import (
    PROMPT_SPECS as _PS,
)
from qqbot.prompting import (
    PromptKey as _PromptKey,
)
from qqbot.prompting import (
    PromptTemplate as _PromptTemplate,
)
from qqbot.prompting import (
    TemplateValidationError as _TemplateError,
)
from qqbot.providers import Kind as _Kind
from qqbot.providers.contracts import (
    Message as _DbgMessage,
)
from qqbot.providers.contracts import (
    Message as _PromptMessage,
)
from qqbot.providers.contracts import (
    ModelTurn as _DbgTurn,
)
from qqbot.providers.contracts import (
    Role as _DbgRole,
)
from qqbot.providers.contracts import (
    ToolCall as _PromptCall,
)
from qqbot.providers.contracts import ToolCallId as _OutCallId
from qqbot.providers.contracts import (
    ToolResult as _PromptResult,
)
from qqbot.services.budget import hit_split
from qqbot.services.permissions import Verdict as _V
from qqbot.services.permissions import decide as _decide
from qqbot.util import parse_duration

ROOT = pathlib.Path(__file__).resolve().parent.parent


def test_config_ids_and_persona_inheritance():
    # ---- config
    b = config()
    assert (
        GroupId(" 00123 ") == "123"
        and GroupId("123").to_onebot() == 123
        and (GroupId("123").to_db() == 123)
    ), "group ids normalize once and expose explicit boundaries"
    assert AccountId(" account-01 ") == "account-01" and MessageId(" notice:01 ") == "notice:01", (
        "account and message ids retain opaque text"
    )
    for raw in ("", "0", "-1", "group"):
        with pytest.raises(ValueError):
            GroupId(raw)
    assert b.default.conversation.max_text_chars_per_message == 2000, "config loads"
    assert GroupId("555") in b.personas, "persona loaded"
    cfg, persona = b.for_group(GroupId("12345"))
    assert persona.name == "小X", "for_group falls back to default persona"

    # Several operators share one immutable startup authorization list.
    owners = b.default.bot.owners
    assert isinstance(owners, tuple) and len(owners) == 2, "owners is an immutable sequence"
    assert "10001" in owners, "a listed owner is recognised"
    assert "999999" not in owners, "a stranger is not"
    # An id typed without quotes is an int to YAML; it must land as the string the
    # permission checks compare against, not fail the whole config over a quote.

    assert _OwnerSettings.model_validate(
        {**b.default.model_dump(), "bot": {"owners": [10001, "10002"]}}
    ).bot.owners == ("10001", "10002"), "an unquoted owner id validates as a string"
    with pytest.raises(ValueError):
        _OwnerSettings.model_validate({**b.default.model_dump(), "agreement": {"version": 1}})
    assert not (ROOT / "tests" / "fixtures" / "config" / "agreement.txt").exists(), (
        "fixture configuration loads without an agreement file"
    )
    assert (
        b.default.media.max_image_mb == 8
        and (not hasattr(b.default.media, "shutdown_wait_sec"))
        and (not hasattr(b.default.media, "file_max_age_days"))
    ), "media policy exposes admission rather than lifecycle controls"
    assert (
        b.default.conversation.max_messages_per_reply == 4
        and b.default.conversation.reply_deadline_sec == 180
    ), "reply preferences belong to conversation"
    for _old_parent, _old_key in (
        ((), "gateway"),
        (("backends", "vision"), "max_image_mb"),
        (("backends", "vision"), "file_max_age_days"),
        ((), "tools"),
        ((), "prompt"),
    ):
        _obsolete = deepcopy(b.default.model_dump())
        _place = _obsolete
        for _part in _old_parent:
            _place = _place[_part]
        _place[_old_key] = 1
        with pytest.raises(ValueError):
            _OwnerSettings.model_validate(_obsolete)

    # Persona inheritance: a group file states only its differences. Without this the shared
    # blocks are copied into every group file, and they drift - which is exactly what had
    # happened to the real ones before this was added.
    base_cfg, base_p = b.for_group(GroupId("999999"))
    grp_cfg, grp_p = b.for_group(GroupId("555"))
    assert base_p.name == "小X", "group without a file gets the default persona"
    assert grp_p.name == "小X", "group persona inherits the name"
    assert "不要 Markdown" in grp_p.system_prompt, "group persona inherits the base prompt"
    assert "这个群专门聊测试" in grp_p.system_prompt, "group persona appends its own part"
    assert grp_p.system_prompt.index("不要 Markdown") < grp_p.system_prompt.index(
        "这个群专门聊测试"
    ), "base prompt comes before the group's part"
    assert grp_p.group_knowledge.strip() == "这个群的固定资料只有这一句。", (
        "a field the group sets overrides rather than appends"
    )
    assert grp_p.name == base_p.name, "a field the group omits is inherited"
    assert base_p.group_knowledge.strip() == "测试群。", (
        "and the default is not overwritten by the group's value"
    )
    assert "这个群专门聊测试" not in base_p.system_prompt, (
        "the default persona is not polluted by the group's extra"
    )


def test_markdown_output():
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
        assert got == want, f"strip_markdown {src[:14]!r}"


def test_duration_parsing():
    # ---- /block durations: English suffixes only, typo distinguishable from absence

    assert parse_duration("30m") == _dtd(minutes=30), "30m parses"
    assert parse_duration("12H") == _dtd(hours=12), "12h parses"
    assert parse_duration(" 3 d ") == _dtd(days=3), "3d parses"
    assert parse_duration("3天") is None, "a Chinese suffix is a typo"
    assert parse_duration("30") is None, "a bare number is a typo"
    assert parse_duration("0d") is None, "zero is a typo, not a block"
    assert parse_duration("123456d") is None, "absurd width is refused"


@pytest.fixture
def debug_log_dir(monkeypatch, tmp_path):
    monkeypatch.setenv("LOG_DIR", str(tmp_path))
    _dbg.arm(0, max_rounds=DIAGNOSTIC_LIMITS.debug_max_rounds)
    yield tmp_path
    _dbg.arm(0, max_rounds=DIAGNOSTIC_LIMITS.debug_max_rounds)


def test_debug_capture(debug_log_dir):
    # ---- the debug tap: armed rounds, capped, self-disarming, never raising
    _dbg_dir = debug_log_dir

    _dbg_max = DIAGNOSTIC_LIMITS.debug_max_rounds
    assert _dbg.arm(999, max_rounds=_dbg_max) == _dbg_max, "arming caps at the configured maximum"
    assert _dbg.arm(0, max_rounds=_dbg_max) == 0 and _dbg.armed() == 0, "arming zero disarms"
    _dbg.arm(2, max_rounds=_dbg_max)

    _dbg.capture(
        "777",
        0,
        (_DbgMessage(_DbgRole.USER, "喂"),),
        _DbgTurn(text="好的", model="fake"),
    )
    assert _dbg.armed() == 1, "a captured round decrements the tap"
    _caps = list(pathlib.Path(_dbg_dir, "debug").glob("reply-777-*.json"))
    assert len(_caps) == 1, "and writes one JSON file per round"
    _cap = _json.loads(_caps[0].read_text(encoding="utf-8"))
    assert (
        _cap["prompt"][0]["content"] == "喂"
        and _cap["turn"]["text"] == "好的"
        and ("reasoning" not in _cap["turn"])
    ), "with the neutral request and response and no reasoning field"
    _dbg.capture("777", 1, (object(),), object())  # invalid contract values must not raise
    assert _dbg.armed() == 0, "a capture failure never raises and still disarms"
    _dbg.capture("777", 2, (), _DbgTurn(text="ignored"))
    assert len(list(pathlib.Path(_dbg_dir, "debug").glob("*.json"))) <= 2, (
        "an exhausted tap writes nothing"
    )


def test_nickname_word_boundaries():
    # ---- nickname: word vs substring
    nickname.initialize()
    nicks = ["小夜", "小X", "X酱"]
    nickname.register(nicks)
    assert nickname.word_hit("小夜 在吗", nicks) == "小夜", "nickname word hit"
    assert nickname.word_hit("X酱你说呢", nicks) == "X酱", "nickname variant hit"
    assert nickname.word_hit("今天听了首小夜曲", nicks) is None, "nickname NOT hit inside 小夜曲"
    # What tokenization buys, stated as the contrast: the near-miss really does contain the
    # nickname, and is still not the bot being addressed. Written out rather than through a
    # helper, because the helper existed in the production module for these two lines alone.
    assert any(n in "今天听了首小夜曲" for n in nicks), (
        "and it is a substring, which is exactly why substring matching would misfire"
    )
    assert nickname.word_hit("今天天气不错", nicks) is None, "unrelated text no hit"


def test_reply_trigger_and_initiator():
    b = config()
    # ---- when the bot answers
    # One rule, and nothing to tune. What was here before - a logistic cooldown curve and the
    # adaptive ceiling it was scaled by - decided whether to speak uninvited, and there is no
    # such decision now.
    _st = GroupState(
        group_id=GroupId("555"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    _tcfg = b.for_group(GroupId("555"))[0]

    def _tm(uid, text, is_bot=False):
        return ChatMsg(
            msg_id=f"t-{uid}-{len(text)}",
            user_id=uid,
            nickname=uid,
            text=text,
            ts=now_local(),
            is_bot=is_bot,
        )

    assert trigger.decide(_tm("u1", "在吗"), True, st=_st, cfg=_tcfg).reply, "an @ is answered"
    assert trigger.decide(_tm("u1", "小X 在吗"), False, st=_st, cfg=_tcfg).reply, (
        "a nickname is answered"
    )
    assert not trigger.decide(_tm("u1", "今天天气不错"), False, st=_st, cfg=_tcfg).reply, (
        "anything else is not"
    )

    # One message, one verdict: the initiator is the addressed message's own sender,
    # settled by the same look that decides to reply, and travels on the Decision -
    # the reply quotes their message, the spend is attributed to them. Nothing
    # downstream re-derives it, and nobody else's message can steal it.
    _m1 = _tm("u1", "小X 来评评理")
    _d = trigger.decide(_m1, False, st=_st, cfg=_tcfg)
    assert _d.reply and _d.initiator == "u1" and (_d.initiator_msg_id == _m1.msg_id), (
        "the decision names the addresser"
    )
    _da = trigger.decide(_tm("u1", "在吗"), True, st=_st, cfg=_tcfg)
    _db = trigger.decide(_tm("u2", "小X 你说说"), False, st=_st, cfg=_tcfg)
    assert _da.initiator == "u1" and _db.initiator == "u2", (
        "two callers each earn their own decision"
    )
    assert not trigger.decide(_tm("999", "在的", True), False, st=_st, cfg=_tcfg).reply, (
        "the bot's own line is never the initiator"
    )
    assert not trigger.decide(_tm("u2", "早啊"), False, st=_st, cfg=_tcfg).reply, (
        "no addresser, no initiator"
    )

    _st.muted = True
    assert not trigger.decide(_tm("u1", "在吗"), True, st=_st, cfg=_tcfg).reply, (
        "and muting outranks being addressed"
    )
    _st.muted = False


def test_segment_parsing_and_retry():
    # ---- segment parsing
    segs = [
        {"type": "at", "data": {"qq": "999"}},
        {"type": "text", "data": {"text": " 看看这个 "}},
        {
            "type": "image",
            "data": {"file": "A1B2C3D4E5F60718293A4B5C6D7E8F90.image", "url": "http://x/y"},
        },
        {"type": "mface", "data": {"emoji_id": "e123", "summary": "[开心]"}},
        {"type": "reply", "data": {"id": "555"}},
    ]
    pm = parse_segments(segs, "999", self_name="小X")
    assert pm.at_bot, "at_bot detected"
    _plain_at_me = parse_segments(
        [{"type": "text", "data": {"text": "@我 只是普通文字"}}],
        "999",
        self_name="小X",
    )
    assert not _plain_at_me.at_bot and _plain_at_me.render() == "@我 只是普通文字", (
        "typed @me is not a structured bot mention"
    )
    assert pm.reply_to == "555", "reply_to captured"
    assert pm.refs[0].key == "a1b2c3d4e5f60718293a4b5c6d7e8f90", "md5 key extracted"
    assert pm.refs[1].summary == "开心", "mface summary kept"
    # The reply segment contributes its id and no text. What it quotes is put in front of the
    # model as a pointer to a numbered line (prompt.numbered), not as an excerpt pasted
    # here - an excerpt says what was said but not which line said it.
    assert pm.render() == "@小X 看看这个 ⟦图片⟧ ⟦图片⟧", "a quote adds no text of its own"
    assert "⟦图片:猫⟧" in pm.render({0: "⟦图片:猫⟧"}), "resolved render"

    # Synthetic image and voice payloads with NapCat's wire shapes.
    real_img = parse_segments(
        [
            {
                "type": "image",
                "data": {
                    "url": "https://multimedia.nt.qq.com.cn/download?x=1",
                    "file": "ABCDEF0123456789ABCDEF0123456789.png",
                    "summary": "",
                    "sub_type": 0,
                    "file_size": "222164",
                },
            }
        ],
        "999",
    )
    assert real_img.refs[0].key == "abcdef0123456789abcdef0123456789", (
        "md5 read from a .png filename, not just .image"
    )
    assert real_img.refs[0].summary is None, (
        "an empty summary does not masquerade as a sticker name"
    )

    real_rec = parse_segments(
        [
            {
                "type": "record",
                "data": {
                    "url": "https://multimedia.nt.qq.com.cn/download?y=2",
                    "file": "0123456789abcdef0123456789abcdef.amr",
                    "path": "/app/.config/QQ/nt_qq_example/nt_data/Ptt/2030-01/Ori/01234567.amr",
                    "file_size": "10726",
                },
            }
        ],
        "999",
    )
    assert isinstance(real_rec.refs[0], AudioRef), "voice is an audio ref"
    # The path a clip arrives with is not kept: the file there is SILK, which no
    # transcriber reads, and get_record's WAV is the only route ever taken.
    assert not hasattr(real_rec.refs[0], "path"), "a clip's local path is not carried"

    # reply and forward segments carry content and must parse, not drop
    quoted = parse_segments(
        [{"type": "reply", "data": {"id": "123"}}, {"type": "text", "data": {"text": "这个"}}],
        "999",
    )
    assert quoted.reply_to == "123", "reply records the quoted id"
    # It costs no fetch and adds no text: which message this quotes is answered against the
    # lines the model is already being shown, by number. See prompt.numbered.
    assert quoted.render() == "这个", "and adds nothing to the text"
    assert not quoted.refs, "and asks for no lookup"

    fwd = parse_segments([{"type": "forward", "data": {"id": "abc"}}], "999")
    assert fwd.render() == "⟦转发的聊天记录⟧" and (not fwd.refs), (
        "a forward without its content is a bare marker, not a fetch"
    )

    at_other = parse_segments([{"type": "at", "data": {"qq": "12345"}}], "999")
    assert isinstance(at_other.refs[0], AtRef), "a bare @qq becomes a ref to resolve"
    assert "@12345" in at_other.render(), "unresolved @ falls back to the number"
    at_named = parse_segments([{"type": "at", "data": {"qq": "12345", "name": "阿强"}}], "999")
    assert not at_named.refs and "@阿强" in at_named.render(), "@ with a name needs no lookup"
    _same_name_mentions = [("member-a", "张伟"), ("member-b", "张伟")]
    _numbered_ats = number_at_mentions(
        "@张伟 和 @张伟 都看看",
        _same_name_mentions,
        {"member-a": 3, "member-b": 7}.get,
    )
    assert _numbered_ats == "@张伟⟦3⟧ 和 @张伟⟦7⟧ 都看看", (
        "same-name @ targets keep distinct prompt-local numbers"
    )
    _self_at = number_at_mentions("@小X 在吗", [("999", "小X")], lambda _account: 0)
    assert _self_at == "@小X⟦0⟧ 在吗", "structured bot mentions retain reserved display zero"
    _missing_label = number_at_mentions(
        "@我 @小X 在吗",
        [("999", "")],
        lambda _account: 0,
    )
    assert _missing_label == "@我 @小X 在吗", (
        "a missing at label never marks preceding member-typed text"
    )
    assert (
        at_mentions(
            [
                {"type": "at", "data": {"qq": "member-a", "name": "张伟"}},
                {"type": "at", "data": {"qq": "member-b", "name": "张伟"}},
            ]
        )
        == _same_name_mentions
    ), "raw at segments retain ordered account identity"
    at_all = parse_segments([{"type": "at", "data": {"qq": "all"}}], "999")
    assert not at_all.refs and "@全体成员" in at_all.render(), "@all is not looked up"

    card = parse_segments(
        [
            {
                "type": "json",
                "data": {
                    "data": json.dumps(
                        {
                            "prompt": "[分享]",
                            "meta": {"news": {"title": "标题党", "desc": "正文摘要"}},
                        }
                    )
                },
            }
        ],
        "999",
    )
    assert "标题党" in card.render() and "正文摘要" in card.render(), (
        "share card yields its title and description"
    )
    bad_card = parse_segments([{"type": "json", "data": {"data": "not json"}}], "999")
    assert bad_card.render() == "⟦卡片消息⟧", "an unparseable card degrades quietly"

    assert real_img.needs_model and (not quoted.needs_model) and (not at_other.needs_model), (
        "only media costs a model call"
    )

    # Transient paid failures stay retryable: the describing path marks a rate-limited /
    # cap-blocked / failed turn-away with an explicit retryable result. The coordinator
    # keeps retry state separately from the text projected into the transcript.

    assert _MD.settled(real_img, {0: _Resolution("⟦图片:猫⟧")}), "a described slot settles"
    assert not _MD.settled(real_img, {0: _Resolution("⟦图片:猫⟧", retryable=True)}), (
        "a retryable fallback does not settle"
    )
    assert not _MD.settled(real_img, {}), "an absent paid slot does not settle"
    assert _MD.settled(quoted, {}), "a free-only message settles trivially"
    assert _Resolution("⟦图片:猫⟧", retryable=True).text == "⟦图片:猫⟧", (
        "the fallback text remains available"
    )

    # The bot's own lines render as the send call that sent them, followed by its
    # result: what the model reads of its own output is the shape it should produce.
    # Members' lines keep the transcript form, quote mark and member number included.


def test_history_projection_and_archive():
    _qa = ChatMsg(msg_id="q1", user_id="u1", nickname="阿强", text="在吗", ts=now_local())
    _qb = ChatMsg(
        msg_id="q2",
        user_id="999",
        nickname="小X",
        text="在的",
        ts=now_local(),
        is_bot=True,
        reply_to="q1",
        at=[("u1", "阿强")],
    )
    _qc = ChatMsg(
        msg_id="q3", user_id="u2", nickname="阿花", text="哦哦", ts=now_local(), reply_to="q1"
    )
    _qn, _qm = prompt.numbered([_qa, _qb, _qc])
    _qp = _MN(self_id="999", lookup=_test_db.identities.holder_ids_for_accounts)
    assert _qp.number("999") == 0 and _qp.known("missing") is None and (_qp.number("") is None), (
        "member numbering distinguishes bot zero from unknown"
    )
    assert _qp.account(0) is None and _qp.accounts(0) == [], (
        "bot zero is display-only and never addressable"
    )
    prompt.number_people(_qp, [], [_qa, _qb, _qc], None)
    _qh = prompt.render_history(
        [_qa, _qb, _qc],
        _qn,
        _qm,
        people=_qp,
        max_text_chars=_test_db.test_bundle().default.conversation.max_text_chars_per_message,
    )

    _qcall = _qh[1] if isinstance(_qh[1], _PromptCall) else None
    assert (
        _qcall is not None
        and _qcall.name == "send_message"
        and (
            json.loads(_qcall.arguments or "{}")
            == {
                "content": [
                    {"type": "reply", "data": {"line": 1}},
                    {"type": "at", "data": {"member": 1}},
                    {"type": "text", "data": {"text": "在的"}},
                ]
            }
        )
    ), "the bot's own line renders as its send call"
    _qresult = _qh[2] if isinstance(_qh[2], _PromptResult) else None
    assert (
        _qresult is not None
        and _qcall is not None
        and (_qresult.call_id == _qcall.call_id)
        and str(_qresult.output).startswith("已发送：#2 ⟦")
        and ("依据" not in str(_qresult.output))
    ), "and its result carries only the line number and time"
    assert isinstance(_qh[3], _PromptMessage) and "阿花⟦2⟧: ⟦回复 #1⟧" in _qh[3].content, (
        "a member's line keeps its quote mark and wears its member number"
    )

    _qd = ChatMsg(
        msg_id="q4",
        user_id="999",
        nickname="小X",
        text="⟦骰子:4点⟧",
        ts=now_local(),
        is_bot=True,
        outbound=(_PromptDice(),),
    )
    _qd_items = prompt.own_line(
        _qd,
        nums={"q4": 4},
        people=_qp,
        max_text_chars=_test_db.test_bundle().default.conversation.max_text_chars_per_message,
    )
    _qd_call, _qd_result = _qd_items[-2:]
    assert isinstance(_qd_call, _PromptCall) and json.loads(_qd_call.arguments or "{}") == {
        "content": [{"type": "dice", "data": {}}]
    }, "an observed random result stays out of the legal send arguments"
    assert isinstance(_qd_result, _PromptResult) and "平台显示：⟦骰子:4点⟧" in str(
        _qd_result.output
    ), "and the platform result is visible on the historical tool result"

    _archive_row = {
        "id": _uuid.uuid4(),
        "platform_event_id": "archive-1",
        "group_id": 123,
        "event_type": "message",
        "occurred_at": now_local(),
        "created_at": now_local(),
        "plain_text": "@阿花 已查到",
        "archive_schema": 1,
        "payload": {
            "message_id": "archive-1",
            "author_kind": "bot",
            "self_id": "999",
            "sender": {
                "user_id": "999",
                "nickname": "小X",
                "card": "",
                "role": "member",
            },
            "segments": [
                {"type": "at", "data": {"qq": "u2", "name": "阿花"}},
                {"type": "text", "data": {"text": " 已查到"}},
            ],
            "typed_text": "已查到",
            "reply_to": "member-1",
            "to_me": False,
        },
    }
    _archived = _ArchivedMessage.from_row(_archive_row)
    assert (
        _archived.author_kind is _AuthorKind.BOT
        and _archived.sender.display_name == "小X"
        and (_archived.text == "@阿花 已查到")
        and (_archived.mentions == (("u2", "阿花"),))
    ), "the canonical archive shares author, sender, text and mentions"
    with pytest.raises(TypeError):
        _archived.segments[0]["type"] = "text"
    with pytest.raises(ValueError):
        _ArchivedMessage.from_row({**_archive_row, "archive_schema": 0})

    # Live state trusts the database admission verdict and owns no second idempotency rule.
    _dupe = GroupState(
        group_id=GroupId("777"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    _dupe.add(ChatMsg(msg_id="r1", user_id="u1", nickname="阿强", text="第一句", ts=now_local()))
    _dupe.add(ChatMsg(msg_id="r1", user_id="u1", nickname="阿强", text="第二句", ts=now_local()))
    assert len(_dupe.recent) == 2, "live state performs no independent event deduplication"


@pytest.fixture
def prompt_context():
    b = config()
    cfg, persona = b.for_group(GroupId("12345"))
    st = GroupState(
        group_id=GroupId("12345"),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    for i in range(25):
        st.add(
            ChatMsg(
                msg_id=f"m{i}", user_id="u1", nickname="阿强", text=f"第{i}条消息", ts=now_local()
            )
        )
    asked = ChatMsg(msg_id="m99", user_id="u2", nickname="阿花", text="小X你在吗", ts=now_local())
    st.add(asked)
    return st, asked, cfg, persona


def test_prompt_authority_and_clock(prompt_context):
    st, asked, cfg, persona = prompt_context
    # ---- prompt assembly + ordering
    msgs = prompt.assemble(
        persona=persona,
        cfg=cfg,
        st=st,
        msg=asked,
        profiles=[
            {
                "user_id": "u1",
                "nickname": "阿强",
                "memory_hints": ("事实：爱打游戏（置信度 0.27）",),
            }
        ],
        prompts=_test_db.test_bundle().prompts,
        clock=_test_db.clock,
    )
    assert isinstance(msgs[0], _PromptMessage) and msgs[0].role is _DbgRole.SYSTEM, "system first"
    assert "【怎样发言】" in msgs[0].content, "global policy stays in system"
    assert isinstance(msgs[1], _PromptMessage) and msgs[1].role is _DbgRole.DEVELOPER, (
        "group context follows as developer"
    )
    assert "小X" in msgs[1].content, "persona in developer"
    assert "爱打游戏" in msgs[1].content, "profile in developer"
    assert all(
        isinstance(item, _PromptMessage) and item.role in (_DbgRole.USER, _DbgRole.ASSISTANT)
        for item in msgs[2:-1]
    ), "history follows both authority layers"
    tail = msgs[-1].content
    assert isinstance(msgs[-1], _PromptMessage) and msgs[-1].role is _DbgRole.USER, (
        "tail is last user msg"
    )
    assert "小X你在吗" in tail, "current msg in tail"

    # The clock. A model has no time of its own, so it has to be told - but it must sit past
    # the cache boundary: in the system block it would change every minute and cost the
    # prefix cache on every call.

    assert st.history_anchor is not None
    assert describe_now() in tail, "current time is in the prompt"
    assert "当前时间：" not in msgs[0].content, "clock is NOT in the cached system block"
    assert any(d in tail for d in WEEKDAYS), "clock names the weekday"

    # A NUL inside a message is refused by PostgreSQL in text and jsonb alike, so it has to
    # leave at the envelope: the rendered text through defang, the verbatim segments
    # through the event parser. Before this, one stray NUL cost the whole message its
    # place in the archive (three times in a month).


def test_onebot_envelope_and_scrubbing():
    assert _util.defang("a\x00b⟦c⟧") == "ab[c]", "defang drops NUL"
    assert _util.scrub_nul({"a": ["x\x00", {"b": "\x00y"}], "n": 3}) == {
        "a": ["x", {"b": "y"}],
        "n": 3,
    }, "scrub_nul walks a nested structure"

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
            return [
                SimpleNamespace(
                    type="text",
                    data={"text": "hi\x00there"},
                )
            ]

    _gm = _GM.from_event(_Ev(), "999", clock=_test_db.clock)
    _gm_with_mention = _gm.with_bot_mention("小X")
    _pl_json = json.dumps(_gm_with_mention.as_payload(), ensure_ascii=False)
    assert "\x00" not in _pl_json and _gm.sender.nickname == "王大锤", (
        "an event carrying NUL is archived without it"
    )
    assert _Ev.calls == 1 and _gm.reply_to_message_id == "9", (
        "the live adapter message is captured exactly once"
    )
    assert _gm.typed_text == "hithere" and _gm_with_mention.typed_text == "hithere", (
        "the raw typed text survives mention restoration"
    )
    assert _gm.segments[0]["type"] == "text" and _gm_with_mention.segments[0] == {
        "type": "at",
        "data": {"qq": "999", "name": "小X"},
    }, "an adapter-stripped self mention is restored on the detached envelope"

    class _SelfEv(_Ev):
        user_id = 999
        sender = {"user_id": 10001, "nickname": "小X", "card": ""}

    _self_gm = _GM.from_event(_SelfEv(), "999", clock=_test_db.clock)
    assert (
        _self_gm.author_kind is _EnvelopeAuthor.BOT
        and _self_gm.outbound_schema == 1
        and (_self_gm.sender.user_id == "999")
    ), "top-level authorship classifies a reported self message"
    assert _gm.author_kind is _EnvelopeAuthor.MEMBER and _gm.sender.user_id == "10001", (
        "nested sender metadata cannot fabricate self authorship"
    )
    _sent_event = _SentEvent.model_validate(
        {
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
        }
    )
    _sent_gm = _GM.from_event(_sent_event, "999", clock=_test_db.clock)
    assert _sent_gm.author_kind is _EnvelopeAuthor.BOT and _sent_gm.segments == [
        {"type": "dice", "data": {"result": "2"}}
    ], "NapCat message_sent keeps the ordinary group-message interface"


def test_malformed_segments_and_text_helpers():
    # The per-line cut never leaves a marker half open: an unbalanced bracket in the
    # window is the one thing defang rules out everywhere else.
    assert _util.cut_text("你看 ⟦图片:一只猫⟧", 8) == "你看 ", "cut_text keeps a whole marker"
    assert _util.cut_text("短", 8) == "短", "cut_text leaves short text alone"
    assert _util.cut_text("一二三四五", 3) == "一二三", "cut_text cuts plain text at the limit"
    assert _util.cut_text("a ⟦x ⟦y⟧⟧ tail", 6) == "a ", "cut_text steps back out of nested markers"

    # The parser is total: a forward entry with a stamp no calendar can hold, a
    # numeric faceText and a dice result all still render.
    _odd = parse_segments(
        [
            {
                "type": "forward",
                "data": {
                    "id": "f",
                    "content": [
                        {
                            "sender": {"nickname": "王大锤"},
                            "time": 10**14,
                            "message": [{"type": "text", "data": {"text": "早"}}],
                        }
                    ],
                },
            },
            {"type": "face", "data": {"id": "14", "raw": {"faceText": 5}}},
            {"type": "dice", "data": {"result": 6}},
        ],
        "999",
    )
    _odd_text = _odd.render()
    assert "王大锤: 早" in _odd_text and "⟦转发的聊天记录 1条⟧" in _odd_text, (
        "an out-of-range forward stamp leaves the entry untimed"
    )
    assert "⟦表情:5⟧" in _odd_text, "a numeric faceText still renders"
    assert "⟦骰子:6点⟧" in _odd_text, "a dice result renders its number"

    assert _mdt("#话题 今天") == "#话题 今天", "a hashtag is not a heading"
    _hd = _mdt("## 标题\n正文")
    assert _hd == "标题\n正文", "a real heading loses its hashes"

    # Parsing is total because it runs before database admission: malformed segment data
    # degrades rather than raising and preventing an otherwise archivable event.
    _odd = parse_segments(
        [
            {"type": "json", "data": {"data": "[1, 2]"}},
            {"type": "json", "data": {"data": '{"meta": {"x": {"title": 7}}, "prompt": 3}'}},
            {"type": "image", "data": {"file": "a.jpg", "file_size": "big"}},
            {"type": "text", "data": "not a dict"},
        ],
        "999",
    )
    assert _odd.parts[0] == "⟦卡片消息⟧", "a card that is not an object degrades to the bare marker"
    assert _odd.parts[1] == "⟦分享:7⟧", "a card with non-string fields still renders"
    assert _odd.refs[0].size is None, "a non-numeric size is no size"
    # The trigger reads what was typed, not the render: a share card whose title
    # carries the nickname is not somebody addressing the bot.
    _card_nick = parse_segments(
        [
            {"type": "json", "data": {"data": '{"meta": {"x": {"title": "小X 教程"}}}'}},
            {"type": "text", "data": {"text": "看这个"}},
        ],
        "999",
    )
    assert _card_nick.typed_text == "看这个", "typed text is the text segments alone"
    assert "小X" in _card_nick.render(), "while the render carries the card"

    # A log line is one line: an HTTP client's "for more information see <link>" second
    # line would otherwise appear in the log as a separate, unlabelled event.
    assert (
        _util.why(RuntimeError("bad request\nFor more information check: https://x"))
        == "RuntimeError: bad request"
    ), "why() keeps the first line of a multi-line message"
    assert _util.why(TimeoutError()) == "TimeoutError", "why() still names a message-less exception"

    # No picture rides in the prompt: every marker carries a number and the model opens
    # what it wants to see with open_images. Every picture in the prompt carries a
    # number, and the number is what open_images resolves. It has to be one coordinate
    # rather than two ("the second picture in message #12"), because two is a pair the
    # model gets to miscount independently. Numbered oldest first, like the line
    # numbers, so both count the same direction - and stickers share the run, so there


def test_numbered_images_and_on_demand_opening(prompt_context):
    st, asked, cfg, persona = prompt_context
    # is one rule rather than two.

    _p1 = ChatMsg(
        msg_id="p1",
        user_id="u",
        nickname="王大锤",
        text="看 ⟦图片:一只橘猫⟧",
        ts=now_local(),
        image_refs=[_IR(key="a" * 32)],
    )
    _p2 = ChatMsg(msg_id="p2", user_id="v", nickname="阿旺", text="没有图的一句", ts=now_local())
    _p3 = ChatMsg(
        msg_id="p3",
        user_id="u",
        nickname="王大锤",
        text="还有 ⟦图片:一条狗⟧ 和 ⟦表情:笑到打滚⟧",
        ts=now_local(),
        image_refs=[_IR(key="b" * 32), _IR(key="c" * 32)],
    )
    _per, _by = prompt.numbered_images([_p1, _p2, _p3])
    assert _per == {"p1": [1], "p3": [2, 3]}, (
        "pictures are numbered oldest first, stickers in the same run"
    )
    assert [(m.msg_id, i) for m, i in (_by[1], _by[2], _by[3])] == [
        ("p1", 0),
        ("p3", 0),
        ("p3", 1),
    ], "and the number maps back to the picture it names"
    assert _p3.render(seq=9, pic_nums=_per["p3"]).endswith(
        "还有 ⟦图片2:一条狗⟧ 和 ⟦表情3:笑到打滚⟧"
    ), "the number rides inside the marker, description untouched"
    # A line whose markers outnumber its references (an old archived rendering, a
    # record fetched by id after numbering) is left alone: a number that opened the
    # wrong picture would be worse than none.
    _pf = ChatMsg(
        msg_id="pf",
        user_id="u",
        nickname="小红",
        text="⟦图片:我的图⟧ ⟦转发的聊天记录：李芳: ⟦图片⟧⟧",
        ts=now_local(),
        image_refs=[_IR(key="d" * 32)],
    )
    assert "⟦图片1:" not in _pf.render(seq=1, pic_nums=[1]), (
        "a line whose markers outnumber its pictures stays unnumbered"
    )
    _mn = prompt.assemble(
        persona=persona,
        cfg=cfg,
        st=st,
        msg=asked,
        profiles=[],
        prompts=_test_db.test_bundle().prompts,
        clock=_test_db.clock,
    )
    assert all(
        isinstance(item, _PromptMessage) and isinstance(item.content, str) for item in _mn
    ), "every prompt message is a plain string - pictures are opened, never pushed"
    assert isinstance(_mn[0], _PromptMessage) and "open_images" in _mn[0].content, (
        "the legend tells the model to open pictures by number"
    )


def test_forwarded_messages_and_bounds():
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
    _inner = {
        "type": "forward",
        "data": {
            "id": "in",
            "content": [
                _node(_t0 - 86400, "王大锤", _txt("已经来啦")),
                _node(_t0 - 86000, "王大锤", {"type": "at", "data": {"qq": "999"}}, _txt("草")),
            ],
        },
    }
    _outer = [
        _txt("看这个"),
        {
            "type": "forward",
            "data": {
                "id": "out",
                "content": [
                    _node(_t0 - 3600, "李芳", _txt("苹果的下载榜一了")),
                    _node(_t0 - 3500, "李芳", _img("a")),
                    _node(_t0 - 3400, "李芳", _inner),
                    _node(_t0 - 3300, "李芳", {"type": "record", "data": {"file": "v.amr"}}),
                ],
            },
        },
        _txt("怎么看"),
    ]
    _fw = parse_segments(_outer, "999")
    _fw_text = _fw.render()
    _fw_lines = _fw_text.splitlines()
    assert _fw_lines[0] == "看这个 ⟦转发的聊天记录 4条⟧" and _fw_lines[-1] == "怎么看", (
        "the record renders as a block under the carrying message"
    )
    assert _fw_lines[1].startswith("  ⟦") and _fw_lines[1].endswith("李芳: 苹果的下载榜一了"), (
        "entries are stamped and named, one indent level in"
    )
    assert any(ln.startswith("    ⟦") and ln.endswith("王大锤: 已经来啦") for ln in _fw_lines), (
        "a record inside the record goes one level deeper"
    )
    assert (
        len(_fw.pictures) == 1
        and _fw.pictures[0].nested
        and _fw.pictures[0].free
        and (_fw.pictures[0].key == "a" * 32)
    ), "a forwarded picture is the carrying message's own reference"
    assert "⟦图片7⟧" in ChatMsg(
        msg_id="fw",
        user_id="u",
        nickname="n",
        text=_fw_text,
        ts=now_local(),
        image_refs=_fw.pictures,
    ).render(pic_nums=[7]), "and its marker takes a number like any other"
    assert (
        "  ⟦" in _fw_text
        and _fw_text.count("⟦语音⟧") == 1
        and (not any(not r.free for r in _fw.refs))
    ), "a forwarded voice clip is a bare marker, never transcribed"
    assert (
        not _fw.at_bot
        and any(isinstance(r, AtRef) and r.ident == "999" for r in _fw.refs)
        and (_fw.mentions == [])
    ), "an @ inside the record names the person, never addresses the bot"
    assert _fw.typed_text == "看这个 怎么看", "what was typed excludes the record"
    # The bounds: lines in all, depth, and characters - the rest is said as a count, and
    # a picture in an entry that is not rendered is not registered.
    _tight = replace(PARSE_LIMITS, forward_lines=2)
    _fw2 = parse_segments(_outer, "999", limits=_tight)
    assert "⟦其余2条未显示⟧" in _fw2.render() and "已经来啦" not in _fw2.render(), (
        "past the line bound the rest is counted, not rendered"
    )
    _shallow = replace(PARSE_LIMITS, forward_depth=1)
    _fw3 = parse_segments(_outer, "999", limits=_shallow)
    assert "⟦转发的聊天记录 2条⟧" in _fw3.render() and "已经来啦" not in _fw3.render(), (
        "past the depth bound a record shows only its header"
    )
    _short = replace(PARSE_LIMITS, forward_chars=100)
    _fw4 = parse_segments(_outer, "999", limits=_short)
    assert "未显示⟧" in _fw4.render(), "past the character bound the rest is counted too"
    _onlyfirst = replace(PARSE_LIMITS, forward_lines=1)
    _fw5 = parse_segments(_outer, "999", limits=_onlyfirst)
    assert _fw5.pictures == [] and "⟦图片⟧" not in _fw5.render(), (
        "a picture in an unrendered entry is not registered"
    )


def test_typed_outbound_sends_and_history():
    # ---- ordered outbound QQ segments -----------------------------------------

    _send_spec = _send_def(
        prompts=_test_db.test_bundle().prompts, cfg=_test_db.test_bundle().default
    )
    _send_description = _send_spec.description
    assert (
        "14=微笑" in _send_description
        and "326=生气" in _send_description
        and ("{{FACE_CATALOG}}" not in _send_description)
    ), "send tool expands the fixed QQ face catalog"
    _send_contract = json.dumps(_send_spec.parameters, ensure_ascii=False)
    _send_types = set(
        _send_spec.parameters["properties"]["content"]["items"]["discriminator"]["mapping"]
    )
    assert (
        "mface" not in _send_description
        and "商城表情" not in _send_description
        and ("mface" not in _send_contract)
    ), "send tool does not expose market faces to the model"
    assert _send_types == {
        "text",
        "at",
        "reply",
        "face",
        "dice",
        "rps",
        "contact_member",
        "contact_group",
    } and all(name not in _send_description for name in ("music", "music_custom", "json")), (
        "send tool hides rich card segments from the model"
    )

    _out_people = _MN(self_id="bot", lookup=_test_db.identities.holder_ids_for_accounts)
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
            "send_message",
            json.dumps(
                {"content": content}
                if not more
                else {"messages": [{"content": item} for item in (content, *more)]},
                ensure_ascii=False,
            ),
        )

    _out_content = [
        {"type": "text", "data": {"text": "请"}},
        {"type": "at", "data": {"member": 2}},
        {"type": "text", "data": {"text": "看这里"}},
        {"type": "face", "data": {"id": 14}},
        {"type": "reply", "data": {"line": 7}},
    ]
    _out_reply, _out_note = _parse_send(
        _out_call(_out_content),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _out_reply is not None, "ordered send parses the current composable segments"
    _out_types = tuple(type(segment) for segment in _out_reply.segments)
    assert _out_types[:3] == (_OutText, _OutAt, _OutText), "an @ keeps its arbitrary position"
    assert all(kind in _out_types for kind in (_OutFace, _OutReply)), (
        "current segments remain closed typed variants"
    )
    _out_wire = [_to_onebot(segment) for segment in _out_reply.segments]
    assert [segment["type"] for segment in _out_wire] == ["text", "at", "text", "face", "reply"], (
        "typed variants project to literal OneBot nested segments"
    )
    assert _out_wire[1]["data"]["qq"] == "member-b" and _out_wire[-1]["data"]["id"] == "line-7", (
        "member and message numbers resolve against this snapshot"
    )

    _special = []
    for _item, _kind in (
        ({"type": "dice", "data": {}}, _OutDice),
        ({"type": "rps", "data": {}}, _OutRps),
        ({"type": "contact_member", "data": {"member": 1}}, _OutContact),
        ({"type": "contact_group", "data": {}}, _OutContact),
    ):
        _draft, _note = _parse_send(
            _out_call([_item]),
            people=_out_people,
            lines={7: _out_line},
            group_id=GroupId("123"),
            max_text_chars=2000,
        )
        _special.append(_draft is not None and isinstance(_draft.segments[0], _kind))
    assert all(_special), "standalone segments each produce one QQ message"
    _batch_reply, _ = _parse_send(
        _out_call([{"type": "dice", "data": {}}], [{"type": "rps", "data": {}}]),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _batch_reply is None, "legacy batch arguments are rejected"
    _too_many, _ = _parse_send(
        _out_call([{"type": "face", "data": {"id": 14}} for _ in range(33)]),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _too_many is None, "the segment bound rejects an oversized message"

    _guessed_mface, _guessed_note = _parse_send(
        _out_call(
            [
                {
                    "type": "mface",
                    "data": {
                        "package_id": "pkg",
                        "emoji_id": "emoji",
                        "key": "key",
                    },
                }
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _guessed_mface is None and "无效" in _guessed_note, (
        "a guessed market-face segment rejects the entire model send"
    )
    for _hidden_kind, _hidden_data in (
        ("music", {"platform": "qq", "id": "42"}),
        (
            "music_custom",
            {
                "url": "https://example.invalid/song",
                "audio": "https://example.invalid/song.mp3",
                "title": "测试曲",
                "image": "https://example.invalid/cover.jpg",
            },
        ),
        ("json", {"payload": {"app": "test"}}),
    ):
        _hidden_reply, _ = _parse_send(
            _out_call([{"type": _hidden_kind, "data": _hidden_data}]),
            people=_out_people,
            lines={7: _out_line},
            group_id=GroupId("123"),
            max_text_chars=2000,
        )
        assert _hidden_reply is None, f"hidden {_hidden_kind} rejects current sends"
    _exclusive_mix, _ = _parse_send(
        _out_call(
            [
                {"type": "text", "data": {"text": "掷一下"}},
                {"type": "dice", "data": {}},
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _exclusive_mix is None, "an exclusive segment rejects companions"
    _long_text, _ = _parse_send(
        _out_call(
            [
                {"type": "text", "data": {"text": "123"}},
                {"type": "text", "data": {"text": "456"}},
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=5,
    )
    assert _long_text is None, "the configured text bound applies across all text segments"
    _string_member, _ = _parse_send(
        _out_call(
            [
                {"type": "at", "data": {"member": "1"}},
                {"type": "text", "data": {"text": "不接受字符串编号"}},
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _string_member is None, "member and line numbers are strict integers"
    for _label, _content in (
        (
            "a reply-only or blank message has no visible content",
            [
                {"type": "reply", "data": {"line": 7}},
                {"type": "text", "data": {"text": "   "}},
            ],
        ),
        (
            "a message has at most one reply segment",
            [
                {"type": "reply", "data": {"line": 7}},
                {"type": "reply", "data": {"line": 7}},
                {"type": "text", "data": {"text": "重复引用"}},
            ],
        ),
        (
            "a message has at most five at segments",
            [
                *({"type": "at", "data": {"member": 1}} for _ in range(6)),
                {"type": "text", "data": {"text": "太多"}},
            ],
        ),
        (
            "a message has at most 32 segments",
            [
                *({"type": "face", "data": {"id": 14}} for _ in range(33)),
            ],
        ),
        (
            "unknown nested fields are rejected",
            [
                {"type": "text", "data": {"text": "内容", "extra": True}},
            ],
        ),
    ):
        _bounded, _ = _parse_send(
            _out_call(_content),
            people=_out_people,
            lines={7: _out_line},
            group_id=GroupId("123"),
            max_text_chars=2000,
        )
        assert _bounded is None, _label
    _historical = _from_onebot(
        [
            {
                "type": "mface",
                "data": {
                    "emoji_package_id": "pkg",
                    "emoji_id": "emoji",
                    "key": "key",
                    "summary": "[历史表情]",
                },
            },
            {"type": "music", "data": {"type": "qq", "id": "42"}},
            {
                "type": "music",
                "data": {
                    "type": "custom",
                    "url": "https://example.invalid/song",
                    "audio": "https://example.invalid/song.mp3",
                    "title": "测试曲",
                    "image": "https://example.invalid/cover.jpg",
                },
            },
            {"type": "json", "data": {"data": {"app": "test"}}},
        ]
    )
    assert (
        len(_historical) == 4
        and isinstance(_historical[0], _OutMarketFace)
        and isinstance(_historical[1], _OutMusic)
        and isinstance(_historical[2], _OutCustomMusic)
        and isinstance(_historical[3], _OutJson)
    ), "archived hidden segments remain readable"
    with pytest.raises(TypeError):
        _to_onebot(_historical[0])
    _hidden_history = ChatMsg(
        msg_id="sent-hidden",
        user_id="bot",
        nickname="小X",
        text="平台渲染后的历史卡片",
        ts=now_local(),
        is_bot=True,
        outbound=_historical,
    )
    _hidden_items = prompt.own_line(
        _hidden_history,
        nums={"sent-hidden": 9},
        people=_out_people,
        max_text_chars=_test_db.test_bundle().default.conversation.max_text_chars_per_message,
    )
    assert (
        len(_hidden_items) == 1
        and isinstance(_hidden_items[0], _PromptMessage)
        and _hidden_items[0].content.startswith("平台显示：")
        and ("平台渲染后的历史卡片" in _hidden_items[0].content)
    ), "historical-only segments project as display text rather than hidden tool arguments"

    _repeated, _ = _parse_send(
        _out_call(
            [
                {"type": "at", "data": {"member": 1}},
                {"type": "text", "data": {"text": "和"}},
                {"type": "at", "data": {"member": 1}},
                {"type": "text", "data": {"text": "都来"}},
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _repeated is not None and [
        segment.account for segment in _repeated.segments if isinstance(segment, _OutAt)
    ] == ["member-a", "member-a"], "repeated mentions are preserved rather than deduplicated"
    _invalid_member, _ = _parse_send(
        _out_call(
            [
                {"type": "at", "data": {"member": 99}},
                {"type": "text", "data": {"text": "不会偷发"}},
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _invalid_member is None, "an unknown member number rejects the entire send"
    _zero_member, _ = _parse_send(
        _out_call(
            [
                {"type": "at", "data": {"member": 0}},
                {"type": "text", "data": {"text": "不能给机器人自己发 at"}},
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _zero_member is None, "reserved bot zero is not an addressable send target"
    _invalid_line, _ = _parse_send(
        _out_call(
            [
                {"type": "reply", "data": {"line": 99}},
                {"type": "text", "data": {"text": "不会偷发"}},
            ]
        ),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _invalid_line is None, "an unknown line number rejects the entire send"
    _unsupported, _ = _parse_send(
        _out_call([{"type": "xml", "data": {"data": "<msg/>"}}]),
        people=_out_people,
        lines={7: _out_line},
        group_id=GroupId("123"),
        max_text_chars=2000,
    )
    assert _unsupported is None, "unsupported raw segment kinds cannot cross the closed schema"

    _out_history = ChatMsg(
        msg_id="sent-1",
        user_id="bot",
        nickname="小X",
        text="请@乙看这里",
        ts=now_local(),
        is_bot=True,
        outbound=_out_reply.segments,
    )
    _out_nums = {"line-7": 7, "sent-1": 8}
    _out_first = prompt.own_line(
        _out_history,
        nums=_out_nums,
        people=_out_people,
        max_text_chars=_test_db.test_bundle().default.conversation.max_text_chars_per_message,
    )[0]
    _out_second = prompt.own_line(
        _out_history,
        nums=_out_nums,
        people=_out_people,
        max_text_chars=_test_db.test_bundle().default.conversation.max_text_chars_per_message,
    )[0]
    assert (
        isinstance(_out_first, _PromptCall)
        and isinstance(_out_second, _PromptCall)
        and (_out_first.arguments == _out_second.arguments)
    ), "structured history replay is byte-stable"
    _out_history_args = json.loads(_out_first.arguments)
    assert set(_out_history_args) == {"content"} and bool(_out_history_args["content"]), (
        "each archived bot line projects as a single send"
    )

    # Runtime prompt wording is one YAML bundle with a closed code-owned key and slot


def test_prompt_catalog_validation():
    b = config()
    # contract. Any malformed template rejects the entire candidate configuration before it
    # can replace the active bundle.

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
        _sy.write_text(
            _sy.read_text(encoding="utf-8")
            .replace("prompts_dir: ../../../config/prompts", "prompts_dir: prompts")
            .replace(
                "predicates_file: ../../../config/predicates.yaml",
                "predicates_file: predicates.yaml",
            ),
            encoding="utf-8",
        )
        _bundle_path = _cd / "prompts" / "prompts.yaml"
        _raw_bundle = _yaml.safe_load(_bundle_path.read_text(encoding="utf-8"))
        _raw_bundle["templates"]["vision_system"] = "换一种描述方式。"
        _bundle_path.write_text(
            _yaml.safe_dump(_raw_bundle, allow_unicode=True, sort_keys=False),
            encoding="utf-8",
        )
        _bo = _lb(config_dir=_cd)
        assert _bo.prompts.source(_PromptKey.VISION_SYSTEM) == "换一种描述方式。", (
            "editing the single prompt bundle changes what the catalog serves"
        )
        assert set(_bo.prompts.templates) == set(_PS), "every closed template key was loaded"
        _bundle_text = _bundle_path.read_text(encoding="utf-8")
        _bundle_path.write_text(
            _bundle_text.replace(
                "  vision_system:",
                "  vision_system: duplicate must fail\n  vision_system:",
                1,
            ),
            encoding="utf-8",
        )
        with pytest.raises(ValueError, match="duplicate key"):
            _lb(config_dir=_cd)
        _bundle_path.write_text(_bundle_text, encoding="utf-8")
        _raw_bundle["templates"]["no_such_key"] = "不该被接受。"
        _bundle_path.write_text(
            _yaml.safe_dump(_raw_bundle, allow_unicode=True, sort_keys=False),
            encoding="utf-8",
        )
        with pytest.raises(ValueError, match="no_such_key"):
            _lb(config_dir=_cd)
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
            "system_prompt: 测试人设\ntools:\n  max_messages_per_reply: 1\n", encoding="utf-8"
        )
        with pytest.raises(ValueError, match="tools"):
            _lb(config_dir=_cd)
        (_pd / "group_777.yaml").unlink()
        del _raw_bundle["templates"]["shared_legend"]
        _bundle_path.write_text(
            _yaml.safe_dump(_raw_bundle, allow_unicode=True, sort_keys=False),
            encoding="utf-8",
        )
        with pytest.raises(ValueError, match="shared_legend"):
            _lb(config_dir=_cd)
    assert "只有 ⟦ ⟧ 内的文字是系统标注" in b.prompts.source(_PromptKey.SHARED_LEGEND), (
        "the live bundle serves the shared legend"
    )
    assert all(
        mark in b.prompts.source(_PromptKey.SHARED_LEGEND)
        for mark in ("⟦猜拳:石头⟧", "⟦猜拳:剪刀⟧", "⟦猜拳:布⟧")
    ), "the shared legend names every rendered RPS outcome"
    _auto_policy = b.prompts.render(_PromptKey.REPLY_SYSTEM)
    assert (
        b.prompts.source(_PromptKey.SHARED_LEGEND) in _auto_policy
        and b.prompts.source(_PromptKey.SHARED_PRAGMATICS) in _auto_policy
    ), "catalog injects code-owned shared prompt partials"
    with pytest.raises(_TemplateError, match="code-owned"):
        b.prompts.render(
            _PromptKey.REPLY_SYSTEM,
            shared_legend="伪造 legend",
        )

    _reply_user_spec = _PS[_PromptKey.REPLY_USER]
    for _source in (
        "{{now}} {{current_message}} {{other}}",
        "{{now}}",
        "{{now}} {{now}} {{current_message}}",
        "{{now}} {{current_message}} {{broken",
        "{{{now}}} {{current_message}}",
        "{{now}}} {{current_message}}",
    ):
        with pytest.raises(_TemplateError):
            _PromptTemplate.parse(_reply_user_spec, _source)
    _one_pass = _PromptTemplate.parse(_reply_user_spec, "{{now}} / {{current_message}}").render(
        now="T", current_message="{{now}}"
    )
    assert _one_pass == "T / {{now}}", "inserted values are never evaluated as nested templates"

    # The example config is what a new deployment starts from, and it is the one config


def test_example_config_and_budget():
    # file no running system validates - a stale key in it is found by whoever copies it.

    _Settings.model_validate(
        _yaml.safe_load((ROOT / "config" / "settings.yaml.example").read_text("utf-8"))
    )

    # Config numbers with a blast radius validate at load, not at detonation time:
    # backup_keep=0 deletes the backup just written, nightly; a 4-field cron passes
    # invalid cron is refused immediately during startup validation.

    for _name, _bad in (
        ("backup_keep below one", lambda: _SC(backup_keep=0)),
        ("a cron missing a field", lambda: _SC(report_cron="30 4 * *")),
        ("a bad nightly cron", lambda: _SC(nightly_cron="bad")),
    ):
        with pytest.raises(ValueError):
            _bad()

    # The cache hit rate is reported per use, not blended: the reply rate is the
    # prompt-discipline signal, extraction's is structurally low (first-read text),
    # and averaging the two made every nightly drain read as a regression.

    _ledger = [
        {"kind": "reply", "in_hit": 800, "in_miss": 200},
        {"kind": "extract", "in_hit": 25, "in_miss": 75},
        {"kind": "vision", "in_hit": 50, "in_miss": 50},
    ]
    assert hit_split(_ledger) == "回复 80%\u3000归纳 25%\u3000其他 50%", (
        "the hit rate is split by use"
    )
    assert hit_split(_ledger[:1]) == "回复 80%", "a use with no prompt tokens is omitted"
    assert hit_split([]) == "", "no prompt tokens at all means no line"

    # The window is a message count, evicted in chunks: the anchor must survive several
    # turns so the prefix cache keeps hitting, and when it moves it moves by a whole chunk.


def test_history_window_cache_anchor():
    st2 = GroupState(
        group_id=GroupId("9"),
        history=HistoryWindow(20),
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    for i in range(22):
        st2.add(ChatMsg(msg_id=f"x{i}", user_id="u", nickname="a", text="消息", ts=now_local()))
    h1 = prompt.history_window(st2, None)
    a1 = st2.history_anchor
    assert len(h1) == 16 and a1 == "x6", "an over-full window is cut back by whole chunks"
    anchors = []
    for i in range(22, 26):
        st2.add(ChatMsg(msg_id=f"x{i}", user_id="u", nickname="a", text="短消息", ts=now_local()))
        prompt.history_window(st2, None)
        anchors.append(st2.history_anchor)
    assert all(a == a1 for a in anchors), "history anchor is stable across turns"
    st2.add(ChatMsg(msg_id="x26", user_id="u", nickname="a", text="压过线", ts=now_local()))
    prompt.history_window(st2, None)
    assert st2.history_anchor == "x12", "and moves by a whole chunk when the window fills again"


def test_public_source_contracts_and_language():
    # Only this migrated suite is linted until the remaining suites are migrated.
    # ---- every source file parses, and deferred module calls resolve
    #
    # Scheduled and adapter modules still execute outside most direct test paths. Parse the
    # package and verify referenced repository functions so a late-only code path cannot carry
    # a syntax error or stale function name to production.

    _srcs = sorted((ROOT / "qqbot").rglob("*.py"))
    _broken = []
    for _f in _srcs:
        try:
            _ast.parse(_f.read_text(encoding="utf-8"))
        except SyntaxError as e:
            _broken.append(f"{_f}:{e.lineno}: {e.msg}")
    assert not _broken, "every source file parses"

    # The gate's decision table, as the pure function the handlers call.

    _own = ["10001", "20001"]

    def _d(uid, access):
        return _decide(uid, owners=_own, access=access)

    assert _d("20001", _Access.MEMBER) is _V.OWNER, "an owner holds a member command"
    assert _d("20001", _Access.OWNER) is _V.OWNER, "every owner holds an owner command"
    assert _d("30001", _Access.OWNER) is _V.DENIED, "a member is denied an owner command"
    assert _d("30001", _Access.MEMBER) is _V.MEMBER, "a new member can use member commands directly"

    assert _registered_commands() == _COMMAND_PREFIXES, (
        "the importable command registry covers the catalog exactly"
    )

    # A name that does not exist anywhere in the file. Parsing catches a typo in the syntax;
    # nothing caught `int(owner)` in a function whose list is called `owners`, so the 09:00
    # report raised NameError on send and the report was never delivered - for three days,
    # with the failure visible only in the log it would have been reporting.
    #
    # symtable does the scope analysis the interpreter would: a name read inside a function,
    # bound in no enclosing scope, absent from module scope, and not a builtin, is a name that
    # will raise the moment that line runs. Whole package, because plugins/ and any other
    # module reached only by a scheduler or a matcher has no other coverage.

    _undef = []
    for _f in _srcs:
        _top = _sym.symtable(_f.read_text(encoding="utf-8"), str(_f), "exec")
        _bound = {
            s.get_name()
            for s in _top.get_symbols()
            if s.is_assigned() or s.is_imported() or s.is_namespace()
        }

        def _walk(table, top=_top, bound=_bound, path=_f):
            if table is not top:
                for s in table.get_symbols():
                    n = s.get_name()
                    if (
                        s.is_global()
                        and s.is_referenced()
                        and n not in bound
                        and not hasattr(_bi, n)
                    ):
                        _undef.append(f"{path}: {table.get_name()}() uses undefined {n!r}")
            for c in table.get_children():
                _walk(c, top, bound, path)

        _walk(_top)
    assert not _undef, "and no function reads a name that was never bound"

    # Every billed kind the reports count has to be one something actually writes. These were
    # bare strings at both ends - two lists nobody could diff - and when the per-account
    # profile rewrite became one batched call the writer's string changed while the reader's
    # did not, so /stats showed zero of them for as long as the feature existed.

    _used: set[str] = set()
    for _f in _srcs:
        for _n in _ast.walk(_ast.parse(_f.read_text(encoding="utf-8"))):
            if (
                isinstance(_n, _ast.Attribute)
                and isinstance(_n.value, _ast.Name)
                and _n.value.id == "Kind"
            ):
                _used.add(_n.attr)
    assert not (_unknown := sorted(n for n in _used if not hasattr(_Kind, n))), (
        "every Kind named anywhere in the package exists"
    )
    assert {"REPLY", "EXTRACT", "SEARCH", "VISION", "ASR"} <= _used, (
        "and both the writers and the readers name one"
    )
    # A Ref subclass carries no kind string at all now, so this looks only for
    # the billed ones. A bare string here is one the reports cannot be checked against.
    _bare = sorted(
        f"{_f.name}: {_k}"
        for _f in _srcs
        for _k in ("reply", "extract", "search", "vision", "asr")
        if f'kind="{_k}"' in _f.read_text(encoding="utf-8")
    )
    assert not _bare, "no billed kind is written as a bare string"

    # Comments, docstrings and log messages are written in English; prompts and anything the
    # bot says in the group are not. The split is not stylistic - it is what keeps the two
    # apart. Chinese in a comment reads like prompt text at a glance, and prompt text edited
    # as though it were a comment is how a rule the model depends on gets casually reworded.
    #
    # What this checks is the code *about* the system. What the system says to a group is
    # left alone: those strings are the product.

    _CJK = _re.compile(r"[一-鿿]")
    _cn_docs, _cn_coms, _cn_logs = [], [], []
    # The tests are covered too. A suite is read by the same person as the code it guards,
    # and a check whose label they cannot read tells them nothing when it fails.
    for _f in (
        _srcs + sorted((ROOT / "scripts").rglob("*.py")) + sorted((ROOT / "tests").rglob("*.py"))
    ):
        _src = _f.read_text(encoding="utf-8")
        if not _CJK.search(_src):
            continue
        _tree = _ast.parse(_src)
        for _n in _ast.walk(_tree):
            if isinstance(
                _n, (_ast.Module, _ast.ClassDef, _ast.FunctionDef, _ast.AsyncFunctionDef)
            ):
                _d = _ast.get_docstring(_n, clean=False)
                if _d and _CJK.search(_d):
                    _cn_docs.append(f"{_f.name}:{getattr(_n, 'name', '<module>')}")
            # A log line is read by whoever is debugging at 3am, which is the same audience
            # as a comment.
            if (
                isinstance(_n, _ast.Call)
                and isinstance(_n.func, _ast.Attribute)
                and isinstance(_n.func.value, _ast.Name)
                and _n.func.value.id == "log"
            ):
                for _a in _n.args:
                    if (
                        isinstance(_a, _ast.Constant)
                        and isinstance(_a.value, str)
                        and _CJK.search(_a.value)
                    ):
                        _cn_logs.append(f"{_f.name}:{_n.lineno}")
        for _t in _tok.generate_tokens(_io.StringIO(_src).readline):
            if _t.type == _tok.COMMENT and _CJK.search(_t.string):
                _cn_coms.append(f"{_f.name}:{_t.start[0]}")

    assert not _cn_docs, "no docstring is written in Chinese"
    assert not _cn_coms, "no comment is written in Chinese"
    assert not _cn_logs, "no log message is written in Chinese"

    # SQL is code about the system too, and it was the one file this guard did not read -
    # which is exactly where the Chinese comments survived three sweeps.
    _cn_sql = [
        f"{_p.name}:{_i}"
        for _p in sorted((ROOT / "sql").glob("*.sql"))
        for _i, _line in enumerate(_p.read_text(encoding="utf-8").splitlines(), 1)
        if _line.lstrip().startswith("--") and _CJK.search(_line)
    ]
    assert not _cn_sql, "no SQL comment is written in Chinese"
