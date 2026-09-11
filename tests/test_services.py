"""The half of the service layer that needs no database: the validation rules and the
parsing of tool calls.

Validation is a pure function, so every rule can be pinned down on its own, without
running the whole chain.
"""
import json
import os
import pathlib
import sys
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))

from qqbot.domain.memory import Candidate, CandidateType, RejectReason
from qqbot.services import Validator
from qqbot.services.memory_extractor import (
    ExtractionInput, MemoryExtractor, SourceLine, line_body, predicate_names, tools,
)
from qqbot.util import sysmark

fails = []
E1, E2 = uuid.uuid4(), uuid.uuid4()
CODES = {1: E1, 2: E2}
# One entry per message, not one blob: a quote has to be inside a single message, because
# that is what somebody said. Text that only matches once the lines are joined spans a
# line break, which nobody typed. Rendered the way the worker renders them: a time
# stamp, the speaker's name and code, then the body.
LINES = (f"{sysmark('08-30 14:03')} 老王{sysmark('1')}: 我最近在玩鸣潮",
         f"{sysmark('08-30 14:04')} 小北{sysmark('2')}: 老周你又来了")


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


def cand(ctype, **payload):
    # Every candidate carries the message its quote came from. One without it is one whose
    # quote matched no message, and it is refused - see the check further down.
    return Candidate(candidate_type=ctype, payload=payload, group_id=1,
                     source_event_id=uuid.uuid4(), batch_size=len(LINES))


v = Validator(CODES, LINES)

# ---- facts ----------------------------------------------------------------
ok = cand(CandidateType.FACT, account=1, predicate="plays", object="鸣潮",
          quote="我最近在玩鸣潮")
check("依据原文的事实通过", v.check(ok).ok)

bad_code = cand(CandidateType.FACT, account=7, predicate="plays", object="x",
                quote="我最近在玩鸣潮")
check("a code that is not on the roster is dropped",
      v.check(bad_code).reason is RejectReason.UNKNOWN_ENTITY,
      "答不上来的模型会编一个编号")

made_up = cand(CandidateType.FACT, account=1, predicate="plays", object="原神",
               quote="我最近在玩原神")
check("原文里没有的引用一律拒绝", v.check(made_up).reason is RejectReason.MALFORMED,
      "唯一能挡住「听起来很像但没人说过」的检查")

free_pred = cand(CandidateType.FACT, account=1, predicate="喜欢", object="x",
                 quote="我最近在玩鸣潮")
check("谓词必须在枚举内", v.check(free_pred).reason is RejectReason.MALFORMED,
      "开放字符串会让同一件事写成三条")

check("空候选被拒", v.check(cand(CandidateType.FACT)).reason is RejectReason.EMPTY)

# A quote has to sit inside one message. Joined, these two lines contain the string; but
# nobody said it - it spans a line break, so it is two people's words glued together.
across = cand(CandidateType.FACT, account=1, predicate="plays", object="x",
              quote="我最近在玩鸣潮\n" + LINES[1])
check("跨行拼出来的引用不算引用", v.check(across).reason is RejectReason.MALFORMED,
      "整段拼接里找得到，但没有任何一条发言是这么说的")

# The speaker prefix is not something anybody said. A quote that is a member's name
# would otherwise be "found" on every line that member spoke, and the record filed
# against whichever line came first.
check("the speaker prefix is stripped before matching",
      line_body(LINES[0]) == "我最近在玩鸣潮", line_body(LINES[0]))
check("and a line with no prefix is taken whole", line_body("裸文本") == "裸文本")
named = cand(CandidateType.FACT, account=1, predicate="plays", object="x", quote="老王")
check("a quote that is only a speaker's name is not a quote",
      v.check(named).reason is RejectReason.MALFORMED)
# Said twice, by two people: the quote backs neither of them.
twice = Validator(CODES, LINES + (f"{sysmark('08-30 14:05')} 老王{sysmark('1')}: 老周你又来了",))
echoed = cand(CandidateType.FACT, account=1, predicate="plays", object="x",
              quote="老周你又来了")
check("a quote found in two messages validates against neither",
      twice.check(echoed).reason is RejectReason.MALFORMED)

# The extractor could not attribute the quote to a message, and it must not fall
# back to any other message: a record filed as evidence from a message that does
# not contain it corrupts the very rows that count how many different people used
# a name - the one route by which an observed name becomes usable.
unsourced = Candidate(candidate_type=CandidateType.FACT, group_id=1,
                      payload={"account": 1, "predicate": "plays", "object": "鸣潮",
                               "quote": "我最近在玩鸣潮"},
                      source_event_id=None, batch_size=len(LINES))
check("指不出引文出处的候选被拒", v.check(unsourced).reason is RejectReason.MALFORMED,
      "以前会回退到批次末条，写出一条来源不实的证据")

# ---- names ----------------------------------------------------------------
alias_ok = cand(CandidateType.ALIAS, account=2, alias="老周", kind="nickname",
                quote="老周你又来了")
check("记录里出现过的称呼通过", v.check(alias_ok).ok)

ghost = cand(CandidateType.ALIAS, account=2, alias="老李", kind="nickname",
             quote="老周你又来了")
check("称呼本身不在原文里也要拒", v.check(ghost).reason is RejectReason.MALFORMED)

bad_kind = cand(CandidateType.ALIAS, account=2, alias="老周", kind="随便",
                quote="老周你又来了")
check("称呼类型必须在枚举内", v.check(bad_kind).reason is RejectReason.MALFORMED)

# A name belongs to one person: writing both means both are wrong, so neither is
# written.
both = [
    cand(CandidateType.ALIAS, account=1, alias="老周", kind="nickname", quote="老周你又来了"),
    cand(CandidateType.ALIAS, account=2, alias="老周", kind="nickname", quote="老周你又来了"),
]
check("one name pointing at two accounts is ambiguous", v.ambiguous_aliases(both) == {"老周"})
check("and different names are not",
      v.ambiguous_aliases([both[0]]) == set())

# ---- the tool definitions -------------------------------------------------
TOOL_DEFS = tools()
by_name = {t["function"]["name"]: t["function"] for t in TOOL_DEFS}
check("the model is offered exactly these tools",
      set(by_name) == {"record_alias", "record_fact", "record_group_term",
                       "record_group_topic", "record_episode"},
      str(sorted(by_name)))
# Every record has to carry the sentence it came from. It is the only check that can tell
# something plausible from something somebody said.
for name, fn in by_name.items():
    check(f"{name} requires a verbatim quote",
          "quote" in set(fn["parameters"]["required"]),
          str(set(fn["parameters"]["required"])))
# A record about a person names them by code, never by nickname: two people in one group
# sharing a name is ordinary, and a record filed under the wrong one stays wrong.
for name in ("record_alias", "record_fact"):
    check(f"{name} names an account by code",
          "account" in set(by_name[name]["parameters"]["required"]))
# A record about the group names nobody, so it must not ask for an account at all -
# otherwise the model invents one to fill the field.
for name in ("record_group_term", "record_group_topic"):
    check(f"{name} names nobody",
          "account" not in by_name[name]["parameters"]["properties"],
          str(set(by_name[name]["parameters"]["properties"])))
# An episode is found by who took part in it before anything looks at the text, so the
# participants are the field that has to be right.
check("record_episode names its participants by code",
      by_name["record_episode"]["parameters"]["properties"]["participants"]["items"]
      == {"type": "integer"})
# There is deliberately no tool for an in-joke. It cannot be checked, the model will
# always find one, and once written down it gets used in a reply, archived, and read back
# by the next pass as evidence that the group still says it.
check("and nothing offers to record a joke",
      not any("梗" in json.dumps(t, ensure_ascii=False) for t in TOOL_DEFS))
pred_enum = by_name["record_fact"]["parameters"]["properties"]["predicate"]["enum"]
check("the predicate is an enum in the tool definition itself",
      set(pred_enum) == set(predicate_names()),
      "the model cannot produce a value outside it, which beats checking afterwards")

# ---- the predicate table --------------------------------------------------
# One entry defines a predicate completely - what it means, how many, how fast it is
# forgotten, how it reads. What these check is that everything derived from an entry
# stays derived: the enum the model is offered, the block explaining it, the decay
# classes, the rendering.
from qqbot.services.context_builder import render_fact
from qqbot.services.memory_extractor import (
    decay_classes, multi_valued, opposites, rules_block,
)
from qqbot.settings import config as _cfg

PREDS = predicate_names()
check("the table holds predicates at all", len(PREDS) > 10, str(len(PREDS)))
_explained = rules_block()
check("every predicate the model is offered is explained to it",
      all(f"- {p}（" in _explained or f"- {p}：" in _explained for p in PREDS),
      str([p for p in PREDS
           if f"- {p}（" not in _explained and f"- {p}：" not in _explained]))
check("every predicate renders with no English leaking through",
      all(not any(c.isascii() and c.isalpha()
                  for c in render_fact(p, "某物", "某物"))
          for p in PREDS),
      str([render_fact(p, "某物", "某物") for p in PREDS
           if any(c.isascii() and c.isalpha()
                  for c in render_fact(p, "某物", "某物"))]))
# A predicate dropped from the table leaves its rows behind. They render as nothing
# rather than as the bare English name, which is what the prompt would otherwise carry.
check("a fact whose predicate is no longer configured renders as nothing",
      render_fact("no_such_predicate", "某物") == "",
      render_fact("no_such_predicate", "某物"))
check("a template verb puts the object inside the phrase",
      render_fact("allergic_to", "花生", "花生") == "对花生过敏",
      render_fact("allergic_to", "花生", "花生"))
_stable, _fast = decay_classes()
check("decay classes only name real predicates",
      (set(_stable) | set(_fast)) <= set(PREDS) | {"topic"},
      str((set(_stable) | set(_fast)) - set(PREDS) - {"topic"}))
check("no predicate is in two decay classes", not set(_stable) & set(_fast))
_table = _cfg().predicates.person
check("multi_valued is exactly the predicates the table marks multi",
      set(multi_valued()) == {p for p in PREDS if _table[p].cardinality == "multi"}
      and all(_table[p].cardinality in ("single", "multi") for p in PREDS))
check("opposites are mutual",
      all(opposites().get(b) == a for a, b in opposites().items()),
      str(opposites()))

# The table is config, so the ways it can be wrong are load errors, not surprises at
# 02:30 when the night's extraction runs.
from qqbot.settings import PredicateTable
_ok = {"verb": "喜欢", "cardinality": "multi", "rule": "喜欢的事物。"}
for _name, _bad in (
    ("a predicate with no rule", {"person": {"likes": {"verb": "喜欢",
                                                       "cardinality": "multi"}}}),
    ("a predicate with no verb", {"person": {"likes": {"cardinality": "multi",
                                                      "rule": "x"}}}),
    ("an unknown decay class", {"person": {"likes": _ok | {"decay": "slow"}}}),
    ("a one-sided opposite", {"person": {"likes": _ok | {"opposite": "dislikes"},
                                         "dislikes": _ok}}),
    ("an opposite that names nothing", {"person": {"likes": _ok | {"opposite": "nope"}}}),
    ("a predicate named like a group one", {"person": {"topic": _ok}}),
    ("a name the database could not index", {"person": {"Likes!": _ok}}),
):
    try:
        PredicateTable.model_validate(_bad)
        check(f"{_name} is refused at load", False, "it validated")
    except ValueError:
        check(f"{_name} is refused at load", True)

# ---- tool call -> candidate -----------------------------------------------
BATCH = uuid.uuid4()
SRC = tuple(SourceLine(event_id=uuid.uuid4(), text=t) for t in LINES)
inp = ExtractionInput(group_id=1, transcript="\n".join(LINES),
                      roster="老王[1]\n小北[2]", account_codes=CODES,
                      lines=SRC, source_event_id=BATCH, batch_size=len(SRC))


def call(name, **args):
    return {"function": {"name": name, "arguments": json.dumps(args, ensure_ascii=False)}}


parsed = MemoryExtractor._to_candidate(
    call("record_fact", account=1, predicate="plays", object="鸣潮", quote="我最近在玩鸣潮"),
    inp)
check("a tool call becomes a fact candidate",
      parsed and parsed.candidate_type is CandidateType.FACT
      and parsed.payload["object"] == "鸣潮")
check("a candidate carries its group", parsed.group_id == 1,
      "isolation: a candidate belongs to one group from the moment it exists")
# Two different ids, and both matter. One says which message backs this record; the other
# says which batch to validate it against, later, when the recent messages are a different
# set entirely.
check("a candidate cites the message its quote came from",
      parsed.source_event_id == SRC[0].event_id, str(parsed.source_event_id))
check("and the batch it has to be validated against",
      parsed.batch_event_id == BATCH, str(parsed.batch_event_id))
nowhere = MemoryExtractor._to_candidate(
    call("record_fact", account=1, predicate="plays", object="x", quote="没人说过这句"),
    inp)
check("a quote in no message gets no source, rather than a plausible one",
      nowhere.source_event_id is None, str(nowhere.source_event_id))
check("a quote that is only a speaker's name has no source",
      inp.source_of("老王") is None)
_twice = ExtractionInput(
    group_id=1, transcript="", roster="", account_codes=CODES, batch_size=len(SRC) + 1,
    lines=SRC + (SourceLine(event_id=uuid.uuid4(),
                            text=f"{sysmark('08-30 14:05')} 老王{sysmark('1')}: 老周你又来了"),))
check("a quote found in two messages has no source, rather than the first",
      _twice.source_of("老周你又来了") is None)
check("arguments that are JSON but not an object are dropped",
      MemoryExtractor._to_candidate(
          {"function": {"name": "record_fact", "arguments": "[1, 2]"}}, inp) is None)

group = MemoryExtractor._to_candidate(
    call("record_group_term", term="切片", meaning="把采样切成小段再重排",
         quote="切片就是把采样切成小段再重排"), inp)
check("a group tool becomes a group candidate",
      group and group.candidate_type is CandidateType.GROUP_FACT
      and group.payload["kind"] == "term", str(group))
topic = MemoryExtractor._to_candidate(
    call("record_group_topic", topic="做音乐的群", quote="这个群是做音乐的"), inp)
# Which tool was called is the only thing that tells the two apart; the payload alone
# cannot, and the consolidator needs to know which predicate to write.
check("and the two group tools are distinguishable afterwards",
      topic and topic.payload["kind"] == "topic", str(topic))

check("arguments that are not JSON are dropped",
      MemoryExtractor._to_candidate(
          {"function": {"name": "record_fact", "arguments": "{坏掉"}}, inp) is None)
check("an unknown tool is dropped",
      MemoryExtractor._to_candidate(call("drop_table", x=1), inp) is None)

print()
print("FAILED:", fails if fails else "none")
sys.exit(1 if fails else 0)
