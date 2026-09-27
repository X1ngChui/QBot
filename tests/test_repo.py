"""What db/repo.py still owns, against a real postgres.

Everything about people moved to the repositories package and is covered by
test_repositories.py. What is left here is the group-level plumbing: the L0 archive, the
image cache, the group switches and the cost ledger.

Raw events are written through the Ingestor rather than by an INSERT here, because that
is now the only way they are written in production - a test with its own INSERT would
keep passing after the real path broke.
"""

import pytest


import itertools


from _test_owners import fresh_budget
import _db as _test_db
from _db import pool
from qqbot.gateway.ingest import Ingestor
from qqbot.repositories import IdentityRepository
from qqbot.services import IdentityResolver
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import GroupId
from qqbot.gateway.onebot import GroupMessage, Sender
from _fixtures import now_local, today_local
from _db import reset

G1 = GroupId("7001")
G2 = GroupId("7002")
_SEQ = itertools.count(1)
_INGESTOR: Ingestor | None = None


@pytest.fixture
def repo_ingestor(test_database, monkeypatch):
    monkeypatch.setitem(globals(), "_SEQ", itertools.count(1))
    monkeypatch.setitem(
        globals(),
        "_INGESTOR",
        Ingestor(
            IdentityResolver(IdentityRepository(database=_test_db.pool, clock=_test_db.clock)),
            database=_test_db.pool,
        ),
    )


async def say(group, uid, name, text, *, msg_id=None, at=None):
    """One message through the real append-once inbound path."""
    mid = msg_id or f"auto{next(_SEQ)}"
    await _INGESTOR.ingest(
        GroupMessage(
            message_id=mid,
            group_id=group,
            sender=Sender(user_id=uid, card=name),
            segments=[],
            self_id="999",
            occurred_at=at or now_local(),
            plain_text=text,
        )
    )
    return mid


@pytest.mark.database
@pytest.mark.asyncio
async def test_repo(repo_ingestor, monkeypatch):
    BUDGET = fresh_budget()
    await reset()

    # -- raw events ---------------------------------------------------------
    await say(G1, "u1", "阿强", "hi", msg_id="m1")
    await say(G1, "u1", "阿强", "dup", msg_id="m1")  # a replay after a reconnect
    n = await pool().fetchval("SELECT count(*) FROM raw_event WHERE platform_event_id='m1'")
    assert n == 1, ("a replayed message id does not land twice", f"{n} rows")
    original = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id='m1'"
    )
    assert original == "hi", ("a replay cannot mutate the admitted row", repr(original))

    class BrokenIdentity:
        async def seen(self, *_args, **_kwargs):
            raise RuntimeError("identity projection failed")

    broken = Ingestor(BrokenIdentity(), database=_test_db.pool)
    try:
        await broken.ingest(
            GroupMessage(
                message_id="atomic-failure",
                group_id=G1,
                sender=Sender(user_id="broken-member", card="失败成员"),
                segments=[],
                self_id="999",
                occurred_at=now_local(),
                plain_text="不会留下半条事件",
            )
        )
    except RuntimeError:
        pass
    else:
        pytest.fail("identity failure did not propagate from admission")
    assert (
        await pool().fetchval(
            "SELECT count(*) FROM raw_event WHERE platform_event_id='atomic-failure'"
        )
        == 0
    ), "identity failure rolls back the raw event append"

    await _test_db.archive.backfill_plain_text("m1", "hi [图片:一只猫]")
    got = await pool().fetchval("SELECT plain_text FROM raw_event WHERE platform_event_id='m1'")
    assert got == "hi [图片:一只猫]", ("backfill_plain_text rewrites the reading", str(got))
    segs = await pool().fetchval(
        "SELECT payload->'segments' FROM raw_event WHERE platform_event_id='m1'"
    )
    assert segs is not None, ("and leaves the original segments in place", str(segs))

    # -- image cache --------------------------------------------------------
    assert await _test_db.media_cache.image_cache_get("k1") is None, "image_cache miss"
    await _test_db.media_cache.image_cache_put("k1", "[表情:开心]")
    assert await _test_db.media_cache.image_cache_get("k1") == "[表情:开心]", "image_cache hit"
    await _test_db.media_cache.image_cache_get("k1")
    hits = await pool().fetchval("SELECT hit_count FROM image_cache WHERE key='k1'")
    assert hits == 2, ("image_cache counts hits", str(hits))
    stats = await _test_db.media_cache.image_cache_stats()
    assert stats["n"] == 1 and stats["hits"] == 2, ("image_cache_stats", str(dict(stats)))

    # A description ages out so a better model gets to write it again: the paid
    # describing path asks for one no older than the configured window and treats
    # anything older as a miss. Free paths ask for no age at all - they cannot pay
    # to replace what they reject, and a stale description beats a bare marker.
    from datetime import timedelta as _td

    assert (
        await _test_db.media_cache.image_cache_get("k1", max_age=_td(days=15)) == "[表情:开心]"
    ), "a fresh description satisfies the paid path"
    await pool().execute(
        "UPDATE image_cache SET described_at = now() - interval '20 days' WHERE key='k1'"
    )
    assert await _test_db.media_cache.image_cache_get("k1", max_age=_td(days=15)) is None, (
        "an aged one is a miss there, so it gets described again"
    )
    assert await _test_db.media_cache.image_cache_get("k1") == "[表情:开心]", (
        "but the free path still serves it"
    )
    # A description always carries its time: the schema refuses one without it.
    try:
        await pool().execute("UPDATE image_cache SET described_at = NULL WHERE key='k1'")
        pytest.fail("schema accepted a description without a timestamp")
    except Exception as e:
        assert "image_cache_described_stamped" in str(e), (
            "a description without a time is refused by the schema",
            str(e)[:80],
        )
    # Re-describing stamps the row afresh, which is what ends the expiry.
    await _test_db.media_cache.image_cache_put("k1", "[表情:开心，重描]")
    assert (
        await _test_db.media_cache.image_cache_get("k1", max_age=_td(days=15))
        == "[表情:开心，重描]"
    ), "a rewrite is current again"

    # A picture the backend's content filter declined is stored as a placeholder, and
    # marked as one: it describes nothing, and a climbing count of them says the
    # backend is turning away more than it looks at. It expires like any other, so a
    # different backend - or the same one with different rules - gets to look again.
    await _test_db.media_cache.image_cache_put("k-refused", "⟦图片⟧", refused=True)
    _st = await _test_db.media_cache.image_cache_stats()
    assert _st["refused"] == 1, ("a refusal is stored as a refusal", str(dict(_st)))
    await _test_db.media_cache.image_cache_put("k-refused", "⟦图片:一只猫⟧")
    _st = await _test_db.media_cache.image_cache_stats()
    assert _st["refused"] == 0, ("and stops being one once the backend does look", str(dict(_st)))

    # The upload half can land before the describing half, and neither may clobber the
    # other: a row with only a file_id reads as an undescribed picture, and the later
    # description fills the same row in.
    await _test_db.media_cache.image_cache_set_file("k2", "file-api-abc", provider="deepseek")
    assert await _test_db.media_cache.image_cache_get("k2") is None, (
        "a file_id alone is not a description hit"
    )
    await _test_db.media_cache.image_cache_put("k2", "[图片:占位测试]")
    assert await _test_db.media_cache.image_cache_get("k2") == "[图片:占位测试]", (
        "the description fills the same row"
    )
    assert (
        await _test_db.media_cache.image_cache_file("k2", provider="deepseek") == "file-api-abc"
    ), "and the file_id survives it"
    assert await _test_db.media_cache.image_cache_file("k2", provider="openai_responses") is None, (
        "a file handle never crosses provider identity"
    )
    # The backend expires files, so an id is only trusted while young: one older
    # than the window (or stamped before the upload time was recorded) reads as
    # absent and the picture is uploaded again.
    from datetime import timedelta as _td_f

    assert (
        await _test_db.media_cache.image_cache_file(
            "k2", provider="deepseek", max_age=_td_f(days=1)
        )
        == "file-api-abc"
    ), "a fresh file_id is trusted inside the age window"
    await pool().execute(
        "UPDATE image_cache SET file_uploaded_at = now() - interval '2 days' WHERE key='k2'"
    )
    assert (
        await _test_db.media_cache.image_cache_file(
            "k2", provider="deepseek", max_age=_td_f(days=1)
        )
        is None
    ), "an old file_id reads as absent"
    await pool().execute("UPDATE image_cache SET file_uploaded_at = NULL WHERE key='k2'")
    assert (
        await _test_db.media_cache.image_cache_file(
            "k2", provider="deepseek", max_age=_td_f(days=1)
        )
        is None
    ), "an unstamped file_id reads as absent too"
    assert (
        await _test_db.media_cache.image_cache_file("k2", provider="deepseek") == "file-api-abc"
    ), "and without an age it is still returned as a hint"
    assert await _test_db.media_cache.image_cache_file("k1", provider="deepseek") is None, (
        "a picture never uploaded has no file_id"
    )

    # -- group_state and dynamic block rules -------------------------------
    assert not await _test_db.groups.group_muted(G1) and not await _test_db.groups.block_rules(
        G1
    ), "a group with no rows reads as all-off"
    await _test_db.groups.set_group_muted(G1, True)
    await _test_db.groups.block(G1, "u9")
    await _test_db.groups.block(G1, "u8")
    await _test_db.groups.block(G1, "u9")  # Replacing one exact rule keeps one row.
    rules = await _test_db.groups.block_rules(G1)
    assert (
        await _test_db.groups.group_muted(G1)
        and {row["user_id"] for row in rules} == {"u8", "u9"}
        and await _test_db.groups.blocked(G1, "u9")
    ), "switches and exact rules round-trip"
    assert await _test_db.groups.unblock(G1, "u9") and not await _test_db.groups.unblock(
        G1, "u9"
    ), "unblocking reports whether anything changed"
    await _test_db.groups.set_group_muted(G1, False)
    await _test_db.groups.unblock(G1, "u8")
    assert not await _test_db.groups.group_muted(G1) and not await _test_db.groups.block_rules(
        G1
    ), "and can be cleared"
    await _test_db.groups.set_group_muted(G2, True)
    assert {G1, G2} <= set(await _test_db.groups.groups_with_state()), (
        "groups_with_state lists every group that has a row"
    )
    # From the table, not the in-memory registry: the daily report must list a
    # group muted before the last restart and quiet ever since.
    assert await _test_db.groups.muted_groups() == [G2], "muted_groups reads the persisted flag"

    # -- exact extraction admission -----------------------------------------
    from datetime import timedelta as _td
    from qqbot.repositories import ExtractionRepository as _ER
    from qqbot.repositories import JobQueue as _JQ
    from qqbot.repositories.job import JobType as _JT

    G3 = GroupId("7003")
    extraction = _ER(database=_test_db.pool)
    n0 = await extraction.unconsumed_count(G3)
    assert n0 == 0, ("a new group has no unconsumed archive events", str(n0))
    for i in range(4):
        await say(G3, "u1", "阿强", f"第 {i} 句")
    n1 = await extraction.unconsumed_count(G3)
    assert n1 == 4, ("messages arrive unconsumed", str(n1))

    await _JQ("t", pool).submit(_JT.EXTRACT_MEMORY, {"group_id": G3}, priority=1)
    n2 = await extraction.unconsumed_count(G3)
    assert n2 == 4, ("queueing a pass claims no events", str(n2))

    claimed = await extraction.claim(
        G3,
        limit=10,
        floor=1,
        gap=_td(minutes=30),
    )
    assert len(claimed.events) == 4 and await extraction.unconsumed_count(G3) == 0, (
        "claiming records the exact event set"
    )
    reopened = await extraction.open(G3)
    assert [event.raw_event_id for event in reopened.events] == [
        event.raw_event_id for event in claimed.events
    ], "an open extraction reloads the same ordered ids"
    await say(G3, "u1", "阿强", "稍后才到")
    assert await extraction.unconsumed_count(G3) == 1, "a later event stays outside the open batch"

    # -- cost ledger --------------------------------------------------------
    day = today_local()
    await BUDGET.ledger.ledger_add(
        group_id=G1,
        kind="reply",
        model="deepseek-v4-flash",
        in_hit=1000,
        in_miss=200,
        out=80,
        cny=0.00036,
    )
    await BUDGET.ledger.ledger_add(
        group_id=G1,
        kind="extract",
        model="deepseek-v4-flash",
        in_hit=800,
        in_miss=10,
        out=8,
        cny=0.000042,
    )
    await BUDGET.ledger.ledger_add(group_id=None, kind="search", model="search_std", cny=0.01)
    total = await BUDGET.ledger.day_cost(day)
    assert abs(total - 0.010402) < 1e-6, ("day_cost sums", f"{total:.6f}")
    bd = await BUDGET.ledger.day_breakdown(day)
    assert len(bd) == 3 and bd[0]["kind"] == "search", (
        "day_breakdown groups",
        str([r["kind"] for r in bd]),
    )
    # Without a group id the ledger covers every group, which is what the shared cap is
    # measured against - the budget is not per-group.
    one = await BUDGET.ledger.day_breakdown(day, G1)
    assert {r["kind"] for r in one} == {"reply", "extract"}, (
        "a per-group breakdown leaves out the shared rows",
        str([r["kind"] for r in one]),
    )

    # -- the search allowance, metered off the ledger ------------------------
    # The search backend is free within a monthly credit allowance, and the ledger's
    # call count is the meter itself: no separate counter to drift, and a refusal at
    # the allowance that never reaches the vendor.
    import httpx as _hx
    from qqbot.providers.tavily import TavilySearch
    from qqbot.providers.base import QuotaExhausted, RetryPolicy
    from _fixtures import config as _config

    monkeypatch.setenv("SEARCH_API_KEY", "tvly-test-key")
    base_search = _config().default.backends.search
    from dataclasses import replace
    from qqbot.providers.contracts import SearchOptions

    search_options = SearchOptions(base_search.count, base_search.depth)
    seen_reqs = []

    def _fake_tavily(req):
        seen_reqs.append(req)
        if req.url.path == "/extract":
            return _hx.Response(
                200,
                json={
                    "results": [{"url": "https://a.example/page", "raw_content": "  正文   开头  "}]
                },
            )
        return _hx.Response(
            200,
            json={
                "results": [
                    {"title": "t1", "url": "https://a.example/1", "content": "  spaced   out  "},
                    {"title": "t2", "url": "https://a.example/2", "content": "two"},
                ]
            },
        )

    async def tavily(quota: int) -> TavilySearch:
        cfg = base_search.model_copy(update={"monthly_quota": quota})
        search = TavilySearch(cfg, RetryPolicy(0, 30), budget=BUDGET)
        await search._client.aclose()
        search._client = _hx.AsyncClient(transport=_hx.MockTransport(_fake_tavily))
        return search

    ts = await tavily(1)
    spent_before = await BUDGET.ledger.day_cost(day)
    items = await ts.search("天气 上海", options=search_options, group_id=G1)
    req = seen_reqs[0]
    assert (
        req.headers.get("authorization", "").startswith("Bearer tvly-")
        and b"search_depth" in req.content
        and req.url.path == "/search"
    ), "the request carries the key and the query"
    assert items[0] == {"title": "t1", "link": "https://a.example/1", "content": "spaced out"}, (
        "results are normalised to title/link/content",
        str(items[:1]),
    )
    assert (
        await BUDGET.ledger.month_calls("search", "tavily") == 1
        and abs(await BUDGET.ledger.day_cost(day) - spent_before) < 1e-9
    ), "a free call books a row but no money"
    assert await BUDGET.ledger.month_calls("search", "search_std") >= 1, (
        "rows of another backend do not eat the allowance"
    )

    with pytest.raises(QuotaExhausted):
        await ts.search("再来一次", options=search_options, group_id=G1)
    assert len(seen_reqs) == 1, "and the refusal never reached the vendor"
    await ts.aclose()

    # Advanced depth debits two vendor credits per call, and the meter counts what
    # the vendor counts - metered by calls, the real 1000 would be gone at ~500
    # while the meter read half-full, and every search past that would fail as a
    # transport error instead of the clean quota silence.
    advanced = replace(search_options, depth="advanced")
    ts = await tavily(2)
    with pytest.raises(QuotaExhausted):
        await ts.search("只剩一个 credit", options=advanced, group_id=G1)
    assert len(seen_reqs) == 1, "credit-aware refusal never reaches the vendor"
    await ts.aclose()

    ts = await tavily(100)
    await ts.search("深度搜一次", options=advanced, group_id=G1)
    assert await BUDGET.ledger.month_calls("search", "tavily") == 3, (
        "an advanced search books two credits",
        str(await BUDGET.ledger.month_calls("search", "tavily")),
    )

    # Page extraction rides the same allowance: same key, same proxy, same meter,
    # same refusal at the ceiling.
    text = await ts.read_page("https://a.example/page", group_id=G1)
    assert text == "正文 开头", ("extract returns the page text normalised", repr(text))
    assert await BUDGET.ledger.month_calls("search", "tavily") == 4, (
        "and debits the shared allowance",
        str(await BUDGET.ledger.month_calls("search", "tavily")),
    )
    await ts.aclose()

    ts = await tavily(4)
    with pytest.raises(QuotaExhausted):
        await ts.read_page("https://a.example/page", group_id=G1)
    await ts.aclose()

    # -- Responses failures and served-model billing -------------------------
    # Exercise the real executor/session and replace only the network transport.
    from qqbot.providers.contracts import (
        CallContext,
        CallPurpose,
        GenerationPolicy,
        Message,
        ModelFailure,
        ModelRequest,
        ReasoningEffort,
        Role,
    )
    from qqbot.providers.openai_responses import OpenAIResponses
    from qqbot.providers.openai_transport import TerminalResponse

    class FakeTransport:
        def __init__(self, *steps):
            self.steps = list(steps)
            self.calls = 0

        async def complete(self, request, **kwargs):
            del request, kwargs
            self.calls += 1
            step = self.steps.pop(0)
            if isinstance(step, BaseException):
                raise step
            return TerminalResponse(step, str(step.get("status") or "completed"))

        async def aclose(self):
            pass

    tcfg = _config().default.backends.text.model_copy(deep=True)

    async def model_call(transport, *, retries=0, max_tokens=None):
        model = OpenAIResponses(tcfg, RetryPolicy(0, 30), budget=BUDGET)
        await model._executor._transport.aclose()
        model._executor._transport = transport
        request = ModelRequest(
            prompt=(Message(Role.USER, "你好"),),
            tools=(),
            policy=GenerationPolicy(
                model=tcfg.model,
                reasoning=ReasoningEffort.OFF,
                timeout_sec=tcfg.timeout_sec,
                retries=retries,
                max_output_tokens=max_tokens,
            ),
            context=CallContext(CallPurpose.REPLY, G1),
        )
        try:
            async with model.open_session(request) as session:
                return await session.start()
        finally:
            await model.aclose()

    before_to = await BUDGET.ledger.day_cost(day)
    with pytest.raises(ModelFailure):
        await model_call(FakeTransport(TimeoutError()), max_tokens=100)
    after_to = await BUDGET.ledger.day_cost(day)
    assert after_to > before_to, (
        "and its estimated spend reaches the ledger",
        f"{before_to:.6f} -> {after_to:.6f}",
    )

    transient_transport = FakeTransport(
        {
            "model": "transient-model",
            "status": "failed",
            "output": [],
            "error": {"code": "server_error", "message": "try again"},
            "usage": {"input_tokens": 2, "output_tokens": 1},
        },
        {
            "model": "transient-model",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "recovered"}],
                }
            ],
            "usage": {"input_tokens": 2, "output_tokens": 1},
        },
    )
    recovered = await model_call(transient_transport, retries=1)
    assert transient_transport.calls == 2 and recovered.text == "recovered", (
        "a retryable terminal failure follows the configured retry loop",
        f"calls={transient_transport.calls} text={recovered.text!r}",
    )

    before_failed = await BUDGET.ledger.day_cost(day)
    with pytest.raises(ModelFailure):
        await model_call(
            FakeTransport(
                {
                    "model": "failed-no-usage",
                    "status": "failed",
                    "output": [],
                    "error": {"code": "invalid_request_error", "message": "bad request"},
                }
            ),
            max_tokens=10,
        )
    assert await BUDGET.ledger.day_cost(day) > before_failed, (
        "a failed response without usage is conservatively booked"
    )

    await model_call(
        FakeTransport(
            {
                "model": "served-elsewhere",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "好"}],
                    }
                ],
                "usage": {"input_tokens": 10, "output_tokens": 5},
            }
        )
    )
    _models = {r["model"] for r in await BUDGET.ledger.day_breakdown(day)}
    assert "served-elsewhere" in _models, (
        "a routed call books under the model that served it",
        str(sorted(_models)),
    )

    # -- the window rebuilt from the archive --------------------------------
    # A deque that starts empty on deploy has the bot rejoin conversations it was
    # part of thirty seconds earlier knowing nothing - while the archive holds
    # every message, its own replies included. So the window comes back from the
    # archive.
    from qqbot.conversation.state import GroupState

    await say(G1, "u1", "阿强", "昨天的图在这 https://x.example/cat.jpg")
    await say(G1, "u2", "阿花", "收到了")
    await _INGESTOR.ingest(
        GroupMessage(
            message_id="bot-r1",
            group_id=G1,
            sender=Sender(user_id="999", nickname="小X"),
            segments=[{"type": "text", "data": {"text": "我也看看"}}],
            self_id="999",
            occurred_at=now_local(),
            plain_text="我也看看",
            outbound_schema=1,
            author_kind=AuthorKind.BOT,
        )
    )

    rows = await _test_db.archive.recent(G1, limit=50)
    _stamps = [message.occurred_at for message in rows]
    assert _stamps == sorted(_stamps), ("recent_messages returns oldest first", "")
    assert not any(
        message
        for message in await _test_db.archive.recent(G2, limit=50)
        if "阿强" in message.sender.display_name or "阿强" in message.text
    ), ("and only this group's", "")

    st = GroupState(
        group_id=G1,
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    await st.load_history(self_id="999", owners={"u1"})
    texts = [m.text for m in st.recent]
    assert "收到了" in texts and any("cat.jpg" in t for t in texts), (
        "the window is rebuilt from the archive",
        str(texts[-4:]),
    )
    by_text = {m.text: m for m in st.recent}
    assert by_text["我也看看"].is_bot and by_text["我也看看"].nickname == "小X", (
        "the bot's own replies come back marked as its own"
    )
    assert by_text["收到了"].is_owner is False and any(
        m.is_owner for m in st.recent if m.user_id == "u1"
    ), "and an owner comes back marked as an owner"
    n_before = len(st.recent)
    await st.load_history(self_id="999", owners=set())
    assert len(st.recent) == n_before, "loading twice does not double the window"

    # A line that reached the window before the first chat message must not stand in
    # for the whole archive: the rebuild merges behind it rather than skipping.
    from qqbot.conversation.state import ChatMsg as _CMsg

    st_early = GroupState(
        group_id=G1,
        groups=_test_db.groups,
        archive=_test_db.archive,
        display_zone=_test_db.clock.zone,
    )
    st_early.add(
        _CMsg(
            msg_id="cmd-early",
            user_id="999",
            nickname="小X",
            text="（控制台回答）",
            ts=now_local(),
            is_bot=True,
        )
    )
    await st_early.load_history(self_id="999", owners=set())
    _ids_early = [m.msg_id for m in st_early.recent]
    assert "收到了" in [m.text for m in st_early.recent] and "cmd-early" in _ids_early, (
        "a window seeded before the rebuild still gets the archive",
        str(len(_ids_early)),
    )
    assert _ids_early[-1] == "cmd-early", "and the early line stays newest, after the archived ones"

    # -- the archive, searched ----------------------------------------------
    # The pull half of context: the prompt pushes a fixed window, and everything behind
    # it was unreachable - a link posted yesterday might as well not have existed.
    from qqbot.conversation.tools import search_history

    hit = await search_history(
        G1, "cat.jpg", database=_test_db.pool, identities=_test_db.identities, clock=_test_db.clock
    )
    assert "https://x.example/cat.jpg" in hit, ("the archive answers a keyword", hit)
    assert "阿强" in hit and "]" in hit, ("with when and who", hit)
    assert "没有搜到" in await search_history(
        G1,
        "阿强 不存在的词",
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    ), "all keywords must hit, not any"
    assert "没有搜到" in await search_history(
        G2,
        "cat.jpg",
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    ), "another group's archive is out of reach"
    assert "没有搜到" in await search_history(
        G1, "%", database=_test_db.pool, identities=_test_db.identities, clock=_test_db.clock
    ), ("LIKE pattern characters are literal, not wildcards", "a bare % must not match everything")
    assert "关键词为空" in await search_history(
        G1, "  ", database=_test_db.pool, identities=_test_db.identities, clock=_test_db.clock
    ), "an empty query is refused"

    # -- hits wrapped in their surroundings ----------------------------------
    # Chat is fragments: the line after the link is part of the story. Close
    # hits merge into one block; far-apart hits stay apart with an ellipsis
    # line between them, and the window is exactly context_lines each way.
    assert "收到了" in hit and "我也看看" in hit, ("a hit carries the lines around it", hit)
    await say(G1, "u1", "阿强", "上次说的螺丝刀在哪")
    for i in range(12):
        await say(G1, "u2", "阿花", f"填充话题第{i}句")
    await say(G1, "u2", "阿花", "螺丝刀在工具箱第二层")
    two = await search_history(
        G1, "螺丝刀", database=_test_db.pool, identities=_test_db.identities, clock=_test_db.clock
    )
    assert "……" in two, ("far-apart hits render as separate blocks", two)
    assert "填充话题第0句" in two and "填充话题第11句" in two and "填充话题第5句" not in two, (
        "each block shows its own surroundings, cut at the window",
        two,
    )
    await say(G1, "u1", "阿强", "今晚麻辣香锅怎么样")
    await say(G1, "u2", "阿花", "麻辣香锅可以")
    one = await search_history(
        G1, "麻辣香锅", database=_test_db.pool, identities=_test_db.identities, clock=_test_db.clock
    )
    assert "……" not in one and "麻辣香锅怎么样" in one and "麻辣香锅可以" in one, (
        "adjacent hits merge into one block",
        one,
    )
    from qqbot.conversation.limits import HISTORY_LIMITS

    _base_rcfg = HISTORY_LIMITS
    _saved_ctx = _base_rcfg.context_lines
    _rcfg = replace(_base_rcfg, context_lines=0)
    bare = await search_history(
        G1,
        "cat.jpg",
        rcfg=_rcfg,
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    )
    assert "cat.jpg" in bare and "收到了" not in bare, ("context_lines 0 restores bare hits", bare)

    # -- boolean queries ------------------------------------------------------
    # Lucene syntax through luqum: juxtaposition stays AND, OR groups the
    # synonyms colloquial chat actually needs, - excludes, quoted phrases match
    # whole. Bare-hit mode, so the pins are about which lines are hits.
    await say(G1, "u1", "阿强", "咖啡机到货了，明天开箱")
    await say(G1, "u2", "阿花", "复印机又坏了，打印机也别想跑")
    await say(G1, "u2", "阿花", "打印机换了新喷头 效果不错")
    b1 = await search_history(
        G1,
        "(咖啡机 OR 打印机) -复印",
        rcfg=_rcfg,
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    )
    assert "到货" in b1 and "喷头" in b1 and "别想跑" not in b1, (
        "an OR group hits either word and the exclusion drops its row",
        b1,
    )
    b2 = await search_history(
        G1,
        "（咖啡机 OR 复印机） 坏了",
        rcfg=_rcfg,
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    )
    assert "别想跑" in b2 and "到货" not in b2, (
        "full-width parentheses parse and compose with AND",
        b2,
    )
    b3 = await search_history(
        G1,
        '"新喷头 效果"',
        rcfg=_rcfg,
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    )
    assert "喷头" in b3 and "到货" not in b3, ("a quoted phrase matches whole, space included", b3)
    # A hit comes back whole, and so does the result. The long messages are the
    # substantial ones - a summary, an argument, a piece of writing - and a fixed
    # width cut exactly the part worth searching for, silently and mid-word. There
    # is no length quota in its place either: money already bounds what a reply may
    # spend, and a character budget would be a second, blinder bound on the same thing.
    _long = "螺丝刀的来历要从头说起，" + "这段话很长很长，".join(str(i) for i in range(60))
    await say(G1, "u1", "阿强", _long)
    _wide = await search_history(
        G1,
        "螺丝刀的来历",
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    )
    _widest = max(len(line) for line in _wide.splitlines())
    assert _long in _wide, (
        "a long message comes back whole, not cut mid-word",
        f"{len(_long)} chars in, longest line {_widest}",
    )
    assert "检索式有误" in await search_history(
        G1, "((", database=_test_db.pool, identities=_test_db.identities, clock=_test_db.clock
    ), "a broken expression is answered in words, not raised"
    assert "检索式有误" in await search_history(
        G1,
        "标签:值",
        database=_test_db.pool,
        identities=_test_db.identities,
        clock=_test_db.clock,
    ), "lucene features outside the boolean subset are refused in words"

    # -- schema self-check ---------------------------------------------------
    from qqbot.db.repo import ensure_schema

    await ensure_schema(pool)

    print()
