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

from ..settings import Persona, Settings, ptext
from ..util import defang, describe_now, sysmark
from .state import ChatMsg, GroupState

# The history window is a message count, not a token budget: money bounds what a
# reply may spend, and every other block is rendered whole. The count and its
# eviction chunk are config (prompt.window_chunks, prompt.evict_chunk), and the
# window is their product so the multiple holds by construction. They serve context
# and the cache rather than cost - a prefix token is ~1/30 price, so a wider window
# is nearly free per call while every slide is the miss that costs. Trajectories are
# fetched from reply_trace at assembly and do not consume the count.

#: Section headings. The blocks below answer different questions and carry different
#: authority; without a marked boundary they read as one undifferentiated wall, and the
#: model weighs a guess it wrote last week the same as a fact the platform just returned.
H_PERSONA = "【你的身份】"
H_LEGEND = "【消息标记说明】"
H_GROUP = "【本群背景】"
H_RULES = "【信息解读规则】"
H_PRIVATE = "【不写进回复的内容】"
H_TONE = "【群聊语用】"
H_WHO = "【群成员名册】"


def _block(head: str, entries: list[tuple[str, str]]) -> str:
    """Join a heading and its entries, written out in account order.

    The order is the point: the account id never changes, so the block renders
    identically between turns. This sits inside the cached prefix, and an ordering
    that can change - any ranking by activity can, on any message - costs the cache
    from that line to the end of the prompt.
    """
    if not entries:
        return ""
    return head + "\n" + "\n".join(line for _, line in sorted(entries))


def _known_block(profiles: list[dict]) -> str:
    """What the system can vouch for about who somebody is.

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
        # defang on render as well as at ingest: names are stored as the platform
        # reported them, and the render is where the grammar must hold.
        name = defang(p.get("nickname") or "")
        bits = []
        if former := [defang(n) for n in (p.get("former_names") or [])
                      if n and defang(n) != name]:
            bits.append("曾用名：" + "、".join(former))
        if aliases := [defang(n) for n in (p.get("aliases") or [])
                       if n and defang(n) != name]:
            bits.append("别名：" + "、".join(aliases))
        if note := (p.get("manual_note") or "").strip():
            bits.append(note)
        if bits:
            lines.append((str(p.get("user_id") or ""), f"- {name}，" + "；".join(bits)))
    return _block("已确认（系统记录的名字，以及拥有者或成员本人写明的信息）：", lines)


def _guessed_block(profiles: list[dict]) -> str:
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
        name = defang(p.get("nickname") or p.get("user_id") or "")
        card = defang(p.get("persona_card") or "").strip()
        if not card:
            continue
        lines.append((str(p.get("user_id") or ""), f"- {name}。{card}"))
    return _block("未确认（你自行归纳的印象，不要直接复述）：", lines)


def build_system(
    persona: Persona,
    profiles: list[dict],
    group_facts: list[str] | None = None,
) -> str:
    # Constants lead, so every group shares this opening span instead of each paying for
    # its own. They also have to live here rather than in each persona: a bot that does not
    # know the picture marker is its own eyesight will deny seeing an image it is holding
    # the description of.
    # Text comes through ptext at call time: config/prompts/*.txt is the source of
    # truth, /reload applies.
    blocks = [
        H_LEGEND + "\n" + ptext("legend") + "\n\n" + ptext("legend_reply_note"),
        H_RULES + "\n" + ptext("identity_rules") + "\n\n" + ptext("credibility_rules"),
        H_PRIVATE + "\n" + ptext("private_rules"),
        H_TONE + "\n" + ptext("tone_rules") + "\n\n" + ptext("tone_reply_note"),
        H_PERSONA + "\n" + persona.system_prompt.strip(),
    ]

    # Split by origin, exactly like the identity block below. The hand-written half is a
    # statement; the card is the bot's own summary read back, and running them together
    # would present a guess in the voice of a fact - and a summary can be flatly
    # wrong, up to inventing a member outright. The
    # hand-written half is served first: it was typed deliberately, and it is what
    # corrects the other half when that goes wrong.
    fixed = persona.group_knowledge.strip()
    learned = "\n".join(f"- {f}" for f in (group_facts or []))
    if fixed or learned:
        parts = []
        if fixed:
            parts.append("已确认（固定资料）：\n" + fixed)
        if learned:
            parts.append("未确认（你自行归纳的印象，可能有误或已过时）：\n" + learned)
        blocks.append(H_GROUP + "\n" + "\n\n".join(parts))

    # Identity, split by how the system came to know each part. What code can offer that
    # reading the transcript cannot is certainty, and that is worth nothing unless the
    # certain lines are marked as such - otherwise a nickname the bot guessed last week
    # carries the same weight as a rename the platform reported. Everything outside the
    # certain block comes from the model's own summarising, which is what makes this
    # adaptive - it follows the group rather than a config file someone has to edit.
    known = _known_block(profiles)
    guessed = _guessed_block(profiles)
    parts = [x for x in (known, guessed) if x]
    if parts:
        blocks.append(H_WHO + "\n" + "\n\n".join(parts))

    return "\n\n".join(x for x in blocks if x)


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
    marker pointing at the same kind of thing. The bot's number does appear in an
    assistant turn, where it is an example the model may follow - output.clean_reply
    takes it back off, which is a guard in one place rather than a second quote format
    and a second rule in the legend.

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


def render_history(window: list[ChatMsg], nums: dict[str, int],
                   marks: dict[str, str],
                   traces: dict[str, str] | None = None,
                   pics: dict[str, list[int]] | None = None) -> list[dict]:
    """One chat message per line of transcript, text only.

    No picture rides in the history. Every marker carries a number and the model
    opens what it wants to see with open_images, so a message's render depends on
    nothing but the message: it stays byte-identical between turns, and the
    prefix cache is never spent on a picture that a newer one pushed off a rail.

    A reply with a stored trajectory gets it seated directly before it, as its own
    assistant message: what was looked up, then what was said. Unnumbered and
    unstamped on purpose - numbering is a property of what people can quote, and
    the entry's bytes come straight from the immutable reply_trace row, which is
    what keeps the rendering stable between turns. The deque never holds these;
    the table is the single source and eviction follows the reply's own.
    """
    traces = traces or {}
    pics = pics or {}
    out: list[dict] = []
    for m in window:
        if m.is_bot and (t := traces.get(m.msg_id)):
            out.append({"role": "assistant", "content": t})
        # The bot's own lines never carry the quote mark. Assistant-role content
        # is the strongest imitation signal there is - "what my output looks
        # like" - and a mark shown there comes back in real replies verbatim, which
        # prompt instruction does not reliably stop. Members' lines keep theirs:
        # user-role content teaches reading, not writing, and the pointer is how a
        # quote is understood at all.
        line = m.render(seq=nums.get(m.msg_id, 0),
                        quote="" if m.is_bot else marks.get(m.msg_id, ""),
                        pic_nums=pics.get(m.msg_id))
        out.append({"role": "assistant" if m.is_bot else "user", "content": line})
    return out


def build_tail(*, msg: ChatMsg,
               nums: dict[str, int] | None = None,
               marks: dict[str, str] | None = None,
               pics: dict[str, list[int]] | None = None) -> str:
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
                     pic_nums=pics.get(msg.msg_id))
    # Each reply task carries exactly one addressed message; this header is the
    # anchor reply_final points at when naming which message to answer.
    parts.append("下面是刚收到的消息：\n" + now)
    # Which message to answer, said outright: the history is context; only this block
    # is the question (the wording's full rationale sits in config/prompts/README.md,
    # key "reply_final").
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
    traces: dict[str, str] | None = None,
    window: list[ChatMsg] | None = None,
    nums: dict[str, int] | None = None,
    marks: dict[str, str] | None = None,
    pics: dict[str, list[int]] | None = None,
) -> list[dict]:
    messages = [
        {
            "role": "system",
            "content": build_system(persona, profiles, group_facts),
        }
    ]
    # One pass over the window for both halves: the marks have to agree across the cache
    # boundary, since a quote in the message being answered points at a numbered line
    # above it. A caller that also runs the tool loop MUST pass its own computation
    # in (window/nums/marks) - the tool context's seq->message map and the numbers
    # the model reads have to come from one pass, not from two passes that merely
    # happen to agree while nothing appends to the deque in between.
    if window is None:
        window = history_window(st, msg, cfg)
        nums, marks = numbered(window + [msg])
    if pics is None:
        pics, _ = numbered_images(window + [msg])
    messages.extend(render_history(window, nums, marks, traces, pics))
    messages.append({"role": "user",
                     "content": build_tail(msg=msg, nums=nums, marks=marks, pics=pics)})
    return messages
