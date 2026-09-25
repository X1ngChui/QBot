"""Scored memory visibility without granting candidate names identity authority."""

import asyncio
from datetime import timedelta
import os
from pathlib import Path
import sys
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ["CONFIG_DIR"] = str(ROOT / "tests" / "fixtures" / "config")
from _db import configure_test_database, reset

configure_test_database()
from qqbot.core import prompt, retrieval
from qqbot.db import close_pool, init_pool, pool
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import GroupId
from qqbot.domain.identity import Alias, AliasEvidence, AliasType, EvidenceType
from qqbot.domain.memory import Candidate, CandidateType, Fact
from qqbot.gateway.ingest import Ingestor
from qqbot.gateway.onebot import GroupMessage, Sender
from qqbot.repositories import ExtractionRepository, IdentityRepository, MemoryRepository
from qqbot.repositories.archive import ArchiveRepository
from qqbot.services import IdentityResolver
from qqbot.services.memory_consolidator import Validator
from qqbot.settings import config
from qqbot.util import now_local
from qqbot.workers.memory import MemoryWorker

G = GroupId("8107")
fails = []
ids = IdentityRepository()
mem = MemoryRepository()
directory = retrieval.build_directory(identities=ids)
ingestor = Ingestor(IdentityResolver(ids))


def check(name, condition):
    print(f"[{'ok ' if condition else 'FAIL'}] {name}")
    if not condition:
        fails.append(name)


async def say(key, user, text, *, segments=None, own=False):
    event = GroupMessage(
        message_id=key,
        group_id=G,
        sender=Sender(user_id=user, nickname={"a": "成员甲", "b": "成员乙"}.get(user, user)),
        segments=segments or [{"type": "text", "data": {"text": text}}],
        self_id="999",
        occurred_at=now_local(),
        plain_text=text,
        author_kind=AuthorKind.BOT if own else AuthorKind.MEMBER,
    )
    return await ingestor.ingest(event)


async def main():
    await init_pool()
    await reset()
    event = await say("hint-a", "a", "我喜欢摄影")
    await say("hint-b", "b", "蓝帆住在青岛")
    a = await ids.account_of("qq", "a")
    low = await mem.supersede(
        Fact(subject_entity_id=None, subject_account_id=a.id, group_id=G,
             predicate="likes", object_key="摄影", object_value="摄影", confidence=0.2),
        [], when=now_local(),
    )
    high = await mem.supersede(
        Fact(subject_entity_id=None, subject_account_id=a.id, group_id=G,
             predicate="lives_in", object_value="青岛", confidence=0.95),
        [], when=now_local(),
    )
    await ids.upsert_alias(
        Alias("蓝帆", None, target_account_id=a.id, group_id=G),
        [AliasEvidence(EvidenceType.LLM_INFERENCE, event.raw_event_id)],
    )
    await directory.name(G, "a", "白舟")
    await directory.note(G, "a", "只在周末参加活动")
    roster = await retrieval.gather(group_id=G, directory=directory)
    a_row = next(row for row in roster if row["user_id"] == "a")
    hints = "\n".join(a_row["memory_hints"])
    check("low-score facts and names are both visible with their scores",
          "事实：喜欢摄影（置信度 0.20）" in hints
          and "未确认别名：蓝帆（置信度 0.30）" in hints)
    check("stale unconfirmed platform names stay scored rather than trusted",
          "未确认显示名：成员甲" in hints and a_row["nickname"] == "成员")
    live = next(card for card in await directory.roster(G, display={"a": "成员甲"})
                if card.user_id == "a")
    check("a live platform display is not repeated as a candidate hint",
          live.display == "成员甲"
          and not any("成员甲" in hint for hint in live.memory_hints))
    check("confirmed aliases stay separate from candidate context",
          a_row["aliases"] == ["白舟"] and "白舟" not in hints)
    check("high-score automatic facts remain hints", "青岛（置信度 0.95）" in hints)
    developer = prompt.build_developer(config().persona_for(G), roster)
    trusted, untrusted = developer.split("未确认线索（可能有误或已过时）：", 1)
    check("notes stay trusted and scores are not promoted to trusted claims",
          "只在周末参加活动" in trusted and "白舟" in trusted
          and "成员甲" not in trusted and "成员甲" in untrusted
          and "青岛" not in trusted and "蓝帆" not in trusted
          and "青岛" in untrusted and "蓝帆" in untrusted)
    check("candidate aliases cannot resolve accounts",
          not await ids.lookup(G, "蓝帆")
          and "蓝帆" not in [name for _, name in await ids.account_names(G)])
    await ids.upsert_alias(
        Alias("a", None, target_account_id=a.id, group_id=G),
        [AliasEvidence(EvidenceType.LLM_INFERENCE, event.raw_event_id)],
    )
    numeric = next(row for row in await retrieval.gather(group_id=G, directory=directory)
                   if row["user_id"] == "a")
    check("a candidate matching the account ID cannot label the trusted roster",
          numeric["nickname"] == "成员"
          and any("未确认别名：a（" in hint for hint in numeric["memory_hints"]))

    # Only projection methods are exercised; no provider may be called by this suite.
    worker = MemoryWorker(config().default, SimpleNamespace(
        text=None, embedding=SimpleNamespace(name="visibility-test")
    ))
    rows = await ArchiveRepository().recent(G, limit=100)
    codes, account_roster, lines = await worker._render(G, rows)
    known = await worker._known(G, codes)
    check("extraction sees candidate context outside its confirmed account roster",
          "未确认别名：蓝帆（置信度 0.30）" in known and "蓝帆" not in account_roster)
    check("extraction retains the fact score and the reliable note",
          "事实：likes = 摄影（置信度 0.20）" in known and "备注：只在周末参加活动" in known)
    target_line = next(line for line in lines if "蓝帆住在青岛" in line.evidence_text)
    check("a visible candidate name cannot add a line-local alias target",
          all(target.account_id != a.id for target in target_line.targets))
    codes_only, _, _ = await worker._render(G, [rows[-1]])
    check("a candidate name alone cannot extend the account shortlist",
          a.id not in codes_only.values())

    await say("hint-at", "b", "@成员甲 住在青岛", segments=[
        {"type": "at", "data": {"qq": "a", "name": "成员甲"}},
        {"type": "text", "data": {"text": " 住在青岛"}},
    ])
    await say("hint-repeat", "b", "@成员甲 蓝帆，你照片呢", segments=[
        {"type": "at", "data": {"qq": "a", "name": "成员甲"}},
        {"type": "text", "data": {"text": " 蓝帆，你照片呢"}},
    ])
    await say("hint-bot", "999", "蓝帆住在青岛", own=True)
    batch = await ExtractionRepository().claim(G, limit=100, floor=0, gap=timedelta())
    codes, _, _, snapshot = await worker._render_snapshot(batch)
    code_a = next(code for code, account in codes.items() if account == a.id)
    validator = Validator(snapshot)
    for key, expected in (("hint-b", False), ("hint-at", True), ("hint-bot", False)):
        row = next(event for event in batch.events if str(event.message_id) == key)
        source = next(line for line in snapshot.lines if line.event_id == row.raw_event_id)
        candidate = Candidate(
            CandidateType.FACT,
            {"source": source.ordinal, "quote": source.evidence_text or row.text,
             "account": code_a, "predicate": "lives_in", "object": "青岛"},
            source_event_id=source.event_id,
        )
        check(f"source validation preserves identity and evidence boundaries for {key}",
              validator.check(candidate).ok is expected)
    repeat = next(event for event in batch.events if str(event.message_id) == "hint-repeat")
    source = next(line for line in snapshot.lines if line.event_id == repeat.raw_event_id)
    reuse = Candidate(
        CandidateType.ALIAS,
        {"source": source.ordinal, "quote": source.evidence_text, "account": code_a,
         "alias": "蓝帆", "kind": AliasType.NICKNAME.value},
        source_event_id=source.event_id,
    )
    check("independently targeted candidate reuse is valid fresh alias evidence",
          validator.check(reuse).ok)

    await directory.set_confidence(G, "a", "蓝帆", 0.8)
    promoted = next(row for row in await retrieval.gather(group_id=G, directory=directory)
                    if row["user_id"] == "a")
    check("confirmation invalidates the cached hint and promotes only the alias",
          "蓝帆" in promoted["aliases"]
          and not any("蓝帆" in hint for hint in promoted["memory_hints"]))
    await directory.set_confidence(G, "a", "蓝帆", 0.2)
    demoted = next(row for row in await retrieval.gather(group_id=G, directory=directory)
                   if row["user_id"] == "a")
    check("manual demotion preserves visibility but removes identity authority",
          "蓝帆" not in demoted["aliases"]
          and "未确认别名：蓝帆（置信度 0.20）" in demoted["memory_hints"]
          and not await ids.lookup(G, "蓝帆"))
    await directory.unname(G, "a", "蓝帆")
    await mem.retract(low.id)
    await mem.supersede(
        Fact(subject_entity_id=None, subject_account_id=a.id, group_id=G,
             predicate="lives_in", object_value="南京", confidence=0.27),
        [], when=now_local(),
    )
    after = next(row for row in await retrieval.gather(group_id=G, directory=directory)
                 if row["user_id"] == "a")
    check("retracted and superseded records disappear rather than remaining as hints",
          all(word not in "\n".join(after["memory_hints"])
              for word in ("蓝帆", "摄影", "青岛"))
          and await pool().fetchval("SELECT status FROM memory_fact WHERE id=$1", high.id)
          == "superseded")

    await ids.upsert_alias(
        Alias("已结束称呼", None, target_account_id=a.id, group_id=G),
        [AliasEvidence(EvidenceType.LLM_INFERENCE, event.raw_event_id)],
    )
    await pool().execute(
        "UPDATE alias SET valid_to=NOW(), updated_at=NOW() "
        "WHERE group_id=$1 AND target_account_id=$2 AND alias_text='已结束称呼'",
        G.to_db(), a.id,
    )
    check("closed aliases are excluded from exact and holder views",
          all(name.alias_text != "已结束称呼" for name in await ids.aliases_for_account(G, a.id))
          and all(name.alias_text != "已结束称呼"
                  for name in await ids.aliases_for(G, a.entity_id)))

    alt = await ids.ensure_account("qq", "alt", seen_at=now_local())
    root, _ = await ids.merge_accounts(a.id, alt.id)
    await ids.upsert_alias(
        Alias("小号专属线索", None, target_account_id=alt.id, group_id=G),
        [AliasEvidence(EvidenceType.LLM_INFERENCE, event.raw_event_id)],
    )
    await ids.upsert_alias(
        Alias("共享线索", root, group_id=G, alias_type=AliasType.NICKNAME),
        [AliasEvidence(EvidenceType.LLM_INFERENCE, event.raw_event_id)],
    )
    scoped = await worker._known(G, {1: a.id})
    check("holder hints keep scope without copying another exact account's names",
          "关联集合共享：未确认别名：共享线索" in scoped and "小号专属线索" not in scoped)
    check("the hint projection does not promote candidates or create fresh evidence",
          not await ids.lookup(G, "共享线索")
          and await pool().fetchval(
              "SELECT count(*) FROM alias_evidence e JOIN alias a ON a.id=e.alias_id "
              "WHERE a.group_id=$1 AND a.alias_text='共享线索'", G.to_db()
          ) == 1)
    await close_pool()
    print("FAILED:", fails or "none")
    return bool(fails)


raise SystemExit(asyncio.run(main()))
