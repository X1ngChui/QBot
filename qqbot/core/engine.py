"""The conversation engine: retrieve -> assemble -> call -> strip -> send.

There is no provider fallback and no downgrade, so any failure means the bot says
nothing.

A reply is a call to the send tool (tools.SEND): the text, whom to @ and which
line to reply to are arguments the model fills in, and every one but the text is
optional - with none, the message goes out plain. The call is the only way out:
bare text is never sent. A round that ends in bare text is told so once and gets
one more round to send it properly - the vendor's thinking mode refuses a forced
tool choice, and a deliberating model does now and then write its answer out
instead of calling the tool; a second bare-text round is silence.

Money is what bounds the tool loop - no round count, no per-tool quota. Free tools
run as often as they like; what bounds them is that the rounds carrying them are
paid model calls, billed into the scope opened around this reply. Once that scope
is spent the searching stops and one wrap-up round, offered only the send tool,
answers from what was already fetched: a mid-reply limit ends the spending, not
the speech. Only the daily cap, checked before anything is spent, means silence.
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
from collections.abc import Callable, Coroutine
from dataclasses import dataclass, field

from ..db import repo
from ..gateway.ingest import ingestor
from ..providers import Kind, providers
from ..providers.base import QuotaExhausted
from ..settings import Persona, Settings
from ..util import SYS_L, SYS_R, defang, now_local, sysmark, why
from . import debug, prompt, retrieval, tools
from .botapi import BotApi
from .budget import BUDGET
from .member_numbers import MemberNumbers
from .members import MEMBERS
from .output import clean_reply
from .state import ChatMsg, GroupState

log = logging.getLogger("qqbot.engine")

#: What a tool request is answered with when it is not executed: after the
#: allowance died mid-round, past the per-round cap, or as a repeat of a call
#: already answered - and what the wrap-up round is told. Mechanical one-line
#: notices, so they live in code, not in the prompt registry.
QUOTA_NOTE = "（检索额度已用完，这个查询没有执行。）"
OVERFLOW_NOTE = ("（本轮工具调用次数已达上限，这个调用没有执行；"
                 "可先用已有结果，如需再查请下一轮再调用。）")
REPEAT_NOTE = "（这个查询刚执行过，结果就在上面。换个检索词，或用已有结果。）"
WRAP_UP_NOTE = ("（本次回复的额度已用完，不能再执行任何检索或查看；"
                "请只依据上文已有的材料，直接用 send_message 发出回复，"
                "不要提及额度或系统限制。）")
#: What a send call that cannot be sent is answered with, so the next round can
#: call it again properly.
SEND_UNREADABLE_NOTE = "（send_message 的参数无法解析，没有发出。请重新调用。）"
SEND_EMPTY_NOTE = "（send_message 的正文为空，没有发出。请写好正文后重新调用。）"
#: What a round that ended in bare text is told before its one more round.
UNSENT_NOTE = ("（你刚才输出的文字没有发到群里，因为没有调用 send_message；"
               "请直接调用 send_message 把要说的话发出，不要再输出其他文字。）")

#: The most accounts one message may @. A reply that @-s half the group is a
#: prompt injection's idea of fun, not an answer.
MAX_AT = 5

#: How each query tool reads in the provenance marker and the trace. One dict for
#: both, or a renamed tool would let the marker and the trace quietly diverge.
TOOL_VERB = {"web_search": "搜索", "search_history": "查档", "recall_events": "回忆"}

#: A member number as search results render it behind a name. Taken out of the
#: frozen trace: a number means something only inside the render that assigned it.
_MEMBER_NO = re.compile(rf"{re.escape(SYS_L)}\d{{1,9}}{re.escape(SYS_R)}")


@dataclass
class Reply:
    """One reply, as the model asked for it to be sent.

    `text` is the model's words before output cleaning. `at` holds the accounts to
    @, in order; `reply_to` the id of the message to reply to. `provenance` and
    `trace` are "" for a reply that used no tools: the marker is appended to the
    archived line, the trace filed beside it - neither reaches the group.
    """

    text: str
    at: list[str] = field(default_factory=list)
    reply_to: str | None = None
    provenance: str = ""
    trace: str = ""
    #: Display names the prompt showed for the accounts in `at`, for when the live
    #: member list does not know them.
    names: dict[str, str] = field(default_factory=dict)


def _unsent(group_id: str, text: str) -> None:
    """Log a final round that did not call the send tool. Nothing is sent for it:
    silence looks the same whether the bot chose it or something broke, so it is
    always logged."""
    if text.strip():
        log.warning("group %s: the model wrote %d chars without calling %s; nothing "
                    "sent: %r", group_id, len(text), tools.SEND, text[:60])
    else:
        log.warning("group %s: the model ended without calling %s; nothing sent",
                    group_id, tools.SEND)


def parse_send(call: dict, *, people: MemberNumbers,
               lines: dict[int, ChatMsg]) -> tuple[Reply | None, str]:
    """(the reply a send call asks for, "") or (None, the note to answer it with).

    Only what makes the message unsendable is refused: no readable arguments, or
    no text. A member number or line number the prompt never showed is dropped
    with a log line and the rest still goes out - a round spent correcting an @ is
    a round the asker waits for. `at` given as one number rather than a list is
    read as that one.
    """
    raw = (call.get("function", {}) or {}).get("arguments") or "{}"
    try:
        args = json.loads(raw)
    except json.JSONDecodeError:
        return None, SEND_UNREADABLE_NOTE
    if not isinstance(args, dict):
        return None, SEND_UNREADABLE_NOTE
    text = args.get("text")
    if not isinstance(text, str) or not text.strip():
        return None, SEND_EMPTY_NOTE

    at_arg = args.get("at")
    wanted = at_arg if isinstance(at_arg, list) else [at_arg] if at_arg is not None else []
    at: list[str] = []
    for item in wanted:
        n = tools.number(item)
        account = people.account(n) if n is not None else None
        if account is None:
            log.info("send: no member numbered %r in this prompt, @ dropped", item)
        elif account not in at:
            at.append(account)
    if len(at) > MAX_AT:
        log.info("send: %d accounts to @, keeping the first %d", len(at), MAX_AT)
        at = at[:MAX_AT]

    reply_to: str | None = None
    if (r := args.get("reply")) is not None:
        target = lines.get(tools.number(r) or 0)
        if target is None:
            log.info("send: no line numbered %r in this prompt, sent without replying", r)
        else:
            reply_to = target.msg_id
    return Reply(text=text, at=at, reply_to=reply_to), ""


async def _wrap_up(messages: list, *, cfg: Settings, st: GroupState,
                   round_no: int, people: MemberNumbers,
                   lines: dict[int, ChatMsg]) -> Reply | None:
    """One last call, offered only the send tool, after a limit trips mid-reply.

    The material already fetched is sitting in the tail, paid for; discarding it
    buys nothing but silence. The note says why, and the overshoot is exactly this
    one round. A failure here still ends in silence - the wrap-up is a chance, not
    a guarantee.
    """
    messages.append({"role": "user", "content": WRAP_UP_NOTE})
    try:
        res = await providers().text.chat(
            messages, cfg=cfg.llm.text, tools=[tools.send_def()],
            kind=Kind.REPLY, group_id=st.group_id)
    except Exception as e:
        log.warning("group %s: wrap-up round failed, staying silent: %s",
                    st.group_id, why(e))
        return None
    debug.capture(st.group_id, round_no, messages, res)
    if sends := [c for c in res.tool_calls if _name(c) == tools.SEND]:
        reply, note = parse_send(sends[0], people=people, lines=lines)
        if reply is None:
            log.warning("group %s: the wrap-up send could not be sent: %s",
                        st.group_id, note)
        return reply
    _unsent(st.group_id, res.text)
    return None


def _name(call: dict) -> str:
    return (call.get("function", {}) or {}).get("name") or ""


def _provenance(executed: list[tuple[str, dict, str]], cfg: Settings) -> str:
    """The provenance marker for a reply that used tools - what this answer rested on.

    Appended to the archived/history form of the bot's own line, never to what the
    group is sent. It exists because the bot's past replies are the only trace its
    tool work leaves: without it, a later turn cannot tell an answer backed by a
    search from one improvised off the context, so it either re-searches what was
    just searched or - worse - cites its own guess as fact. Fixed at send time, so
    the line stays byte-identical between turns (cache-safe), and credibility_rules
    explains the epistemics: marked lines may be cited, unmarked ones re-verified.
    """
    parts: list[str] = []
    for name, args, out in executed:
        if not tools.verified(out):
            # A failed or empty-handed call earned no badge: the marker is what
            # tells a later turn this line may be cited without re-checking, and
            # an answer improvised after a failed search is exactly the guess it
            # must not exempt. The trace still records the attempt honestly.
            continue
        if name == "read_url":
            parts.append("读了网页")
        elif name == "open_images":
            # No number: numbering is per-render and shifts on every eviction, while
            # this marker is frozen into the archive - a stale one would point at
            # whatever picture sits there next week. That a picture was looked at is
            # the part that stays true.
            parts.append("看了图")
        elif name in TOOL_VERB:
            q = str(args.get("query") or args.get("question") or "").strip()
            parts.append(f"{TOOL_VERB[name]}“{q[:cfg.prompt.provenance_query_chars]}”")
    if not parts:
        return ""
    cap = cfg.prompt.provenance_items
    shown, extra = parts[:cap], len(parts) - cap
    # defang the queries: they are model-written, and a model echoing chat can
    # echo anything. The wrap itself is the reserved pair.
    return sysmark("依据:" + defang("、".join(shown)) + ("等" if extra > 0 else ""))


def _label(name: str, args: dict, cfg: Settings) -> str:
    """One tool use as the trace names it - the provenance vocabulary, reused.

    No numbers anywhere in here: a number is a position in one render, and this
    text is frozen into reply_trace forever."""
    # defang the model's own arguments: the trace is frozen and replayed.
    if name == "read_url":
        url = defang(str(args.get("url") or ""))
        return f"读网页 {url[:cfg.prompt.provenance_query_chars]}"
    if name == "open_images":
        return "看了图"
    q = defang(str(args.get("query") or args.get("question") or "")).strip()
    return f"{TOOL_VERB.get(name, name)}“{q}”"


def _trace(executed: list[tuple[str, dict, str]], cfg: Settings) -> str:
    """The trajectory entry kept in the conversation window beside the reply it fed.

    The reply is the model's synthesis for the question that was asked; a follow-up
    on the same topic often needs a different slice of the same results, and without
    this it either re-searches what was just searched or leans on its own summary.
    Digested per call and capped as a whole (prompt.trace_result_chars and
    prompt.trace_total_chars) - a record for reuse, not a replay.
    Persisted in reply_trace and nowhere else: tool output is not something said
    in the group, so it never enters the archive and search_history must not start
    returning the bot's own search results. Prompt assembly queries the table for
    the window's replies and seats each entry with the send call it fed - the deque
    itself holds only conversation, and eviction needs no bookkeeping.
    """
    if not executed:
        return ""
    lines = [sysmark("检索记录")]
    used = 0
    for name, args, out in executed:
        # Member numbers come out first: they belong to the render that produced
        # this result, and the entry is replayed under later ones. What is left
        # is defanged - web pages and search results are outside text, and a page
        # carrying the system brackets must not smuggle markup into the trace.
        digest = defang(" ".join(_MEMBER_NO.sub("", out or "").split()))
        digest = digest[:cfg.prompt.trace_result_chars]
        line = f"{_label(name, args, cfg)}：{digest}"
        if used + len(line) > cfg.prompt.trace_total_chars:
            lines.append("（其余从略）")
            break
        lines.append(line)
        used += len(line)
    return "\n".join(lines)


async def generate(
    *,
    bot: BotApi,
    st: GroupState,
    cfg: Settings,
    persona: Persona,
    msg: ChatMsg,
    window: list[ChatMsg] | None = None,
) -> Reply | None:
    """The reply to `msg`, as the model asked for it to be sent; None for silence."""
    # Names in history were captured when each message arrived. Re-read them so a rename
    # does not leave the same person appearing under two names across the prompt.
    await MEMBERS.relabel(bot, st.group_id, list(st.recent))

    profiles = await retrieval.gather(group_id=st.group_id, bot=bot)
    # Episodic memory is not pushed here: what is injected uninvited sits right next
    # to the incoming message, and an elliptical question resolves against it instead
    # of against the conversation. The model pulls with recall_events instead.
    #
    # Window and numbering are computed once and handed to both the tool context and
    # assemble: every number the model writes back - a picture to open, a member to
    # @, a line to reply to - resolves against the maps of the very render it read.
    # The caller normally passes the window in, cut when the message arrived, so
    # later arrivals cannot shift what this reply is looking at. Trajectories are
    # fetched from reply_trace by id - the deque holds only conversation.
    if window is None:
        window = prompt.history_window(st, msg, cfg)
    shown = window + [msg]
    nums, marks = prompt.numbered(shown)
    pics, by_pic = prompt.numbered_images(shown)
    lines = {n: m for m in shown if (n := nums.get(m.msg_id))}

    people = MemberNumbers(self_id=str(bot.self_id))
    prompt.teach_roster(people, profiles)
    await people.learn([m.user_id for m in shown]
                       + [a for m in window if m.is_bot for a, _ in m.at])
    prompt.number_people(people, profiles, window, msg)

    ctx = tools.ToolCtx(bot=bot, by_pic=by_pic, people=people)
    traces = await repo.traces_for(
        int(st.group_id), [m.msg_id for m in window if m.is_bot])
    messages = prompt.assemble(
        persona=persona,
        cfg=cfg,
        st=st,
        msg=msg,
        profiles=profiles,
        group_facts=await retrieval.group_knowledge(st.group_id),
        traces=traces,
        window=window, nums=nums, marks=marks, pics=pics, people=people,
    )
    names = {m.user_id: m.nickname for m in shown if not m.is_bot}
    names.update({a: n for m in window if m.is_bot for a, n in m.at if n})

    def done(reply: Reply | None) -> Reply | None:
        if reply is not None:
            reply.provenance, reply.trace = _provenance(executed, cfg), _trace(executed, cfg)
            reply.names = {a: names[a] for a in reply.at if a in names}
        return reply

    # Web for what the model cannot know; the archive for what the group said outside
    # the window it can see; recalled events for what was said in other words than the
    # archive holds; a page read for a link somebody posted; a second look at a
    # picture for details past its one-line description. The middle two are the pull
    # half of context - the prompt pushes a fixed window, and they are the only way
    # to reach anything behind it.
    tool_defs = tools.tool_defs(cfg)
    seen_calls: set[tuple[str, str]] = set()
    executed: list[tuple[str, dict, str]] = []
    told_unsent = False

    with BUDGET.scope(cfg.budget.per_reply_cny) as spend:
        for round_no in range(cfg.retrieval.max_rounds):
            res = await providers().text.chat(
                messages,
                cfg=cfg.llm.text,
                tools=tool_defs,
                # Deliberation for replies is the config grade
                # (llm.text.reasoning_effort); no per-call override here.
                kind=Kind.REPLY,
                group_id=st.group_id,
            )
            debug.capture(st.group_id, round_no, messages, res)
            if not res.tool_calls:
                if told_unsent or not res.text.strip() or spend.exhausted:
                    _unsent(st.group_id, res.text)
                    return None
                log.info("group %s: round %d wrote its reply without calling %s; "
                         "telling it so", st.group_id, round_no + 1, tools.SEND)
                told_unsent = True
                messages += [{"role": "assistant", "content": res.text},
                             {"role": "user", "content": UNSENT_NOTE}]
                continue

            # A send ends the reply, whatever else the round asked for: the calls
            # beside it could only feed a round that will not happen. One that
            # cannot be sent is answered in words like any failed tool, and the
            # loop goes on so the model can send again.
            send_note = ""
            if sends := [c for c in res.tool_calls if _name(c) == tools.SEND]:
                reply, send_note = parse_send(sends[0], people=people, lines=lines)
                if reply is not None:
                    if len(res.tool_calls) > 1:
                        log.info("group %s: %d other call(s) sent alongside the reply "
                                 "left unexecuted", st.group_id, len(res.tool_calls) - 1)
                    return done(reply)

            # The gate sits between the request for tools and their execution:
            # tools whose results no further round could read would burn search
            # allowance and latency for nothing. It reads money already spent,
            # never a forecast of the next round - a forecast needs a price for
            # the model, and one the price table does not know fails it forever,
            # disarming the tool loop at zero spend. The requested calls are
            # dropped unexecuted and the wrap-up answers from what earlier
            # rounds fetched.
            if spend.exhausted:
                log.info("group %s: per-reply budget exhausted after %d round(s), "
                         "wrapping up with the send tool only (%.4f of %.4f used)",
                         st.group_id, round_no + 1, spend.spent, spend.cap)
                return done(await _wrap_up(
                    messages, cfg=cfg, st=st,
                    round_no=round_no + 1, people=people, lines=lines))

            # Tool results stay at the very tail, after the cache boundary: they
            # differ every round, and anything above them would be re-read with them.
            messages.append(
                {"role": "assistant", "content": res.text or None,
                 "tool_calls": res.tool_calls}
            )
            answers, quota_hit = await _run_round(
                res.tool_calls, cfg=cfg, st=st, ctx=ctx,
                seen_calls=seen_calls, executed=executed, send_note=send_note)
            messages.extend(answers)
            if quota_hit:
                return done(await _wrap_up(
                    messages, cfg=cfg, st=st,
                    round_no=round_no + 1, people=people, lines=lines))
            log.info("group %s: tool round %d done (%.4f CNY of %.4f used)",
                     st.group_id, round_no + 1, spend.spent, spend.cap)

    log.error("group %s: %d tool rounds without running out of money - a backend "
              "is billing zero; giving up", st.group_id, cfg.retrieval.max_rounds)
    return None


async def _run_round(
    calls: list[dict], *, cfg: Settings, st: GroupState, ctx: tools.ToolCtx,
    seen_calls: set[tuple[str, str]], executed: list[tuple[str, dict, str]],
    send_note: str = "",
) -> tuple[list[dict], bool]:
    """One round's tool requests, each answered: the tool messages to append, and
    whether the allowance died on the way.

    Every request gets its tool message whatever became of it, so the next call
    is protocol-clean. Every tool here is free per call - a SQL query, an HTTP
    fetch, a file upload; what money bounds is the rounds that carry them, each
    a paid model call. What is not executed is answered in words: a send that
    could not be sent (`send_note` says why), a repeat of a call already answered
    (nothing here is paginated, so the same call returns the same bytes, and a
    model repeating itself is a model stuck - handing it identical results once
    more is how a bounded loop spends its whole bound standing still), anything
    past the per-round cap, and everything after the allowance died mid-round.
    The cap is retrieval.max_tool_calls_per_round: each result is appended to the
    prompt, and a round asking for thirty pages at once would grow the next
    request past the model's context before money had a chance to bind - a failed
    call and silence, not a budget stop.
    """
    cap = cfg.retrieval.max_tool_calls_per_round
    answers: list[dict] = []
    quota_hit = False
    for n, call in enumerate(calls):
        name = _name(call)
        raw = ((call.get("function", {}) or {}).get("arguments") or "").strip()
        try:
            args = json.loads(raw or "{}")
        except json.JSONDecodeError:
            args = None
        # Keyed on the parsed arguments, so the same query spelled with different
        # whitespace or key order is the same call. execute() parses again and
        # answers a malformed string in-band; the raw string keys those.
        key = (name, json.dumps(args, sort_keys=True, ensure_ascii=False)
               if isinstance(args, dict) else raw)
        if name == tools.SEND:
            out = send_note or SEND_UNREADABLE_NOTE
        elif quota_hit:
            out = QUOTA_NOTE
        elif n >= cap:
            out = OVERFLOW_NOTE
        elif key in seen_calls:
            out = REPEAT_NOTE
        else:
            try:
                out = await tools.execute(call, cfg=cfg, group_id=st.group_id, ctx=ctx)
            except QuotaExhausted as e:
                log.info("group %s: %s - wrapping up on what is already fetched",
                         st.group_id, why(e))
                quota_hit = True
                out = QUOTA_NOTE
            else:
                executed.append((name, args if isinstance(args, dict) else {}, out))
                # Only a call that obtained something is a repeat worth refusing:
                # a failed one (a network blip, a bad page) may be retried, and
                # "the result is above" would be false for it.
                if tools.verified(out):
                    seen_calls.add(key)
        # A picture answers as a content array carrying the file block; the
        # vendor takes one on a tool message, so what was asked for arrives
        # as the answer to the call rather than as a turn appended behind it.
        answers.append({
            "role": "tool", "tool_call_id": call.get("id", ""),
            "content": out.content() if isinstance(out, tools.Attachment) else out,
        })
    if len(calls) > cap:
        log.info("group %s: %d tool calls in one round, %d past the cap left unexecuted",
                 st.group_id, len(calls), len(calls) - cap)
    return answers, quota_hit


def _strip_addresses(text: str, names: list[str]) -> str:
    """The body without the @s the send adds itself.

    The group reads "@name text": the at segments carry the addresses, so a model
    that also opened its text with them would send each one twice. Only whole
    names of the accounts being @-ed, at the very start: an address of a longer
    name that merely starts with one must survive, as must anyone else's.
    """
    forms = sorted({n for n in names if n}, key=len, reverse=True)
    while forms:
        for form in forms:
            m = re.match(rf"@{re.escape(form)}(?=$|\s|[:：,，@])[\s:：,，]*", text)
            if m:
                text = text[m.end():]
                break
        else:
            break
    return text


async def respond(
    *,
    bot: BotApi,
    st: GroupState,
    cfg: Settings,
    persona: Persona,
    msg: ChatMsg,
    window: list[ChatMsg] | None = None,
    track: Callable[[Coroutine], asyncio.Task] | None = None,
) -> bool:
    """One reply, sent. `track` registers the record of a delivered reply with
    whoever waits out loose work at shutdown, so the pool is not closed under
    it; without one the record runs as a bare task."""
    try:
        reply = await generate(
            bot=bot, st=st, cfg=cfg, persona=persona, msg=msg, window=window)
    except Exception as e:
        # A provider or transport failure is a one-line warning; anything else is
        # a fault in this code, and the traceback is the only way to find it.
        log.warning("group %s: generation failed, staying silent: %s", st.group_id, why(e),
                    exc_info=not _expected(e))
        return False

    if reply is None:
        return False   # generate logged why
    text = clean_reply(reply.text)
    if not text:
        # Silence is the one symptom that looks the same whether the bot chose
        # not to speak or something broke, so an emptied reply is always logged.
        log.warning("group %s: reply stripped to nothing (%d chars: %r)",
                    st.group_id, len(reply.text), reply.text[:60])
        return False

    live = await MEMBERS.names_of(bot, st.group_id, reply.at) if reply.at else {}
    addressees = [(a, live.get(a) or reply.names.get(a) or "成员") for a in reply.at]
    text = _strip_addresses(text, [n for _, n in addressees])
    if not text:
        log.warning("group %s: reply was nothing but the names it @-ed", st.group_id)
        return False
    if len(text) > cfg.gateway.max_msg_len:
        text = text[: cfg.gateway.max_msg_len]

    # What the bot remembers is what it sent, plus the provenance marker: the
    # window and the archive keep whom it @-ed and which line it replied to, so a
    # later turn can see whom each of its own answers was for - two people asking
    # at once get two answers, and without the addresses nothing ties either to
    # its question - and what the answer rested on, so a later turn can cite a
    # searched answer instead of re-searching, and knows an unmarked one was
    # improvised off the context.
    kept = f"{text} {reply.provenance}" if reply.provenance else text

    # Segments, not a string: segments are taken literally, so nothing in the
    # model's text can smuggle a control code into the send.
    ats: list[dict] = []
    for account, _ in addressees:
        ats += [{"type": "at", "data": {"qq": account}},
                {"type": "text", "data": {"text": " "}}]
    body = [*ats, {"type": "text", "data": {"text": text}}]
    message = ([{"type": "reply", "data": {"id": reply.reply_to}}] + body
               if reply.reply_to else body)
    reply_to = reply.reply_to or ""
    try:
        sent = await bot.send_group_msg(group_id=int(st.group_id), message=message)
    except Exception as e:
        # The replied-to message can be recalled between trigger and send, and a
        # reply segment pointing at a recalled id may be refused whole. The
        # words are already paid for - retry them once without the reply segment
        # before giving up. Only when the API itself refused (ActionFailed,
        # matched by name so core stays importable without the adapter) is the
        # send known undelivered; a transport error may have delivered it, and a
        # retry then would say the same thing twice.
        if not reply.reply_to or type(e).__name__ != "ActionFailed":
            log.warning("group %s: send failed: %s", st.group_id, why(e))
            return False
        log.warning("group %s: send with a reply segment failed (%s), retrying "
                    "without it", st.group_id, why(e))
        reply_to = ""
        try:
            sent = await bot.send_group_msg(group_id=int(st.group_id), message=body)
        except Exception as e2:
            log.warning("group %s: send failed: %s", st.group_id, why(e2))
            return False

    msg_id = str((sent or {}).get("message_id") or f"self-{now_local().timestamp()}")
    # The words are in the group now; the record of them must land whatever
    # happens to this task. Shielded because shutdown cancels reply tasks, and a
    # delivered reply missing from the window and the archive is exactly the
    # one-sided conversation the restart rebuild must never read.
    record = _record(bot, st, persona, msg_id=msg_id, text=kept, trace=reply.trace,
                     reply_to=reply_to, addressees=addressees)
    await asyncio.shield(track(record) if track else asyncio.create_task(record))
    log.info("group %s: replied (%d chars, %d @, %s)", st.group_id, len(text),
             len(addressees), "replying" if reply_to else "not replying")
    return True


def _expected(e: BaseException) -> bool:
    """Whether a generation failure is the kind a log line explains on its own:
    the provider or the network said no. Matched by name so this module stays
    importable without the SDK's error classes at hand."""
    names = {c.__name__ for c in type(e).__mro__}
    return bool(names & {"APIError", "HTTPError", "TimeoutError", "QuotaExhausted",
                         "OSError"})


async def _record(bot: BotApi, st: GroupState, persona: Persona, *, msg_id: str,
                  text: str, trace: str, reply_to: str,
                  addressees: list[tuple[str, str]]) -> None:
    """The bot's own line, into the window and the archive, with its trajectory.

    The trajectory is persisted and nothing else: reply_trace is the single
    source of truth, the deque holds only conversation, and the next prompt
    assembly queries the table for the window's replies and seats each entry
    with the send call it fed (see _trace and prompt.own_line).
    """
    now = now_local()
    st.add(
        ChatMsg(
            msg_id=msg_id,
            user_id=str(bot.self_id),
            nickname=persona.name,
            text=text,
            ts=now,
            is_bot=True,
            reply_to=reply_to or None,
            at=list(addressees),
        )
    )
    if trace:
        try:
            await repo.trace_add(int(st.group_id), msg_id, trace)
        except Exception:
            log.exception("failed to persist the reply trace in group %s", st.group_id)
    # Archive it like any other message. NapCat is configured not to report the bot's
    # own messages, so nothing else ever writes them down - and the archive feeds both
    # the restart-rebuilt history and the group card memory reads back, neither of which
    # may hold only one side of a conversation.
    try:
        await ingestor().record_own_reply(
            group_id=int(st.group_id),
            self_id=str(bot.self_id),
            message_id=msg_id,
            text=text,
            at=now,
            name=persona.name,
            reply_to=reply_to,
            addressees=addressees,
        )
    except Exception:
        log.exception("failed to archive the bot's own reply in group %s", st.group_id)
