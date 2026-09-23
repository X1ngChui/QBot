"""Pure extraction schemas, source binding, and candidate validation."""

import json
import os
import pathlib
import sys
import uuid
from datetime import UTC, datetime

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))

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

fails: list[str] = []
ACCOUNT_1, ACCOUNT_2, ACCOUNT_3 = uuid.uuid4(), uuid.uuid4(), uuid.uuid4()
EVENT_1, EVENT_2, EVENT_3, BOT_EVENT, NOTICE_EVENT = (uuid.uuid4() for _ in range(5))
WHEN = datetime.now(UTC)
CODES = {1: ACCOUNT_1, 2: ACCOUNT_2, 3: ACCOUNT_3}


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"[{'ok ' if condition else 'FAIL'}] {name}  {detail}")
    if not condition:
        fails.append(name)


def source_line(
    ordinal: int,
    event_id: uuid.UUID,
    evidence: str,
    *,
    author: uuid.UUID | None,
    targets: tuple[SnapshotTarget, ...],
    own: bool = False,
    event_type: str = "message",
) -> SnapshotLine:
    return SnapshotLine(
        ordinal=ordinal,
        event_id=event_id,
        occurred_at=WHEN,
        text=f"{sysmark(f'来源:{ordinal}')} 某人: {evidence}",
        own=own,
        event_type=event_type,
        evidence_text=evidence,
        author_account_id=author,
        targets=targets,
    )


LINES = (
    source_line(
        1,
        EVENT_1,
        "我最近在玩鸣潮",
        author=ACCOUNT_1,
        targets=(SnapshotTarget(ACCOUNT_1, "author"),),
    ),
    source_line(
        2,
        EVENT_2,
        "老王最近在玩绝区零",
        author=ACCOUNT_2,
        targets=(
            SnapshotTarget(ACCOUNT_2, "author"),
            SnapshotTarget(ACCOUNT_1, "alias", "老王"),
        ),
    ),
    source_line(
        3,
        EVENT_3,
        f"@小北{sysmark('2')} 住在杭州",
        author=ACCOUNT_1,
        targets=(
            SnapshotTarget(ACCOUNT_1, "author"),
            SnapshotTarget(ACCOUNT_2, "mention", sysmark("2")),
        ),
    ),
    source_line(
        4,
        BOT_EVENT,
        "机器人说过的话",
        author=None,
        targets=(),
        own=True,
    ),
    source_line(
        5,
        NOTICE_EVENT,
        "加入了本群",
        author=ACCOUNT_3,
        targets=(),
        event_type="notice",
    ),
)
SNAPSHOT = ExtractionSnapshot(tuple(CODES.items()), LINES)
VALIDATOR = Validator(SNAPSHOT)


def candidate(ctype: CandidateType, source: int, event: uuid.UUID, **payload) -> Candidate:
    return Candidate(
        candidate_type=ctype,
        payload={"source": source, **payload},
        group_id=1,
        source_event_id=event,
    )


speaker_fact = candidate(
    CandidateType.FACT,
    1,
    EVENT_1,
    account=1,
    predicate="plays",
    object="鸣潮",
    quote="我最近在玩鸣潮",
)
check("a speaker fact binds to its exact account", VALIDATOR.check(speaker_fact).ok)

third_party = candidate(
    CandidateType.FACT,
    2,
    EVENT_2,
    account=1,
    predicate="plays",
    object="绝区零",
    quote="老王最近在玩绝区零",
)
check("a unique literal alias may identify a non-speaker", VALIDATOR.check(third_party).ok)

mentioned = candidate(
    CandidateType.FACT,
    3,
    EVENT_3,
    account=2,
    predicate="lives_in",
    object="杭州",
    quote=f"@小北{sysmark('2')} 住在杭州",
)
check("a structured mention may identify a third account", VALIDATOR.check(mentioned).ok)

unknown = candidate(
    CandidateType.FACT,
    1,
    EVENT_1,
    account=9,
    predicate="plays",
    object="鸣潮",
    quote="我最近在玩鸣潮",
)
check(
    "an unknown account code is rejected",
    VALIDATOR.check(unknown).reason is RejectReason.UNKNOWN_ENTITY,
)

uncited = candidate(
    CandidateType.FACT,
    1,
    EVENT_1,
    account=3,
    predicate="plays",
    object="鸣潮",
    quote="我最近在玩鸣潮",
)
check(
    "a roster account not resolved on this source is rejected",
    VALIDATOR.check(uncited).reason is RejectReason.MALFORMED,
)

wrong_source = candidate(
    CandidateType.FACT,
    2,
    EVENT_2,
    account=1,
    predicate="plays",
    object="鸣潮",
    quote="我最近在玩鸣潮",
)
check(
    "a quote cannot borrow text from another source",
    VALIDATOR.check(wrong_source).reason is RejectReason.MALFORMED,
)

wrong_event = candidate(
    CandidateType.FACT,
    1,
    EVENT_2,
    account=1,
    predicate="plays",
    object="鸣潮",
    quote="我最近在玩鸣潮",
)
check(
    "the staged event must match the declared source",
    VALIDATOR.check(wrong_event).reason is RejectReason.MALFORMED,
)

bot_fact = candidate(
    CandidateType.FACT,
    4,
    BOT_EVENT,
    account=1,
    predicate="plays",
    object="鸣潮",
    quote="机器人说过的话",
)
notice_fact = candidate(
    CandidateType.FACT,
    5,
    NOTICE_EVENT,
    account=3,
    predicate="plays",
    object="鸣潮",
    quote="加入了本群",
)
check("bot output is never evidence", VALIDATOR.check(bot_fact).reason is RejectReason.MALFORMED)
check(
    "notice text is never evidence", VALIDATOR.check(notice_fact).reason is RejectReason.MALFORMED
)

alias = candidate(
    CandidateType.ALIAS,
    2,
    EVENT_2,
    account=1,
    alias="老王",
    kind="nickname",
    quote="老王最近在玩绝区零",
)
check("an alias must appear in its cited quote", VALIDATOR.check(alias).ok)
missing_alias = candidate(
    CandidateType.ALIAS,
    2,
    EVENT_2,
    account=1,
    alias="王哥",
    kind="nickname",
    quote="老王最近在玩绝区零",
)
check(
    "an alias absent from its quote is rejected",
    VALIDATOR.check(missing_alias).reason is RejectReason.MALFORMED,
)

bad_predicate = candidate(
    CandidateType.FACT,
    1,
    EVENT_1,
    account=1,
    predicate="随便",
    object="x",
    quote="我最近在玩鸣潮",
)
check(
    "the predicate set is closed",
    VALIDATOR.check(bad_predicate).reason is RejectReason.MALFORMED,
)

ambiguous = [
    alias,
    candidate(
        CandidateType.ALIAS,
        2,
        EVENT_2,
        account=2,
        alias="老王",
        kind="nickname",
        quote="老王最近在玩绝区零",
    ),
]
check(
    "one alias proposed for two accounts invalidates both",
    VALIDATOR.ambiguous_aliases(ambiguous) == {"老王"},
)

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
    source_event_id=EVENT_1,
)
check("an episode accepts multiple exact sources", VALIDATOR.check(episode).ok)
check(
    "all episode source events survive validation",
    [line.event_id for line in VALIDATOR.source_lines(episode)] == [EVENT_1, EVENT_3],
)
invalid_episode = Candidate(
    candidate_type=CandidateType.EPISODE,
    payload={
        "summary": "错误来源",
        "sources": [{"source": 4, "quote": "机器人说过的话"}],
    },
    group_id=1,
    source_event_id=BOT_EVENT,
)
check(
    "an episode cannot use a bot source",
    VALIDATOR.check(invalid_episode).reason is RejectReason.MALFORMED,
)

TOOL_DEFS = tools()
BY_NAME = {tool.name: tool for tool in TOOL_DEFS}
check(
    "the model is offered exactly the structured memory tools",
    set(BY_NAME)
    == {
        "record_alias",
        "record_fact",
        "record_group_term",
        "record_group_topic",
        "record_episode",
    },
)
for name in ("record_alias", "record_fact", "record_group_term", "record_group_topic"):
    required = set(BY_NAME[name].parameters["required"])
    check(f"{name} requires source and quote", {"source", "quote"} <= required, str(required))
episode_schema = BY_NAME["record_episode"].parameters
check(
    "record_episode requires a bounded sources array",
    episode_schema["required"] == ["summary", "sources"]
    and episode_schema["properties"]["sources"]["maxItems"] == 8,
)
check("record_episode has no participant field", "participants" not in episode_schema["properties"])
for name in ("record_alias", "record_fact"):
    check(f"{name} names an account by code", "account" in BY_NAME[name].parameters["required"])
for name in ("record_group_term", "record_group_topic"):
    check(f"{name} names no account", "account" not in BY_NAME[name].parameters["properties"])
check(
    "no tool offers to record a self-reinforcing joke category",
    not any(
        "梗"
        in json.dumps(
            {"name": tool.name, "description": tool.description, "parameters": tool.parameters},
            ensure_ascii=False,
        )
        for tool in TOOL_DEFS
    ),
)

PREDS = predicate_names()
check("the predicate table is non-trivial", len(PREDS) > 10, str(len(PREDS)))
explained = rules_block()
check(
    "every offered predicate is explained",
    all(f"- {predicate}（" in explained or f"- {predicate}：" in explained for predicate in PREDS),
)
from qqbot.services.context_builder import render_fact
from qqbot.settings import PredicateTable, config

check(
    "every configured predicate renders without an English key",
    all(
        not any(
            char.isascii() and char.isalpha() for char in render_fact(predicate, "某物", "某物")
        )
        for predicate in PREDS
    ),
)
check("a retired predicate renders as nothing", render_fact("no_such_predicate", "某物") == "")
stable, fast = decay_classes()
check(
    "decay classes only name configured or group predicates",
    (set(stable) | set(fast)) <= set(PREDS) | {"topic"},
)
check("decay classes do not overlap", not set(stable) & set(fast))
table = config().predicates.person
check(
    "multi-valued predicates derive from the same table",
    set(multi_valued()) == {name for name in PREDS if table[name].cardinality == "multi"},
)
check(
    "opposites are mutual",
    all(opposites().get(right) == left for left, right in opposites().items()),
)

valid_predicate = {"verb": "喜欢", "cardinality": "multi", "rule": "喜欢的事物。"}
for name, bad in (
    ("missing rule", {"person": {"likes": {"verb": "喜欢", "cardinality": "multi"}}}),
    ("missing verb", {"person": {"likes": {"cardinality": "multi", "rule": "x"}}}),
    ("unknown decay", {"person": {"likes": valid_predicate | {"decay": "slow"}}}),
    (
        "one-sided opposite",
        {
            "person": {
                "likes": valid_predicate | {"opposite": "dislikes"},
                "dislikes": valid_predicate,
            }
        },
    ),
    ("missing opposite", {"person": {"likes": valid_predicate | {"opposite": "nope"}}}),
    ("reserved group predicate", {"person": {"topic": valid_predicate}}),
    ("unindexable name", {"person": {"Likes!": valid_predicate}}),
):
    try:
        PredicateTable.model_validate(bad)
        check(f"{name} is refused at load", False)
    except ValueError:
        check(f"{name} is refused at load", True)

# Tool calls bind explicit source ordinals before candidates are staged.
BATCH = uuid.uuid4()
SOURCE_LINES = tuple(
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
    for line in LINES
)
INPUT = ExtractionInput(
    group_id=1,
    transcript="\n".join(line.text for line in SOURCE_LINES),
    roster="老王[1]\n小北[2]",
    account_codes=CODES,
    lines=SOURCE_LINES,
    extraction_id=BATCH,
)


def call(name: str, **arguments) -> ToolCall:
    return ToolCall(
        ToolCallId("extract-call"),
        name,
        json.dumps(arguments, ensure_ascii=False),
    )


parsed = MemoryExtractor._to_candidate(
    call(
        "record_fact",
        source=1,
        account=1,
        predicate="plays",
        object="鸣潮",
        quote="我最近在玩鸣潮",
    ),
    INPUT,
)
check(
    "a tool call becomes a fact candidate",
    parsed is not None and parsed.candidate_type is CandidateType.FACT,
)
check(
    "a candidate cites the declared source event",
    parsed is not None and parsed.source_event_id == EVENT_1,
)
check(
    "a candidate carries its extraction batch", parsed is not None and parsed.extraction_id == BATCH
)
nowhere = MemoryExtractor._to_candidate(
    call(
        "record_fact",
        source=2,
        account=1,
        predicate="plays",
        object="x",
        quote="我最近在玩鸣潮",
    ),
    INPUT,
)
check(
    "a source and quote mismatch stages no event citation",
    nowhere is not None and nowhere.source_event_id is None,
)
parsed_episode = MemoryExtractor._to_candidate(
    call(
        "record_episode",
        summary="讨论游戏与居住地",
        sources=[
            {"source": 1, "quote": "我最近在玩鸣潮"},
            {"source": 3, "quote": "住在杭州"},
        ],
    ),
    INPUT,
)
check(
    "an episode candidate cites its first source before full validation",
    parsed_episode is not None and parsed_episode.source_event_id == EVENT_1,
)
check(
    "non-object JSON arguments are dropped",
    MemoryExtractor._to_candidate(ToolCall(ToolCallId("bad"), "record_fact", "[1]"), INPUT) is None,
)
check(
    "invalid JSON arguments are dropped",
    MemoryExtractor._to_candidate(ToolCall(ToolCallId("bad"), "record_fact", "{坏"), INPUT) is None,
)
check(
    "unknown tools are dropped",
    MemoryExtractor._to_candidate(call("drop_table", value=1), INPUT) is None,
)

print()
print("FAILED:", fails if fails else "none")
sys.exit(1 if fails else 0)
