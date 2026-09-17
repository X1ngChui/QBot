"""Prompt assembly.

The ordering is cost discipline and must not be violated - on the configured text
backend a prefix-cache hit is ~30x cheaper than a miss:

  legend + rules (global constants) -> persona (per group, changes when config does)
  -> group knowledge (rewritten daily) -> who is who (renames, then impressions)
  -> conversation history (append-only)
  -> [cache boundary] -> tool results -> current message

Ordered by how often each part changes, most stable first, because a prefix cache matches
from the beginning: anything above a changed block is re-read as well. The constants lead
so that every group shares that opening span rather than each paying for its own.

Nothing that changes every turn may sit before the history. History is evicted in chunks
rather than one message per turn: every slide of the window invalidates the prefix once,
so sliding rarely is most of what there is to win.
"""

from __future__ import annotations

import json
import re

from ..providers.contracts import Message, PromptItem, Role, ToolCall, ToolCallId, ToolResult
from ..settings import Persona, Settings, ptext
from ..util import SYS_L, SYS_R, defang, describe_now, fmt_when, sysmark
from .member_numbers import MemberNumbers
from .outbound import (
    AtSegment,
    ContactKind,
    ContactSegment,
    CustomMusicSegment,
    DiceSegment,
    FaceSegment,
    JsonCardSegment,
    MarketFaceSegment,
    MusicSegment,
    OutboundSegment,
    ReplySegment,
    RpsSegment,
    TextSegment,
)
from .state import ChatMsg, GroupState
from .tools import SEND
#: The provenance marker at the end of one of the bot's own archived lines.
_PROV_TAIL = re.compile(
    rf"\s*({re.escape(SYS_L)}依据[:：][^{re.escape(SYS_R)}]*{re.escape(SYS_R)})\s*$")

# The history window is a message count, not a token budget: money bounds what a
# reply may spend, and every other block is rendered whole. The count and its
# eviction chunk are config (prompt.window_chunks, prompt.evict_chunk), and the
# window is their product so the multiple holds by construction. They serve context
# and the cache rather than cost - a prefix token is ~1/30 price, so a wider window
# is nearly free per call while every slide is the miss that costs. Evidence memos are
# fetched by reply id at assembly and do not consume the count.

#: Section headings. The blocks below answer different questions and carry different
#: authority; without a marked boundary they read as one undifferentiated wall, and the
#: model weighs a guess it wrote last week the same as a fact the platform just returned.
H_SEND = "【怎样发言】"
H_LEGEND = "【消息记录读法】"
H_IDENTITY = "【成员与编号】"
H_CREDIBILITY = "【信息与检索】"
H_PRIVATE = "【不说出去的内容】"
H_TONE = "【群聊语用】"
H_PERSONA = "【你的身份】"
H_GROUP = "【本群背景】"
H_WHO = "【群成员名册】"


def _block(head: str, entries: list[tuple[int, str]]) -> str:
    """Join a heading and its entries, written out in member-number order.

    The order is the point: the numbering follows the roster, which is ordered by
    first appearance, so the block renders identically between turns. This sits inside
    the cached prefix, and an ordering that can change - any ranking by activity can,
    on any message - costs the cache from that line to the end of the prompt.
    """
    if not entries:
        return ""
    return head + "\n" + "\n".join(line for _, line in sorted(entries))


def _name(p: dict, people: MemberNumbers) -> tuple[int, str]:
    """A roster row's member number, and its name wearing that number. defang on
    render as well as at ingest: names are stored as the platform reported them, and
    the render is where the grammar must hold."""
    n = people.number(str(p.get("user_id") or ""))
    return n, defang(p.get("nickname") or "") + (sysmark(str(n)) if n else "")


def _known_block(profiles: list[dict], people: MemberNumbers) -> str:
    """What the system can vouch for about who somebody is: every person in the roster,
    by name and member number, with whatever is on record beside the name.

    Three things qualify, and they are three different kinds of claim, so the line says
    which is which rather than running them together:

      former names - the account displayed them, and QQ said so. Every message files the
        current group card as an alias, so changing it leaves the old one behind rather
        than overwriting it; that is what makes "what was he called before" answerable.
      aliases      - written in by hand through /alias. Certain because somebody typed it
        deliberately, but a claim about what people call him, not about what the account
        displayed.
      the note     - written in by hand through /note.

    The first two must never be flattened into one list: a name an owner registered
    because the group says it out loud is not a name the account ever carried, and
    labelling it a former name tells the model the opposite of the truth.

    The hand-written entries are also the only way to correct the model once it settles on
    something wrong about a person - which is exactly the moment its own account of them
    should not be the one being read.
    """
    lines = []
    for p in profiles:
        name = defang(p.get("nickname") or "")
        bits = []
        if former := [defang(n) for n in (p.get("former_names") or [])
                      if n and defang(n) != name]:
            bits.append("曾用名：" + "、".join(former))
        if aliases := [defang(n) for n in (p.get("aliases") or [])
                       if n and defang(n) != name]:
            bits.append("别名：" + "、".join(aliases))
        if note := defang(p.get("manual_note") or "").strip():
            bits.append(note)
        n, shown = _name(p, people)
        lines.append((n, f"- {shown}，" + "；".join(bits) if bits else f"- {shown}"))
    return _block("已确认（系统记录的名字，以及拥有者或成员本人写明的信息）：", lines)


def _guessed_block(profiles: list[dict], people: MemberNumbers) -> str:
    """What the bot worked out by watching, in its own words.

    Kept apart from the block above because a guess and a fact the platform reported are
    not the same kind of thing, and one list covering both would say they were.

    The names people call someone by live in this prose, not in a structured field
    beside it: a name only means anything with the sentence around it - "everyone calls
    him that" and "somebody called him that once and nobody followed" are different
    claims that reduce to the same array entry.
    """
    lines = []
    for p in profiles:
        card = defang(p.get("persona_card") or "").strip()
        if not card:
            continue
        n, shown = _name(p, people)
        lines.append((n, f"- {shown}。{card}"))
    return _block("未确认（你自行归纳的印象，可能有误或已过时）：", lines)


def build_policy() -> str:
    """Cross-group invariants with the highest, cache-stable authority."""

    blocks = [
        H_SEND + "\n" + ptext("send_rules"),
        H_LEGEND + "\n" + ptext("legend") + "\n\n" + ptext("legend_reply_note"),
        H_IDENTITY + "\n" + ptext("identity_rules"),
        H_CREDIBILITY + "\n" + ptext("credibility_rules"),
        H_PRIVATE + "\n" + ptext("private_rules"),
        H_TONE + "\n" + ptext("tone_rules"),
    ]
    return "\n\n".join(blocks)


def build_developer(
    persona: Persona,
    profiles: list[dict],
    group_facts: list[str] | None = None,
    people: MemberNumbers | None = None,
) -> str:
    """Group-scoped identity and context below global policy authority."""

    blocks = [H_PERSONA + "\n" + persona.system_prompt.strip()]

    # Split by origin, exactly like the identity block below. The hand-written half is a
    # statement; the card is the bot's own summary read back, and running them together
    # would present a guess in the voice of a fact.
    fixed = persona.group_knowledge.strip()
    learned = "\n".join(f"- {fact}" for fact in (group_facts or []))
    if fixed or learned:
        parts = []
        if fixed:
            parts.append("已确认（固定资料）：\n" + fixed)
        if learned:
            parts.append("未确认（你自行归纳的印象，可能有误或已过时）：\n" + learned)
        blocks.append(H_GROUP + "\n" + "\n\n".join(parts))

    if people is None:
        people = MemberNumbers()
        number_people(people, profiles, [], None)
    known = _known_block(profiles, people)
    guessed = _guessed_block(profiles, people)
    parts = [part for part in (known, guessed) if part]
    if parts:
        blocks.append(H_WHO + "\n" + "\n\n".join(parts))
    return "\n\n".join(block for block in blocks if block)


def build_system(
    persona: Persona,
    profiles: list[dict],
    group_facts: list[str] | None = None,
    people: MemberNumbers | None = None,
) -> str:
    """Combined reading retained for diagnostics and extraction-adjacent tests."""

    return "\n\n".join(
        (build_policy(), build_developer(persona, profiles, group_facts, people))
    )


def history_window(st: GroupState, msg: ChatMsg | None,
                   cfg: Settings) -> list[ChatMsg]:
    """The messages that will actually appear in the prompt, oldest first: the
    context before `msg`, the message being answered (None for a bare render
    of the window).

    Separate from rendering them because the answer is needed before the prompt is
    built: what the window holds decides which backlog media is worth settling.
    """
    hist = [m for m in st.recent if msg is None or m.msg_id != msg.msg_id]
    if not hist:
        st.history_anchor = None
        return []

    ids = [m.msg_id for m in hist]
    start = ids.index(st.history_anchor) if st.history_anchor in ids else 0

    # A count, not a measurement: with no token budget there is nothing to measure, and
    # the anchor moves only when the window fills - in whole chunks, so the prefix
    # survives many turns between slides.
    chunk = cfg.prompt.evict_chunk
    while len(hist) - start > chunk * cfg.prompt.window_chunks:
        start += chunk

    st.history_anchor = hist[start].msg_id if start < len(hist) else None
    return hist[start:]


def numbered(visible: list[ChatMsg]) -> tuple[dict[str, int], dict[str, str]]:
    """Line numbers for one prompt's worth of messages, and where each quote points.

    A quote is shown as a pointer to a numbered line, not as an excerpt: an excerpt says
    what was said and not which line said it, and in a group the same words get said
    twice. A number is exact and costs no call - the quoted message is nearly always one
    already on screen.

    The number is the message's position in what is being shown, worked out here rather
    than carried on the message. Position is enough: appending leaves every existing line
    where it was, and the only thing that shifts them is eviction, which moves the start
    of the block and has already cost the prefix cache whatever the numbers do.

    Every line is numbered, the bot's own included, so a quote of anything is the same
    marker pointing at the same kind of thing, and the send tool can reply to any of
    them. A number the model copies into its text anyway is taken back off by
    output.clean_reply.

    Out of the window, a quote is reported as unavailable. Fetching it would put text on
    screen belonging to no line the model can see - the same ambiguity the numbers exist
    to remove - and the case is rare: people quote what is still in front of them.
    """
    nums = {m.msg_id: i for i, m in enumerate(visible, 1)}
    by_id = {m.msg_id: m for m in visible}
    marks: dict[str, str] = {}
    for m in visible:
        if not m.reply_to:
            continue
        quoted = by_id.get(m.reply_to)
        marks[m.msg_id] = (
            sysmark(f"回复 #{nums[quoted.msg_id]}") if quoted is not None
            else sysmark("回复更早的消息")
        )
    return nums, marks


def numbered_images(visible: list[ChatMsg]) -> tuple[dict[str, list[int]], dict[int, tuple]]:
    """Give every picture in this prompt a number, oldest first.

    The reply model reads pictures itself, and a number is how it names the ones it
    wants opened - it has to be a number rather than "the second picture in message
    #12" because that is two coordinates for one thing and the model gets to pick
    which it miscounts. Pictures inside a forwarded record count with the message
    that carries the record, in the order its block renders them.

    Numbered oldest first, like the line numbers, so both count the same direction.
    Returns the numbers per message - the render puts them into the markers - and the
    map back to (message, index into image_refs) that open_images resolves against.
    """
    per_msg: dict[str, list[int]] = {}
    by_pic: dict[int, tuple] = {}
    n = 0
    for m in visible:
        refs = getattr(m, "image_refs", None) or []
        if not refs:
            continue
        got = []
        for i in range(len(refs)):
            n += 1
            got.append(n)
            by_pic[n] = (m, i)
        per_msg[m.msg_id] = got
    return per_msg, by_pic


def teach_roster(people: MemberNumbers, profiles: list[dict]) -> None:
    """Tell the numbering which person each roster account belongs to, so merged
    accounts share their person's number without a lookup."""
    for p in profiles:
        for account in p.get("accounts") or ():
            people.teach(account, p.get("entity_id") or p.get("user_id"))


def number_people(people: MemberNumbers, profiles: list[dict],
                  window: list[ChatMsg], msg: ChatMsg | None) -> None:
    """Assign member numbers in the order the prompt shows people.

    The roster first, in its own order (first appearance in the group), and the
    roster holds everyone who has appeared here - so the numbers are the roster's,
    depend on nothing the conversation does, and keep the system block cached while
    it moves. Anybody the window shows who is not in the roster yet (a first message
    still being archived) is numbered after it, oldest first, then the message being
    answered.
    """
    teach_roster(people, profiles)
    for p in profiles:
        people.number(str(p.get("user_id") or ""))
    for m in [*window, *([msg] if msg is not None else [])]:
        if m.is_bot:
            for account, _ in m.at:
                people.number(account)
        else:
            people.number(m.user_id, spoke=True)


def _call_id(msg_id: str) -> str:
    """A stable tool-call id for one of the bot's past messages: derived from the
    message id, so the rendered history is byte-identical between turns."""
    return SEND + "_" + re.sub(r"[^A-Za-z0-9_-]", "_", msg_id)


def _segment_arg(
    segment: OutboundSegment,
    *,
    nums: dict[str, int],
    people: MemberNumbers | None,
) -> dict | None:
    """One archived outbound segment in the model-facing send schema."""

    match segment:
        case TextSegment(text):
            return {"type": "text", "data": {"text": text}}
        case AtSegment(account):
            number = people.number(account) if people is not None else 0
            return {"type": "at", "data": {"member": number}} if number else None
        case ReplySegment(message_id):
            return (
                {"type": "reply", "data": {"line": nums[message_id]}}
                if message_id in nums else None
            )
        case FaceSegment(face_id):
            return {"type": "face", "data": {"id": face_id}}
        case MarketFaceSegment(package_id, emoji_id, key, summary):
            data = {"package_id": package_id, "emoji_id": emoji_id, "key": key}
            if summary:
                data["summary"] = summary
            return {"type": "mface", "data": data}
        case DiceSegment():
            return {"type": "dice", "data": {}}
        case RpsSegment():
            return {"type": "rps", "data": {}}
        case ContactSegment(ContactKind.MEMBER, target_id):
            number = people.number(target_id) if people is not None else 0
            return (
                {"type": "contact_member", "data": {"member": number}}
                if number else None
            )
        case ContactSegment():
            return {"type": "contact_group", "data": {}}
        case MusicSegment(platform, track_id):
            return {
                "type": "music",
                "data": {"platform": platform.value, "id": track_id},
            }
        case CustomMusicSegment(url, audio, title, image, singer):
            data = {"url": url, "audio": audio, "title": title, "image": image}
            if singer:
                data["singer"] = singer
            return {"type": "music_custom", "data": data}
        case JsonCardSegment(data):
            try:
                payload = json.loads(data)
            except json.JSONDecodeError:
                return None
            return {"type": "json", "data": {"payload": payload}}


def own_line(
    m: ChatMsg,
    *,
    nums: dict[str, int],
    people: MemberNumbers | None,
    evidence: str = "",
) -> list[PromptItem]:
    """One of the bot's own messages, as the send call that sent it and its result.

    The model sends every reply through the send tool, so its past messages are
    shown in exactly that form: the text, whom it @-ed and which line it replied to
    as arguments, then a tool result carrying the line number, the send time and the
    provenance marker. Shown this way, what the model reads of its own output is the
    shape it should produce, rather than a transcript line - numbers, stamps,
    brackets - that it would copy into the text.

    A rendered evidence memo rides immediately before the send call it supported. It is
    reconstructed from bounded structured storage and remains separate from the archive.
    """
    body = m.text
    prov = ""
    if found := _PROV_TAIL.search(body):
        prov, body = found.group(1), body[:found.start()]
    if m.outbound:
        content = [
            item
            for segment in m.outbound
            if (item := _segment_arg(segment, nums=nums, people=people)) is not None
        ]
        # Command replies were historically archived with reply metadata but without
        # an explicit reply segment. Preserve that relation during the transition.
        if (
            m.reply_to
            and m.reply_to in nums
            and not any(item["type"] == "reply" for item in content)
        ):
            content.insert(0, {"type": "reply", "data": {"line": nums[m.reply_to]}})
        args: dict = {"content": content}
    else:
        # Legacy rows without structured segments use the former flat contract.
        content = [{"type": "text", "data": {"text": body}}]
        for account, _ in m.at:
            number = people.number(account) if people is not None else 0
            if number:
                content.insert(-1, {"type": "at", "data": {"member": number}})
        if m.reply_to and (line := nums.get(m.reply_to)):
            content.insert(0, {"type": "reply", "data": {"line": line}})
        args = {"content": content}
    call_id = ToolCallId(_call_id(m.msg_id))
    result = f"已发送：#{nums.get(m.msg_id, 0)} {sysmark(fmt_when(m.ts))}"
    out: list[PromptItem] = []
    if evidence:
        out.append(Message(Role.ASSISTANT, evidence))
    out.extend(
        [
            ToolCall(call_id, SEND, json.dumps(args, ensure_ascii=False)),
            ToolResult(call_id, f"{result} {prov}" if prov else result),
        ]
    )
    return out


def render_history(
    window: list[ChatMsg],
    nums: dict[str, int],
    marks: dict[str, str],
    evidence: dict[str, str] | None = None,
    pics: dict[str, list[int]] | None = None,
    people: MemberNumbers | None = None,
) -> list[PromptItem]:
    """The window as chat messages: one user message per member line, one send
    call and its result per line the bot sent (see own_line).

    No picture rides in the history. Every marker carries a number and the model
    opens what it wants to see with open_images, so a message's render depends on
    nothing but the message and the numbering: it stays byte-identical between
    turns, and the prefix cache is never spent on a picture that a newer one pushed
    off a rail.
    """
    evidence = evidence or {}
    pics = pics or {}
    out: list[PromptItem] = []
    for m in window:
        if m.is_bot:
            out.extend(
                own_line(
                    m,
                    nums=nums,
                    people=people,
                    evidence=evidence.get(m.msg_id, ""),
                )
            )
            continue
        line = m.render(seq=nums.get(m.msg_id, 0), quote=marks.get(m.msg_id, ""),
                        pic_nums=pics.get(m.msg_id),
                        member_no=people.number(m.user_id) if people is not None else 0)
        out.append(Message(Role.USER, line))
    return out


def build_tail(*, msg: ChatMsg,
               nums: dict[str, int] | None = None,
               marks: dict[str, str] | None = None,
               pics: dict[str, list[int]] | None = None,
               people: MemberNumbers | None = None) -> str:
    """Everything after the cache boundary: the clock, then the current message.

    Nothing else is pushed here on purpose. Whatever sits in this tail is the
    nearest context the incoming message has, and an elliptical question will
    resolve against it in preference to the transcript above, so a block of past
    events pushed here hijacks the question. The past is pulled (recall_events),
    never pushed.
    """
    parts: list[str] = []

    # A model has no clock. This has to sit after the cache boundary: in the system
    # block it would change every minute and cost the prefix cache on every call.
    # Here it is already past the boundary, so it is free.
    parts.append("当前时间：" + describe_now() + "。")

    nums, marks, pics = nums or {}, marks or {}, pics or {}
    now = msg.render(seq=nums.get(msg.msg_id, 0), quote=marks.get(msg.msg_id, ""),
                     pic_nums=pics.get(msg.msg_id),
                     member_no=people.number(msg.user_id) if people is not None else 0)
    # Each reply task carries exactly one addressed message; this header is the
    # anchor reply_final points at when naming which message to answer.
    parts.append("下面是刚收到的消息：\n" + now)
    # Which message to answer, said outright: the history is context; only this block
    # is the question (see config/prompts/README.md, key "reply_final").
    parts.append(ptext("reply_final"))
    return "\n\n".join(parts)


def assemble(
    *,
    persona: Persona,
    cfg: Settings,
    st: GroupState,
    msg: ChatMsg,
    profiles: list[dict],
    group_facts: list[str] | None = None,
    evidence: dict[str, str] | None = None,
    window: list[ChatMsg] | None = None,
    nums: dict[str, int] | None = None,
    marks: dict[str, str] | None = None,
    pics: dict[str, list[int]] | None = None,
    people: MemberNumbers | None = None,
) -> tuple[PromptItem, ...]:
    # One pass over the window for both halves: the marks have to agree across the cache
    # boundary, since a quote in the message being answered points at a numbered line
    # above it. A caller that also runs the tool loop MUST pass its own computation
    # in (window/nums/marks/people) - the tool context resolves the numbers the model
    # writes back against these very maps, and two passes that merely happen to agree
    # while nothing appends to the deque in between are not the same thing.
    if window is None:
        window = history_window(st, msg, cfg)
        nums, marks = numbered(window + [msg])
    if pics is None:
        pics, _ = numbered_images(window + [msg])
    if people is None:
        people = MemberNumbers()
        number_people(people, profiles, window, msg)
    messages: list[PromptItem] = [
        Message(Role.SYSTEM, build_policy()),
        Message(
            Role.DEVELOPER,
            build_developer(persona, profiles, group_facts, people),
        ),
    ]
    messages.extend(render_history(window, nums, marks, evidence, pics, people))
    messages.append(
        Message(
            Role.USER,
            build_tail(msg=msg, nums=nums, marks=marks, pics=pics, people=people),
        )
    )
    return tuple(messages)
