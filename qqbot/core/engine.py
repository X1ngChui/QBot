"""The conversation engine: retrieve -> assemble -> call -> strip -> send (section 2).

The tool loop is bounded; there is no fallback and no downgrade, so any failure means the
bot simply says nothing. A mid-reply limit is not a failure: it ends the spending, and a
tool-less wrap-up round answers from what was already fetched (see RUNAWAY_ROUNDS).
"""

from __future__ import annotations

import logging

import json

from ..db import repo
from ..gateway.ingest import ingestor
from ..providers import Kind, providers
from ..providers.base import QuotaExhausted
from ..settings import Persona, Settings
from ..util import now_local, why
from . import debug, prompt, retrieval, tools
from .budget import BUDGET
from .members import MEMBERS
from .output import clean_reply
from .state import ChatMsg, GroupState

log = logging.getLogger("qqbot.engine")

#: The loop's constraint is money: every round is itself a paid model call, so the scope
#: opened around the reply is what ends the loop - when what remains cannot cover
#: another round, the searching stops and one tool-less wrap-up round answers from what
#: is already fetched. The owner's rule (design goal 6, revised): a mid-reply limit
#: stops the *spending*, not the speech - what the reply has already paid for is worth
#: one bounded closing call, and the daily cap, checked before anything is spent, still
#: means silence. No round count, no per-tool quota: free tools run as often as they
#: like, and what bounds them is that the rounds carrying them are not free.
#:
#: This one number is not a policy but a tripwire. A backend that reported zero cost
#: would make a money-bounded loop unbounded, and that failure should be a loud log and
#: a stop, not an infinite loop.
RUNAWAY_ROUNDS = 20

#: What an unexecuted tool request is answered with once the allowance died mid-round,
#: and what the wrap-up round is told. Mechanical one-line notices, so they live in
#: code like the repeated-call notice, not in the prompt registry.
QUOTA_NOTE = "（检索额度已用完，这个查询没有执行。）"
WRAP_UP_NOTE = ("（本次回复的额度已用完，不能再执行任何检索或查看；"
                "请只依据上文已有的材料直接作答，不要提及额度或系统限制。）")

#: What one round is assumed to cost when deciding whether another is affordable:
#: (cache-hit, cache-miss, output) tokens, priced at the moment of asking. An estimate
#: for the money bound, not a budget - nothing limits what a round actually reads or
#: writes. Deliberately rough: it decides when to stop searching, not what anything
#: costs.
ROUND_TOKENS = (20_000, 2_000, 2_000)

#: Caps on the provenance marker appended to the bot's own archived line: how many
#: tool uses it names, and how much of each query survives. A record, not a transcript
#: - enough for a later turn to see what that answer rested on, not to replay it.
PROV_ITEMS = 4
PROV_QUERY_CHARS = 20

#: Caps on the trajectory entry kept in the conversation window: how much of each
#: tool result survives, and how large the whole entry may grow. A digest for
#: follow-ups on the same topic - the tools are still there when more is needed.
TRACE_RESULT_CHARS = 200
TRACE_TOTAL_CHARS = 900

#: How each query tool reads in the provenance marker and the trace. One dict for
#: both, or a renamed tool would let the marker and the trace quietly diverge.
TOOL_VERB = {"web_search": "搜索", "search_history": "查档", "recall_events": "回忆"}


async def _wrap_up(messages: list, *, cfg: Settings, st: GroupState,
                   executed: list[tuple[str, dict, str]],
                   round_no: int) -> tuple[str | None, str, str]:
    """One last tool-less call after a limit trips mid-reply.

    The material already fetched is sitting in the tail, paid for; discarding it
    bought nothing but silence. So the limit stops the spending, not the speech:
    no tools are offered, the note says why, and the overshoot is exactly one
    bounded round - the same slack the affordability gate's estimate already
    tolerates. A failure here still ends in silence; the wrap-up is a chance,
    not a guarantee.
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
    return res.text or None, _provenance(executed), _trace(executed)


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
        elif name == "inspect_image":
            # Never the line number: numbering is per-render and shifts on every
            # eviction, while this marker is frozen into the archive - a stale #N
            # would point at whatever message sits there next week. The question
            # is the stable half of the call.
            q = str(args.get("question") or "").strip()
            parts.append(f"看图查证“{q[:PROV_QUERY_CHARS]}”")
        elif name in TOOL_VERB:
            q = str(args.get("query") or args.get("question") or "").strip()
            parts.append(f"{TOOL_VERB[name]}“{q[:PROV_QUERY_CHARS]}”")
    if not parts:
        return ""
    shown, extra = parts[:PROV_ITEMS], len(parts) - PROV_ITEMS
    return "[依据:" + "、".join(shown) + ("等" if extra > 0 else "") + "]"


def _label(name: str, args: dict) -> str:
    """One tool use as the trace names it - the provenance vocabulary, reused.

    No line numbers anywhere in here: a number is a position in one render, and
    this text is frozen into reply_trace forever - the question asked of a picture
    is the reference that stays true."""
    if name == "read_url":
        return f"读网页 {str(args.get('url') or '')[:60]}"
    if name == "inspect_image":
        return f"看图查证“{str(args.get('question') or '').strip()}”"
    q = str(args.get("query") or args.get("question") or "").strip()
    return f"{TOOL_VERB.get(name, name)}“{q}”"


def _trace(executed: list[tuple[str, dict, str]]) -> str:
    """The trajectory entry kept in the conversation window beside the reply it fed.

    The reply is the model's synthesis for the question that was asked; a follow-up
    on the same topic often needs a different slice of the same results, and without
    this it either re-searches what was just searched or leans on its own summary.
    Digested per call and capped as a whole - a record for reuse, not a replay.
    Persisted in reply_trace and nowhere else: tool output is not something said
    in the group, so it never enters the archive and search_history must not start
    returning the bot's own search results. Prompt assembly queries the table for
    the window's replies and seats each entry before the reply it fed - the deque
    itself holds only conversation, and eviction needs no bookkeeping.
    """
    if not executed:
        return ""
    lines = ["[检索记录]"]
    used = 0
    for name, args, out in executed:
        digest = " ".join((out or "").split())[:TRACE_RESULT_CHARS]
        line = f"{_label(name, args)}：{digest}"
        if used + len(line) > TRACE_TOTAL_CHARS:
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
    # Episodic memory is deliberately not pushed here: what is injected uninvited
    # sits right next to the incoming message, and an elliptical question resolves
    # against it instead of the conversation (a real misfire, not a hypothetical).
    # The model pulls with recall_events when it actually wants the past.
    # The window and numbering are computed exactly once and handed both to
    # the tool context and to assemble: the seq->message map inspect_image resolves
    # against and the numbers the model reads must come from the same pass. The
    # caller normally passes the window in - the slice was cut when the message
    # arrived, so concurrent tasks and later arrivals cannot shift what this
    # reply is looking at. The stored trajectories for the window's own replies
    # are fetched by id - the table is the single source of truth, the deque
    # holds only conversation - and render_history seats each one right before
    # the reply it fed. Eviction needs no bookkeeping: a reply that slides out
    # of the window simply stops being asked about.
    if window is None:
        window = prompt.history_window(st, batch, cfg)
    nums, marks = prompt.numbered(window + list(batch))
    ctx = tools.ToolCtx(bot=bot,
                        by_seq={nums[m.msg_id]: m for m in window + list(batch)})
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
        window=window, nums=nums, marks=marks,
    )

    # Web for what the model cannot know; the archive for what the group said outside
    # the window it can see; recalled events for what was said in other words than the
    # archive holds; a page read for a link somebody posted; a second look at a
    # picture for details past its one-line description. The middle two are the pull
    # half of context - the prompt pushes a fixed window, and they are the only way
    # to reach anything behind it.
    tool_defs = tools.tool_defs()
    model = cfg.llm.text.model
    seen_calls: set[tuple[str, str]] = set()
    executed: list[tuple[str, dict, str]] = []

    # The scope is what ends the loop; when it does, the wrap-up round speaks from
    # what was already paid for (the module comment above holds the full statement).
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
                return res.text or None, _provenance(executed), _trace(executed)

            # The affordability gate sits between the request for tools and their
            # execution: the first round always runs (a plain answer must never be
            # silenced by a rough estimate exceeding a tight cap), but tools whose
            # results no affordable round could ever read are not worth running -
            # they would burn search allowance and latency without being usable.
            # Priced at the moment of asking, because the price moves with the clock.
            # The requested calls are dropped unexecuted; the wrap-up answers from
            # what earlier rounds already fetched.
            round_cost = providers().text.rate_for(model).tokens(*ROUND_TOKENS)
            if not spend.can_afford(round_cost):
                log.info("group %s: per-reply budget exhausted after %d round(s), "
                         "wrapping up without tools (%.4f of %.4f used)",
                         st.group_id, round_no + 1, spend.spent, spend.cap)
                return await _wrap_up(messages, cfg=cfg, st=st,
                                      executed=executed, round_no=round_no + 1)

            # Tool results stay at the very tail, after the cache boundary (section 6.2).
            messages.append(
                {"role": "assistant", "content": res.text or None,
                 "tool_calls": res.tool_calls}
            )
            quota_hit = False
            for call in res.tool_calls:
                fn = call.get("function", {}) or {}
                key = (fn.get("name") or "", (fn.get("arguments") or "").strip())
                if quota_hit:
                    # The allowance died mid-round; the remaining requests are
                    # answered with the placeholder so every tool_call id gets
                    # its reply and the wrap-up call is protocol-clean.
                    out = QUOTA_NOTE
                elif key in seen_calls:
                    # Nothing here is paginated, so the same call again returns the same
                    # bytes. Answered in words rather than re-executed: a model repeating
                    # itself is a model stuck, and handing it identical results once more
                    # is how a bounded loop spends its whole bound standing still.
                    out = "（这个查询刚执行过，结果就在上面。换个关键词，或用已有资料回答。）"
                else:
                    seen_calls.add(key)
                    try:
                        _args = json.loads(key[1] or "{}")
                    except json.JSONDecodeError:
                        _args = {}
                    try:
                        # The retrieval tools are free per call; inspect_image is the
                        # one paid tool, and its vision call books itself into this
                        # reply's scope like any other spend. Either way the loop is
                        # what money bounds - each round carrying the calls is a paid
                        # call, and the gate above already priced the next one.
                        out = await tools.execute(call, cfg=cfg, group_id=st.group_id,
                                                  ctx=ctx)
                    except QuotaExhausted as e:
                        log.info("group %s: %s - wrapping up on what is already "
                                 "fetched", st.group_id, why(e))
                        quota_hit = True
                        out = QUOTA_NOTE
                    else:
                        executed.append(
                            (key[0], _args if isinstance(_args, dict) else {}, out))
                    # A round carrying several inspect_image calls can overshoot
                    # the scope by their sum before the next gate reads it - a
                    # known, bounded slack (vision runs at flash rates; one
                    # round's worth is well under the cap), accepted over a
                    # per-call gate that would complicate every free tool's path.
                messages.append(
                    {"role": "tool", "tool_call_id": call.get("id", ""), "content": out}
                )
            if quota_hit:
                return await _wrap_up(messages, cfg=cfg, st=st,
                                      executed=executed, round_no=round_no + 1)
            log.info("group %s: tool round %d done (%.4f CNY of %.4f used)",
                     st.group_id, round_no + 1, spend.spent, spend.cap)

    # A tripwire, not a policy: money is the bound, and only a backend billing zero
    # can run this many rounds without exhausting it.
    log.error("group %s: %d tool rounds without running out of money - a backend "
              "is billing zero; giving up", st.group_id, RUNAWAY_ROUNDS)
    return None, "", ""


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
        log.exception("failed to archive our own reply in group %s", st.group_id)
    log.info("group %s: replied (%d chars)", st.group_id, len(text))
    return True
