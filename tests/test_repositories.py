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
from _db import configure_test_database

configure_test_database()

from qqbot.db import close_pool, init_pool, pool, repo
from qqbot.domain.ids import GroupId
from qqbot.domain.identity import (
    Alias,
    AliasEvidence,
    AliasStatus,
    AliasType,
    EvidenceType,
)
from qqbot.domain.memory import (
    Candidate,
    CandidateType,
    Episode,
    EpisodeType,
    ExtractionSnapshot,
    Fact,
    FactEvidence,
    MemoryType,
    RejectReason,
)
from qqbot.repositories import (
    EpisodeRepository,
    ExtractionRepository,
    IdentityLinkRepository,
    IdentityRepository,
    JobQueue,
    LinkChallengeError,
    MemoryRepository,
    VectorRepository,
)
from qqbot.repositories.job import JobType
from qqbot.services import IdentityLinkService, IdentityResolver
from qqbot.settings import config

fails = []
GROUP_A = GroupId("111")
GROUP_B = GroupId("222")


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


# The shared reset discovers tables from the catalog rather than keeping its own
# list: a hand-maintained TRUNCATE list drifts against dropped or added tables
# and the suite only passes by accident of the test database's age.
from _db import reset


async def an_event(group_id: GroupId) -> uuid.UUID:
    return await pool().fetchval(
        """INSERT INTO raw_event (platform, event_type, group_id, occurred_at, payload)
           VALUES ('qq','message',$1,NOW(),'{}'::jsonb) RETURNING id""",
        group_id.to_db(),
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
    check(
        "不同账号默认是不同的人", alt.entity_id != acc.entity_id, "自动合并出过错，所以默认不合并"
    )

    # ---- merge: the accounts move, the history does not ------------------
    await ids.merge(alt.entity_id, acc.entity_id)
    survived = await ids.entity(alt.entity_id)
    check("读被合并的实体会跳到存活的那个", survived and survived.id == acc.entity_id)
    both = await ids.accounts_of(acc.entity_id)
    check(
        "合并后两个账号都挂在同一个人名下", {a.platform_user_id for a in both} == {"1001", "1002"}
    )
    check(
        "被合并的实体不物理删除",
        await pool().fetchval("SELECT status FROM entity WHERE id=$1", alt.entity_id) == "merged",
    )

    # ---- source event for evidence-backed records ------------------------
    ev_id = await an_event(GROUP_A)

    # ---- exact-account rows survive union and detach ---------------------
    scope_main = await ids.ensure_account("qq", "scope-main", seen_at=now)
    scope_alt = await ids.ensure_account("qq", "scope-alt", seen_at=now)
    root, changed = await ids.merge_accounts(scope_main.id, scope_alt.id)
    check("account union is symmetric and changes two roots once", changed)
    linked_alt = await ids.account_by_id(scope_alt.id)
    check(
        "both exact accounts now share the deterministic root",
        linked_alt is not None and linked_alt.entity_id == root,
    )

    await ids.upsert_alias(
        Alias(
            alias_text="精确小号",
            target_entity_id=None,
            target_account_id=scope_alt.id,
            group_id=GROUP_A,
        ),
        [AliasEvidence(EvidenceType.MANUAL)],
    )
    await ids.upsert_alias(
        Alias(alias_text="整组称呼", target_entity_id=root, group_id=GROUP_A),
        [AliasEvidence(EvidenceType.MANUAL)],
    )
    await mem.supersede(
        Fact(
            subject_entity_id=None,
            subject_account_id=scope_alt.id,
            predicate="plays",
            object_value="虚构游戏",
            group_id=GROUP_A,
        ),
        [FactEvidence(ev_id)],
        when=now,
    )
    await mem.supersede(
        Fact(subject_entity_id=root, predicate="lives_in", object_value="苏州", group_id=GROUP_A),
        [FactEvidence(ev_id)],
        when=now,
    )
    await repo.block_holder(GROUP_A, root)
    check(
        "one holder rule resolves both currently linked accounts",
        await repo.blocked(GROUP_A, "scope-main") and await repo.blocked(GROUP_A, "scope-alt"),
    )

    rule_a = await ids.ensure_account("qq", "rule-a", seen_at=now)
    rule_b = await ids.ensure_account("qq", "rule-b", seen_at=now)
    await repo.block_holder(GROUP_A, rule_a.entity_id)
    await repo.block_holder(GROUP_A, rule_b.entity_id)
    rule_root, _ = await ids.merge_accounts(rule_a.id, rule_b.id)
    rule_rows = await repo.block_rules(GROUP_A)
    check(
        "merged holder rules render once at the current root",
        len([row for row in rule_rows if row["entity_id"] == rule_root]) == 1,
    )
    check(
        "rules created on either old root cover the merged holder",
        await repo.blocked(GROUP_A, "rule-a") and await repo.blocked(GROUP_A, "rule-b"),
    )
    check(
        "removing the current holder rule removes historical-root rules",
        await repo.unblock_holder(GROUP_A, rule_root)
        and not await repo.blocked(GROUP_A, "rule-a")
        and not await repo.blocked(GROUP_A, "rule-b"),
    )

    detached_root = await ids.split(scope_alt)
    check("split detaches only the selected exact account", detached_root != root)
    check(
        "holder rules follow current identity membership after split",
        await repo.blocked(GROUP_A, "scope-main") and not await repo.blocked(GROUP_A, "scope-alt"),
    )
    await repo.unblock_holder(GROUP_A, root)
    check(
        "the other account remains on the original holder",
        (await ids.account_by_id(scope_main.id)).entity_id == root,
    )
    check(
        "exact aliases follow their account after split",
        {alias.alias_text for alias in await ids.aliases_for_account(GROUP_A, scope_alt.id)}
        == {"精确小号"},
    )
    check(
        "holder aliases remain with the original holder",
        "整组称呼" in {alias.alias_text for alias in await ids.aliases_for(GROUP_A, root)}
        and "整组称呼"
        not in {alias.alias_text for alias in await ids.aliases_for(GROUP_A, detached_root)},
    )
    check(
        "exact facts follow their account after split",
        {fact.object_value for fact in await mem.current_account_facts(GROUP_A, [scope_alt.id])}
        == {"虚构游戏"},
    )
    check(
        "holder facts remain with the original holder",
        {fact.object_value for fact in await mem.current_entity_facts(GROUP_A, [root])} == {"苏州"}
        and not await mem.current_entity_facts(GROUP_A, [detached_root]),
    )

    # ---- durable two-account confirmation --------------------------------
    link_a = await ids.ensure_account("qq", "link-a", seen_at=now)
    link_b = await ids.ensure_account("qq", "link-b", seen_at=now)
    link_service = IdentityLinkService(
        config().default.identity_link,
        IdentityResolver(ids),
        ids,
        IdentityLinkRepository(),
    )
    challenge, code = await link_service.issue(
        group_id=GROUP_A,
        initiator_user_id="link-a",
        target_user_id="link-b",
        created_event_id=ev_id,
    )
    check(
        "a link challenge binds exact endpoint accounts",
        challenge.initiator_account_id == link_a.id and challenge.target_account_id == link_b.id,
    )
    confirm_event = await an_event(GROUP_A)
    applied = await link_service.confirm(
        group_id=GROUP_A,
        actor_user_id="link-b",
        code=code,
        confirmed_event_id=confirm_event,
    )
    check("target confirmation atomically applies the union", applied.status.value == "applied")
    check(
        "replaying the same target confirmation is idempotent",
        (
            await link_service.confirm(
                group_id=GROUP_A,
                actor_user_id="link-b",
                code=code,
                confirmed_event_id=confirm_event,
            )
        ).status.value
        == "applied",
    )
    check(
        "confirmed endpoints now resolve to one holder",
        (await ids.account_by_id(link_a.id)).entity_id
        == (await ids.account_by_id(link_b.id)).entity_id,
    )

    for user_id in ("limit-a", "limit-b", "limit-c"):
        await ids.ensure_account("qq", user_id, seen_at=now)
    limited_service = IdentityLinkService(
        config().default.identity_link.model_copy(update={"max_pending_per_account": 1}),
        IdentityResolver(ids),
        ids,
        IdentityLinkRepository(),
    )
    _pending, pending_code = await limited_service.issue(
        group_id=GROUP_A,
        initiator_user_id="limit-a",
        target_user_id="limit-b",
        created_event_id=await an_event(GROUP_A),
    )
    try:
        await limited_service.issue(
            group_id=GROUP_A,
            initiator_user_id="limit-c",
            target_user_id="limit-b",
            created_event_id=await an_event(GROUP_A),
        )
        target_capped = False
    except LinkChallengeError:
        target_capped = True
    check("the per-account challenge cap also protects target accounts", target_capped)
    await limited_service.cancel(
        group_id=GROUP_A,
        actor_user_id="limit-a",
        code=pending_code,
    )

    changing_a = await ids.ensure_account("qq", "changing-a", seen_at=now)
    changing_peer = await ids.ensure_account("qq", "changing-peer", seen_at=now)
    await ids.ensure_account("qq", "changing-b", seen_at=now)
    await ids.merge_accounts(changing_a.id, changing_peer.id)
    changing_challenge, changing_code = await link_service.issue(
        group_id=GROUP_A,
        initiator_user_id="changing-a",
        target_user_id="changing-b",
        created_event_id=await an_event(GROUP_A),
    )
    await ids.split(await ids.account_by_id(changing_peer.id))
    unchanged_endpoint = await ids.account_by_id(changing_a.id)
    try:
        await link_service.confirm(
            group_id=GROUP_A,
            actor_user_id="changing-b",
            code=changing_code,
            confirmed_event_id=await an_event(GROUP_A),
        )
        changed_rejected = False
    except LinkChallengeError:
        changed_rejected = True
    check(
        "a component change invalidates a challenge even when its endpoint root stays",
        changed_rejected and unchanged_endpoint.entity_id == changing_challenge.initiator_entity_id,
    )
    check(
        "identity-invalidated challenges are durably expired",
        await pool().fetchval(
            "SELECT status FROM account_link_challenge WHERE id=$1",
            changing_challenge.id,
        )
        == "expired",
    )

    # ---- names: evidence decides whether one may be used -----------------
    a = Alias(
        alias_text="老周",
        target_entity_id=acc.entity_id,
        group_id=GROUP_A,
        alias_type=AliasType.NICKNAME,
    )
    weak = await ids.upsert_alias(a, [AliasEvidence(EvidenceType.LLM_INFERENCE, ev_id)])
    check("模型推断只写成候选", weak.status is AliasStatus.CANDIDATE)
    check("候选查不出来", await ids.lookup(GROUP_A, "老周") == [], "lookup 只返回 confirmed")

    strong = await ids.upsert_alias(a, [AliasEvidence(EvidenceType.EXPLICIT_AT, ev_id)])
    check("补上一次显式 @ 就确认", strong.status is AliasStatus.CONFIRMED)
    hit = await ids.lookup(GROUP_A, "老周")
    check("确认后查得到", len(hit) == 1 and hit[0].target_entity_id == acc.entity_id)
    check(
        "同一个称呼不重复建行",
        await pool().fetchval("SELECT count(*) FROM alias WHERE normalized_text='老周'") == 1,
    )
    check(
        "证据两条都留着",
        await pool().fetchval(
            "SELECT count(*) FROM alias_evidence WHERE alias_id=$1",
            strong.id,
        )
        == 2,
    )

    # The per-message fast path: the platform re-reports the card on every message,
    # and a second same-day sighting must file no new trail row and run no rescore -
    # without this the trail grew two rows per message, and every message paid
    # aggregates over the whole trail.
    card = Alias(
        alias_text="老王本王",
        target_entity_id=acc.entity_id,
        group_id=GROUP_A,
        alias_type=AliasType.GROUP_CARD,
    )
    await ids.upsert_alias(card, [AliasEvidence(EvidenceType.GROUP_CARD, ev_id)])
    n_ev = await pool().fetchval("SELECT count(*) FROM alias_evidence")
    before = await pool().fetchval("SELECT last_used_at FROM alias WHERE alias_text='老王本王'")
    await ids.upsert_alias(card, [AliasEvidence(EvidenceType.GROUP_CARD, ev_id)])
    check(
        "同日重复上报的平台名不再堆证据行",
        await pool().fetchval("SELECT count(*) FROM alias_evidence") == n_ev,
    )
    check(
        "但 last_used_at 照常前移",
        await pool().fetchval("SELECT last_used_at FROM alias WHERE alias_text='老王本王'")
        >= before,
    )

    # ---- group isolation --------------------------------------------------
    check(
        "A 群的称呼在 B 群查不到", await ids.lookup(GROUP_B, "老周") == [], "隔离：称呼的作用域是群"
    )
    glob = Alias(alias_text="小X", target_entity_id=acc.entity_id, group_id=None)
    await ids.upsert_alias(glob, [AliasEvidence(EvidenceType.MANUAL)])
    check(
        "全局称呼在任何群都查得到",
        len(await ids.lookup(GROUP_A, "小X")) == 1 and len(await ids.lookup(GROUP_B, "小X")) == 1,
    )

    # ---- normalized forms still match -------------------------------------
    check("全角写法也查得到", len(await ids.lookup(GROUP_A, "老周")) == 1)

    # ---- facts: one current value, the old one closed out -----------------
    f1 = Fact(
        subject_entity_id=acc.entity_id,
        predicate="likes",
        object_value="原神",
        group_id=GROUP_A,
        memory_type=MemoryType.PREFERENCE,
        confidence=0.8,
    )
    await mem.supersede(f1, [FactEvidence(ev_id)], when=now)
    f2 = Fact(
        subject_entity_id=acc.entity_id,
        predicate="likes",
        object_value="鸣潮",
        group_id=GROUP_A,
        memory_type=MemoryType.PREFERENCE,
        confidence=0.9,
    )
    await mem.supersede(f2, [FactEvidence(ev_id)], when=now + dt.timedelta(days=1))

    cur = await mem.current_facts(GROUP_A, [acc.entity_id])
    check("同一个谓词只有一条当前事实", len(cur) == 1 and cur[0].object_value == "鸣潮")
    # Observed straight off the table: the invariant belongs to supersede(), and no
    # production code reads fact history, so there is no reader to go through.
    hist = await pool().fetch(
        """SELECT object_value, valid_to FROM memory_fact
            WHERE group_id=$1 AND subject_entity_id=$2 AND predicate='likes'""",
        GROUP_A.to_db(),
        acc.entity_id,
    )
    check(
        "旧事实没有被删除，只是盖了 valid_to",
        len(hist) == 2 and any("原神" in str(h["object_value"]) and h["valid_to"] for h in hist),
        "「他以前玩过吗」靠存档和这些行答",
    )

    await mem.supersede(f2, [FactEvidence(ev_id)], when=now + dt.timedelta(days=2))
    n_rows = await pool().fetchval(
        """SELECT count(*) FROM memory_fact
            WHERE group_id=$1 AND subject_entity_id=$2 AND predicate='likes'""",
        GROUP_A.to_db(),
        acc.entity_id,
    )
    check("重复确认同一个值不新增行", n_rows == 2, "群里每周都会重复说的事，不该堆成几十行")

    # ---- facts are group-scoped too ---------------------------------------
    fb = Fact(
        subject_entity_id=acc.entity_id,
        predicate="likes",
        object_value="别的",
        group_id=GROUP_B,
        confidence=0.7,
    )
    await mem.supersede(fb, [], when=now)
    check(
        "B 群的事实不影响 A 群的当前值",
        (await mem.current_facts(GROUP_A, [acc.entity_id]))[0].object_value == "鸣潮",
    )
    check("A 群读不到 B 群的事实", len(await mem.current_facts(GROUP_A, [acc.entity_id])) == 1)

    # ---- candidates -------------------------------------------------------
    extraction_id = await pool().fetchval(
        """INSERT INTO memory_extraction (group_id, status)
           VALUES ($1,'extracting') RETURNING id""",
        GROUP_A.to_db(),
    )
    await pool().execute(
        """INSERT INTO memory_extraction_event
               (extraction_id, raw_event_id, ordinal) VALUES ($1,$2,1)""",
        extraction_id,
        ev_id,
    )
    cand = Candidate(
        candidate_type=CandidateType.ALIAS,
        payload={"alias": "狗王"},
        group_id=GROUP_A,
        source_event_id=ev_id,
        extraction_id=extraction_id,
        confidence=0.3,
    )
    extractions = ExtractionRepository()
    await extractions.stage(extraction_id, ExtractionSnapshot((), ()), [cand])
    check("候选进的是候选表", len(await extractions.candidates(extraction_id)) == 1)
    await mem.settle(cand.rejected(RejectReason.AMBIGUOUS_ALIAS))
    check(
        "否掉之后不再是待处理",
        len(
            [
                item
                for item in await extractions.candidates(extraction_id)
                if item.status.value == "pending"
            ]
        )
        == 0,
    )
    check(
        "否掉的理由留档",
        await pool().fetchval("SELECT reject_reason FROM memory_candidate LIMIT 1")
        == "ambiguous_alias",
    )

    # ---- episodes ---------------------------------------------------------
    ep = Episode(
        group_id=GROUP_A,
        summary="讨论了买哪把键盘",
        episode_type=EpisodeType.DISCUSSION,
        importance=0.7,
        extraction_id=extraction_id,
        event_ids=(ev_id,),
    )
    await eps.add(ep)
    recent = await eps.recent_active(GROUP_A, limit=10)
    check("近期事件按群查得回", len(recent) == 1 and "键盘" in recent[0].summary)
    check("别的群查不到", not await eps.recent_active(GROUP_B, limit=10))
    check(
        "事件保留归纳批次与原始来源",
        await pool().fetchval("SELECT extraction_id FROM episode WHERE id=$1", ep.id)
        == extraction_id
        and await pool().fetchval("SELECT count(*) FROM episode_event WHERE episode_id=$1", ep.id)
        == 1,
    )

    # ---- vectors ----------------------------------------------------------
    vec = VectorRepository("test-embed", 1)
    v1 = [0.0] * 2048
    v1[0] = 1.0
    v2 = [0.0] * 2048
    v2[1] = 1.0
    check(
        "活动事件可安全写入向量",
        await vec.put_episode(group_id=GROUP_A, episode_id=ep.id, embedding=v1),
    )
    near = await vec.search(group_id=GROUP_A, object_type="episode", embedding=v1)
    check("向量查得回自己", len(near) == 1 and near[0][0] == ep.id)
    far = await vec.search(group_id=GROUP_A, object_type="episode", embedding=v2)
    check(
        "不够接近就返回空，不硬凑一个", far == [], "检索的失败应该是「没找到」而不是「找了个像的」"
    )
    check(
        "向量检索也按群隔离",
        await vec.search(group_id=GROUP_B, object_type="episode", embedding=v1) == [],
    )
    ep2 = Episode(group_id=GROUP_A, summary="没有向量的另一件事")
    await eps.add(ep2)
    todo = await vec.unembedded_episodes(GROUP_A, limit=200)
    check("能问出哪些事件还没算向量", [t[0] for t in todo] == [ep2.id], str(todo))
    check(
        "换嵌入模型后旧向量不算数，全部待补",
        len(await VectorRepository("new-embed", 1).unembedded_episodes(GROUP_A, limit=200)) == 2,
    )

    old = Episode(
        group_id=GROUP_A,
        summary="很久以前讨论过旧设备",
        started_at=now - dt.timedelta(days=120),
        ended_at=now - dt.timedelta(days=120),
        event_ids=(ev_id,),
    )
    await eps.add(old)
    old_model = VectorRepository("old-embed", 1)
    check(
        "旧事件在过期前仍可建立所有模型的投影",
        await vec.put_episode(group_id=GROUP_A, episode_id=old.id, embedding=v1)
        and await old_model.put_episode(group_id=GROUP_A, episode_id=old.id, embedding=v2),
    )
    check("别的群的衰减不碰它", await eps.decay(GROUP_B, ttl_days=90) == 0)
    check("超过保留期的事件会退出活动记忆", await eps.decay(GROUP_A, ttl_days=90) == 1)
    old_row = await pool().fetchrow("SELECT status, revision FROM episode WHERE id=$1", old.id)
    check(
        "过期留下状态和修订记录",
        old_row["status"] == "expired" and old_row["revision"] == 2,
        str(dict(old_row)),
    )
    check(
        "过期事件不再从任何活动读取路径返回",
        old.id not in {e.id for e in await eps.recent_active(GROUP_A, limit=20)}
        and not await eps.by_ids(GROUP_A, [old.id])
        and not await eps.around(GROUP_A, [old.id], 1),
    )
    check(
        "过期事件不再等待向量补全",
        old.id not in {eid for eid, _ in await vec.unembedded_episodes(GROUP_A, limit=200)},
    )
    check(
        "过期删除所有模型版本的派生向量",
        await pool().fetchval("SELECT count(*) FROM embedding_index WHERE object_id=$1", old.id)
        == 0,
    )
    check(
        "但事件的原始证据仍然保留",
        await pool().fetchval("SELECT count(*) FROM episode_event WHERE episode_id=$1", old.id)
        == 1,
    )
    check(
        "晚到的嵌入不能复活过期事件向量",
        not await vec.put_episode(group_id=GROUP_A, episode_id=old.id, embedding=v1)
        and await pool().fetchval("SELECT count(*) FROM embedding_index WHERE object_id=$1", old.id)
        == 0,
    )
    check(
        "新事件的向量不受旧事件衰减影响",
        bool(await vec.search(group_id=GROUP_A, object_type="episode", embedding=v1)),
    )

    # ---- the job queue ----------------------------------------------------
    q = JobQueue("worker-1")
    await q.submit(JobType.EMBED, {"object_id": str(ep.id)}, priority=5)
    await q.submit(JobType.DECAY, {}, priority=1)
    first = await q.claim()
    check("优先级高的先出队", first and first.job_type is JobType.EMBED)
    second = await JobQueue("worker-2").claim()
    check("另一个 worker 拿到的是另一件活", second and second.id != first.id, "SKIP LOCKED")
    await q.done(first.id)
    check("做完的不再被取到", (await q.depth()).get("done") == 1)

    # done rows are pure history and get purged after 30 days; dead rows stay.
    await pool().execute(
        "UPDATE memory_job SET finished_at = NOW() - INTERVAL '40 days' WHERE status='done'"
    )
    check("过期的已完成作业会被清掉", await q.purge_done(days=30) == 1)
    check("清完队列里不再有 done", not (await q.depth()).get("done"))

    # fail() now demands the failing worker still holds the job (the locked_by
    # guard), so the queue that claimed it is the one that reports the failure.
    await JobQueue("worker-2").fail(second, "boom", backoff=dt.timedelta(seconds=-1))
    retried = await JobQueue("worker-3").claim()
    check("失败的会被重试", retried and retried.id == second.id and retried.retry_count == 1)
    # The retried job is currently claimed; put it back, or nothing can take it until its
    # lease expires.
    await JobQueue("worker-3").fail(retried, "boom", backoff=dt.timedelta(seconds=-1))
    # Drain until empty, with a ceiling so a bug here cannot loop forever.
    for _ in range(retried.max_retry + 3):
        got = await JobQueue("w").claim()
        if got is None:
            break
        await JobQueue("w").fail(got, "boom", backoff=dt.timedelta(seconds=-1))
    depth = await q.depth()
    check(
        "重试次数用尽后停下",
        depth.get("dead", 0) >= 1,
        f"{depth}　无限重试会变成一台永动的烧钱机器",
    )

    # A running job whose twin was legally re-submitted (the dedupe index only sees
    # pending rows) must not crash fail(): demoting it back to pending would collide
    # with the twin, and an exception escaping step() leaves the job stuck in
    # 'running' with no error recorded.
    qa = JobQueue("twin-a")
    await qa.submit(JobType.EXTRACT_MEMORY, {"group_id": 42})
    running = await qa.claim()
    twin_id = await qa.submit(JobType.EXTRACT_MEMORY, {"group_id": 42})
    check("a twin may be submitted while the first runs", twin_id is not None)
    await qa.fail(running, "boom", backoff=dt.timedelta(seconds=60))
    yielded = await pool().fetchval("SELECT last_error FROM memory_job WHERE id=$1", running.id)
    check(
        "the failed twin steps aside instead of crashing",
        yielded is not None and yielded.startswith("yielded to a newer pending twin"),
        str(yielded),
    )
    check(
        "and the fresh twin still holds the pending slot",
        await pool().fetchval("SELECT status FROM memory_job WHERE id=$1", twin_id) == "pending",
    )

    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
