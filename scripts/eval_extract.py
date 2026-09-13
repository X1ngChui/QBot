"""Behavioural evals for the extraction path, run against the real model.

The Validator guards form - verbatim quotes, legal codes - and nothing guards
behaviour: whether a joke stays out of memory, a known fact stays unrepeated, a
known alias keeps earning its confirmations. This is that guard, run before and
after any change to the extract prompt family or the text model, exactly like
eval_replies for the reply path. One batch, one model call per run.

Verdicts per assertion: PASS / FAIL; the full candidate list is printed for
human eyes either way.

Usage (workstation, test DB up, real keys in .env):
    docker start qbot-pgtest
    .venv/Scripts/python.exe scripts/eval_extract.py
"""

import asyncio
import os
import pathlib
import re
import sys
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "config"))
os.environ["DATABASE_URL"] = "postgresql://qqbot@127.0.0.1:15432/qqbot"
os.environ["DATABASE_PASSWORD"] = "testpw"

if not (ROOT / ".env").exists():
    sys.exit("eval makes real model calls and needs credentials: "
             "create .env at the repo root (see .env.example)")
from _env import load_dotenv

load_dotenv(ROOT / ".env")

from qqbot.db import close_pool, init_pool
from qqbot.providers import build_default, set_providers
from qqbot.services import ExtractionInput, MemoryExtractor
from qqbot.services.memory_extractor import SourceLine
from qqbot.settings import config
from qqbot.workers.memory import transcript_legend

#: Same eval-only group as eval_replies, so the ledger rows stay attributable.
GROUP = 424242

ROSTER = "\n".join([
    "王大锤⟦1⟧（也叫：老王）",
    "小红⟦2⟧",
    "陈其⟦3⟧",
])

KNOWN = "\n".join([
    "本群：",
    "- term 切片 = 把采样切成小段再重排",
    "⟦1⟧：lives_in = 苏州；备注：本名王小锤，只在周末上线",
    "已记过的事：",
    "- 王大锤答应周末把切片做完",
    # An infected prior episode, planted on purpose: editorial style once written
    # comes back through the known block every night, and the model must not
    # copy it (the failure mode is a summary that grows a tail of editorial
    # commentary on the previous summary).
    "- 小红和陈其向王大锤连环提问考他的知识，属又一轮对老王记忆能力的测试",
])

#: (time, code-rendered line). Every behaviour under test hangs on one line:
#: a moved city (a real change, must re-record), a known alias reused (must
#: re-record - confirmations are how names live), a joke identity (must not
#: become anything), a known term restated (must not be re-recorded), and the
#: owner's note deliberately echoed by nobody (nothing may derive from it).
LINES = [
    "⟦09-07 21:00⟧ 王大锤⟦1⟧: 我上个月搬到无锡了，现在每天通勤半小时",
    "⟦09-07 21:01⟧ 小红⟦2⟧: 老王你搬家了怎么不早说",
    "⟦09-07 21:02⟧ 王大锤⟦1⟧: 就是换了个住处，工作没变",
    # The boundaries the newer predicates draw, each against the one it would
    # otherwise leak into: a school already finished (not studies_at), a city
    # lived in before (not lives_in, which the same batch changes to a new city), an
    # account that has to arrive in the platform:id shape whatever the sentence said, and a
    # name the person asks for (not an alias somebody else used).
    "⟦09-07 21:02⟧ 王大锤⟦1⟧: 我临江大学毕业好几年了，现在早不读书了",
    "⟦09-07 21:02⟧ 王大锤⟦1⟧: 之前在成都住过三年，那边冬天湿冷",
    "⟦09-07 21:02⟧ 王大锤⟦1⟧: 我微博是 @dachui2020，有事艾特我",
    "⟦09-07 21:02⟧ 王大锤⟦1⟧: 以后你们叫我锤子就行，别喊全名",
    "⟦09-07 21:03⟧ 陈其⟦3⟧: 我是秦始皇，你们都得听我的",
    "⟦09-07 21:04⟧ 小红⟦2⟧: 哈哈哈哈陛下饶命",
    "⟦09-07 21:05⟧ 小红⟦2⟧: 切片就是把采样切成小段再重排嘛，这个我们早说过了",
    # A concrete, recordable event: a plan with a time, a place and a taker -
    # the episode the style assertions below inspect.
    "⟦09-07 21:20⟧ 陈其⟦3⟧: 这周六晚上老地方聚餐，都能来吧",
    "⟦09-07 21:21⟧ 小红⟦2⟧: 能，我来订位子，就订七点的",
    "⟦09-07 21:22⟧ 王大锤⟦1⟧: 行，周六我在",
    # The bot addressed by its own trigger name (SELF_NAMES below): production
    # showed the extractor filing its own name as a member's alias when nobody
    # told it whose name that is.
    "⟦09-07 21:30⟧ 陈其⟦3⟧: 阿旺你说周六吃什么好",
    # The bot's own line, marked as such: readable for coherence, and nothing
    # may derive from or quote it (the own flag also blocks it mechanically).
    "⟦09-07 21:31⟧ 阿旺⟦你⟧: 那我周六早点到，帮你们占座，我头像是只柴犬",
]

#: What the worker passes as ExtractionInput.self_names in production.
SELF_NAMES = "阿旺、旺财"


def main_checks(cands: list) -> list[tuple[str, bool, str]]:
    """(label, passed, detail) per deterministic assertion."""
    def payloads(kind=None):
        return [c.payload for c in cands
                if kind is None or c.payload.get("_tool") == kind]

    alias_ok = any(p.get("alias") == "老王" and p.get("account") == 1
                   for p in payloads("record_alias"))
    joke = [p for p in payloads() if "秦始皇" in str(p)]
    note = [p for p in payloads() if "王小锤" in str(p) or "周末上线" in str(p)]
    term = [p for p in payloads("record_group_term")
            if p.get("term") == "切片"]
    moved = any(p.get("account") == 1 and p.get("predicate") == "lives_in"
                and "无锡" in str(p.get("object", ""))
                for p in payloads("record_fact"))
    codes_ok = all(isinstance(p.get("account"), int) and p["account"] in (1, 2, 3)
                   for p in payloads() if "account" in p)
    episodes = [str(p.get("summary", "")) for p in payloads("record_episode")]
    dinner = any("聚餐" in s or "周六" in s for s in episodes)
    # The infected known episode plants the style; a clean summary neither
    # editorialises (calling it a test or an ordeal) nor reaches outside its own batch
    # (calling it another round).
    styled = [s for s in episodes
              if re.search(r"又一轮|考验|属.{0,10}(测试|考察)", s)]
    self_named = [p for p in payloads("record_alias")
                  if p.get("alias") in ("阿旺", "旺财")]
    own_derived = [p for p in payloads()
                   if "占座" in str(p) or "柴犬" in str(p)]
    facts = payloads("record_fact")

    def objects(pred):
        return [str(p.get("object", "")) for p in facts if p.get("predicate") == pred]

    grad, still = objects("graduated_from"), objects("studies_at")
    lived, live = objects("lived_in"), objects("lives_in")
    handles = objects("handle_on")
    called = objects("preferred_name")
    return [
        ("a reused known alias is re-recorded (confirmation evidence)",
         alias_ok, ""),
        ("a finished degree is a graduation, not an enrolment",
         any("临江" in o for o in grad) and not any("临江" in o for o in still),
         f"graduated_from={grad} studies_at={still}"),
        ("a city lived in before does not overwrite the current one",
         any("成都" in o for o in lived) and not any("成都" in o for o in live),
         f"lived_in={lived} lives_in={live}"),
        ("an account arrives as platform and id, however it was said",
         any("微博" in o and "dachui2020" in o for o in handles), str(handles)),
        ("a name somebody asks for is a preference, not an alias",
         any("锤子" in o for o in called), str(called)),
        ("a joke identity produces nothing", not joke, str(joke)),
        ("nothing derives from the owner's note", not note, str(note)),
        ("a known term restated is not re-recorded", not term, str(term)),
        ("a changed fact is re-recorded (he moved)", moved, ""),
        ("every account reference is a legal code", codes_ok, ""),
        ("a concrete plan becomes an episode", dinner, str(episodes)),
        ("no episode copies the infected editorial style", not styled,
         str(styled)),
        ("the bot's own name never becomes a member's alias", not self_named,
         str(self_named)),
        ("nothing derives from the bot's own marked line", not own_derived,
         str(own_derived)),
    ]


async def main() -> int:
    set_providers(build_default())
    await init_pool()

    from qqbot.core.budget import BUDGET
    spent0 = await BUDGET.spent_today()
    cfg = config().default
    extractor = MemoryExtractor(cfg, legend=transcript_legend())
    lines = tuple(SourceLine(event_id=uuid.uuid4(), text=t, own="⟦你⟧" in t)
                  for t in LINES)
    inp = ExtractionInput(
        group_id=GROUP, transcript="\n".join(LINES), roster=ROSTER,
        account_codes={1: uuid.uuid4(), 2: uuid.uuid4(), 3: uuid.uuid4()},
        lines=lines, batch_size=len(lines), known=KNOWN,
        self_names=SELF_NAMES,
    )
    cands = await extractor.extract(inp)
    # The tool name decides how a payload is read; carry it beside the payload
    # the way the consolidator would learn it.
    for c in cands:
        kind = {"ALIAS": "record_alias", "FACT": "record_fact",
                "GROUP_FACT": "record_group_term"
                if c.payload.get("kind") == "term" else "record_group_topic",
                "EPISODE": "record_episode"}[c.candidate_type.name]
        c.payload["_tool"] = kind

    print("candidates the model proposed:")
    for c in cands:
        print("  -", dict(c.payload))
    print()

    fails = []
    for label, ok, detail in main_checks(cands):
        print(f"[{'PASS' if ok else 'FAIL'}] {label}  {detail}")
        if not ok:
            fails.append(label)

    print(f"\nspend this run: CNY {await BUDGET.spent_today() - spent0:.4f} "
          "(booked to the test ledger)")
    print("RESULT:", "ok" if not fails else f"FAIL ({len(fails)} assertion(s))")
    await close_pool()
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
