"""The conversation engine: retrieve -> assemble -> call -> strip -> send.

There is no fallback and no downgrade, so any failure means the bot says nothing.

Money is what bounds the tool loop - no round count, no per-tool quota. Free tools
run as often as they like; what bounds them is that the rounds carrying them are
paid model calls, billed into the scope opened around this reply. Once that scope
is spent the searching stops and one tool-less wrap-up round answers from what was
already fetched: a mid-reply limit ends the spending, not the speech. Only the
daily cap, checked before anything is spent, means silence.
"""

from __future__ import annotations

import json
import logging

from ..db import repo
from ..gateway.ingest import ingestor
from ..providers import Kind, providers
from ..providers.base import QuotaExhausted
from ..settings import Persona, Settings
from ..util import defang, now_local, sysmark, why
from . import debug, prompt, retrieval, tools
from .budget import BUDGET
from .members import MEMBERS
from .output import clean_reply
from .state import ChatMsg, GroupState

log = logging.getLogger("qqbot.engine")

#: A tripwire, not a policy: money ends the loop, and only a backend reporting zero
#: cost could make a money-bounded loop unbounded. That failure deserves a loud log
#: and a stop rather than an endless loop.
RUNAWAY_ROUNDS = 20

#: What a tool request is answered with when it is not executed: after the
#: allowance died mid-round, past the per-round cap, or as a repeat of a call
#: already answered - and what the wrap-up round is told. Mechanical one-line
#: notices, so they live in code, not in the prompt registry.
QUOTA_NOTE = "（检索额度已用完，这个查询没有执行。）"
OVERFLOW_NOTE = ("（本轮工具调用次数已达上限，这个查询没有执行；"
                 "先用已有结果作答，如需再查请下一轮再发。）")
REPEAT_NOTE = "（这个查询刚执行过，结果就在上面。换个关键词，或用已有资料回答。）"
WRAP_UP_NOTE = ("（本次回复的额度已用完，不能再执行任何检索或查看；"
                "请只依据上文已有的材料直接作答，不要提及额度或系统限制。）")

#: Caps on the provenance marker appended to the bot's own archived line: how many
#: tool uses it names, and how much of each query survives. A record, not a transcript
#: - enough for a later turn to see what that answer rested on, not to replay it.
#: The query has to survive whole to be a record at all: a boolean search expression
#: cut in half reads as a different search than the one that ran.
PROV_ITEMS = 4
PROV_QUERY_CHARS = 80

#: How each query tool reads in the provenance marker and the trace. One dict for
#: both, or a renamed tool would let the marker and the trace quietly diverge.
TOOL_VERB = {"web_search": "搜索", "search_history": "查档", "recall_events": "回忆"}


async def _wrap_up(messages: list, *, cfg: Settings, st: GroupState,
                   executed: list[tuple[str, dict, str]],
                   round_no: int) -> tuple[str | None, str, str]:
    """One last tool-less call after a limit trips mid-reply.

    The material already fetched is sitting in the tail, paid for; discarding it
    buys nothing but silence. No tools are offered, the note says why, and the
    overshoot is exactly this one round. A failure here still ends in silence -
    the wrap-up is a chance, not a guarantee.
    """
    messages.append({"role": "user", "content": WRAP_UP_NOTE})
    try:
        res = await providers().text.chat(
            messages, cfg=cfg.llm.text, kind=Kind.REPLY, group_id=st.group_id)
    except Exception as e:
        log.warning("group %s: wrap-up round failed, staying silent: %s",
                    st.group_id, why(e))
        return None, "", ""
    debug.capture(st.group_id, round_no, messages, res)
    return res.text or None, _provenance(executed), _trace(executed, cfg)


def _provenance(executed: list[tuple[str, dict, str]]) -> str:
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
        elif name == "open_image":
            # No number: numbering is per-render and shifts on every eviction, while
            # this marker is frozen into the archive - a stale one would point at
            # whatever picture sits there next week. That a picture was looked at is
            # the part that stays true.
            parts.append("看了图")
        elif name in TOOL_VERB:
            q = str(args.get("query") or args.get("question") or "").strip()
            parts.append(f"{TOOL_VERB[name]}“{q[:PROV_QUERY_CHARS]}”")
    if not parts:
        return ""
    shown, extra = parts[:PROV_ITEMS], len(parts) - PROV_ITEMS
    # defang the queries: they are model-written, and a model echoing chat can
    # echo anything. The wrap itself is the reserved pair.
    return sysmark("依据:" + defang("、".join(shown)) + ("等" if extra > 0 else ""))


def _label(name: str, args: dict) -> str:
    """One tool use as the trace names it - the provenance vocabulary, reused.

    No numbers anywhere in here: a number is a position in one render, and this
    text is frozen into reply_trace forever."""
    if name == "read_url":
        return f"读网页 {str(args.get('url') or '')[:60]}"
    if name == "open_image":
        return "看了图"
    q = str(args.get("query") or args.get("question") or "").strip()
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
    the window's replies and seats each entry before the reply it fed - the deque
    itself holds only conversation, and eviction needs no bookkeeping.
    """
    if not executed:
        return ""
    lines = [sysmark("检索记录")]
    used = 0
    for name, args, out in executed:
        # Web pages and search results are outside text; a page carrying the
        # system brackets must not smuggle markup into the frozen trace.
        digest = defang(" ".join((out or "").split()))[:cfg.prompt.trace_result_chars]
        line = f"{_label(name, args)}：{digest}"
        if used + len(line) > cfg.prompt.trace_total_chars:
            lines.append("（其余从略）")
            break
        lines.append(line)
        used += len(line)
    return "\n".join(lines)


async def generate(
    *,
    bot,
    st: GroupState,
    cfg: Settings,
    persona: Persona,
    batch: list[ChatMsg],
    window: list[ChatMsg] | None = None,
) -> tuple[str | None, str, str]:
    """Returns (reply text, provenance marker, trajectory entry). Both extras are ""
    for a reply that used no tools; the caller appends the marker to the archived
    line and files the trajectory as its own window entry - neither reaches what is
    sent to the group."""
    # Names in history were captured when each message arrived. Re-read them so a rename
    # does not leave the same person appearing under two names across the prompt.
    await MEMBERS.relabel(bot, st.group_id, list(st.recent))

    profiles = await retrieval.gather(group_id=st.group_id, bot=bot)
    # Episodic memory is not pushed here: what is injected uninvited sits right next
    # to the incoming message, and an elliptical question resolves against it instead
    # of against the conversation. The model pulls with recall_events instead.
    #
    # Window and numbering are computed once and handed to both the tool context and
    # assemble: the picture numbers open_image resolves against and the numbers
    # the model reads must come from the same pass. The caller normally passes the
    # window in, cut when the message arrived, so later arrivals cannot shift what
    # this reply is looking at. Trajectories are fetched from reply_trace by id - the
    # deque holds only conversation - and render_history seats each before the reply
    # it fed, so eviction needs no bookkeeping.
    if window is None:
        window = prompt.history_window(st, batch, cfg)
    nums, marks = prompt.numbered(window + list(batch))
    pics, by_pic = prompt.numbered_images(window + list(batch))
    ctx = tools.ToolCtx(bot=bot, by_pic=by_pic)
    traces = await repo.traces_for(
        int(st.group_id), [m.msg_id for m in window if m.is_bot])
    messages = prompt.assemble(
        persona=persona,
        cfg=cfg,
        st=st,
        batch=batch,
        profiles=profiles,
        group_facts=await retrieval.group_knowledge(st.group_id),
        traces=traces,
        window=window, nums=nums, marks=marks, pics=pics,
    )

    # Web for what the model cannot know; the archive for what the group said outside
    # the window it can see; recalled events for what was said in other words than the
    # archive holds; a page read for a link somebody posted; a second look at a
    # picture for details past its one-line description. The middle two are the pull
    # half of context - the prompt pushes a fixed window, and they are the only way
    # to reach anything behind it.
    tool_defs = tools.tool_defs()
    seen_calls: set[tuple[str, str]] = set()
    executed: list[tuple[str, dict, str]] = []

    with BUDGET.scope(cfg.budget.per_reply_cny) as spend:
        for round_no in range(RUNAWAY_ROUNDS):
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
                return res.text or None, _provenance(executed), _trace(executed, cfg)

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
                         "wrapping up without tools (%.4f of %.4f used)",
                         st.group_id, round_no + 1, spend.spent, spend.cap)
                return await _wrap_up(messages, cfg=cfg, st=st,
                                      executed=executed, round_no=round_no + 1)

            # Tool results stay at the very tail, after the cache boundary: they
            # differ every round, and anything above them would be re-read with them.
            messages.append(
                {"role": "assistant", "content": res.text or None,
                 "tool_calls": res.tool_calls}
            )
            answers, quota_hit = await _run_round(
                res.tool_calls, cfg=cfg, st=st, ctx=ctx,
                seen_calls=seen_calls, executed=executed)
            messages.extend(answers)
            if quota_hit:
                return await _wrap_up(messages, cfg=cfg, st=st,
                                      executed=executed, round_no=round_no + 1)
            log.info("group %s: tool round %d done (%.4f CNY of %.4f used)",
                     st.group_id, round_no + 1, spend.spent, spend.cap)

    log.error("group %s: %d tool rounds without running out of money - a backend "
              "is billing zero; giving up", st.group_id, RUNAWAY_ROUNDS)
    return None, "", ""


async def _run_round(
    calls: list[dict], *, cfg: Settings, st: GroupState, ctx: tools.ToolCtx,
    seen_calls: set[tuple[str, str]], executed: list[tuple[str, dict, str]],
) -> tuple[list[dict], bool]:
    """One round's tool requests, each answered: the tool messages to append, and
    whether the allowance died on the way.

    Every request gets its tool message whatever became of it, so the next call
    is protocol-clean. Every tool here is free per call - a SQL query, an HTTP
    fetch, a file upload; what money bounds is the rounds that carry them, each
    a paid model call. What is not executed is answered in words: a repeat of a
    call already answered (nothing here is paginated, so the same call returns
    the same bytes, and a model repeating itself is a model stuck - handing it
    identical results once more is how a bounded loop spends its whole bound
    standing still), anything past the per-round cap, and everything after the
    allowance died mid-round. The cap is retrieval.max_tool_calls_per_round:
    each result is appended to the prompt, and a round asking for thirty pages at
    once would grow the next request past the model's context before money had a
    chance to bind - a failed call and silence, not a budget stop.
    """
    cap = cfg.retrieval.max_tool_calls_per_round
    answers: list[dict] = []
    quota_hit = False
    for n, call in enumerate(calls):
        fn = call.get("function", {}) or {}
        name = fn.get("name") or ""
        raw = (fn.get("arguments") or "").strip()
        try:
            args = json.loads(raw or "{}")
        except json.JSONDecodeError:
            args = None
        # Keyed on the parsed arguments, so the same query spelled with different
        # whitespace or key order is the same call. execute() parses again and
        # answers a malformed string in-band; the raw string keys those.
        key = (name, json.dumps(args, sort_keys=True, ensure_ascii=False)
               if isinstance(args, dict) else raw)
        if quota_hit:
            out = QUOTA_NOTE
        elif n >= cap:
            out = OVERFLOW_NOTE
        elif key in seen_calls:
            out = REPEAT_NOTE
        else:
            seen_calls.add(key)
            try:
                out = await tools.execute(call, cfg=cfg, group_id=st.group_id, ctx=ctx)
            except QuotaExhausted as e:
                log.info("group %s: %s - wrapping up on what is already fetched",
                         st.group_id, why(e))
                quota_hit = True
                out = QUOTA_NOTE
            else:
                executed.append((name, args if isinstance(args, dict) else {}, out))
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


async def respond(
    *,
    bot,
    st: GroupState,
    cfg: Settings,
    persona: Persona,
    batch: list[ChatMsg],
    window: list[ChatMsg] | None = None,
    reply_to: str = "",
    initiator: str = "",
) -> bool:
    try:
        raw, prov, trace = await generate(
            bot=bot, st=st, cfg=cfg, persona=persona, batch=batch, window=window)
    except Exception as e:
        log.warning("group %s: generation failed, staying silent: %s", st.group_id, why(e))
        return False

    text = clean_reply(raw or "")
    if not text:
        return False
    if len(text) > cfg.gateway.max_msg_len:
        text = text[: cfg.gateway.max_msg_len]

    # What the group reads and what the bot remembers differ by exactly the
    # provenance marker: the archived/window form carries what this answer rested
    # on, so a later turn can cite a searched answer instead of re-searching, and
    # knows an unmarked one was improvised off the context.
    kept = f"{text} {prov}" if prov else text

    # The reply quotes the message that asked for it and @-es its sender - the
    # exact shape QQ's own reply button produces, so the answer reads native and
    # the asker gets their notification. A segment array, not a string: segments
    # are taken literally, so nothing in the model's text can smuggle a control
    # code into the send.
    message: list = [{"type": "text", "data": {"text": text}}]
    if reply_to and initiator:
        message = [{"type": "reply", "data": {"id": reply_to}},
                   {"type": "at", "data": {"qq": initiator}},
                   {"type": "text", "data": {"text": " " + text}}]
    try:
        sent = await bot.send_group_msg(group_id=int(st.group_id), message=message)
    except Exception as e:
        # The quoted message can be recalled between trigger and send, and a
        # reply segment pointing at a recalled id may be refused whole. The
        # words are already paid for - retry them once, bare, before giving up.
        # Only when the API itself refused (ActionFailed, matched by name so
        # core stays importable without the adapter) is the send known
        # undelivered; a transport error may have delivered it, and a retry
        # then would say the same thing twice.
        if len(message) == 1 or type(e).__name__ != "ActionFailed":
            log.warning("group %s: send failed: %s", st.group_id, why(e))
            return False
        log.warning("group %s: quoted send failed (%s), retrying as plain text",
                    st.group_id, why(e))
        try:
            sent = await bot.send_group_msg(
                group_id=int(st.group_id),
                message=[{"type": "text", "data": {"text": text}}])
        except Exception as e2:
            log.warning("group %s: send failed: %s", st.group_id, why(e2))
            return False

    msg_id = str((sent or {}).get("message_id") or f"self-{now_local().timestamp()}")
    now = now_local()
    # The trajectory is persisted and nothing else: reply_trace is the single
    # source of truth, the deque holds only conversation, and the next prompt
    # assembly queries the table for the window's replies and seats each entry
    # right before the reply it fed (see _trace and prompt.render_history).
    if trace:
        try:
            await repo.trace_add(int(st.group_id), msg_id, trace)
        except Exception:
            log.exception("failed to persist the reply trace in group %s", st.group_id)
    st.add(
        ChatMsg(
            msg_id=msg_id,
            user_id=str(bot.self_id),
            nickname=persona.name,
            text=kept,
            ts=now,
            is_bot=True,
            # The same quote pointer any member's reply carries: the window line
            # renders with the ordinary quote mark, so the model can see which
            # message each of its own answers was anchored to.
            reply_to=reply_to or None,
        )
    )
    # Archive it like any other message. NapCat is configured not to report the bot's
    # own messages, so nothing else ever writes them down - and the archive feeds both
    # the restart-rebuilt history and the group card memory reads back, neither of which
    # may hold only one side of a conversation.
    try:
        await ingestor().record_own_reply(
            group_id=int(st.group_id),
            self_id=str(bot.self_id),
            message_id=msg_id,
            text=kept,
            at=now,
            name=persona.name,
            reply_to=reply_to,
        )
    except Exception:
        log.exception("failed to archive the bot's own reply in group %s", st.group_id)
    log.info("group %s: replied (%d chars)", st.group_id, len(text))
    return True
