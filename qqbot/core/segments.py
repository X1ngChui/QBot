"""A QQ message as segments, turned into the text a model can read.

Pure: no network, no database, no model. Given the segment list an adapter hands over,
this decides what each part becomes - a string outright, or a Ref standing for something
that has to be fetched. Everything with an I/O cost lives in media.py, which is what a
Ref's resolve() calls back into.

The split is worth having because this half is where the format lives - what QQ sends,
what the model reads, and the markers that connect them - and it is the half that can be
tested by calling a function.
"""

from __future__ import annotations

import json
import logging
import re
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from ..settings import Settings
from .botapi import BotApi

if TYPE_CHECKING:                       # resolve() delegates to it; importing it here
    from .media import MediaProcessor   # would be a cycle, and it is only an annotation

log = logging.getLogger("qqbot.media")


#: Segment types already reported, so an unknown one logs once rather than per message.
_SEEN_UNKNOWN: set[str] = set()

_MD5 = re.compile(r"([0-9a-fA-F]{32})")



@dataclass
class Ref:
    """A part of a message whose text has to be fetched or inferred.

    One class per kind, because they have nothing in common but a slot: an image carries
    a url, a size and a cache key, a mention carries an account id, and the fields of one
    are meaningless on the other.

    `free` is the other thing the kind decides: whether reading it costs a model call.
    Nested content and the arrival pass both resolve only what is free, and asking the
    ref is the alternative to listing the paid kinds at each of those places.
    """

    slot: int = 0
    #: Whether resolving this is a lookup rather than a model call.
    free: bool = True

    def placeholder(self) -> str:
        """What the model sees when this could not be resolved."""
        return "[消息]"

    async def resolve(self, proc: MediaProcessor, *, bot: BotApi, group_id: str,
                      cfg: Settings, self_id: str) -> str | None:
        """The text this stands for, or None if it cannot be had right now."""
        return None


@dataclass
class AtRef(Ref):
    """Somebody named in the message. The number alone tells the model nothing."""

    ident: str = ""

    def placeholder(self) -> str:
        return f"@{self.ident}"

    async def resolve(self, proc: MediaProcessor, *, bot: BotApi, group_id: str,
                      cfg: Settings, self_id: str) -> str | None:
        return await proc.name_for(self, bot=bot, group_id=group_id)


@dataclass
class ImageRef(Ref):
    """A picture or a sticker. `sticker` changes only the label."""

    free: bool = False
    key: str | None = None       # mface emoji_id, or the image md5
    sticker: bool = False
    url: str | None = None
    file: str | None = None
    path: str | None = None      # napcat's own path, usable through the shared mount
    summary: str | None = None
    #: Where the vision backend filed this picture, once ensure_uploaded has run. What
    #: a chat message's file block carries; None until then, or when the backend keeps
    #: no files.
    file_id: str | None = None
    size: int | None = None

    def placeholder(self) -> str:
        return "[图片]"

    async def resolve(self, proc: MediaProcessor, *, bot: BotApi, group_id: str,
                      cfg: Settings, self_id: str) -> str | None:
        # Filing comes first and is free: the download link is freshest now, and the id
        # must exist for the prompt to attach the original. Describing follows, with
        # its own cache, rate limit and budget gate.
        await proc.ensure_uploaded(self, bot=bot, group_id=group_id, cfg=cfg)
        return await proc.describe_image(self, bot=bot, group_id=group_id, cfg=cfg)


@dataclass
class AudioRef(Ref):
    """A voice clip. No cache: the same clip is never sent twice."""

    free: bool = False
    file: str | None = None
    path: str | None = None
    url: str | None = None
    size: int | None = None

    def placeholder(self) -> str:
        return "[语音]"

    async def resolve(self, proc: MediaProcessor, *, bot: BotApi, group_id: str,
                      cfg: Settings, self_id: str) -> str | None:
        return await proc.transcribe(self, bot=bot, group_id=group_id, cfg=cfg)


@dataclass
class ForwardRef(Ref):
    """A merged-forward bundle, whose contents live behind one more API call."""

    ident: str = ""

    def placeholder(self) -> str:
        return "[转发的聊天记录]"

    async def resolve(self, proc: MediaProcessor, *, bot: BotApi, group_id: str,
                      cfg: Settings, self_id: str) -> str | None:
        return await proc.read_forward(self, bot=bot, group_id=group_id, self_id=self_id)


@dataclass
class ParsedMessage:
    parts: list = field(default_factory=list)     # str | Ref
    refs: list[Ref] = field(default_factory=list)
    at_bot: bool = False
    reply_to: str | None = None
    #: Accounts this message addressed, by id. Not the bot itself - that is at_bot.
    mentions: list[str] = field(default_factory=list)

    def render(self, resolved: dict[int, str] | None = None) -> str:
        resolved = resolved or {}
        out = []
        for p in self.parts:
            out.append(resolved.get(p.slot) or p.placeholder() if isinstance(p, Ref) else p)
        return " ".join(x for x in (s.strip() for s in out) if x)

    @property
    def needs_model(self) -> bool:
        """Whether resolving this costs an API call, as opposed to a lookup."""
        return any(not r.free for r in self.refs)


#: Classic QQ emoticons carry only a numeric id; the newer ones carry a name in
#: raw.faceText and need no table. Rendering the common ones by name is worth the lines:
#: A bare sticker marker says nothing, while one carrying the drawing's description
#: carries the tone the sender meant.
#: Anything not listed falls back to the bare marker.
FACE_NAMES = {
    "0": "惊讶", "1": "撇嘴", "2": "色", "3": "发呆", "4": "得意", "5": "流泪",
    "6": "害羞", "7": "闭嘴", "8": "睡", "9": "大哭", "10": "尴尬", "11": "发怒",
    "12": "调皮", "13": "呲牙", "14": "微笑", "15": "难过", "16": "酷", "18": "抓狂",
    "19": "吐", "20": "偷笑", "21": "可爱", "22": "白眼", "23": "傲慢", "25": "困",
    "26": "惊恐", "27": "流汗", "28": "憨笑", "30": "奋斗", "32": "疑问", "33": "嘘",
    "34": "晕", "36": "衰", "38": "敲打", "39": "再见", "46": "猪头", "49": "拥抱",
    "63": "玫瑰", "64": "凋谢", "66": "爱心", "76": "赞", "77": "踩", "78": "握手",
    "79": "胜利", "96": "冷汗", "97": "擦汗", "99": "鼓掌", "100": "糗大了",
    "101": "坏笑", "104": "哈欠", "105": "鄙视", "106": "委屈", "107": "快哭了",
    "108": "阴险", "109": "亲亲", "110": "吓", "111": "可怜", "116": "示爱",
    "118": "抱拳", "120": "拳头", "121": "差劲", "122": "爱你", "123": "NO",
    "124": "OK", "144": "喝彩", "147": "棒棒糖", "172": "眨眼睛", "173": "泪奔",
    "174": "无奈", "175": "卖萌", "176": "小纠结", "177": "喷血", "178": "斜眼笑",
    "179": "doge", "180": "惊喜", "182": "笑哭", "183": "我最美", "187": "幽灵",
    "193": "大笑", "194": "不开心", "197": "冷漠", "198": "呃", "199": "好棒",
    "200": "拜托", "201": "点赞", "202": "无聊", "203": "托脸", "204": "吃",
    "205": "送花", "206": "害怕", "210": "飙泪", "211": "我不看", "212": "托腮",
    "214": "啵啵", "222": "抱抱", "227": "拍手", "228": "恭喜", "229": "干杯",
    "230": "嘲讽", "231": "哼", "232": "佛系", "234": "惊呆", "237": "偷看",
    "239": "原谅", "241": "生日快乐", "262": "脑阔疼", "263": "沧桑", "264": "捂脸",
    "265": "辣眼睛", "266": "哦哟", "267": "头秃", "268": "问号脸", "269": "暗中观察",
    "270": "emm", "271": "吃瓜", "272": "呵呵哒", "273": "我酸了", "277": "汪汪",
    "278": "汗", "281": "无眼笑", "282": "敬礼", "283": "狂笑", "284": "面无表情",
    "285": "摸鱼", "287": "哦", "289": "睁眼", "290": "敲开心", "293": "摸锦鲤",
    "294": "期待", "297": "拜谢", "299": "牛啊", "306": "牛气冲天", "307": "喵喵",
    "314": "仔细分析", "315": "加油", "318": "崇拜", "319": "比心", "320": "庆祝",
    "324": "吃糖", "325": "惊吓", "326": "生气",
}

#: rps result is 1-3 for rock, scissors, paper - in that order, which is not the order the
#: name suggests.
RPS_NAMES = {"1": "石头", "2": "剪刀", "3": "布"}

_MD_IMAGE = re.compile(r"!\[[^\]]*\]\([^)]*\)")
_MD_LINK = re.compile(r"\[([^\]]*)\]\([^)]*\)")
_MD_HEADING = re.compile(r"^\s{0,3}#{1,6}\s*", re.M)
_MD_QUOTE = re.compile(r"^\s{0,3}>\s?", re.M)
_MD_BLANKS = re.compile(r"\n{2,}")


def _markdown_text(md: str) -> str:
    """Flatten a markdown segment to the text a person would see.

    Bots on QQ send their output this way - divination results, game results, anything
    with formatting. The markup itself is noise: mqqapi:// links, sizing hints, an empty
    link at the top carrying a version number.
    """
    text = _MD_IMAGE.sub("[图片]", md or "")
    text = _MD_LINK.sub(r"\1", text)      # keep the label, drop the target
    text = _MD_HEADING.sub("", text)
    text = _MD_QUOTE.sub("", text)
    text = _MD_BLANKS.sub("\n", text)
    return "\n".join(ln.strip() for ln in text.splitlines() if ln.strip()).strip()


def _card_text(raw: str) -> str:
    """Share cards and mini-programs arrive as a JSON blob. The interesting part is a
    title and a description buried a few levels down; the rest is layout."""
    try:
        data = json.loads(raw)
    except (ValueError, TypeError):
        return "[卡片消息]"
    prompt = (data.get("prompt") or "").strip()
    meta = data.get("meta") or {}
    for entry in meta.values():
        if not isinstance(entry, dict):
            continue
        title = (entry.get("title") or entry.get("tag") or "").strip()
        desc = (entry.get("desc") or entry.get("summary") or "").strip()
        if title or desc:
            body = f"{title}：{desc}" if title and desc else (title or desc)
            return f"[分享:{body[:80]}]"
    return f"[分享:{prompt[:80]}]" if prompt else "[卡片消息]"


def parse_segments(segments: list[dict], self_id: str) -> ParsedMessage:
    """Synchronous, allocation-only. Anything needing an API call becomes a Ref."""
    pm = ParsedMessage()
    slot = 0

    def add_ref(cls, **kw) -> None:
        nonlocal slot
        ref = cls(slot=slot, **kw)
        pm.parts.append(ref)
        pm.refs.append(ref)
        slot += 1

    for seg in segments:
        stype = seg.get("type")
        data = seg.get("data") or {}

        if stype == "text":
            txt = (data.get("text") or "").strip()
            if txt:
                pm.parts.append(txt)
        elif stype == "at":
            qq = str(data.get("qq") or "")
            if qq == self_id:
                pm.at_bot = True
                pm.parts.append("@我")
            elif qq == "all":
                pm.parts.append("@全体成员")
            else:
                # Recorded whether or not the name came with it: being addressed is what
                # makes someone part of this exchange, and their profile is worth loading
                # even though they have not spoken.
                pm.mentions.append(qq)
                if data.get("name"):
                    pm.parts.append(f"@{data['name']}")
                else:
                    # Only the number is given, which is meaningless to the model.
                    add_ref(AtRef, ident=qq)
        elif stype == "face":
            raw = data.get("raw") if isinstance(data.get("raw"), dict) else {}
            name = (raw.get("faceText") or "").strip().lstrip("/")
            name = name or FACE_NAMES.get(str(data.get("id") or ""), "")
            pm.parts.append(f"[表情:{name}]" if name else "[表情]")
        elif stype == "mface":
            add_ref(
                ImageRef,
                sticker=True,
                key=str(data.get("emoji_id") or "") or None,
                url=data.get("url"),
                summary=(data.get("summary") or "").strip("[]") or None,
            )
        elif stype == "image":
            file_field = str(data.get("file") or "")
            m = _MD5.search(file_field) or _MD5.search(str(data.get("file_id") or ""))
            add_ref(
                ImageRef,
                key=(m.group(1).lower() if m else None),
                url=data.get("url"),
                file=file_field or None,
                path=data.get("path"),
                size=int(data.get("file_size") or 0) or None,
                summary=(data.get("summary") or "").strip("[]") or None,
            )
        elif stype == "record":
            add_ref(
                AudioRef,
                url=data.get("url"),
                file=str(data.get("file") or "") or None,
                path=data.get("path"),
                size=int(data.get("file_size") or 0) or None,
            )
        elif stype == "reply":
            # Recorded, not rendered. The quoted message is almost always one the bot is
            # already being shown, so the prompt points at it by number instead of pasting
            # an excerpt in - an excerpt says what was said but not which line said it,
            # and two people saying the same thing is ordinary in a group. See
            # prompt.numbered.
            pm.reply_to = str(data.get("id") or "") or None
        elif stype == "forward":
            fid = str(data.get("id") or "")
            add_ref(ForwardRef, ident=fid) if fid else pm.parts.append("[转发的聊天记录]")
        elif stype == "json":
            pm.parts.append(_card_text(data.get("data") or ""))
        elif stype == "xml":
            pm.parts.append("[卡片消息]")
        elif stype == "video":
            pm.parts.append("[视频]")
        elif stype == "file":
            name = (data.get("file") or data.get("name") or "").strip()
            pm.parts.append(f"[文件:{name}]" if name else "[文件]")
        elif stype == "poke":
            pm.parts.append("[戳一戳]")
        elif stype == "markdown":
            # Bots on QQ send their output as markdown, and the segment carries the
            # whole body, not a decoration on it - dropping it drops the entire message.
            body = _markdown_text(str((data.get("content") or data.get("data") or "")))
            if body:
                pm.parts.append(body)
        elif stype == "dice":
            pm.parts.append(f"[骰子:{data.get('result')}点]" if data.get("result") else "[骰子]")
        elif stype == "rps":
            name = RPS_NAMES.get(str(data.get("result") or ""))
            pm.parts.append(f"[猜拳:{name}]" if name else "[猜拳]")
        elif stype:
            # A type nobody has taught this function about. Say something rather than
            # drop the message on the floor, and log it once so it can be added - QQ
            # keeps inventing these, and a silent drop is invisible from outside.
            if stype not in _SEEN_UNKNOWN:
                _SEEN_UNKNOWN.add(stype)
                log.info("unhandled message segment type %r: %s", stype, list(data)[:8])
            pm.parts.append(f"[{stype}]")
    return pm


def segments_of(msg: dict) -> tuple[list | None, str]:
    """A fetched message's segments, or its raw text when there are none.

    NapCat's messagePostFormat decides whether `message` is an array of segments or a
    CQ-code string, and it is one setting in a config this project does not own. Set to
    "string" the array is simply absent, and every quote would come back empty - silently,
    an empty body looking exactly like a message that had no text. `raw_message` is always
    there, so it is the floor.
    """
    segs = msg.get("message")
    if not isinstance(segs, list):
        segs = msg.get("content")
    if isinstance(segs, list):
        return segs, ""
    return None, str(msg.get("raw_message") or segs or "").strip()


