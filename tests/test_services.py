"""Extraction schemas, exact source binding and candidate validation."""

import json
import uuid
from datetime import UTC, datetime

import pytest

from _fixtures import config
from qqbot.configuration import PredicateTable
from qqbot.domain.memory import (
    Candidate,
    CandidateType,
    ExtractionSnapshot,
    RejectReason,
    SnapshotLine,
    SnapshotTarget,
)
from qqbot.providers.contracts import ToolCall, ToolCallId
from qqbot.services import Validator
from qqbot.services.context_builder import render_fact
from qqbot.services.memory_extractor import (
    ExtractionInput,
    MemoryExtractor,
    SourceLine,
    decay_classes,
    multi_valued,
    opposites,
    predicate_names,
    rules_block,
    tools,
)
from qqbot.util import sysmark


@pytest.fixture(scope="module")
def sources():
    accounts = [uuid.uuid4() for _ in range(3)]
    events = [uuid.uuid4() for _ in range(5)]
    when = datetime.now(UTC)

    def line(ordinal, evidence, *, author, targets, own=False, event_type="message"):
        return SnapshotLine(
            ordinal=ordinal,
            event_id=events[ordinal - 1],
            occurred_at=when,
            text=f"{sysmark(f'来源:{ordinal}')} 某人: {evidence}",
            own=own,
            event_type=event_type,
            evidence_text=evidence,
            author_account_id=author,
            targets=targets,
        )

    lines = (
        line(
            1,
            "我最近在玩鸣潮",
            author=accounts[0],
            targets=(SnapshotTarget(accounts[0], "author"),),
        ),
        line(
            2,
            "老王最近在玩绝区零",
            author=accounts[1],
            targets=(
                SnapshotTarget(accounts[1], "author"),
                SnapshotTarget(accounts[0], "alias", "老王"),
            ),
        ),
        line(
            3,
            f"@小北{sysmark('2')} 住在杭州",
            author=accounts[0],
            targets=(
                SnapshotTarget(accounts[0], "author"),
                SnapshotTarget(accounts[1], "mention", sysmark("2")),
            ),
        ),
        line(4, "机器人说过的话", author=None, targets=(), own=True),
        line(5, "加入了本群", author=accounts[2], targets=(), event_type="notice"),
    )
    codes = dict(enumerate(accounts, start=1))
    snapshot = ExtractionSnapshot(tuple(codes.items()), lines)
    validator = Validator(snapshot, predicates=config().predicates)
    extraction_id = uuid.uuid4()
    source_lines = tuple(
        SourceLine(
            ordinal=line.ordinal,
            event_id=line.event_id,
            text=line.text,
            own=line.own,
            event_type=line.event_type,
            evidence_text=line.evidence_text,
            author_account_id=line.author_account_id,
            targets=line.targets,
        )
        for line in lines
    )
    extraction = ExtractionInput(
        group_id=1,
        transcript="\n".join(line.text for line in source_lines),
        roster="老王[1]\n小北[2]",
        account_codes=codes,
        lines=source_lines,
        extraction_id=extraction_id,
    )
    return validator, events, extraction


def candidate(candidate_type, source, events, **payload):
    return Candidate(
        candidate_type=candidate_type,
        payload={"source": source, **payload},
        group_id=1,
        source_event_id=events[source - 1],
    )


@pytest.mark.parametrize(
    ("source", "account", "predicate", "object", "quote"),
    [
        (1, 1, "plays", "鸣潮", "我最近在玩鸣潮"),
        (2, 1, "plays", "绝区零", "老王最近在玩绝区零"),
        (3, 2, "lives_in", "杭州", f"@小北{sysmark('2')} 住在杭州"),
    ],
)
def test_exact_account_binding(sources, source, account, predicate, object, quote):
    validator, events, _ = sources
    fact = candidate(
        CandidateType.FACT,
        source,
        events,
        account=account,
        predicate=predicate,
        object=object,
        quote=quote,
    )
    assert validator.check(fact).ok


@pytest.mark.parametrize(
    ("source", "account", "predicate", "object", "quote", "expected"),
    [
        (1, 9, "plays", "鸣潮", "我最近在玩鸣潮", RejectReason.UNKNOWN_ENTITY),
        (1, 3, "plays", "鸣潮", "我最近在玩鸣潮", RejectReason.MALFORMED),
        (2, 1, "plays", "鸣潮", "我最近在玩鸣潮", RejectReason.MALFORMED),
        (4, 1, "plays", "鸣潮", "机器人说过的话", RejectReason.MALFORMED),
        (5, 3, "plays", "鸣潮", "加入了本群", RejectReason.MALFORMED),
        (1, 1, "随便", "x", "我最近在玩鸣潮", RejectReason.MALFORMED),
    ],
)
def test_invalid_fact_citations(sources, source, account, predicate, object, quote, expected):
    validator, events, _ = sources
    fact = candidate(
        CandidateType.FACT,
        source,
        events,
        account=account,
        predicate=predicate,
        object=object,
        quote=quote,
    )
    assert validator.check(fact).reason is expected


def test_staged_event_must_match_source(sources):
    validator, events, _ = sources
    fact = candidate(
        CandidateType.FACT,
        1,
        events,
        account=1,
        predicate="plays",
        object="鸣潮",
        quote="我最近在玩鸣潮",
    )
    wrong_event = Candidate(
        candidate_type=CandidateType.FACT,
        payload=fact.payload,
        group_id=1,
        source_event_id=events[1],
    )
    assert validator.check(wrong_event).reason is RejectReason.MALFORMED


def test_alias_quote_and_ambiguous_targets(sources):
    validator, events, _ = sources
    alias = candidate(
        CandidateType.ALIAS,
        2,
        events,
        account=1,
        alias="老王",
        kind="nickname",
        quote="老王最近在玩绝区零",
    )
    assert validator.check(alias).ok
    missing = candidate(
        CandidateType.ALIAS,
        2,
        events,
        account=1,
        alias="王哥",
        kind="nickname",
        quote="老王最近在玩绝区零",
    )
    assert validator.check(missing).reason is RejectReason.MALFORMED
    other = candidate(
        CandidateType.ALIAS,
        2,
        events,
        account=2,
        alias="老王",
        kind="nickname",
        quote="老王最近在玩绝区零",
    )
    assert validator.ambiguous_aliases([alias, other]) == {"老王"}


def test_episode_checks_all_sources_and_excludes_bot_output(sources):
    validator, events, _ = sources
    episode = Candidate(
        candidate_type=CandidateType.EPISODE,
        payload={
            "summary": "两名成员讨论了近期玩的游戏和居住地",
            "sources": [
                {"source": 1, "quote": "我最近在玩鸣潮"},
                {"source": 3, "quote": "住在杭州"},
            ],
        },
        group_id=1,
        source_event_id=events[0],
    )
    assert validator.check(episode).ok
    assert [line.event_id for line in validator.source_lines(episode)] == [events[0], events[2]]
    bad = Candidate(
        candidate_type=CandidateType.EPISODE,
        payload={"summary": "错误来源", "sources": [{"source": 4, "quote": "机器人说过的话"}]},
        group_id=1,
        source_event_id=events[3],
    )
    assert validator.check(bad).reason is RejectReason.MALFORMED


def test_memory_tool_schemas_require_citations_and_bounded_episodes():
    definitions = tools(predicates=config().predicates)
    by_name = {tool.name: tool for tool in definitions}
    assert set(by_name) == {
        "record_alias",
        "record_fact",
        "record_group_term",
        "record_group_topic",
        "record_episode",
    }
    for name in ("record_alias", "record_fact", "record_group_term", "record_group_topic"):
        assert {"source", "quote"} <= set(by_name[name].parameters["required"])
    episode = by_name["record_episode"].parameters
    assert episode["required"] == ["summary", "sources"]
    assert episode["properties"]["sources"]["maxItems"] == 8
    assert "participants" not in episode["properties"]
    for name in ("record_alias", "record_fact"):
        assert "account" in by_name[name].parameters["required"]
    for name in ("record_group_term", "record_group_topic"):
        assert "account" not in by_name[name].parameters["properties"]
    assert not any(
        "梗"
        in json.dumps(
            {"name": tool.name, "description": tool.description, "parameters": tool.parameters},
            ensure_ascii=False,
        )
        for tool in definitions
    )


def test_predicate_schemas_render_and_decay_from_one_table():
    predicates = config().predicates
    names = predicate_names(predicates=predicates)
    assert len(names) > 10
    explained = rules_block(predicates=predicates)
    assert all(f"- {name}（" in explained or f"- {name}：" in explained for name in names)
    assert all(
        not any(
            char.isascii() and char.isalpha()
            for char in render_fact(name, "某物", "某物", predicates=predicates)
        )
        for name in names
    )
    assert render_fact("no_such_predicate", "某物", predicates=predicates) == ""
    stable, fast = decay_classes(predicates=predicates)
    assert (set(stable) | set(fast)) <= set(names) | {"topic"}
    assert not set(stable) & set(fast)
    table = predicates.person
    assert set(multi_valued(predicates=predicates)) == {
        name for name in names if table[name].cardinality == "multi"
    }
    opposites_map = opposites(predicates=predicates)
    assert all(opposites_map.get(right) == left for left, right in opposites_map.items())


GOOD_PREDICATE = {"verb": "喜欢", "cardinality": "multi", "rule": "喜欢的事物。"}
BAD_PREDICATES = [
    {"person": {"likes": {"verb": "喜欢", "cardinality": "multi"}}},
    {"person": {"likes": {"cardinality": "multi", "rule": "x"}}},
    {"person": {"likes": GOOD_PREDICATE | {"decay": "slow"}}},
    {"person": {"likes": GOOD_PREDICATE | {"opposite": "dislikes"}, "dislikes": GOOD_PREDICATE}},
    {"person": {"likes": GOOD_PREDICATE | {"opposite": "nope"}}},
    {"person": {"topic": GOOD_PREDICATE}},
    {"person": {"Likes!": GOOD_PREDICATE}},
]


@pytest.mark.parametrize("invalid", BAD_PREDICATES)
def test_predicate_table_rejects_invalid_definitions(invalid):
    with pytest.raises(ValueError):
        PredicateTable.model_validate(invalid)


def _call(name, **arguments):
    return ToolCall(ToolCallId("extract-call"), name, json.dumps(arguments, ensure_ascii=False))


def test_extracted_fact_binds_source_event_and_batch(sources):
    _, events, extraction = sources
    parsed = MemoryExtractor._to_candidate(
        _call(
            "record_fact",
            source=1,
            account=1,
            predicate="plays",
            object="鸣潮",
            quote="我最近在玩鸣潮",
        ),
        extraction,
    )
    assert parsed is not None
    assert parsed.candidate_type is CandidateType.FACT
    assert parsed.source_event_id == events[0]
    assert parsed.extraction_id == extraction.extraction_id
    mismatched = MemoryExtractor._to_candidate(
        _call(
            "record_fact",
            source=2,
            account=1,
            predicate="plays",
            object="x",
            quote="我最近在玩鸣潮",
        ),
        extraction,
    )
    assert mismatched is not None
    assert mismatched.source_event_id is None


def test_extracted_episode_cites_first_source(sources):
    _, events, extraction = sources
    parsed = MemoryExtractor._to_candidate(
        _call(
            "record_episode",
            summary="讨论游戏与居住地",
            sources=[
                {"source": 1, "quote": "我最近在玩鸣潮"},
                {"source": 3, "quote": "住在杭州"},
            ],
        ),
        extraction,
    )
    assert parsed is not None
    assert parsed.source_event_id == events[0]


@pytest.mark.parametrize(
    "call",
    [
        ToolCall(ToolCallId("bad"), "record_fact", "[1]"),
        ToolCall(ToolCallId("bad"), "record_fact", "{坏"),
        _call("drop_table", value=1),
    ],
)
def test_malformed_and_unknown_tool_calls_are_dropped(sources, call):
    assert MemoryExtractor._to_candidate(call, sources[2]) is None
