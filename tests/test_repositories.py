"""The repository layer, against a real PostgreSQL rather than a mock.

The invariants here - one current fact, the scope a name holds in, group isolation - are
guaranteed by SQL and by transactions, so testing them against fakes tests nothing. Only
a real database will tell you whether the unique index was actually built the way you
think it was.
"""
import asyncio
import datetime as dt
import os
import pathlib
import sys
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
os.environ.setdefault("DATABASE_URL", "postgresql://qqbot@127.0.0.1:15432/qqbot")
os.environ.setdefault("DATABASE_PASSWORD", "testpw")

from qqbot.db import close_pool, init_pool, pool  # noqa: E402
from qqbot.domain.identity import (  # noqa: E402
    Alias, AliasEvidence, AliasStatus, AliasType, EvidenceType,
)
from qqbot.domain.memory import (  # noqa: E402
    Candidate, CandidateType, Episode, EpisodeType, Fact, FactEvidence, MemoryType,
    Participant, RejectReason,
)
from qqbot.repositories import (  # noqa: E402
    EpisodeRepository, IdentityRepository, JobQueue, MemoryRepository, VectorRepository,
)
from qqbot.repositories.job import JobType  # noqa: E402

fails = []
GROUP_A = 111
GROUP_B = 222


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


# The shared reset discovers tables from the catalog rather than keeping its own
# list: a hand-maintained TRUNCATE list drifts against dropped or added tables
# and the suite only passes by accident of the test database's age.
from _db import reset  # noqa: E402


async def an_event(group_id: int) -> uuid.UUID:
    return await pool().fetchval(
        """INSERT INTO raw_event (platform, event_type, group_id, occurred_at, payload)
           VALUES ('qq','message',$1,NOW(),'{}'::jsonb) RETURNING id""",
        group_id,
    )


async def main() -> int:
    await init_pool()
    await reset()
    ids, mem, eps = IdentityRepository(), MemoryRepository(), EpisodeRepository()
    now = dt.datetime.now(dt.UTC)

    # ---- a first sighting gives the account an owner ---------------------
    acc = await ids.ensure_account("qq", "1001", seen_at=now, name="老王")
    check("第一次见到账号就建人", acc.entity_id is not None)
    again = await ids.ensure_account("qq", "1001", seen_at=now)
    check("再见到同一个账号不重复建人", again.entity_id == acc.entity_id)

    alt = await ids.ensure_account("qq", "1002", seen_at=now, name="老王小号")
    check("不同账号默认是不同的人", alt.entity_id != acc.entity_id,
          "自动合并出过错，所以默认不合并")

    # ---- merge: the accounts move, the history does not ------------------
    await ids.merge(alt.entity_id, acc.entity_id)
    survived = await ids.entity(alt.entity_id)
    check("读被合并的实体会跳到存活的那个", survived and survived.id == acc.entity_id)
    both = await ids.accounts_of(acc.entity_id)
    check("合并后两个账号都挂在同一个人名下", {a.platform_user_id for a in both} == {"1001", "1002"})
    check("被合并的实体不物理删除",
          await pool().fetchval("SELECT status FROM entity WHERE id=$1", alt.entity_id)
          == "merged")

    # ---- names: evidence decides whether one may be used -----------------
    ev_id = await an_event(GROUP_A)
    a = Alias(alias_text="老周", target_entity_id=acc.entity_id, group_id=GROUP_A,
              alias_type=AliasType.NICKNAME)
    weak = await ids.upsert_alias(a, [AliasEvidence(EvidenceType.LLM_INFERENCE, ev_id)])
    check("模型推断只写成候选", weak.status is AliasStatus.CANDIDATE)
    check("候选查不出来", await ids.lookup(GROUP_A, "老周") == [],
          "lookup 只返回 confirmed")

    strong = await ids.upsert_alias(a, [AliasEvidence(EvidenceType.EXPLICIT_AT, ev_id)])
    check("补上一次显式 @ 就确认", strong.status is AliasStatus.CONFIRMED)
    hit = await ids.lookup(GROUP_A, "老周")
    check("确认后查得到", len(hit) == 1 and hit[0].target_entity_id == acc.entity_id)
    check("同一个称呼不重复建行",
          await pool().fetchval("SELECT count(*) FROM alias WHERE normalized_text='老周'") == 1)
    check("证据两条都留着",
          await pool().fetchval("SELECT count(*) FROM alias_evidence") == 2)

    # The per-message fast path: the platform re-reports the card on every message,
    # and a second same-day sighting must file no new trail row and run no rescore -
    # without this the trail grew two rows per message, and every message paid
    # aggregates over the whole trail.
    card = Alias(alias_text="老王本王", target_entity_id=acc.entity_id, group_id=GROUP_A,
                 alias_type=AliasType.GROUP_CARD)
    await ids.upsert_alias(card, [AliasEvidence(EvidenceType.GROUP_CARD, ev_id)])
    n_ev = await pool().fetchval("SELECT count(*) FROM alias_evidence")
    before = await pool().fetchval(
        "SELECT last_used_at FROM alias WHERE alias_text='老王本王'")
    await ids.upsert_alias(card, [AliasEvidence(EvidenceType.GROUP_CARD, ev_id)])
    check("同日重复上报的平台名不再堆证据行",
          await pool().fetchval("SELECT count(*) FROM alias_evidence") == n_ev)
    check("但 last_used_at 照常前移",
          await pool().fetchval(
              "SELECT last_used_at FROM alias WHERE alias_text='老王本王'") >= before)

    # ---- group isolation --------------------------------------------------
    check("A 群的称呼在 B 群查不到", await ids.lookup(GROUP_B, "老周") == [],
          "隔离：称呼的作用域是群")
    glob = Alias(alias_text="小X", target_entity_id=acc.entity_id, group_id=None)
    await ids.upsert_alias(glob, [AliasEvidence(EvidenceType.MANUAL)])
    check("全局称呼在任何群都查得到",
          len(await ids.lookup(GROUP_A, "小X")) == 1
          and len(await ids.lookup(GROUP_B, "小X")) == 1)

    # ---- normalized forms still match -------------------------------------
    check("全角写法也查得到", len(await ids.lookup(GROUP_A, "老周")) == 1)

    # ---- facts: one current value, the old one closed out -----------------
    f1 = Fact(subject_entity_id=acc.entity_id, predicate="likes", object_value="原神",
              group_id=GROUP_A, memory_type=MemoryType.PREFERENCE, confidence=0.8)
    await mem.supersede(f1, [FactEvidence(ev_id)], when=now)
    f2 = Fact(subject_entity_id=acc.entity_id, predicate="likes", object_value="鸣潮",
              group_id=GROUP_A, memory_type=MemoryType.PREFERENCE, confidence=0.9)
    await mem.supersede(f2, [FactEvidence(ev_id)], when=now + dt.timedelta(days=1))

    cur = await mem.current_facts(GROUP_A, [acc.entity_id])
    check("同一个谓词只有一条当前事实", len(cur) == 1 and cur[0].object_value == "鸣潮")
    # Observed straight off the table: the invariant belongs to supersede(), and the
    # dedicated history reader it once had shipped no consumer.
    hist = await pool().fetch(
        """SELECT object_value, valid_to FROM memory_fact
            WHERE group_id=$1 AND subject_entity_id=$2 AND predicate='likes'""",
        GROUP_A, acc.entity_id)
    check("旧事实没有被删除，只是盖了 valid_to",
          len(hist) == 2 and any("原神" in str(h["object_value"]) and h["valid_to"]
                                 for h in hist),
          "「他以前玩过吗」靠存档和这些行答")

    await mem.supersede(f2, [FactEvidence(ev_id)], when=now + dt.timedelta(days=2))
    n_rows = await pool().fetchval(
        """SELECT count(*) FROM memory_fact
            WHERE group_id=$1 AND subject_entity_id=$2 AND predicate='likes'""",
        GROUP_A, acc.entity_id)
    check("重复确认同一个值不新增行", n_rows == 2,
          "群里每周都会重复说的事，不该堆成几十行")

    # ---- facts are group-scoped too ---------------------------------------
    fb = Fact(subject_entity_id=acc.entity_id, predicate="likes", object_value="别的",
              group_id=GROUP_B, confidence=0.7)
    await mem.supersede(fb, [], when=now)
    check("B 群的事实不影响 A 群的当前值",
          (await mem.current_facts(GROUP_A, [acc.entity_id]))[0].object_value == "鸣潮")
    check("A 群读不到 B 群的事实",
          len(await mem.current_facts(GROUP_A, [acc.entity_id])) == 1)

    # ---- candidates -------------------------------------------------------
    cand = Candidate(candidate_type=CandidateType.ALIAS, payload={"alias": "狗王"},
                     group_id=GROUP_A, source_event_id=ev_id, confidence=0.3)
    await mem.stage([cand])
    check("候选进的是候选表", len(await mem.pending(GROUP_A)) == 1)
    await mem.settle(cand.rejected(RejectReason.AMBIGUOUS_ALIAS))
    check("否掉之后不再是待处理", len(await mem.pending(GROUP_A)) == 0)
    check("否掉的理由留档",
          await pool().fetchval("SELECT reject_reason FROM memory_candidate LIMIT 1")
          == "ambiguous_alias")

    # ---- episodes ---------------------------------------------------------
    ep = Episode(group_id=GROUP_A, summary="讨论了买哪把键盘",
                 episode_type=EpisodeType.DISCUSSION, importance=0.7,
                 participants=(Participant(entity_id=acc.entity_id, role="推荐者"),),
                 event_ids=(ev_id,))
    await eps.add(ep)
    mine = await eps.involving(GROUP_A, acc.entity_id)
    check("按参与者查得到事件", len(mine) == 1 and "键盘" in mine[0].summary)
    check("别的群查不到", len(await eps.involving(GROUP_B, acc.entity_id)) == 0)

    # ---- vectors ----------------------------------------------------------
    vec = VectorRepository("test-embed", 1)
    v1 = [0.0] * 2048
    v1[0] = 1.0
    v2 = [0.0] * 2048
    v2[1] = 1.0
    await vec.put(group_id=GROUP_A, object_type="episode", object_id=ep.id, embedding=v1)
    near = await vec.search(group_id=GROUP_A, object_type="episode", embedding=v1)
    check("向量查得回自己", len(near) == 1 and near[0][0] == ep.id)
    far = await vec.search(group_id=GROUP_A, object_type="episode", embedding=v2)
    check("不够接近就返回空，不硬凑一个", far == [],
          "检索的失败应该是「没找到」而不是「找了个像的」")
    check("向量检索也按群隔离",
          await vec.search(group_id=GROUP_B, object_type="episode", embedding=v1) == [])
    ep2 = Episode(group_id=GROUP_A, summary="没有向量的另一件事")
    await eps.add(ep2)
    todo = await vec.unembedded_episodes(GROUP_A)
    check("能问出哪些事件还没算向量", [t[0] for t in todo] == [ep2.id], str(todo))
    check("换嵌入模型后旧向量不算数，全部待补",
          len(await VectorRepository("new-embed", 1).unembedded_episodes(GROUP_A)) == 2)

    # ---- the job queue ----------------------------------------------------
    q = JobQueue("worker-1")
    await q.submit(JobType.EMBED, {"object_id": str(ep.id)}, priority=5)
    await q.submit(JobType.DECAY, {}, priority=1)
    first = await q.claim()
    check("优先级高的先出队", first and first.job_type is JobType.EMBED)
    second = await JobQueue("worker-2").claim()
    check("另一个 worker 拿到的是另一件活",
          second and second.id != first.id, "SKIP LOCKED")
    await q.done(first.id)
    check("做完的不再被取到", (await q.depth()).get("done") == 1)

    # done rows are pure history and get purged after 30 days; dead rows stay.
    await pool().execute(
        "UPDATE memory_job SET finished_at = NOW() - INTERVAL '40 days' "
        "WHERE status='done'")
    check("过期的已完成作业会被清掉", await q.purge_done() == 1)
    check("清完队列里不再有 done", not (await q.depth()).get("done"))

    # fail() now demands the failing worker still holds the job (the locked_by
    # guard), so the queue that claimed it is the one that reports the failure.
    await JobQueue("worker-2").fail(second, "boom", backoff=dt.timedelta(seconds=-1))
    retried = await JobQueue("worker-3").claim()
    check("失败的会被重试", retried and retried.id == second.id and retried.retry_count == 1)
    # The retried job is currently claimed; put it back, or nothing can take it until its
    # lease expires.
    await JobQueue("worker-3").fail(retried, "boom", backoff=dt.timedelta(seconds=-1))
    for _ in range(retried.max_retry + 3):     # Drain until empty, with a ceiling so a bug here cannot loop forever.
        got = await JobQueue("w").claim()
        if got is None:
            break
        await JobQueue("w").fail(got, "boom", backoff=dt.timedelta(seconds=-1))
    depth = await q.depth()
    check("重试次数用尽后停下", depth.get("dead", 0) >= 1,
          f"{depth}　无限重试会变成一台永动的烧钱机器")

    # A running job whose twin was legally re-submitted (the dedupe index only sees
    # pending rows) must not crash fail(): demoting it back to pending would collide
    # with the twin, and an exception escaping step() leaves the job stuck in
    # 'running' with no error recorded.
    qa = JobQueue("twin-a")
    await qa.submit(JobType.CONSOLIDATE, {"group_id": 42})
    running = await qa.claim()
    twin_id = await qa.submit(JobType.CONSOLIDATE, {"group_id": 42})
    check("a twin may be submitted while the first runs", twin_id is not None)
    await qa.fail(running, "boom", backoff=dt.timedelta(seconds=60))
    yielded = await pool().fetchval(
        "SELECT last_error FROM memory_job WHERE id=$1", running.id)
    check("the failed twin steps aside instead of crashing",
          yielded is not None and yielded.startswith("yielded to a newer pending twin"),
          str(yielded))
    check("and the fresh twin still holds the pending slot",
          await pool().fetchval(
              "SELECT status FROM memory_job WHERE id=$1", twin_id) == "pending")

    # /relearn's force flag must reach whichever job actually runs: a submit
    # collapsed by the dedup index amends the pending twin instead of vanishing.
    qa2 = JobQueue("amend")
    await qa2.submit(JobType.EXTRACT_MEMORY, {"group_id": 77}, priority=9)
    check("a collapsed submit can amend its pending twin",
          await qa2.amend_pending(JobType.EXTRACT_MEMORY, 77, {"force": True}))
    got2 = await qa2.claim()
    check("and the flag rides the job that runs",
          got2 is not None and got2.payload.get("force") is True,
          str(got2 and got2.payload))

    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
