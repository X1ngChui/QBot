"""The memory chain end to end: one extraction call, against a real database.

This is the check the redesign turns on. One call produces four kinds of record - a name,
a fact about a person, two facts about the group, and an episode - and each has to land in
its own table with its evidence attached. The fifth is a fact whose quote appears nowhere
in the transcript, and it has to be refused: that check is the only thing standing between
long-term memory and something that merely sounds plausible.

The model is stubbed, because what is under test is the chain, not the model. The database
is not stubbed: its constraints enforce exact source events, account-scoped person memory,
and group-scoped episode recall.
"""

import os
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
from _db import configure_test_database

configure_test_database()

import asyncio

from qqbot.core import retrieval
from qqbot.core.media import MediaProcessor
from qqbot.core.retrieval import build_directory
from qqbot.core.tools import ToolCtx
from qqbot.db import close_pool, init_pool, pool
from qqbot.gateway.ingest import Ingestor
from qqbot.repositories.archive import ARCHIVE_COLUMNS, archived_messages
from qqbot.repositories.extraction import ExtractionRepository
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import GroupId
from qqbot.gateway.onebot import GroupMessage, Sender
from qqbot.providers import AsrModel, Providers, SearchEngine, TextModel, VisionModel
from qqbot.providers.base import Rate
from qqbot.services import IdentityResolver
from qqbot.settings import config
from qqbot.util import now_local
from qqbot.repositories import (
    IdentityRepository as ids_probe,
    MemoryRepository as mem_probe,
)
from qqbot.workers.memory import MemoryWorker

#: The extraction chunk width, from config - the tests build batches around it.
WINDOW = config().default.memory.extract_window
from _db import reset
from _stubs import FakeEmbedding, LegacyTextSession, function_call, response

#: One stub for every bundle in this suite.
_EMBED = FakeEmbedding()

fails = []
G = GroupId("9001")
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

    async def respond(
        self, input, *, cfg, tools=None, max_tokens=None, effort=None, kind="reply", group_id=None
    ):
        CALLS.append(kind)
        SEEN_MODELS.append(cfg.model)
        LAST_PROMPT.append(input[-1]["content"])
        if kind == "extract":
            SEEN_EXTRACT_CFG.append((cfg.model, cfg.reasoning_effort, cfg.timeout_sec))
        # Nothing overrides the grade at the call: a use of the text model carries
        # its settings in its own config, so a caller that needs different ones
        # passes a different config.
        assert effort is None, "the grade belongs to the config, not the call"

        transcript = input[-1]["content"]

        def cited(fragment):
            for line in transcript.splitlines():
                if fragment not in line:
                    continue
                match = re.search(r"⟦来源:(\d+)⟧.*?⟧: (.*)$", line)
                if match:
                    return int(match.group(1)), match.group(2)
            return 1, fragment

        alias_source, alias_quote = cited("老周你那个切片做完没")
        fact_source, fact_quote = cited("我最近在玩鸣潮")
        term_source, term_quote = cited("切片就是把采样切成小段再重排")
        topic_source, topic_quote = cited("这个群是做音乐的")
        return response(
            model=self.MODEL,
            tool_calls=[
                tool(
                    "record_alias",
                    source=alias_source,
                    alias="老周",
                    account=1,
                    kind="nickname",
                    quote=alias_quote,
                ),
                tool(
                    "record_fact",
                    source=fact_source,
                    account=1,
                    predicate="plays",
                    object="鸣潮",
                    quote=fact_quote,
                ),
                tool(
                    "record_group_term",
                    source=term_source,
                    term="切片",
                    meaning="把采样切成小段再重排",
                    quote=term_quote,
                ),
                tool(
                    "record_group_topic", source=topic_source, topic="做音乐的群", quote=topic_quote
                ),
                tool(
                    "record_episode",
                    summary="老周答应周末把切片做完",
                    sources=[{"source": alias_source, "quote": alias_quote}],
                ),
                # Rejected on purpose: the quote is not in its declared source.
                tool(
                    "record_fact",
                    source=1,
                    account=1,
                    predicate="lives_in",
                    object="火星",
                    quote="我住在火星",
                ),
            ],
        )

    async def aclose(self):
        pass


class Unused(VisionModel, AsrModel, SearchEngine):
    name = "unused"

    def rate_for(self, model):
        return Rate("call")

    async def describe(self, data, *, prompt="", mime="image/jpeg", group_id=None):
        raise AssertionError("not reached")

    async def transcribe(self, data, *, fmt="wav", seconds=None, group_id=None):
        raise AssertionError("not reached")

    async def search(self, query, *, options, group_id=None):
        raise AssertionError("not reached")

    async def aclose(self):
        pass


_DIRECTORY = build_directory()
_INGESTOR = Ingestor(IdentityResolver(ids_probe()))
_PROVIDERS = None
_MEDIA = None


def set_providers(bundle):
    global _PROVIDERS, _MEDIA
    _PROVIDERS = bundle
    if _MEDIA is None:
        _MEDIA = MediaProcessor(config().default.media, bundle, _DIRECTORY)
    else:
        _MEDIA._providers = bundle


def tool_context():
    return ToolCtx(providers=_PROVIDERS, media=_MEDIA)


async def say(uid, name, text, mid, gid=G):
    await _INGESTOR.ingest(
        GroupMessage(
            message_id=mid,
            group_id=gid,
            sender=Sender(user_id=uid, card=name),
            segments=[{"type": "text", "data": {"text": text}}],
            self_id="999",
            occurred_at=now_local(),
            plain_text=text,
            typed_text=text,
        )
    )


async def say_at(uid, name, target, target_name, text, mid, gid=G):
    segments = [
        {"type": "at", "data": {"qq": target, "name": target_name}},
        {"type": "text", "data": {"text": text}},
    ]
    await _INGESTOR.ingest(
        GroupMessage(
            message_id=mid,
            group_id=gid,
            sender=Sender(user_id=uid, card=name),
            segments=segments,
            self_id="999",
            occurred_at=now_local(),
            plain_text=f"@{target_name} {text}",
            typed_text=text,
        ),
        at_accounts=[target],
    )


async def main():
    set_providers(
        Providers(text=FakeText(), vision=Unused(), asr=Unused(), embedding=_EMBED, search=Unused())
    )
    await init_pool()
    await reset()

    await say("u1", "董自豪", "我最近在玩鸣潮", "e1")
    await say_at("u2", "小北", "u1", "董自豪", "老周你那个切片做完没", "e2")
    await say("u1", "董自豪", "切片就是把采样切成小段再重排", "e3")
    await say("u2", "小北", "这个群是做音乐的", "e4")
    await say_at("u2", "小北", "u3", "李芳", "帮忙看看", "e-at")

    threshold_worker = MemoryWorker(config().default, _PROVIDERS, worker_id="floor")
    check(
        "under the drain floor nothing is paid for",
        await threshold_worker.extract(G) == 0 and CALLS.count("extract") == 0,
    )
    test_cfg = config().default.model_copy(
        update={"memory": config().default.memory.model_copy(update={"drain_floor": 0})}
    )
    w = MemoryWorker(test_cfg, _PROVIDERS, worker_id="e2e")
    n = await w.extract(G)
    # Everything the batch is worth comes out of one call. Four kinds of record used to
    # mean three separate passes over the same transcript, two of which wrote prose.
    check(
        "one call produces every kind of record",
        n == 6 and CALLS.count("extract") == 1,
        f"{n} candidates in {CALLS.count('extract')} call(s)",
    )
    written = await pool().fetchval(
        "SELECT count(*) FROM memory_candidate WHERE group_id=$1 AND status='accepted'",
        G.to_db(),
    )
    rejected = await pool().fetchval(
        "SELECT count(*) FROM memory_candidate WHERE group_id=$1 AND status='rejected'",
        G.to_db(),
    )
    check("what survives validation is written", written == 5, str(written))
    check("and a quote nobody said is refused", rejected == 1, str(rejected))
    reason = await pool().fetchval(
        "SELECT reject_reason FROM memory_candidate WHERE group_id=$1 AND status='rejected'",
        G.to_db(),
    )
    check(
        "with the reason kept, so a recurring mistake is countable",
        reason == "malformed",
        str(reason),
    )

    # -- validation reads the batch the model read --------------------------
    # Extraction and validation are separate jobs, so time passes between them. When
    # validation re-fetched "the recent messages" it got a window that had moved on, and
    # records quoting the messages that had fallen off the back were rejected as though
    # the model had invented them - which is what production showed. Here the group keeps
    # talking hard enough to push the whole batch out, and the same records still stand.
    before = await pool().fetchval(
        "SELECT count(*) FROM memory_fact WHERE group_id=$1 AND status='active'", G.to_db()
    )

    # A pass costs a model call, so nothing new means nothing spent. This is the last
    # gate before the money: everything upstream of it sits in front of a queue, where a
    # retry or two racing jobs can all arrive with the batch already read.
    calls_before = CALLS.count("extract")
    check(
        "a second pass with nothing new does not call the model",
        await w.extract(G) == 0 and CALLS.count("extract") == calls_before,
        f"{CALLS.count('extract') - calls_before} call(s)",
    )

    # The conversation partly repeats, and another pass runs again - the gate is
    # "nothing new", not "already ran once". Re-said because each batch is exact:
    # the batch is these three rows and only these, so a stub quote must actually
    # be in them to survive. The plays line is deliberately NOT re-said - the
    # confidence checks below need its evidence to stay one message deep. And
    # u1 deliberately speaks first: account codes are positions within one batch,
    # the stub hard-codes account=1, and a batch opening with u2 would file the
    # nickname against the wrong person (which is exactly what codes-are-local
    # protects against in real batches, where the model reads the roster).
    await say("u1", "董自豪", "切片就是把采样切成小段再重排", "e7")
    await say_at("u2", "小北", "u1", "董自豪", "老周你那个切片做完没", "e6")
    await say("u2", "小北", "这个群是做音乐的", "e8")
    n2 = await w.extract(G)
    check(
        "but new messages are enough to justify one",
        n2 > 0 and CALLS.count("extract") == calls_before + 1,
        str(n2),
    )
    for i in range(WINDOW + 5):
        await say("u2", "小北", f"把窗口顶出去的第 {i} 句", f"push{i}")
    newest_extraction = await pool().fetchval(
        """SELECT id FROM memory_extraction WHERE group_id=$1
            ORDER BY started_at DESC, id DESC LIMIT 1""",
        G.to_db(),
    )
    written2 = await pool().fetchval(
        """SELECT count(*) FROM memory_candidate
            WHERE extraction_id=$1 AND status='accepted'""",
        newest_extraction,
    )
    rejected2 = await pool().fetchval(
        """SELECT count(*) FROM memory_candidate
            WHERE extraction_id=$1 AND status='rejected'""",
        newest_extraction,
    )
    # Two rejections now: the invented Mars quote, and the plays quote - said in
    # the first batch but not in this one, and exact membership means "in the batch"
    # is exactly what it says.
    check(
        "a batch is validated against itself, however much arrived since",
        rejected2 == 2 and written2 == n2 - 2,
        f"{written2} written, {rejected2} rejected of {n2}",
    )
    after = await pool().fetchval(
        "SELECT count(*) FROM memory_fact WHERE group_id=$1 AND status='active'", G.to_db()
    )
    # Same records, re-proposed: confirmed in place rather than rewritten, so the
    # evidence accumulates and the record ages on how often the group said it
    # rather than on when it was last rewritten.
    check(
        "re-proposing what is already known adds no rows", after == before, f"{before} -> {after}"
    )
    confirmed = await pool().fetchval(
        """SELECT count(*) FROM memory_fact f
            WHERE f.group_id=$1 AND f.status='active'
              AND (SELECT count(*) FROM memory_fact_evidence e
                    WHERE e.fact_id=f.id AND e.relation='supports') > 1""",
        G.to_db(),
    )
    check("it accumulates evidence instead", confirmed > 0, f"{confirmed} facts")
    # Confidence is earned from evidence, not frozen at the write-time guess - but earned
    # from *distinct* supporting events. Both consolidations quoted the same message, so
    # re-reading it must buy nothing: the same premise counted once.
    from qqbot.domain.memory import Fact, FactEvidence, MemoryType, earned_confidence

    conf_now = await pool().fetchval(
        "SELECT confidence FROM memory_fact WHERE group_id=$1"
        " AND predicate='plays' AND object_key='鸣潮'"
        " AND status='active'",
        G.to_db(),
    )
    check(
        "re-reading the same message does not raise confidence",
        conf_now is not None and abs(conf_now - earned_confidence(1)) < 1e-3,
        f"{conf_now} vs {earned_confidence(1):.2f}",
    )
    # A confirmation from a different message is genuinely new evidence, and the number
    # moves: a confidence that only ever kept the larger of two constants would
    # freeze every fact at its initial guess.
    account = await ids_probe().account_of("qq", "u1")
    other_event = await pool().fetchval("SELECT id FROM raw_event WHERE platform_event_id='e3'")
    await mem_probe().supersede(
        Fact(
            subject_entity_id=None,
            subject_account_id=account.id,
            predicate="plays",
            object_key="鸣潮",
            object_value="鸣潮",
            group_id=G,
            memory_type=MemoryType.PREFERENCE,
            confidence=earned_confidence(1),
        ),
        [FactEvidence(other_event)],
        when=now_local(),
    )
    conf_after = await pool().fetchval(
        "SELECT confidence FROM memory_fact WHERE group_id=$1"
        " AND predicate='plays' AND object_key='鸣潮'"
        " AND status='active'",
        G.to_db(),
    )
    check(
        "a confirmation from a different message raises it",
        abs(conf_after - earned_confidence(2)) < 1e-3,
        f"{conf_after} vs expected {earned_confidence(2):.2f}",
    )

    # And the model is told what is on record, so it can stop re-deriving it. Without
    # this it re-read the same conversation every batch and worded the answer differently
    # each time, which storage could only read as a new fact overturning the old one.
    check(
        "the extractor is shown what is already recorded",
        "topic" in LAST_PROMPT[-1] and "plays = 鸣潮" in LAST_PROMPT[-1],
        LAST_PROMPT[-1][:120],
    )

    # -- where each kind landed ---------------------------------------------
    facts = {
        (r["entity_type"], r["predicate"], r["object_key"]): r["object_value"]
        for r in await pool().fetch(
            """SELECT COALESCE(e.entity_type, 'person') AS entity_type,
                      f.predicate, f.object_key, f.object_value
                 FROM memory_fact f
                 LEFT JOIN entity e ON e.id = f.subject_entity_id
                WHERE f.group_id=$1 AND f.status='active'""",
            G.to_db(),
        )
    }
    # Multi-valued predicates carry their object in the key, so a second game somebody
    # plays is a second row rather than one overwriting the other.
    check(
        "a fact about a person is filed under that person",
        facts.get(("person", "plays", "鸣潮")) == "鸣潮",
        str(facts),
    )
    # The group is an entity, which is what lets what the bot knows about a group use the
    # same evidence, supersession and ageing as everything else.
    check(
        "a fact about the group is filed under the group",
        facts.get(("group", "topic", None)) == "做音乐的群"
        and facts.get(("group", "term", "切片")) == "把采样切成小段再重排",
        str(facts),
    )

    # The model's own guess at a name is worth one weak piece of evidence, which does not
    # reach confirmed. Confirming takes an @ or an owner typing it.
    alias = await pool().fetchrow(
        "SELECT status, confidence FROM alias WHERE group_id=$1 AND alias_text='老周'", G.to_db()
    )
    check(
        "a name the model guessed stays a candidate",
        alias and alias["status"] == "candidate" and alias["confidence"] < 0.75,
        str(dict(alias) if alias else None),
    )

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
            WHERE a.group_id=$1 AND a.alias_text='老周'""",
        G.to_db(),
    )
    check("a name cites the message that used it", cited == "e2", str(cited))
    cited_fact = await pool().fetchval(
        """SELECT r.platform_event_id
             FROM memory_fact f
             JOIN memory_fact_evidence e ON e.fact_id = f.id
             JOIN raw_event r ON r.id = e.raw_event_id
            WHERE f.group_id=$1 AND f.predicate='plays'""",
        G.to_db(),
    )
    check("and a fact cites the message that stated it", cited_fact == "e1", str(cited_fact))

    # What the bot said is readable but never evidence. It renders into the
    # transcript with the self marker so the extractor reads both halves of
    # every conversation it took part in - and it stays out of the roster (nothing
    # gives the bot an entity, so it can never take a code) and out of
    # evidence (own=True lines are skipped by source_of, so a candidate
    # quoting one dies in validation).
    await _INGESTOR.ingest(
        GroupMessage(
            message_id="b1",
            group_id=G,
            sender=Sender(user_id="999", nickname="小X"),
            segments=[{"type": "text", "data": {"text": "我也在玩鸣潮"}}],
            self_id="999",
            occurred_at=now_local(),
            plain_text="我也在玩鸣潮",
            outbound_schema=1,
            author_kind=AuthorKind.BOT,
        )
    )
    _codes, _roster, lines = await w._render(
        G,
        archived_messages(
            await pool().fetch(
                f"""SELECT {ARCHIVE_COLUMNS} FROM raw_event
            WHERE group_id=$1 AND event_type='message' ORDER BY occurred_at""",
                G.to_db(),
            )
        ),
    )
    _own0 = [ln for ln in lines if ln.own]
    _mention_number = re.search(r"李芳⟦(\d+)⟧", _roster)
    check(
        "a mentioned non-speaker enters the extraction roster and keeps identity",
        _mention_number is not None
        and any(f"@李芳⟦{_mention_number.group(1)}⟧" in line.text for line in lines),
        f"{_roster} / {[line.text for line in lines]}",
    )
    check(
        "the bot's own line is in the transcript, marked as its own",
        len(_own0) == 1 and "小X⟦0⟧: 我也在玩鸣潮" in _own0[0].text,
        str([ln.text for ln in lines]),
    )
    from qqbot.services import ExtractionInput as _EI0

    _probe = _EI0(
        group_id=G, transcript="", roster=_roster, account_codes=_codes, lines=tuple(lines)
    )
    check(
        "but never on the roster and never a source",
        "小X" not in _roster and _probe.source_of(_own0[0].ordinal, "我也在玩鸣潮") is None,
        _roster,
    )

    ep = await pool().fetchrow(
        "SELECT id, summary, extraction_id FROM episode WHERE group_id=$1",
        G.to_db(),
    )
    events = await pool().fetchval(
        "SELECT count(*) FROM episode_event WHERE episode_id=$1", ep["id"]
    )
    check(
        "an episode keeps exact extraction and event provenance",
        ep["extraction_id"] is not None and events == 1,
        f"extraction={ep['extraction_id']} events={events}",
    )

    # The vectors the worker computes have to be the ones retrieval reads. They were not:
    # the reply path's recall must read the same store the worker writes, or the
    # worker pays for embeddings nothing ever queries.
    await w.embed(G)
    stored = await pool().fetchval(
        "SELECT count(*) FROM embedding_index WHERE group_id=$1 AND object_type='episode'",
        G.to_db(),
    )
    live = await pool().fetchval(
        "SELECT count(*) FROM episode WHERE group_id=$1 AND status='active'", G.to_db()
    )
    # Every active episode receives a vector; no alternate identity index exists.
    check("every episode gets a vector", stored == live and live > 0, f"{stored}/{live}")
    check(
        "and it was this backend that produced it",
        FakeEmbedding.EMBED_CALLS >= 1,
        str(FakeEmbedding.EMBED_CALLS),
    )

    # -- the model asking its own memory -------------------------------------
    # Cross-person questions may concern people absent from the current turn, so episode
    # recall is deliberately group-scoped.
    from qqbot.core import tools as tools_mod

    def _call(name, **args):
        return function_call(name, args, call_id="tool-call")

    got = await tools_mod.execute(
        _call("recall_events", question="切片的约定"),
        cfg=config().default,
        group_id=G,
        ctx=tool_context(),
    )
    check("recalled events answer a question", "老周答应周末把切片做完" in got, got)
    check("dated, so the model can say when", "⟦" in got and "⟧" in got, got)
    check(
        "another group's memory is out of reach",
        "没有相关的事"
        in await tools_mod.execute(
            _call("recall_events", question="切片的约定"),
            cfg=config().default,
            group_id=GroupId("424242"),
            ctx=tool_context(),
        ),
    )
    check(
        "an empty question is refused",
        "问题为空"
        in await tools_mod.execute(
            _call("recall_events", question="  "), cfg=config().default, group_id=G
        ),
    )

    # A recalled episode arrives framed by its neighbours in group time: the
    # stretches before and after are the story's cause and consequence. The
    # neighbours have no vectors on purpose - they must arrive by adjacency,
    # not by similarity.
    from datetime import timedelta as _td

    from qqbot.domain.memory.episode import Episode as _Ep
    from qqbot.repositories import EpisodeRepository as _EpRepo

    _anchor = await pool().fetchrow("SELECT started_at FROM episode WHERE group_id=$1", G.to_db())
    await _EpRepo().add(
        _Ep(
            group_id=G,
            summary="大家商量下个月团建去哪",
            started_at=_anchor["started_at"] - _td(days=1),
            ended_at=_anchor["started_at"] - _td(days=1),
        )
    )
    await _EpRepo().add(
        _Ep(
            group_id=G,
            summary="切片如期交上来了",
            started_at=_anchor["started_at"] + _td(days=1),
            ended_at=_anchor["started_at"] + _td(days=1),
        )
    )
    framed = await tools_mod.execute(
        _call("recall_events", question="切片的约定"),
        cfg=config().default,
        group_id=G,
        ctx=tool_context(),
    )
    check(
        "a recalled event brings its neighbours in time",
        "团建" in framed and "如期交上来" in framed,
        framed,
    )
    check(
        "and the frame reads chronologically",
        framed.index("团建") < framed.index("老周答应") < framed.index("如期交上来"),
        framed,
    )

    # search_history narrows by speaker: keywords alone match everyone who mentioned
    # the word, and "what did X say about Y" needs the person, not the topic.
    # Bare-hit mode for the pin: with context on, the other speaker's line would
    # legitimately come back as the hit's surroundings.
    _base_search_cfg = config().default
    _rcfg = _base_search_cfg.tools.search_history.model_copy(update={"context_lines": 0})
    _bare_search_cfg = _base_search_cfg.model_copy(
        update={
            "tools": _base_search_cfg.tools.model_copy(
                update={
                    "search_history": _rcfg,
                }
            ),
        }
    )
    by_person = await tools_mod.execute(
        _call("search_history", query="切片", speaker_name="小北"), cfg=_bare_search_cfg, group_id=G
    )
    check(
        "a speaker filter keeps only that person's lines",
        "老周你那个切片做完没" in by_person and "采样切成小段" not in by_person,
        by_person,
    )
    fresh = await tools_mod.execute(
        _call("search_history", query="切片", days=7), cfg=config().default, group_id=G
    )
    check("a days filter still finds what was just said", "切片" in fresh, fresh)

    # -- and how the reply path reads it back --------------------------------
    check(
        "the group's own facts render for the prompt",
        await retrieval.group_knowledge(G) == ["做音乐的群", "切片：把采样切成小段再重排"],
        str(await retrieval.group_knowledge(G)),
    )
    # Episodes reach a reply only through the recall_events tool (covered above):
    # nothing episodic is pushed per turn, so there is no per-message lookup here.
    roster = await retrieval.gather(group_id=G, directory=_DIRECTORY)
    check(
        "the roster carries the person's fact, not the group's - and lists everyone",
        [(r["nickname"], r["persona_card"]) for r in roster]
        == [("董自豪", "在玩鸣潮"), ("小北", "")],
        str(roster),
    )
    # A confirmed name rides the extraction roster as a comprehension key: the
    # extractor can resolve in-chat nicknames it would otherwise have to guess
    # at. (Registered after the reply-roster pins above - a new alias row is
    # roster content there too.)
    await _DIRECTORY.name(G, "u2", "小豪豪")
    _codes2, _roster2, _ = await w._render(
        G,
        archived_messages(
            await pool().fetch(
                f"""SELECT {ARCHIVE_COLUMNS} FROM raw_event
            WHERE group_id=$1 AND event_type='message' ORDER BY occurred_at""",
                G.to_db(),
            )
        ),
    )
    check(
        "the extraction roster lists known names beside the current card",
        "（也叫：" in _roster2 and "小豪豪" in _roster2,
        _roster2,
    )
    # An owner's note reaches extraction read-only, under its own label - a
    # comprehension key, never a source of candidates (the prompt forbids it,
    # and no quote from a note can validate: it is not a transcript line).
    await _DIRECTORY.note(G, "u1", "只在周末上线")
    _known2 = await w._known(G, _codes2)
    check(
        "the owner's note rides the known block under its own label",
        "备注：只在周末上线" in _known2,
        _known2,
    )
    # The persona's hand-written group background reaches extraction too - the
    # same fixed material the reply path reads, because understanding is
    # upstream of extraction.
    check(
        "the owner's group background heads the known block",
        "本群固定资料：" in _known2 and "测试群。" in _known2,
        _known2,
    )

    # The bot's own lines enter the transcript marked and codeless, and stay
    # out of evidence by construction: comprehension without the self-loop.
    from qqbot.services import ExtractionInput as _EI

    await _INGESTOR.ingest(
        GroupMessage(
            message_id="own-1",
            group_id=G,
            sender=Sender(user_id="999", nickname="小X"),
            segments=[{"type": "text", "data": {"text": "切片记得用新采样"}}],
            self_id="999",
            occurred_at=now_local(),
            plain_text="切片记得用新采样",
            outbound_schema=1,
            author_kind=AuthorKind.BOT,
        )
    )
    _rows3 = archived_messages(
        await pool().fetch(
            f"""SELECT {ARCHIVE_COLUMNS} FROM raw_event
            WHERE group_id=$1 AND event_type='message' ORDER BY occurred_at""",
            G.to_db(),
        )
    )
    _codes3, _roster3, _lines3 = await w._render(G, _rows3)
    _own = [ln for ln in _lines3 if ln.own]
    check(
        "the bot's own line renders marked, codeless and off the roster",
        any("小X⟦0⟧: 切片记得用新采样" in ln.text for ln in _own) and "小X" not in _roster3,
        str(_own),
    )
    _inp3 = _EI(
        group_id=G,
        transcript="\n".join(ln.text for ln in _lines3),
        roster=_roster3,
        account_codes=_codes3,
        lines=tuple(_lines3),
    )
    member_line = next(line for line in _lines3 if "我最近在玩鸣潮" in line.evidence_text)
    check(
        "a quote from the bot's own line validates nowhere",
        _inp3.source_of(_own[0].ordinal, "切片记得用新采样") is None,
    )
    check(
        "while a member's line still sources",
        _inp3.source_of(member_line.ordinal, "我最近在玩鸣潮") is not None,
    )
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
        await retrieval.gather(group_id=G, directory=_DIRECTORY)
        calls["n"] = 0
        await retrieval.gather(group_id=G, directory=_DIRECTORY)
        warm = calls["n"]
        from qqbot.domain.memory import Fact, MemoryType

        eid = (await ids_probe().account_of("qq", "u1")).entity_id
        await mem_probe().supersede(
            Fact(
                subject_entity_id=eid,
                predicate="from_place",
                object_value="东北",
                group_id=G,
                memory_type=MemoryType.ATTRIBUTE,
                confidence=0.5,
            ),
            [],
            when=now_local(),
        )
        calls["n"] = 0
        await retrieval.gather(group_id=G, directory=_DIRECTORY)
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
            "SELECT id FROM raw_event WHERE platform_event_id=$1", f"n{i}"
        )
        seen[speaker] = await ids.upsert_alias(
            Alias(
                alias_text="阿豪",
                target_entity_id=target,
                group_id=G,
                alias_type=AliasType.NICKNAME,
            ),
            [AliasEvidence(EvidenceType.LLM_INFERENCE, event)],
        )
    check(
        "one person using a name leaves it a guess",
        seen["u2"].status == "candidate",
        str(seen["u2"].confidence),
    )
    check("two is still not enough", seen["u3"].status == "candidate", str(seen["u3"].confidence))
    check(
        "three different people make it a name the group uses",
        seen["u4"].status == "confirmed",
        str(seen["u4"].confidence),
    )
    roster_now = {r["user_id"]: r for r in await retrieval.gather(group_id=G, directory=_DIRECTORY)}
    check(
        "and it reaches the prompt as a name people call him, not a former name",
        roster_now["u1"]["aliases"] == ["阿豪"] and "阿豪" not in roster_now["u1"]["former_names"],
        str(roster_now["u1"]),
    )

    # The same person repeating themselves must not get there, however often.
    solo = (await ids.account_of("qq", "u2")).entity_id
    for i in range(5):
        await say("u1", "董自豪", "小北小北小北", f"s{i}")
        event = await pool().fetchval(
            "SELECT id FROM raw_event WHERE platform_event_id=$1", f"s{i}"
        )
        alone = await ids.upsert_alias(
            Alias(
                alias_text="小北北",
                target_entity_id=solo,
                group_id=G,
                alias_type=AliasType.NICKNAME,
            ),
            [AliasEvidence(EvidenceType.LLM_INFERENCE, event)],
        )
    check(
        "but one person repeating themselves never does",
        alone.status == "candidate",
        str(alone.confidence),
    )

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
        "WHERE group_id=$1 AND alias_text IN ('正经称呼','玩笑称呼')",
        G.to_db(),
    )
    await ids.decay_aliases(G, unused_days=30.0, joke_days=7.0)
    left = dict(
        await pool().fetch(
            "SELECT alias_text, status FROM alias WHERE group_id=$1 "
            "AND alias_text IN ('正经称呼','玩笑称呼')",
            G.to_db(),
        )
    )
    check(
        "a joke name expires sooner than an ordinary one",
        left == {"正经称呼": "candidate", "玩笑称呼": "inactive"},
        str(left),
    )

    # -- how many of a thing a person can be -------------------------------
    # The storage layer holds one current fact per subject per predicate. For "lives in"
    # that is right and is what makes a move expressible. For "likes" it was a bug: a
    # second thing somebody liked silently overturned the first, and with eighteen
    # predicates that would have been most of them.
    from qqbot.services.memory_extractor import multi_valued

    person = (await ids_probe().account_of("qq", "u1")).entity_id

    async def record(pred, obj):
        await mem_probe().supersede(
            Fact(
                subject_entity_id=person,
                predicate=pred,
                object_key=obj if pred in multi_valued() else None,
                object_value=obj,
                group_id=G,
                memory_type=MemoryType.PREFERENCE,
                confidence=0.5,
            ),
            [],
            when=now_local(),
        )

    async def current():
        return {
            (f.predicate, f.object_key): f.object_value
            for f in await mem_probe().current_facts(G, [person])
        }

    await record("likes", "辣的")
    await record("likes", "咖啡")
    live = await current()
    check(
        "a second thing somebody likes does not overturn the first",
        live.get(("likes", "辣的")) == "辣的" and live.get(("likes", "咖啡")) == "咖啡",
        str(live),
    )

    await record("lives_in", "杭州")
    await record("lives_in", "上海")
    live = await current()
    check(
        "but moving does overturn where they lived",
        live.get(("lives_in", None)) == "上海"
        and len([k for k in live if k[0] == "lives_in"]) == 1,
        str(live),
    )

    # -- forgetting ----------------------------------------------------------
    # Nothing here is old enough to expire, which is the point: decay must not touch what
    # was just learned.
    check("a fresh batch survives a decay pass", await w.decay(G) == (0, 0, 0))
    await pool().execute(
        "UPDATE memory_fact SET last_confirmed_at = NOW() - INTERVAL '400 days' WHERE group_id=$1",
        G.to_db(),
    )
    await pool().execute(
        "UPDATE alias SET last_used_at = NOW() - INTERVAL '400 days' WHERE group_id=$1", G.to_db()
    )
    await pool().execute(
        """UPDATE episode
              SET started_at=NOW() - INTERVAL '100 days',
                  ended_at=NOW() - INTERVAL '100 days'
            WHERE id=$1""",
        ep["id"],
    )
    gone_facts, gone_names, gone_episodes = await w.decay(G)
    # Eight names, not three: platform cards now start as candidates until they endure a
    # second day, so a card worn once and never seen again is swept with the guesses.
    check(
        "but what nothing has confirmed for a year is let go",
        gone_facts == 7 and gone_names == 8 and gone_episodes == 1,
        f"{gone_facts} facts, {gone_names} names, {gone_episodes} episodes",
    )
    expired_ep = await pool().fetchrow("SELECT status, revision FROM episode WHERE id=$1", ep["id"])
    check(
        "an old episode leaves recall without losing its provenance",
        expired_ep["status"] == "expired"
        and expired_ep["revision"] == 2
        and await pool().fetchval(
            "SELECT count(*) FROM episode_event WHERE episode_id=$1", ep["id"]
        )
        == 1,
        str(dict(expired_ep)),
    )
    check(
        "episode decay removes its rebuildable vector",
        await pool().fetchval("SELECT count(*) FROM embedding_index WHERE object_id=$1", ep["id"])
        == 0,
    )
    # A name that reached confirmed is not a guess any more, so it does not expire with
    # them: three people agreed on it, and their agreeing does not stop being true.
    kept = await pool().fetchval(
        "SELECT status FROM alias WHERE group_id=$1 AND alias_text='阿豪'", G.to_db()
    )
    check(
        "a name the group converged on is not forgotten with the guesses",
        kept == "confirmed",
        str(kept),
    )

    # -- forgetting is class-aware ------------------------------------------
    # The kind of fact, not the repetition count, is the primary axis: what somebody is
    # currently playing goes stale in weeks, where they live holds for months. A uniform
    # clock got both wrong - much-repeated ephemera outlived once-stated stable facts.
    subj = (await ids_probe().account_of("qq", "u2")).entity_id
    for pred, obj in (("plays", "某新游"), ("lives_in", "南京")):
        await mem_probe().supersede(
            Fact(
                subject_entity_id=subj,
                predicate=pred,
                object_value=obj,
                group_id=G,
                memory_type=MemoryType.ATTRIBUTE,
                confidence=0.2,
            ),
            [],
            when=now_local(),
        )
    await pool().execute(
        """UPDATE memory_fact SET last_confirmed_at = NOW() - INTERVAL '40 days'
            WHERE group_id=$1 AND subject_entity_id=$2""",
        G.to_db(),
        subj,
    )
    await w.decay(G)
    left = {
        r["predicate"]: r["status"]
        for r in await pool().fetch(
            "SELECT predicate, status FROM memory_fact WHERE group_id=$1 AND subject_entity_id=$2",
            G.to_db(),
            subj,
        )
    }
    # Aged out reads as expired, not superseded: nothing contradicted it.
    check(
        "at forty days a current-state fact is gone and a stable one holds",
        left == {"plays": "expired", "lives_in": "active"},
        str(left),
    )

    # -- exact batch recovery and selection boundaries -----------------------
    class FailingText(FakeText):
        async def respond(self, input, **kw):
            raise RuntimeError("model down")

    exact = ExtractionRepository()
    await say("u1", "董自豪", "批次在失败后仍要保留", "exact-fail-1")
    await say("u2", "小北", "重试必须读取同一批事件", "exact-fail-2")
    before = await exact.unconsumed_count(G)
    set_providers(
        Providers(
            text=FailingText(), vision=Unused(), asr=Unused(), embedding=_EMBED, search=Unused()
        )
    )
    failing_worker = MemoryWorker(test_cfg, _PROVIDERS, worker_id="exact-fail")
    try:
        await failing_worker.extract(G)
        crashed = False
    except RuntimeError:
        crashed = True
    reserved = await exact.open(G)
    reserved_ids = tuple(event.raw_event_id for event in reserved.events)
    after_claim = await exact.unconsumed_count(G)
    check("a failed extraction propagates to the job layer", crashed)
    check(
        "and leaves one durable exact batch for the retry",
        reserved is not None
        and reserved.status.value == "extracting"
        and before - after_claim == len(reserved_ids),
        f"{len(reserved_ids)} reserved, {after_claim}/{before} unconsumed",
    )
    set_providers(
        Providers(text=FakeText(), vision=Unused(), asr=Unused(), embedding=_EMBED, search=Unused())
    )
    retry_worker = MemoryWorker(test_cfg, _PROVIDERS, worker_id="exact-retry")
    n_retry = await retry_worker.extract(G)
    check(
        "the retry resumes the reserved batch and drains without a watermark",
        n_retry > 0 and await exact.open(G) is None and await exact.unconsumed_count(G) == 0,
        f"{n_retry} candidates",
    )

    from datetime import timedelta as _td

    batch_gap = _td(minutes=config().default.memory.batch_gap_min)

    # Equal created_at values need no trimming: membership records exact ids, so rows
    # outside the limit remain independently claimable however their timestamps tie.
    G2 = GroupId("9002")
    for i in range(WINDOW + 2):
        await say("t1", "阿花", f"同一时间的第 {i} 句", f"tie{i}", gid=G2)
    await pool().execute(
        """UPDATE raw_event
              SET created_at=(SELECT min(created_at) FROM raw_event WHERE group_id=$1)
            WHERE group_id=$1""",
        G2.to_db(),
    )
    tied = await exact.claim(
        G2,
        limit=WINDOW,
        floor=0,
        gap=batch_gap,
    )
    check(
        "equal timestamps keep an exact full batch and leave the suffix",
        len(tied.events) == WINDOW and await exact.unconsumed_count(G2) == 2,
        f"{len(tied.events)} claimed",
    )

    G3 = GroupId("9003")
    for i in range(WINDOW):
        await say("t2", "阿强", f"分段测试第 {i} 句", f"gap{i}", gid=G3)
    gap_ids = [
        row["id"]
        for row in await pool().fetch(
            "SELECT id FROM raw_event WHERE group_id=$1 ORDER BY created_at,id",
            G3.to_db(),
        )
    ]
    await pool().execute(
        """UPDATE raw_event SET occurred_at=occurred_at + $2::interval
            WHERE id=ANY($1::uuid[])""",
        gap_ids[100:],
        batch_gap,
    )
    gapped = await exact.claim(
        G3,
        limit=WINDOW,
        floor=0,
        gap=batch_gap,
    )
    check(
        "a full exact batch still cuts at the last tail-half conversation gap",
        len(gapped.events) == 100,
        f"{len(gapped.events)} events",
    )

    G4 = GroupId("9004")
    for i in range(WINDOW + 25):
        await say("t3", "阿强", f"积压第 {i} 句", f"backlog{i}", gid=G4)
    calls_before_drain = CALLS.count("extract")
    await w.extract(G4)
    check(
        "a backlog wider than one window drains in exactly two exact batches",
        CALLS.count("extract") == calls_before_drain + 2 and await exact.unconsumed_count(G4) == 0,
        f"{CALLS.count('extract') - calls_before_drain} calls",
    )

    G5 = GroupId("9005")
    for i in range(3):
        await say("t4", "阿花", f"冻结前第 {i} 句", f"frozen{i}", gid=G5)
    frozen = await exact.claim(G5, limit=WINDOW, floor=0, gap=batch_gap)
    frozen_ids = tuple(event.raw_event_id for event in frozen.events)
    await say("t4", "阿花", "冻结后才到", "late-after-claim", gid=G5)
    resumed = await exact.open(G5)
    check(
        "events arriving after claim stay out of the frozen batch",
        tuple(event.raw_event_id for event in resumed.events) == frozen_ids
        and await exact.unconsumed_count(G5) == 1,
    )

    # Two workers may legally receive twin jobs while one is running. Exact event
    # membership prevents a second batch, and the session lock prevents a second paid
    # call for the same extracting batch without holding a transaction over the model.
    class BlockingText(FakeText):
        def __init__(self):
            self.entered = asyncio.Event()
            self.release = asyncio.Event()
            self.calls = 0
            self.slot_without_transaction = False

        async def respond(self, input, **kw):
            if kw.get("kind") == "extract":
                self.calls += 1
                locks = await pool().fetch(
                    """SELECT a.xact_start
                         FROM pg_locks l JOIN pg_stat_activity a USING (pid)
                        WHERE l.locktype='advisory' AND l.granted"""
                )
                self.slot_without_transaction = bool(locks) and all(
                    row["xact_start"] is None for row in locks
                )
                self.entered.set()
                await self.release.wait()
            return await super().respond(input, **kw)

    G6 = GroupId("9006")
    await say("t6", "阿文", "并发批次只付一次", "concurrent-claim", gid=G6)
    blocking = BlockingText()
    set_providers(
        Providers(text=blocking, vision=Unused(), asr=Unused(), embedding=_EMBED, search=Unused())
    )
    first_worker = MemoryWorker(test_cfg, _PROVIDERS, worker_id="concurrent-a")
    second_worker = MemoryWorker(test_cfg, _PROVIDERS, worker_id="concurrent-b")
    first_task = asyncio.create_task(first_worker.extract(G6))
    await asyncio.wait_for(blocking.entered.wait(), timeout=2)
    second_result = await asyncio.wait_for(second_worker.extract(G6), timeout=2)
    blocking.release.set()
    await asyncio.wait_for(first_task, timeout=5)
    check(
        "concurrent workers make one provider call for one exact batch",
        blocking.calls == 1 and second_result == 0,
        f"{blocking.calls} calls",
    )
    check("the provider slot holds no database transaction", blocking.slot_without_transaction)

    set_providers(
        Providers(text=FakeText(), vision=Unused(), asr=Unused(), embedding=_EMBED, search=Unused())
    )
    from qqbot.domain.memory import Candidate, CandidateType

    # A staged checkpoint is already-paid output. A fresh worker applies it directly,
    # including the zero-candidate case, and never calls the provider again.
    G7 = GroupId("9007")
    await say("t7", "阿青", "这个群最近在讨论精确批次", "staged-restart", gid=G7)
    staged = await exact.claim(G7, limit=WINDOW, floor=0, gap=batch_gap)
    _codes, _roster, _lines, staged_snapshot = await w._render_snapshot(staged)
    staged_candidate = Candidate(
        candidate_type=CandidateType.GROUP_FACT,
        payload={
            "kind": "topic",
            "topic": "讨论精确批次",
            "source": staged_snapshot.lines[0].ordinal,
            "quote": "这个群最近在讨论精确批次",
        },
        group_id=G7,
        source_event_id=staged.events[0].raw_event_id,
        extraction_id=staged.id,
    )
    await exact.stage(staged.id, staged_snapshot, [staged_candidate])
    calls_before_stage_resume = CALLS.count("extract")
    await MemoryWorker(test_cfg, _PROVIDERS, worker_id="staged-resume").extract(G7)
    staged_state = await pool().fetchval(
        "SELECT status FROM memory_extraction WHERE id=$1", staged.id
    )
    staged_audit = await pool().fetchval(
        "SELECT status FROM memory_candidate WHERE id=$1", staged_candidate.id
    )
    check(
        "a staged restart applies without paying the provider again",
        CALLS.count("extract") == calls_before_stage_resume
        and staged_state == "applied"
        and staged_audit == "accepted",
    )

    G8 = GroupId("9008")
    await say("t8", "阿蓝", "这批没有长期信息", "zero-candidate", gid=G8)
    empty = await exact.claim(G8, limit=WINDOW, floor=0, gap=batch_gap)
    _codes, _roster, _lines, empty_snapshot = await w._render_snapshot(empty)
    await exact.stage(empty.id, empty_snapshot, [])
    calls_before_empty = CALLS.count("extract")
    await MemoryWorker(test_cfg, _PROVIDERS, worker_id="zero-resume").extract(G8)
    check(
        "a zero-candidate checkpoint still consumes its exact events",
        CALLS.count("extract") == calls_before_empty
        and await pool().fetchval("SELECT status FROM memory_extraction WHERE id=$1", empty.id)
        == "applied"
        and await exact.unconsumed_count(G8) == 0,
    )

    # Inject a failure after a real fact write. The candidate audit and extraction state
    # must roll back with the projection, then the staged checkpoint retries locally.
    G10 = GroupId("9010")
    await say("t10", "阿白", "回滚词就是回滚定义", "projection-rollback", gid=G10)
    rollback_batch = await exact.claim(G10, limit=WINDOW, floor=0, gap=batch_gap)
    _codes, _roster, _lines, rollback_snapshot = await w._render_snapshot(rollback_batch)
    rollback_candidate = Candidate(
        candidate_type=CandidateType.GROUP_FACT,
        payload={
            "kind": "term",
            "term": "回滚词",
            "meaning": "回滚定义",
            "source": rollback_snapshot.lines[0].ordinal,
            "quote": "回滚词就是回滚定义",
        },
        group_id=G10,
        source_event_id=rollback_batch.events[0].raw_event_id,
        extraction_id=rollback_batch.id,
    )
    await exact.stage(rollback_batch.id, rollback_snapshot, [rollback_candidate])
    rollback_worker = MemoryWorker(test_cfg, _PROVIDERS, worker_id="projection-fault")
    original_supersede = rollback_worker._mem.supersede

    async def write_then_fail(*args, **kwargs):
        await original_supersede(*args, **kwargs)
        raise RuntimeError("injected projection failure")

    rollback_worker._mem.supersede = write_then_fail
    try:
        await rollback_worker.extract(G10)
        projection_failed = False
    except RuntimeError:
        projection_failed = True
    finally:
        rollback_worker._mem.supersede = original_supersede
    check(
        "a projection failure rolls back every write and audit change",
        projection_failed
        and await pool().fetchval(
            "SELECT status FROM memory_extraction WHERE id=$1", rollback_batch.id
        )
        == "staged"
        and await pool().fetchval(
            "SELECT status FROM memory_candidate WHERE id=$1", rollback_candidate.id
        )
        == "pending"
        and await pool().fetchval(
            """SELECT count(*) FROM memory_fact
                  WHERE group_id=$1 AND predicate='term' AND object_key='回滚词'""",
            G10.to_db(),
        )
        == 0,
    )
    calls_before_projection_retry = CALLS.count("extract")
    await rollback_worker.extract(G10)
    check(
        "the staged projection retry succeeds without another provider call",
        CALLS.count("extract") == calls_before_projection_retry
        and await pool().fetchval(
            "SELECT status FROM memory_candidate WHERE id=$1", rollback_candidate.id
        )
        == "accepted",
    )

    # The episode, its accepted audit row, the extraction state, and its EMBED job are
    # one commit. Failing after enqueue must leave none of them half-applied.
    G11 = GroupId("9011")
    await say("t11", "阿金", "今天完成了原子投影测试", "episode-atomic", gid=G11)
    episode_batch = await exact.claim(G11, limit=WINDOW, floor=0, gap=batch_gap)
    _codes, _roster, _lines, episode_snapshot = await w._render_snapshot(episode_batch)
    episode_candidate = Candidate(
        candidate_type=CandidateType.EPISODE,
        payload={
            "summary": "完成了原子投影测试",
            "sources": [
                {
                    "source": episode_snapshot.lines[0].ordinal,
                    "quote": "今天完成了原子投影测试",
                }
            ],
        },
        group_id=G11,
        source_event_id=episode_batch.events[0].raw_event_id,
        extraction_id=episode_batch.id,
    )
    await exact.stage(episode_batch.id, episode_snapshot, [episode_candidate])
    episode_worker = MemoryWorker(test_cfg, _PROVIDERS, worker_id="episode-fault")
    original_submit = episode_worker._queue.submit

    async def enqueue_then_fail(job_type, payload, **kwargs):
        result = await original_submit(job_type, payload, **kwargs)
        if job_type is not None and job_type.value == "embed":
            raise RuntimeError("injected enqueue failure")
        return result

    episode_worker._queue.submit = enqueue_then_fail
    try:
        await episode_worker.extract(G11)
        enqueue_failed = False
    except RuntimeError:
        enqueue_failed = True
    finally:
        episode_worker._queue.submit = original_submit
    check(
        "episode projection and EMBED enqueue roll back together",
        enqueue_failed
        and await pool().fetchval("SELECT count(*) FROM episode WHERE id=$1", episode_candidate.id)
        == 0
        and await pool().fetchval(
            """SELECT count(*) FROM memory_job
                  WHERE job_type='embed' AND payload->>'group_id'=$1""",
            str(G11),
        )
        == 0
        and await pool().fetchval(
            "SELECT status FROM memory_candidate WHERE id=$1", episode_candidate.id
        )
        == "pending",
    )
    await episode_worker.extract(G11)
    check(
        "episode, audit, applied state and EMBED job commit together",
        await pool().fetchval("SELECT count(*) FROM episode WHERE id=$1", episode_candidate.id) == 1
        and await pool().fetchval(
            """SELECT count(*) FROM memory_job
                  WHERE job_type='embed' AND payload->>'group_id'=$1""",
            str(G11),
        )
        == 1
        and await pool().fetchval(
            "SELECT status FROM memory_candidate WHERE id=$1", episode_candidate.id
        )
        == "accepted"
        and await pool().fetchval(
            "SELECT status FROM memory_extraction WHERE id=$1", episode_batch.id
        )
        == "applied",
    )

    # Extraction is a use of the text model with its own model, grade and timeout,
    # while the shared wiring (endpoint, key, backend, concurrency) stays the reply
    # path's. The grade and timeout hold for every extraction above; the model
    # override is probed here, last, because it adds an extract call the counting
    # assertions above must not see.
    _txt = config().default.capabilities.text
    check(
        "extraction carries its own grade and timeout, not the reply path's",
        SEEN_EXTRACT_CFG
        and all(
            g == _txt.extract.reasoning_effort and t == _txt.extract.timeout_sec
            for _m, g, t in SEEN_EXTRACT_CFG
        )
        and _txt.extract.timeout_sec != _txt.timeout_sec,
        str(SEEN_EXTRACT_CFG[:2]),
    )
    from qqbot.services import ExtractionInput as _EIovr, MemoryExtractor as _MEovr

    _base = config().default
    _covr = _base.model_copy(
        update={
            "capabilities": _base.capabilities.model_copy(
                update={
                    "text": _base.capabilities.text.model_copy(
                        update={
                            "extract": _base.capabilities.text.extract.model_copy(
                                update={"model": "flash-probe"}
                            )
                        }
                    )
                }
            )
        }
    )
    await _MEovr(_covr, _PROVIDERS.text).extract(
        _EIovr(
            group_id=G,
            transcript="⟦09-18 00:00⟧ 成员⟦1⟧: 测试",
            roster="成员⟦1⟧",
            account_codes={},
            lines=(),
        )
    )
    check(
        "the extract model override moves extraction alone",
        SEEN_MODELS[-1] == "flash-probe" and SEEN_MODELS[0] == _txt.model,
        f"first={SEEN_MODELS[0]} last={SEEN_MODELS[-1]}",
    )

    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
