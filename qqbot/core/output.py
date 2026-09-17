"""What is taken back out of a reply before it is sent.

Every rule here exists because the prompt already asks for it, and asking does not hold.

The line this draws is what QQ can show. Markdown is not rendered there, so `**`, `#`,
backticks and rule lines arrive as the characters themselves and leak the bot's
formatting into the chat. What survives being sent as plain text is left alone: a
hyphen bullet reads as a list, and a numbered line reads as a numbered line.
The history hands the model a number in front of every line - its own included, so that a
quote can point at one - which is an example it can follow. And a model that wants to call
a tool it has not been given will write the call out as text instead.

No message splitting, no fake typing delay.
"""

from __future__ import annotations

import logging
import re
from collections import Counter

from ..util import SYS_L, SYS_R, defang

log = logging.getLogger("qqbot.output")

_FENCE = re.compile(r"```[a-zA-Z0-9_+-]*\n?")
_IMG = re.compile(r"!\[([^\]]*)\]\([^)]*\)")
_LINK = re.compile(r"\[([^\]]+)\]\((https?://[^)]+)\)")
_HEAD = re.compile(r"^\s{0,3}#{1,6}\s+", re.M)
_QUOTE = re.compile(r"^\s{0,3}>\s?", re.M)
#: Bullets are kept, and the two Markdown spellings are folded into the plain hyphen.
#: A hyphen at the head of a line displays on QQ exactly as it was meant to, so a
#: genuine list - a build sheet, a few options, the steps of something - reads better
#: with them than without. An asterisk is the same list wearing Markdown, which is
#: what QQ cannot show. Numbered lists need no rule: they are ordinary text already.
_BULLET_MARK = re.compile(r"^(\s*)[*+]\s+", re.M)
_HR = re.compile(r"^\s*(?:\*\s*){3,}$|^\s*(?:-\s*){3,}$", re.M)
#: Strong emphasis takes two or three markers. A single asterisk is handled apart,
#: because one is also multiplication: "3*5*2" is arithmetic, and a rule that reads
#: any starred span as emphasis turns it into "352". Italics therefore need a word
#: boundary on the outside of each marker, which a product sign never has. The
#: underscore form needs the same boundary for a different reason: `__init__` is
#: an identifier, and Markdown itself does not read underscores inside a word.
_BOLD_STAR = re.compile(r"(\*{2,3})(?=\S)(.+?)(?<=\S)\1", re.S)
_BOLD_UNDER = re.compile(r"(?<!\w)(_{2,3})(?=\S)(.+?)(?<=\S)\1(?!\w)", re.S)
_ITALIC = re.compile(r"(?<![\w*])\*(?=\S)([^*\n]+?)(?<=\S)\*(?![\w*])")
_CODE = re.compile(r"`+([^`]+)`+")
_MULTI_NL = re.compile(r"\n{3,}")
#: The reserved pair, escaped for the patterns below - one spelling with util.
_L, _R = re.escape(SYS_L), re.escape(SYS_R)
#: A line number copied back out of the history, with or without the speaker prefix that
#: follows it there.
#:
#: The negative lookahead is the whole subtlety: a "#1" written mid-sentence is ordinary
#: text in which the number is the message. Eating that corrupts a real reply, while
#: leaving a stray 「#7 」 in front of one is merely untidy - so where the two readings
#: collide, the guard declines. The character class is the short list of classifiers a
#: number can take.
_LINE_NO = re.compile(
    rf"^\s*#\d{{1,4}}(?:\s+{_L}\d{{2}}-\d{{2}} \d{{2}}:\d{{2}}{_R})?(?:\s+[^\s:：]{{1,20}}[:：])?"
    r"\s*+(?![号位名楼队班组层期版区])",
    re.M,  # every line: a multi-line reply imitates the numbered format on each one
)

#: A time stamp copied back out of the history without its line number. Only the stamp
#: is eaten, never a speaker after it: a bracketed date-time opening a line is format
#: imitation, but "X：" after one could be the reply's own words - where the readings
#: collide, the guard declines, same as _LINE_NO. Only the reserved pair: a square
#: form is what a member's imitation looks like after defang, and quoting a
#: member is content.
_TS_ONLY = re.compile(rf"^\s*{_L}\d{{2}}-\d{{2}} \d{{2}}:\d{{2}}{_R}\s*", re.M)
#: The provenance marker the engine appends to the bot's own archived lines. The
#: history is an example the model may follow, and a reply that imitates it would
#: leak a system annotation into the group. Nobody writes the bracketed form by
#: hand, so this one is stripped wherever it appears.
_PROV = re.compile(rf"\s*{_L}依据[:：][^{_R}]*{_R}")
#: A line imitating the trajectory-entry marker. Whole lines carrying it are
#: dropped: the marker is system-written and must never reach the group, while the
#: digest lines that follow one read as ordinary speech and are left to stand.
_TRACE_LINE = re.compile(rf"^.*{_L}检索记录{_R}.*$\n?", re.M)
#: The quote pointer copied back out of the history. The real quote is the reply
#: segment the send path attaches; the bracketed form is transcript notation, and
#: the prompt instruction not to reproduce it is not reliable on its own. Anchored
#: to line starts like _LINE_NO and _TS_ONLY: format imitation reproduces the
#: transcript line's shape, which opens with the mark, while one sitting
#: mid-sentence is likelier the reply's own content (somebody's words restated, or
#: the notation being talked about) - where the readings collide, the guard
#: declines.
_REPLY_MARK = re.compile(rf"^\s*{_L}回复\s*(?:#\d{{1,4}}|更早的消息){_R}\s*", re.M)
#: Speaker tags copied out of the history: the owner and self annotations and the
#: member numbers that ride behind names in transcripts. Dropped whole wherever
#: they appear - they are annotations about a name, never words anyone says.
_NAME_TAG = re.compile(
    rf"{re.escape(SYS_L)}(?:拥有者|你|\d{{1,9}}){re.escape(SYS_R)}")


def strip_markdown(text: str) -> str:
    if not text:
        return ""
    t = text.replace("\r\n", "\n")
    t = _FENCE.sub("", t)
    t = _IMG.sub(r"\1", t)
    t = _LINK.sub(r"\1 \2", t)
    t = _HR.sub("", t)
    t = _HEAD.sub("", t)
    t = _QUOTE.sub("", t)
    t = _BULLET_MARK.sub(r"\1- ", t)
    for _ in range(3):  # nested emphasis such as ***x***
        new = _BOLD_UNDER.sub(r"\2", _BOLD_STAR.sub(r"\2", t))
        if new == t:
            break
        t = new
    t = _ITALIC.sub(r"\1", t)
    t = _CODE.sub(r"\1", t)
    t = _MULTI_NL.sub("\n\n", t)
    return t.strip()


#: How often each marker stripper actually fired, since the process started.
#: Every hit is a near-miss leak - the model wrote a system marker and only the
#: stripper kept it from the group - so the counts are the online signal that
#: format discipline is regressing, surfaced in the daily report. In-memory like
#: the error ring: a restart resets it, and that is accepted for the same reason.
STRIPPED: Counter[str] = Counter()

_MARKER_STAGES = (
    ("quote_mark", _REPLY_MARK),
    ("line_no", _LINE_NO),
    ("timestamp", _TS_ONLY),
    ("provenance", _PROV),
    ("trace_line", _TRACE_LINE),
    ("name_tag", _NAME_TAG),
)


def clean_reply(text: str) -> str:
    """Everything taken back out of a reply before it goes to the group.

    The quote mark goes first: a line imitating both markers opens with the
    quote mark and hides the line number behind it, and _LINE_NO declines lines
    that do not start with the number - stripping in the other order would
    uncover a line number and then keep it.

    The last stage is the floor under the whole reserved-bracket grammar: no
    reply may carry the system brackets into the group, whatever surrounds them.
    Ingest would defang them right back on the next archive read, and a marker
    shape the strippers above did not anticipate must still not reach members.
    """
    t = strip_markdown(_before_markup(text))
    for name, rx in _MARKER_STAGES:
        new = rx.sub("", t)
        if new != t:
            STRIPPED[name] += 1
            t = new
    if SYS_L in t or SYS_R in t:
        STRIPPED["reserved"] += 1
        t = defang(t)
    return t.strip()


#: Where a model stopped writing a reply and started writing a tool call.
#:
#: A backend that wants a tool it has not been given writes the call out in its own
#: markup instead - a whole `<｜｜DSML｜｜tool_calls>` block, search query and all, sent
#: as if it were the reply. The vertical bar is full-width in that markup, which is why
#: both forms are matched; the bare tag names cover the backends that use those instead.
#:
#: Nothing here is ordinary text. `<` followed by a vertical bar does not occur in Chinese
#: writing, and the tag names are not words anybody types into a group chat.
_TOOL_MARKUP = re.compile(
    r"</?\s*[｜|]"
    r"|</?\s*(?:tool_call|tool_calls|function_call|invoke|antml:invoke)\b",
    re.I,
)


def _before_markup(text: str) -> str:
    """Whatever was written before the model started emitting machinery.

    Cut rather than deleted in place: what follows the first marker is the call's own
    arguments, and stripping only the tags would post the search query as if it were the
    answer. An answer that came first is kept, and a reply that is nothing but a tool call
    becomes nothing at all - which the caller already handles by staying silent.

    Logged, because silence is the one symptom that looks the same whether the bot chose
    not to speak or something broke.
    """
    if not text:
        return ""
    m = _TOOL_MARKUP.search(text)
    if m is None:
        return text
    kept = text[:m.start()]
    log.warning(
        "reply contained a tool call written as text; dropped %d chars, kept %d. "
        "The model wanted a tool it was not offered on this round: %s",
        len(text) - len(kept), len(kept.strip()), text[m.start():m.start() + 60],
    )
    return kept
