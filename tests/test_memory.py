"""The memory chain end to end: one extraction call, against a real database.

This is the check the redesign turns on. One call produces four kinds of record - a name,
a fact about a person, two facts about the group, and an episode - and each has to land in
its own table with its evidence attached. The fifth is a fact whose quote appears nowhere
in the transcript, and it has to be refused: that check is the only thing standing between
long-term memory and something that merely sounds plausible.

The model is stubbed, because what is under test is the chain, not the model. What is not
stubbed is the database: the invariants here - one current fact per predicate, a group
that is an entity, an episode reachable by participant - are enforced by SQL.
"""
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
from _db import configure_test_database

configure_test_database()

import asyncio

from qqbot.core import retrieval
from qqbot.db import close_pool, init_pool, pool
from qqbot.gateway.ingest import ingestor
from qqbot.repositories.event import EventRepository
from qqbot.gateway.onebot import GroupMessage, Sender
from qqbot.providers import (AsrModel, Providers, SearchEngine, TextModel,
                             VisionModel, set_providers)
from qqbot.providers.base import Rate
from qqbot.settings import config
from qqbot.util import now_local
from qqbot.repositories import (
    IdentityRepository as ids_probe, MemoryRepository as mem_probe,
)
from qqbot.workers.memory import MemoryWorker

#: The extraction chunk width, from config - the tests build batches around it.
WINDOW = config().default.memory.extract_window
from _db import reset
from _stubs import FakeEmbedding, LegacyTextSession, function_call, response

#: One stub for every bundle in this suite.
_EMBED = FakeEmbedding()

fails = []
G = 9001
CALLS = []
#: cfg.model of every FakeText call - what pins the extraction model override.
SEEN_MODELS = []
#: (model, grade, timeout) of every extraction call, from the cfg it was handed.
SEEN_EXTRACT_CFG = []
#: What the model was actually handed, so the test can check what it was told rather than
#: only what came back.
LAST_PROMPT = [""]


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


def tool(name, **args):
    return function_call(name, args, call_id=f"c{len(CALLS)}")


class FakeText(TextModel):
    MODEL = "fake"

    def rate_for(self, model):
        return Rate("Mtoken", in_hit=0.02, in_miss=1.0, out=2.0)

    def open_session(self, request):
        return LegacyTextSession(self, request)

    async def respond(self, input, *, cfg, tools=None,
                      max_tokens=None, effort=None, kind="reply", group_id=None):
        CALLS.append(kind)
        SEEN_MODELS.append(cfg.model)
        LAST_PROMPT.append(input[-1]["content"])
        if kind == "extract":
            SEEN_EXTRACT_CFG.append(
                (cfg.model, cfg.reasoning_effort, cfg.timeout_sec))
        # Nothing overrides the grade at the call: a use of the text model carries
        # its settings in its own config, so a caller that needs different ones
        # passes a different config.
        assert effort is None, "the grade belongs to the config, not the call"
        return response(model=self.MODEL, tool_calls=[
            tool("record_alias", alias="老周", account=1, kind="nickname",
                 quote="老周你那个切片做完没"),
            tool("record_fact", account=1, predicate="plays", object="鸣潮",
                 quote="我最近在玩鸣潮"),
            tool("record_group_term", term="切片", meaning="把采样切成小段再重排",
                 quote="切片就是把采样切成小段再重排"),
            tool("record_group_topic", topic="做音乐的群",
                 quote="这个群是做音乐的"),
            tool("record_episode", summary="老周答应周末把切片做完",
                 participants=[1, 2], quote="老周你那个切片做完没"),
            # Rejected on purpose: the quote is not in the transcript.
            tool("record_fact", account=1, predicate="lives_in", object="火星",
                 quote="我住在火星"),
        ])

    async def aclose(self):
        pass


class Unused(VisionModel, AsrModel, SearchEngine):
    name = "unused"

    def rate_for(self, model):
        return Rate("call")

    async def describe(self, data, *, cfg, prompt="", mime="image/jpeg", group_id=None):
        raise AssertionError("not reached")

    async def transcribe(self, data, *, cfg, fmt="wav", seconds=None, group_id=None):
        raise AssertionError("not reached")

    async def search(self, query, *, cfg, group_id=None):
        raise AssertionError("not reached")

    async def aclose(self):
        pass


async def settle(mid):
    """Age one archived row past the extraction watermark's settling margin.

    The unread predicate leaves out the newest few seconds of ingest (see
    EventRepository.UNREAD_MESSAGE), and a test that writes and reads within the
    same second would otherwise see nothing to extract. Backdating created_at is
    what the passage of time would do.
    """
    await pool().execute(
        """UPDATE raw_event SET created_at = created_at - INTERVAL '10 seconds'
            WHERE platform='qq' AND platform_event_id=$1""", mid)


async def say(uid, name, text, mid, gid=G):
    await ingestor().ingest(
        GroupMessage(message_id=mid, group_id=gid, sender=Sender(user_id=uid, card=name),
                     segments=[], self_id="999", occurred_at=now_local(),
                     plain_text=text))
    await settle(mid)


async def main():
    set_providers(Providers(text=FakeText(), vision=Unused(), asr=Unused(),
                            embedding=_EMBED, search=Unused()))
    await init_pool()
    await reset()

    await say("u1", "董自豪", "我最近在玩鸣潮", "e1")
    await say("u2", "小北", "老周你那个切片做完没", "e2")
    await say("u1", "董自豪", "切片就是把采样切成小段再重排", "e3")
    await say("u2", "小北", "这个群是做音乐的", "e4")

    w = MemoryWorker(config().default, worker_id="e2e")
    # The drain floor would skip these four messages (a handful is not worth a
    # pass); the tests force past it the way /relearn does.
    check("under the drain floor nothing is paid for",
          await w.extract(G) == 0 and CALLS.count("extract") == 0)
    n = await w.extract(G, force=True)
    # Everything the batch is worth comes out of one call. Four kinds of record used to
    # mean three separate passes over the same transcript, two of which wrote prose.
    check("one call produces every kind of record", n == 6 and CALLS.count("extract") == 1,
          f"{n} candidates in {CALLS.count('extract')} call(s)")
    written, rejected = await w.consolidate(G)
    check("what survives validation is written", written == 5, str(written))
    check("and a quote nobody said is refused", rejected == 1, str(rejected))
    reason = await pool().fetchval(
        "SELECT reject_reason FROM memory_candidate WHERE group_id=$1 AND status='rejected'",
        G)
    check("with the reason kept, so a recurring mistake is countable",
          reason == "malformed", str(reason))

    # -- validation reads the batch the model read --------------------------
    # Extraction and validation are separate jobs, so time passes between them. When
    # validation re-fetched "the recent messages" it got a window that had moved on, and
    # records quoting the messages that had fallen off the back were rejected as though
    # the model had invented them - which is what production showed. Here the group keeps
    # talking hard enough to push the whole batch out, and the same records still stand.
    before = await pool().fetchval(
        "SELECT count(*) FROM memory_fact WHERE group_id=$1 AND status='active'", G)

    # A pass costs a model call, so nothing new means nothing spent. This is the last
    # gate before the money: everything upstream of it sits in front of a queue, where a
    # retry, a hand-triggered pass or two racing jobs can all arrive with the batch
    # already read.
    calls_before = CALLS.count("extract")
    check("a second pass with nothing new does not call the model",
          await w.extract(G, force=True) == 0
          and CALLS.count("extract") == calls_before,
          f"{CALLS.count('extract') - calls_before} call(s)")

    # The conversation partly repeats, and a forced pass runs again - the gate is
    # "nothing new", not "already ran once". Re-said because replay is now exact:
    # the batch is these three rows and only these, so a stub quote must actually
    # be in them to survive. The plays line is deliberately NOT re-said - the
    # confidence checks below need its evidence to stay one message deep. And
    # u1 deliberately speaks first: account codes are positions within one batch,
    # the stub hard-codes account=1, and a batch opening with u2 would file the
    # nickname against the wrong person (which is exactly what codes-are-local
    # protects against in real batches, where the model reads the roster).
    await say("u1", "董自豪", "切片就是把采样切成小段再重排", "e7")
    await say("u2", "小北", "老周你那个切片做完没", "e6")
    await say("u2", "小北", "这个群是做音乐的", "e8")
    n2 = await w.extract(G, force=True)
    check("but new messages are enough to justify one",
          n2 > 0 and CALLS.count("extract") == calls_before + 1, str(n2))
    for i in range(WINDOW + 5):
        await say("u2", "小北", f"把窗口顶出去的第 {i} 句", f"push{i}")
    written2, rejected2 = await w.consolidate(G)
    # Two rejections now: the invented Mars quote, and the plays quote - said in
    # the first batch but not in this one, and exact replay means "in the batch"
    # is exactly what it says.
    check("a batch is validated against itself, however much arrived since",
          rejected2 == 2 and written2 == n2 - 2,
          f"{written2} written, {rejected2} rejected of {n2}")
    after = await pool().fetchval(
        "SELECT count(*) FROM memory_fact WHERE group_id=$1 AND status='active'", G)
    # Same records, re-proposed: confirmed in place rather than rewritten, so the
    # evidence accumulates and the record ages on how often the group said it
    # rather than on when it was last rewritten.
    check("re-proposing what is already known adds no rows", after == before,
          f"{before} -> {after}")
    confirmed = await pool().fetchval(
        """SELECT count(*) FROM memory_fact f
            WHERE f.group_id=$1 AND f.status='active'
              AND (SELECT count(*) FROM memory_fact_evidence e
                    WHERE e.fact_id=f.id AND e.relation='supports') > 1""", G)
    check("it accumulates evidence instead", confirmed > 0, f"{confirmed} facts")
    # Confidence is earned from evidence, not frozen at the write-time guess - but earned
    # from *distinct* supporting events. Both consolidations quoted the same message, so
    # re-reading it must buy nothing: the same premise counted once.
    from qqbot.domain.memory import Fact, FactEvidence, MemoryType, earned_confidence
    conf_now = await pool().fetchval(
        "SELECT confidence FROM memory_fact WHERE group_id=$1"
        " AND predicate='plays' AND object_key='鸣潮'"
        " AND status='active'", G)
    check("re-reading the same message does not raise confidence",
          conf_now is not None and abs(conf_now - earned_confidence(1)) < 1e-3,
          f"{conf_now} vs {earned_confidence(1):.2f}")
    # A confirmation from a different message is genuinely new evidence, and the number
    # moves: a confidence that only ever kept the larger of two constants would
    # freeze every fact at its initial guess.
    person = (await ids_probe().account_of("qq", "u1")).entity_id
    other_event = await pool().fetchval(
        "SELECT id FROM raw_event WHERE platform_event_id='e3'")
    await mem_probe().supersede(
        Fact(subject_entity_id=person, predicate="plays", object_key="鸣潮",
             object_value="鸣潮",
             group_id=G, memory_type=MemoryType.PREFERENCE,
             confidence=earned_confidence(1)),
        [FactEvidence(other_event)], when=now_local())
    conf_after = await pool().fetchval(
        "SELECT confidence FROM memory_fact WHERE group_id=$1"
        " AND predicate='plays' AND object_key='鸣潮'"
        " AND status='active'", G)
    check("a confirmation from a different message raises it",
          abs(conf_after - earned_confidence(2)) < 1e-3,
          f"{conf_after} vs expected {earned_confidence(2):.2f}")

    # And the model is told what is on record, so it can stop re-deriving it. Without
    # this it re-read the same conversation every batch and worded the answer differently
    # each time, which storage could only read as a new fact overturning the old one.
    check("the extractor is shown what is already recorded",
          "已经记过的" in LAST_PROMPT[-1] and "topic" in LAST_PROMPT[-1],
          LAST_PROMPT[-1][:120])

    # -- where each kind landed ---------------------------------------------
    facts = {
        (r["entity_type"], r["predicate"], r["object_key"]): r["object_value"]
        for r in await pool().fetch(
            """SELECT e.entity_type, f.predicate, f.object_key, f.object_value
                 FROM memory_fact f JOIN entity e ON e.id = f.subject_entity_id
                WHERE f.group_id=$1 AND f.status='active'""", G)
    }
    # Multi-valued predicates carry their object in the key, so a second game somebody
    # plays is a second row rather than one overwriting the other.
    check("a fact about a person is filed under that person",
          facts.get(("person", "plays", "鸣潮")) == "鸣潮", str(facts))
    # The group is an entity, which is what lets what the bot knows about a group use the
    # same evidence, supersession and ageing as everything else.
    check("a fact about the group is filed under the group",
          facts.get(("group", "topic", None)) == "做音乐的群"
          and facts.get(("group", "term", "切片")) == "把采样切成小段再重排", str(facts))

    # The model's own guess at a name is worth one weak piece of evidence, which does not
    # reach confirmed. Confirming takes an @ or an owner typing it.
    alias = await pool().fetchrow(
        "SELECT status, confidence FROM alias WHERE group_id=$1 AND alias_text='老周'", G)
    check("a name the model guessed stays a candidate",
          alias and alias["status"] == "candidate" and alias["confidence"] < 0.75,
          str(dict(alias) if alias else None))

    # Each record cites the message its quote came from, not the batch. Records
    # that all cited one batch event would make the evidence chain decorative -
    # and make the one question it exists to answer unanswerable: how many *different*
    # people used a name. That count is the only route by which a name the model merely
    # observed can be confirmed, so with it stuck at one, nothing the model noticed could
    # ever reach the prompt.
    cited = await pool().fetchval(
        """SELECT r.platform_event_id
             FROM alias a
             JOIN alias_evidence ae ON ae.alias_id = a.id
             JOIN raw_event r ON r.id = ae.raw_event_id
            WHERE a.group_id=$1 AND a.alias_text='老周'""", G)
    check("a name cites the message that used it", cited == "e2", str(cited))
    cited_fact = await pool().fetchval(
        """SELECT r.platform_event_id
             FROM memory_fact f
             JOIN memory_fact_evidence e ON e.fact_id = f.id
             JOIN raw_event r ON r.id = e.raw_event_id
            WHERE f.group_id=$1 AND f.predicate='plays'""", G)
    check("and a fact cites the message that stated it", cited_fact == "e1",
          str(cited_fact))

    # What the bot said is readable but never evidence. It renders into the
    # transcript with the self marker so the extractor reads both halves of
    # every conversation it took part in - and it stays out of the roster (nothing
    # gives the bot an entity, so it can never take a code) and out of
    # evidence (own=True lines are skipped by source_of, so a candidate
    # quoting one dies in validation).
    await ingestor().record_own_reply(
        group_id=G, self_id="999", message_id="b1", text="我也在玩鸣潮",
        at=now_local(), name="小X")
    await settle("b1")
    _codes, _roster, lines = await w._render(G, await pool().fetch(
        """SELECT id, platform_user_id, occurred_at, payload, plain_text FROM raw_event
            WHERE group_id=$1 AND event_type='message' ORDER BY occurred_at""", G))
    _own0 = [ln for ln in lines if ln.own]
    check("the bot's own line is in the transcript, marked as its own",
          len(_own0) == 1 and "小X⟦你⟧: 我也在玩鸣潮" in _own0[0].text,
          str([ln.text for ln in lines]))
    from qqbot.services import ExtractionInput as _EI0
    _probe = _EI0(group_id=G, transcript="", roster=_roster, account_codes=_codes,
                  lines=tuple(lines), batch_size=len(lines))
    check("but never on the roster and never a source",
          "小X" not in _roster and _probe.source_of("我也在玩鸣潮") is None,
          _roster)

    ep = await pool().fetchrow("SELECT id, summary FROM episode WHERE group_id=$1", G)
    people = await pool().fetchval(
        "SELECT count(*) FROM episode_participant WHERE episode_id=$1", ep["id"])
    events = await pool().fetchval(
        "SELECT count(*) FROM episode_event WHERE episode_id=$1", ep["id"])
    check("an episode keeps who was in it and what it came from",
          people == 2 and events == 1, f"{people} people, {events} events")

    # The vectors the worker computes have to be the ones retrieval reads. They were not:
    # the reply path's recall must read the same store the worker writes, or the
    # worker pays for embeddings nothing ever queries.
    await w.embed(G)
    stored = await pool().fetchval(
        "SELECT count(*) FROM embedding_index WHERE group_id=$1 AND object_type='episode'",
        G)
    live = await pool().fetchval(
        "SELECT count(*) FROM episode WHERE group_id=$1 AND status='active'", G)
    # Every active episode, not a fixed number: an episode with no vector is one that can
    # only ever be reached by participant, and it would look exactly like one nobody asked
    # about.
    check("every episode gets a vector", stored == live and live > 0, f"{stored}/{live}")
    check("and it was this backend that produced it",
          FakeEmbedding.EMBED_CALLS >= 1, str(FakeEmbedding.EMBED_CALLS))

    # -- the model asking its own memory -------------------------------------
    # The pull path: cross-person questions ("who promised what") are precisely about
    # people the current turn does not contain, so this search is deliberately unfiltered
    # by participant - and doubly group-scoped instead.
    from qqbot.core import tools as tools_mod

    def _call(name, **args):
        return function_call(name, args, call_id="tool-call")

    got = await tools_mod.execute(_call("recall_events", question="切片的约定"),
                                  cfg=config().default, group_id=str(G))
    check("recalled events answer a question", "老周答应周末把切片做完" in got, got)
    check("dated, so the model can say when", "⟦" in got and "⟧" in got, got)
    check("another group's memory is out of reach",
          "没有相关的事" in await tools_mod.execute(
              _call("recall_events", question="切片的约定"),
              cfg=config().default, group_id="424242"))
    check("an empty question is refused",
          "问题为空" in await tools_mod.execute(
              _call("recall_events", question="  "), cfg=config().default,
              group_id=str(G)))

    # A recalled episode arrives framed by its neighbours in group time: the
    # stretches before and after are the story's cause and consequence. The
    # neighbours have no vectors on purpose - they must arrive by adjacency,
    # not by similarity.
    from datetime import timedelta as _td

    from qqbot.domain.memory.episode import Episode as _Ep
    from qqbot.repositories import EpisodeRepository as _EpRepo

    _anchor = await pool().fetchrow(
        "SELECT started_at FROM episode WHERE group_id=$1", G)
    await _EpRepo().add(_Ep(group_id=G, summary="大家商量下个月团建去哪",
                            started_at=_anchor["started_at"] - _td(days=1),
                            ended_at=_anchor["started_at"] - _td(days=1)))
    await _EpRepo().add(_Ep(group_id=G, summary="切片如期交上来了",
                            started_at=_anchor["started_at"] + _td(days=1),
                            ended_at=_anchor["started_at"] + _td(days=1)))
    framed = await tools_mod.execute(_call("recall_events", question="切片的约定"),
                                     cfg=config().default, group_id=str(G))
    check("a recalled event brings its neighbours in time",
          "团建" in framed and "如期交上来" in framed, framed)
    check("and the frame reads chronologically",
          framed.index("团建") < framed.index("老周答应")
          < framed.index("如期交上来"), framed)

    # search_history narrows by speaker: keywords alone match everyone who mentioned
    # the word, and "what did X say about Y" needs the person, not the topic.
    # Bare-hit mode for the pin: with context on, the other speaker's line would
    # legitimately come back as the hit's surroundings.
    _rcfg = config().default.retrieval
    _ctx_saved, _rcfg.history_context = _rcfg.history_context, 0
    by_person = await tools_mod.execute(
        _call("search_history", query="切片", speaker_name="小北"),
        cfg=config().default, group_id=str(G))
    _rcfg.history_context = _ctx_saved
    check("a speaker filter keeps only that person's lines",
          "老周你那个切片做完没" in by_person and "采样切成小段" not in by_person,
          by_person)
    fresh = await tools_mod.execute(
        _call("search_history", query="切片", days=7),
        cfg=config().default, group_id=str(G))
    check("a days filter still finds what was just said", "切片" in fresh, fresh)

    # -- and how the reply path reads it back --------------------------------
    check("the group's own facts render for the prompt",
          await retrieval.group_knowledge(str(G))
          == ["做音乐的群", "切片：把采样切成小段再重排"],
          str(await retrieval.group_knowledge(str(G))))
    # Episodes reach a reply only through the recall_events tool (covered above):
    # nothing episodic is pushed per turn, so there is no per-message lookup here.
    roster = await retrieval.gather(group_id=str(G))
    check("the roster carries the person's fact, not the group's - and lists everyone",
          [(r["nickname"], r["persona_card"]) for r in roster]
          == [("董自豪", "在玩鸣潮"), ("小北", "")],
          str(roster))
    # A confirmed name rides the extraction roster as a comprehension key: the
    # extractor can resolve in-chat nicknames it would otherwise have to guess
    # at. (Registered after the reply-roster pins above - a new alias row is
    # roster content there too.)
    from qqbot.core.retrieval import directory as _dir
    await _dir().name(G, "u2", "小豪豪")
    _codes2, _roster2, _ = await w._render(G, await pool().fetch(
        """SELECT id, platform_user_id, occurred_at, payload, plain_text FROM raw_event
            WHERE group_id=$1 AND event_type='message' ORDER BY occurred_at""", G))
    check("the extraction roster lists known names beside the current card",
          "（也叫：" in _roster2 and "小豪豪" in _roster2, _roster2)
    # An owner's note reaches extraction read-only, under its own label - a
    # comprehension key, never a source of candidates (the prompt forbids it,
    # and no quote from a note can validate: it is not a transcript line).
    await _dir().note(G, "u1", "只在周末上线")
    _known2 = await w._known(G, _codes2)
    check("the owner's note rides the known block under its own label",
          "备注：只在周末上线" in _known2, _known2)
    # The persona's hand-written group background reaches extraction too - the
    # same fixed material the reply path reads, because understanding is
    # upstream of extraction.
    check("the owner's group background heads the known block",
          "本群固定资料：" in _known2 and "测试群。" in _known2, _known2)

    # The bot's own lines enter the transcript marked and codeless, and stay
    # out of evidence by construction: comprehension without the self-loop.
    from qqbot.gateway.ingest import ingestor as _ing2
    from qqbot.services import ExtractionInput as _EI

    await _ing2().record_own_reply(group_id=G, self_id="999", message_id="own-1",
                                   text="切片记得用新采样", at=now_local(),
                                   name="小X")
    await settle("own-1")
    _rows3 = await pool().fetch(
        """SELECT id, platform_user_id, occurred_at, payload, plain_text FROM raw_event
            WHERE group_id=$1 AND event_type='message' ORDER BY occurred_at""", G)
    _codes3, _roster3, _lines3 = await w._render(G, _rows3)
    _own = [ln for ln in _lines3 if ln.own]
    check("the bot's own line renders marked, codeless and off the roster",
          any("小X⟦你⟧: 切片记得用新采样" in ln.text for ln in _own)
          and "小X" not in _roster3, str(_own))
    _inp3 = _EI(group_id=G, transcript="\n".join(ln.text for ln in _lines3),
                roster=_roster3, account_codes=_codes3, lines=tuple(_lines3),
                batch_size=len(_lines3))
    check("a quote from the bot's own line validates nowhere",
          _inp3.source_of("切片记得用新采样") is None)
    check("while a member's line still sources",
          _inp3.source_of("我最近在玩鸣潮") is not None)
    # Left in place on purpose: the decay pin below reads u2, and u1 is
    # nobody else's subject.

    # -- what a reply costs to look up ---------------------------------------
    # The roster is deliberately identical between turns - that is why it is ordered by
    # account rather than by recency - and it was still being rebuilt from a query per
    # person on every message, to produce the same bytes. The stamp it is cached on is
    # derived from the data rather than invalidated by hand, so a new write path cannot
    # forget to clear a cache it does not know about.
    calls = {"n": 0}
    _orig = type(pool()).fetch, type(pool()).fetchrow, type(pool()).fetchval

    def _counting(orig):
        async def f(self, *a, **k):
            calls["n"] += 1
            return await orig(self, *a, **k)
        return f

    for _name, _o in zip(("fetch", "fetchrow", "fetchval"), _orig, strict=True):
        setattr(type(pool()), _name, _counting(_o))
    try:
        await retrieval.gather(group_id=str(G))
        calls["n"] = 0
        await retrieval.gather(group_id=str(G))
        warm = calls["n"]
        from qqbot.domain.memory import Fact, MemoryType
        eid = (await ids_probe().account_of("qq", "u1")).entity_id
        await mem_probe().supersede(
            Fact(subject_entity_id=eid, predicate="from_place", object_value="东北",
                 group_id=G, memory_type=MemoryType.ATTRIBUTE, confidence=0.5),
            [], when=now_local())
        calls["n"] = 0
        await retrieval.gather(group_id=str(G))
        after = calls["n"]
    finally:
        for _name, _o in zip(("fetch", "fetchrow", "fetchval"), _orig, strict=True):
            setattr(type(pool()), _name, _o)
    check("an unchanged roster is not rebuilt", warm <= 3, f"{warm} queries")
    check("but a new fact rebuilds it", after > warm, f"{after} queries")

    # -- a name the model only observed --------------------------------------
    # The one route by which a guess becomes certain, and it turns on how many *different*
    # people were seen using the name. One person saying it a hundred times is that
    # person's habit; three people saying it is what a name is.
    #
    # Which means the evidence has to be attributed to the message it came from:
    # records all citing the batch's last event would pin this count at 1, and
    # nothing the model noticed would ever reach the prompt.
    from qqbot.domain.identity import Alias, AliasEvidence, AliasType, EvidenceType
    from qqbot.repositories import IdentityRepository as _IR

    ids = _IR()
    target = (await ids.account_of("qq", "u1")).entity_id
    seen = {}
    for i, speaker in enumerate(("u2", "u3", "u4"), start=1):
        await say(speaker, f"路人{i}", "阿豪你来一下", f"n{i}")
        event = await pool().fetchval(
            "SELECT id FROM raw_event WHERE platform_event_id=$1", f"n{i}")
        seen[speaker] = await ids.upsert_alias(
            Alias(alias_text="阿豪", target_entity_id=target, group_id=G,
                  alias_type=AliasType.NICKNAME),
            [AliasEvidence(EvidenceType.LLM_INFERENCE, event)],
        )
    check("one person using a name leaves it a guess",
          seen["u2"].status == "candidate", str(seen["u2"].confidence))
    check("two is still not enough",
          seen["u3"].status == "candidate", str(seen["u3"].confidence))
    check("three different people make it a name the group uses",
          seen["u4"].status == "confirmed", str(seen["u4"].confidence))
    roster_now = {r["user_id"]: r for r in await retrieval.gather(group_id=str(G))}
    check("and it reaches the prompt as a name people call him, not a former name",
          roster_now["u1"]["aliases"] == ["阿豪"]
          and "阿豪" not in roster_now["u1"]["former_names"],
          str(roster_now["u1"]))

    # The same person repeating themselves must not get there, however often.
    solo = (await ids.account_of("qq", "u2")).entity_id
    for i in range(5):
        await say("u1", "董自豪", "小北小北小北", f"s{i}")
        event = await pool().fetchval(
            "SELECT id FROM raw_event WHERE platform_event_id=$1", f"s{i}")
        alone = await ids.upsert_alias(
            Alias(alias_text="小北北", target_entity_id=solo, group_id=G,
                  alias_type=AliasType.NICKNAME),
            [AliasEvidence(EvidenceType.LLM_INFERENCE, event)],
        )
    check("but one person repeating themselves never does",
          alone.status == "candidate", str(alone.confidence))

    # A name the model marked as a joke gets a shorter window than one it did not. That
    # is the only thing `kind` decides - before this it was a field the model was asked to
    # fill and nothing ever read.
    for text, kind in (("正经称呼", AliasType.NICKNAME), ("玩笑称呼", AliasType.JOKE_NAME)):
        await ids.upsert_alias(
            Alias(alias_text=text, target_entity_id=target, group_id=G, alias_type=kind),
            [AliasEvidence(EvidenceType.LLM_INFERENCE, None)],
        )
    await pool().execute(
        "UPDATE alias SET last_used_at = NOW() - INTERVAL '10 days' "
        "WHERE group_id=$1 AND alias_text IN ('正经称呼','玩笑称呼')", G)
    await ids.decay_aliases(G, unused_days=30.0, joke_days=7.0)
    left = dict(await pool().fetch(
        "SELECT alias_text, status FROM alias WHERE group_id=$1 "
        "AND alias_text IN ('正经称呼','玩笑称呼')", G))
    check("a joke name expires sooner than an ordinary one",
          left == {"正经称呼": "candidate", "玩笑称呼": "inactive"}, str(left))

    # -- how many of a thing a person can be -------------------------------
    # The storage layer holds one current fact per subject per predicate. For "lives in"
    # that is right and is what makes a move expressible. For "likes" it was a bug: a
    # second thing somebody liked silently overturned the first, and with eighteen
    # predicates that would have been most of them.
    from qqbot.services.memory_extractor import multi_valued
    person = (await ids_probe().account_of("qq", "u1")).entity_id

    async def record(pred, obj):
        await mem_probe().supersede(
            Fact(subject_entity_id=person, predicate=pred,
                 object_key=obj if pred in multi_valued() else None,
                 object_value=obj, group_id=G, memory_type=MemoryType.PREFERENCE,
                 confidence=0.5),
            [], when=now_local())

    async def current():
        return {(f.predicate, f.object_key): f.object_value
                for f in await mem_probe().current_facts(G, [person])}

    await record("likes", "辣的")
    await record("likes", "咖啡")
    live = await current()
    check("a second thing somebody likes does not overturn the first",
          live.get(("likes", "辣的")) == "辣的"
          and live.get(("likes", "咖啡")) == "咖啡", str(live))

    await record("lives_in", "杭州")
    await record("lives_in", "上海")
    live = await current()
    check("but moving does overturn where they lived",
          live.get(("lives_in", None)) == "上海"
          and len([k for k in live if k[0] == "lives_in"]) == 1, str(live))

    # -- forgetting ----------------------------------------------------------
    # Nothing here is old enough to expire, which is the point: decay must not touch what
    # was just learned.
    check("a fresh batch survives a decay pass", await w.decay(G) == (0, 0))
    await pool().execute(
        "UPDATE memory_fact SET last_confirmed_at = NOW() - INTERVAL '400 days' "
        "WHERE group_id=$1", G)
    await pool().execute(
        "UPDATE alias SET last_used_at = NOW() - INTERVAL '400 days' WHERE group_id=$1", G)
    gone_facts, gone_names = await w.decay(G)
    # Eight names, not three: platform cards now start as candidates until they endure a
    # second day, so a card worn once and never seen again is swept with the guesses.
    check("but what nothing has confirmed for a year is let go",
          gone_facts == 7 and gone_names == 8, f"{gone_facts} facts, {gone_names} names")
    # A name that reached confirmed is not a guess any more, so it does not expire with
    # them: three people agreed on it, and their agreeing does not stop being true.
    kept = await pool().fetchval(
        "SELECT status FROM alias WHERE group_id=$1 AND alias_text='阿豪'", G)
    check("a name the group converged on is not forgotten with the guesses",
          kept == "confirmed", str(kept))

    # -- forgetting is class-aware ------------------------------------------
    # The kind of fact, not the repetition count, is the primary axis: what somebody is
    # currently playing goes stale in weeks, where they live holds for months. A uniform
    # clock got both wrong - much-repeated ephemera outlived once-stated stable facts.
    subj = (await ids_probe().account_of("qq", "u2")).entity_id
    for pred, obj in (("plays", "某新游"), ("lives_in", "南京")):
        await mem_probe().supersede(
            Fact(subject_entity_id=subj, predicate=pred, object_value=obj,
                 group_id=G, memory_type=MemoryType.ATTRIBUTE,
                 confidence=0.2),
            [], when=now_local())
    await pool().execute(
        """UPDATE memory_fact SET last_confirmed_at = NOW() - INTERVAL '40 days'
            WHERE group_id=$1 AND subject_entity_id=$2""", G, subj)
    await w.decay(G)
    left = {r["predicate"]: r["status"] for r in await pool().fetch(
        "SELECT predicate, status FROM memory_fact WHERE group_id=$1"
        " AND subject_entity_id=$2", G, subj)}
    # Aged out reads as expired, not superseded: nothing contradicted it.
    check("at forty days a current-state fact is gone and a stable one holds",
          left == {"plays": "expired", "lives_in": "active"}, str(left))

    # /relearn's contract, last because it dirties the watermark: an owner asking for a
    # re-read gets one even though nothing is new. The reset is what makes the gate
    # answer yes; without it the request is silently swallowed by the nothing-unread
    # skip (a regression this pins).
    from qqbot.db import repo as _dbrepo
    await w.extract(G, force=True)   # consume whatever the sections above left unread
    _calls = CALLS.count("extract")
    check("with nothing unread a fresh pass still refuses",
          await w.extract(G, force=True) == 0 and CALLS.count("extract") == _calls)
    await _dbrepo.reset_extract_watermark(int(G), keep=WINDOW)
    await w.extract(G, force=True)
    check("after a watermark reset the same window is read again",
          CALLS.count("extract") >= _calls + 1)

    # -- a failed extraction leaves the batch unread --------------------------
    # The watermark lands only after the extractor returns. Moved before the paid
    # call, a timed-out extraction would meet a retry that finds nothing unread,
    # and the batch - a whole quiet evening, in the idle-flush case - would be
    # skipped forever.
    class FailingText(FakeText):
        async def respond(self, input, **kw):
            raise RuntimeError("model down")

    await say("u1", "董自豪", "水位线在失败后不能动", "wm1")
    await say("u2", "小北", "这两条必须还能被重读", "wm2")
    before, _ = await EventRepository().unread_since_extract(G)
    set_providers(Providers(text=FailingText(), vision=Unused(), asr=Unused(),
                            embedding=_EMBED, search=Unused()))
    try:
        await w.extract(G, force=True)
        crashed = False
    except RuntimeError:
        crashed = True
    unread, _ = await EventRepository().unread_since_extract(G)
    check("a failed extraction propagates to the job layer", crashed)
    check("and leaves the batch unread for the retry",
          before > 0 and unread == before, f"{unread}/{before}")
    set_providers(Providers(text=FakeText(), vision=Unused(), asr=Unused(),
                            embedding=_EMBED, search=Unused()))
    n_retry = await w.extract(G, force=True)
    unread, _ = await EventRepository().unread_since_extract(G)
    check("the retry reads the same batch and the mark then moves",
          n_retry > 0 and unread == 0, f"{n_retry} cands, {unread} unread")

    # -- the drain's edge machinery ------------------------------------------
    # These branches only ever run on a full chunk at the nightly drain, which is
    # exactly when nobody is watching - so they are exercised here or never.
    from datetime import timedelta as _td
    BATCH_GAP = _td(minutes=config().default.memory.batch_gap_min)

    base = now_local()
    flat = [{"occurred_at": base + _td(seconds=i)} for i in range(WINDOW)]
    check("a full chunk with no conversation gap is taken whole",
          w._gap_cut(list(flat)) == flat)
    gapped = list(flat)
    for i in range(100, WINDOW):
        gapped[i] = {"occurred_at": base + BATCH_GAP + _td(seconds=i)}
    check("a full chunk is cut at the last tail-half conversation gap",
          w._gap_cut(list(gapped)) == gapped[:100],
          f"{len(w._gap_cut(list(gapped)))} rows kept")
    fronted = list(flat)
    for i in range(WINDOW // 2, WINDOW):
        fronted[i] = {"occurred_at": base + BATCH_GAP + _td(seconds=i)}
    check("a gap in the front half is not a cut point (it would starve the pass)",
          w._gap_cut(list(fronted)) == fronted)

    # The boundary tie-trim: the watermark is a bare created_at with a strictly-
    # greater filter, so a chunk ending mid-tie would mark the tied leftovers read
    # without ever reading them - silently, forever.
    G2 = 9002
    for i in range(WINDOW):
        await say("t1", "阿花", f"第一天的第 {i} 句", f"tie{i}", gid=G2)
    ids = [r["id"] for r in await pool().fetch(
        "SELECT id FROM raw_event WHERE group_id=$1 ORDER BY created_at, id", G2)]
    await pool().execute(
        """UPDATE raw_event
              SET created_at = (SELECT created_at FROM raw_event WHERE id = $1)
            WHERE id = ANY($2::uuid[])""", ids[-3], ids[-2:])
    rows = await w._next_unread(G2)
    check("a full chunk never ends mid-tie: the tied tail is trimmed whole",
          len(rows) == WINDOW - 3 and rows[-1]["id"] == ids[-4],
          f"{len(rows)} rows")
    await pool().execute(
        """UPDATE raw_event
              SET created_at = (SELECT min(created_at) FROM raw_event
                                 WHERE group_id = $1)
            WHERE group_id = $1""", G2)
    check("a chunk that is one giant tie is taken whole, not trimmed to nothing",
          len(await w._next_unread(G2)) == WINDOW)
    _calls_t = CALLS.count("extract")
    await w.extract(G2, force=True)
    unread_t, _ = await EventRepository().unread_since_extract(G2)
    check("and it drains in a single pass with nothing left behind",
          CALLS.count("extract") == _calls_t + 1 and unread_t == 0,
          f"{CALLS.count('extract') - _calls_t} call(s), {unread_t} unread")

    # A backlog wider than one window: the drain loop keeps going until the
    # watermark catches up, one paid pass per chunk.
    for i in range(WINDOW + 25):
        await say("t2", "阿强", f"第二天的第 {i} 句", f"d2-{i}", gid=G2)
    _calls_d = CALLS.count("extract")
    await w.extract(G2, force=True)
    unread_d, _ = await EventRepository().unread_since_extract(G2)
    check("a backlog wider than one window drains in exactly two passes",
          CALLS.count("extract") == _calls_d + 2 and unread_d == 0,
          f"{CALLS.count('extract') - _calls_d} call(s), {unread_d} unread")

    # Replay: a candidate's stored batch size gets back precisely the rows its
    # extraction read.
    all_rows = await pool().fetch(
        "SELECT id FROM raw_event WHERE group_id=$1 ORDER BY created_at, id", G2)
    anchor = all_rows[-1]["id"]
    exact = await w._replay(G2, ending_at=anchor, size=5)
    check("an exact replay returns precisely the stored batch",
          [r["id"] for r in exact] == [r["id"] for r in all_rows[-5:]],
          f"{len(exact)} rows")

    # Extraction is a use of the text model with its own model, grade and timeout,
    # while the shared wiring (endpoint, key, backend, concurrency) stays the reply
    # path's. The grade and timeout hold for every extraction above; the model
    # override is probed here, last, because it adds an extract call the counting
    # assertions above must not see.
    _txt = config().default.capabilities.text
    check("extraction carries its own grade and timeout, not the reply path's",
          SEEN_EXTRACT_CFG
          and all(g == _txt.extract.reasoning_effort and t == _txt.extract.timeout_sec
                  for _m, g, t in SEEN_EXTRACT_CFG)
          and _txt.extract.timeout_sec != _txt.timeout_sec,
          str(SEEN_EXTRACT_CFG[:2]))
    from qqbot.services import ExtractionInput as _EIovr, MemoryExtractor as _MEovr
    _covr = config().default.model_copy(deep=True)
    _covr.capabilities.text.extract.model = "flash-probe"
    await _MEovr(_covr, legend="x").extract(_EIovr(
        group_id=G, transcript="", roster="", account_codes={}, lines=(), batch_size=0))
    check("the extract model override moves extraction alone",
          SEEN_MODELS[-1] == "flash-probe" and SEEN_MODELS[0] == _txt.model,
          f"first={SEEN_MODELS[0]} last={SEEN_MODELS[-1]}")

    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
