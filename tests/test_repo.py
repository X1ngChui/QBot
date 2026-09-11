"""What db/repo.py still owns, against a real postgres.

Everything about people moved to the repositories package and is covered by
test_repositories.py. What is left here is the group-level plumbing: the L0 archive, the
image cache, the group switches and the cost ledger.

Raw events are written through the Ingestor rather than by an INSERT here, because that
is now the only way they are written in production - a test with its own INSERT would
keep passing after the real path broke.
"""
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
os.environ.setdefault("DATABASE_URL", "postgresql://qqbot@127.0.0.1:15432/qqbot")
os.environ.setdefault("DATABASE_PASSWORD", "testpw")
import asyncio
import itertools


from qqbot.db import init_pool, close_pool, pool
from qqbot.db import repo
from qqbot.gateway.ingest import ingestor
from qqbot.repositories.event import EventRepository
from qqbot.gateway.onebot import GroupMessage, Sender
from qqbot.util import now_local, today_local
from _db import reset

fails = []
G1 = 7001
G2 = 7002
_SEQ = itertools.count(1)


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


async def say(group, uid, name, text, *, msg_id=None, at=None):
    """One message through the real inbound path.

    Backdated by a few seconds afterwards: the extraction watermark leaves out the
    newest seconds of ingest (see EventRepository.UNREAD_MESSAGE), and a test that
    writes and counts within the same second would otherwise see nothing unread.
    """
    mid = msg_id or f"auto{next(_SEQ)}"
    await ingestor().ingest(
        GroupMessage(
            message_id=mid, group_id=group,
            sender=Sender(user_id=uid, card=name), segments=[],
            self_id="999", occurred_at=at or now_local(), plain_text=text,
        )
    )
    await pool().execute(
        """UPDATE raw_event SET created_at = created_at - INTERVAL '10 seconds'
            WHERE platform='qq' AND platform_event_id=$1""", mid)
    return mid


async def main():
    await init_pool()
    await reset()

    # -- raw events ---------------------------------------------------------
    await say(G1, "u1", "阿强", "hi", msg_id="m1")
    await say(G1, "u1", "阿强", "dup", msg_id="m1")   # a replay after a reconnect
    n = await pool().fetchval(
        "SELECT count(*) FROM raw_event WHERE platform_event_id='m1'")
    check("a replayed message id does not land twice", n == 1, f"{n} rows")

    await repo.backfill_plain_text("m1", "hi [图片:一只猫]")
    got = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id='m1'")
    check("backfill_plain_text rewrites the reading", got == "hi [图片:一只猫]", str(got))
    segs = await pool().fetchval(
        "SELECT payload->'segments' FROM raw_event WHERE platform_event_id='m1'")
    check("and leaves the original segments in place", segs is not None, str(segs))

    # -- image cache --------------------------------------------------------
    check("image_cache miss", await repo.image_cache_get("k1") is None)
    await repo.image_cache_put("k1", "[表情:开心]")
    check("image_cache hit", await repo.image_cache_get("k1") == "[表情:开心]")
    await repo.image_cache_get("k1")
    hits = await pool().fetchval("SELECT hit_count FROM image_cache WHERE key='k1'")
    check("image_cache counts hits", hits == 2, str(hits))
    stats = await repo.image_cache_stats()
    check("image_cache_stats", stats["n"] == 1 and stats["hits"] == 2, str(dict(stats)))

    # A description ages out so a better model gets to write it again: the paid
    # describing path asks for one no older than the configured window and treats
    # anything older as a miss. Free paths ask for no age at all - they cannot pay
    # to replace what they reject, and a stale description beats a bare marker.
    from datetime import timedelta as _td
    check("a fresh description satisfies the paid path",
          await repo.image_cache_get("k1", max_age=_td(days=15)) == "[表情:开心]")
    await pool().execute(
        "UPDATE image_cache SET described_at = now() - interval '20 days' WHERE key='k1'")
    check("an aged one is a miss there, so it gets described again",
          await repo.image_cache_get("k1", max_age=_td(days=15)) is None)
    check("but the free path still serves it",
          await repo.image_cache_get("k1") == "[表情:开心]")
    # A description always carries its time: the schema refuses one without it.
    try:
        await pool().execute("UPDATE image_cache SET described_at = NULL WHERE key='k1'")
        check("a description without a time is refused by the schema", False, "accepted")
    except Exception as e:
        check("a description without a time is refused by the schema",
              "image_cache_described_stamped" in str(e), str(e)[:80])
    # Re-describing stamps the row afresh, which is what ends the expiry.
    await repo.image_cache_put("k1", "[表情:开心，重描]")
    check("a rewrite is current again",
          await repo.image_cache_get("k1", max_age=_td(days=15)) == "[表情:开心，重描]")

    # A picture the backend's content filter declined is stored as a placeholder, and
    # marked as one: it describes nothing, and a climbing count of them says the
    # backend is turning away more than it looks at. It expires like any other, so a
    # different backend - or the same one with different rules - gets to look again.
    await repo.image_cache_put("k-refused", "⟦图片⟧", refused=True)
    _st = await repo.image_cache_stats()
    check("a refusal is stored as a refusal", _st["refused"] == 1, str(dict(_st)))
    await repo.image_cache_put("k-refused", "⟦图片:一只猫⟧")
    _st = await repo.image_cache_stats()
    check("and stops being one once the backend does look",
          _st["refused"] == 0, str(dict(_st)))

    # The upload half can land before the describing half, and neither may clobber the
    # other: a row with only a file_id reads as an undescribed picture, and the later
    # description fills the same row in.
    await repo.image_cache_set_file("k2", "file-api-abc")
    check("a file_id alone is not a description hit",
          await repo.image_cache_get("k2") is None)
    await repo.image_cache_put("k2", "[图片:占位测试]")
    check("the description fills the same row",
          await repo.image_cache_get("k2") == "[图片:占位测试]")
    check("and the file_id survives it",
          await repo.image_cache_file("k2") == "file-api-abc")
    # The backend expires files, so an id is only trusted while young: one older
    # than the window (or stamped before the upload time was recorded) reads as
    # absent and the picture is uploaded again.
    from datetime import timedelta as _td_f
    check("a fresh file_id is trusted inside the age window",
          await repo.image_cache_file("k2", max_age=_td_f(days=1)) == "file-api-abc")
    await pool().execute(
        "UPDATE image_cache SET file_uploaded_at = now() - interval '2 days' WHERE key='k2'")
    check("an old file_id reads as absent",
          await repo.image_cache_file("k2", max_age=_td_f(days=1)) is None)
    await pool().execute("UPDATE image_cache SET file_uploaded_at = NULL WHERE key='k2'")
    check("an unstamped file_id reads as absent too",
          await repo.image_cache_file("k2", max_age=_td_f(days=1)) is None)
    check("and without an age it is still returned as a hint",
          await repo.image_cache_file("k2") == "file-api-abc")
    check("a picture never uploaded has no file_id",
          await repo.image_cache_file("k1") is None)

    # -- group_state and the blocklist --------------------------------------
    # Typed columns and a proper relation. The blocklist was an array column for a day,
    # which first normal form has opinions about: a group blocking people is one-to-many,
    # so it is a table with a two-column key, and membership is a WHERE clause.
    check("a group with no rows reads as all-off",
          await repo.group_switches(G1) == (False, {}))
    await repo.set_group_muted(G1, True)
    await repo.block(G1, "u9")
    await repo.block(G1, "u8")
    await repo.block(G1, "u9")   # blocking twice is once
    check("switches round-trip",
          await repo.group_switches(G1) == (True, {"u8": None, "u9": None}))
    check("unblocking reports whether anything changed",
          await repo.unblock(G1, "u9") and not await repo.unblock(G1, "u9"))
    await repo.set_group_muted(G1, False)
    await repo.unblock(G1, "u8")
    check("and can be cleared", await repo.group_switches(G1) == (False, {}))
    await repo.set_group_muted(G2, True)
    check("groups_with_state lists every group that has a row",
          {G1, G2} <= set(await repo.groups_with_state()))
    # From the table, not the in-memory registry: the daily report must list a
    # group muted before the last restart and quiet ever since.
    check("muted_groups reads the persisted flag",
          await repo.muted_groups() == [G2])

    # -- the extraction watermark -------------------------------------------
    # The watermark records what an extraction has *read*. Queueing a pass must not move
    # it: the worker reads this to decide whether a model call is justified at all, and a
    # mark set at queue time would tell it its own batch had already been handled.
    from datetime import timedelta as _td
    from qqbot.repositories import JobQueue as _JQ
    from qqbot.repositories.job import JobType as _JT

    G3 = 7003
    n0, _ = await EventRepository().unread_since_extract(G3)
    check("a group nobody has read has nothing unread", n0 == 0, str(n0))
    for i in range(4):
        await say(G3, "u1", "阿强", f"第 {i} 句")
    n1, newest = await EventRepository().unread_since_extract(G3)
    check("messages arrive unread", n1 == 4 and newest is not None, str(n1))

    await _JQ("t").submit(_JT.EXTRACT_MEMORY, {"group_id": G3}, priority=1)
    n2, _ = await EventRepository().unread_since_extract(G3)
    check("queueing a pass reads nothing, so the count does not move", n2 == 4, str(n2))

    await repo.mark_extracted(G3, newest)
    n3, _ = await EventRepository().unread_since_extract(G3)
    check("marking what was read is what clears them", n3 == 0, str(n3))

    # A watermark never goes backwards: two passes can overlap, and the one that finishes
    # later must not reopen what the other already read.
    await repo.mark_extracted(G3, newest - _td(hours=1))
    n4, _ = await EventRepository().unread_since_extract(G3)
    check("and it never moves backwards", n4 == 0, str(n4))

    # ...except for the one sanctioned reset: /relearn means "read it again", and the
    # gate answering "nothing new" to that request would be the gate malfunctioning.
    # The reset pulls back exactly one window and no further - extraction drains
    # oldest-first from the watermark now, and a bare NULL would send the next
    # drain through the entire archive at model prices.
    from qqbot.settings import config as _cfgw
    _EW = _cfgw().default.memory.extract_window
    await repo.reset_extract_watermark(G3, keep=_EW)
    n5, _ = await EventRepository().unread_since_extract(G3)
    check("a reset makes the window count as unread again", n5 == 4, str(n5))
    await repo.mark_extracted(G3, newest)

    # -- cost ledger --------------------------------------------------------
    day = today_local()
    await repo.ledger_add(group_id=str(G1), kind="reply", model="deepseek-v4-flash",
                          in_hit=1000, in_miss=200, out=80, cny=0.00036)
    await repo.ledger_add(group_id=str(G1), kind="extract", model="deepseek-v4-flash",
                          in_hit=800, in_miss=10, out=8, cny=0.000042)
    await repo.ledger_add(group_id=None, kind="search", model="search_std", cny=0.01)
    total = await repo.day_cost(day)
    check("day_cost sums", abs(total - 0.010402) < 1e-6, f"{total:.6f}")
    bd = await repo.day_breakdown(day)
    check("day_breakdown groups", len(bd) == 3 and bd[0]["kind"] == "search",
          str([r["kind"] for r in bd]))
    # Without a group id the ledger covers every group, which is what the shared cap is
    # measured against - the budget is not per-group.
    one = await repo.day_breakdown(day, str(G1))
    check("a per-group breakdown leaves out the shared rows",
          {r["kind"] for r in one} == {"reply", "extract"}, str([r["kind"] for r in one]))

    # -- the search allowance, metered off the ledger ------------------------
    # The search backend is free within a monthly credit allowance, and the ledger's
    # call count is the meter itself: no separate counter to drift, and a refusal at
    # the allowance that never reaches the vendor.
    import os as _os
    import httpx as _hx
    from qqbot.providers.tavily import TavilySearch
    from qqbot.providers.base import QuotaExhausted
    from qqbot.settings import config as _config

    _os.environ.setdefault("SEARCH_API_KEY", "tvly-test-key")
    scfg = _config().default.llm.search.model_copy(deep=True)
    seen_reqs = []

    def _fake_tavily(req):
        seen_reqs.append(req)
        if req.url.path == "/extract":
            return _hx.Response(200, json={"results": [
                {"url": "https://a.example/page", "raw_content": "  正文   开头  "}]})
        return _hx.Response(200, json={"results": [
            {"title": "t1", "url": "https://a.example/1", "content": "  spaced   out  "},
            {"title": "t2", "url": "https://a.example/2", "content": "two"},
        ]})

    ts = TavilySearch()
    ts._client = _hx.AsyncClient(transport=_hx.MockTransport(_fake_tavily))
    ts._id = (scfg.timeout_sec, scfg.proxy)   # what _http() keys the cached client on

    spent_before = await repo.day_cost(day)
    items = await ts.search("天气 上海", cfg=scfg, group_id=str(G1))
    req = seen_reqs[0]
    check("the request carries the key and the query",
          req.headers.get("authorization", "").startswith("Bearer tvly-")
          and b"search_depth" in req.content and req.url.path == "/search")
    check("results are normalised to title/link/content",
          items[0] == {"title": "t1", "link": "https://a.example/1",
                       "content": "spaced out"}, str(items[:1]))
    check("a free call books a row but no money",
          await repo.month_calls("search", "tavily") == 1
          and abs(await repo.day_cost(day) - spent_before) < 1e-9)
    check("rows of another backend do not eat the allowance",
          await repo.month_calls("search", "search_std") >= 1)

    scfg.monthly_quota = 1
    try:
        await ts.search("再来一次", cfg=scfg, group_id=str(G1))
        check("at the allowance the backend refuses", False, "it searched")
    except QuotaExhausted:
        check("at the allowance the backend refuses", True)
    check("and the refusal never reached the vendor", len(seen_reqs) == 1)

    # Advanced depth debits two vendor credits per call, and the meter counts what
    # the vendor counts - metered by calls, the real 1000 would be gone at ~500
    # while the meter read half-full, and every search past that would fail as a
    # transport error instead of the clean quota silence.
    scfg.monthly_quota = 100
    scfg.depth = "advanced"
    await ts.search("深度搜一次", cfg=scfg, group_id=str(G1))
    check("an advanced search books two credits",
          await repo.month_calls("search", "tavily") == 3,
          str(await repo.month_calls("search", "tavily")))

    # Page extraction rides the same allowance: same key, same proxy, same meter,
    # same refusal at the ceiling.
    text = await ts.extract("https://a.example/page", cfg=scfg, group_id=str(G1))
    check("extract returns the page text normalised", text == "正文 开头", repr(text))
    check("and debits the shared allowance",
          await repo.month_calls("search", "tavily") == 4,
          str(await repo.month_calls("search", "tavily")))
    scfg.monthly_quota = 4
    try:
        await ts.extract("https://a.example/page", cfg=scfg, group_id=str(G1))
        check("extract refuses at the allowance", False, "it extracted")
    except QuotaExhausted:
        check("extract refuses at the allowance", True)
    await ts.aclose()

    # -- a timed-out attempt still books its spend ---------------------------
    # Usage travels in the stream's final chunk, which a timeout never reads; the
    # vendor billed the prompt and everything generated up to the cut all the same.
    # This was the one spending path that systematically understated, in a design
    # whose every other guess deliberately leans high.
    from qqbot.providers.openai_compat import OpenAICompatChat

    class TimingOut(OpenAICompatChat):
        async def _stream_once(self, *a, **kw):
            raise TimeoutError

    tcfg = _config().default.llm.text.model_copy(deep=True)
    tcfg.retries = 0
    before_to = await repo.day_cost(day)
    try:
        await TimingOut().chat([{"role": "user", "content": "你好"}],
                               cfg=tcfg, max_tokens=100, kind="reply",
                               group_id=str(G1))
        check("a timed-out chat still raises", False, "it returned")
    except TimeoutError:
        check("a timed-out chat still raises", True)
    after_to = await repo.day_cost(day)
    check("and its estimated spend reaches the ledger", after_to > before_to,
          f"{before_to:.6f} -> {after_to:.6f}")

    # -- the ledger names the model that actually served -------------------
    # A vendor may retire an id and route it to a successor billed at another
    # rate. Booking the requested name would price the call from a table entry
    # that no longer describes it, in either direction.
    from qqbot.providers.base import ChatResult as _CR

    class Routed(OpenAICompatChat):
        async def _stream_once(self, *a, **kw):
            return _CR(model="served-elsewhere", text="好", in_miss=10, out=5)

    await Routed().chat([{"role": "user", "content": "你好"}], cfg=tcfg,
                        kind="reply", group_id=str(G1))
    _models = {r["model"] for r in await repo.day_breakdown(day)}
    check("a routed call books under the model that served it",
          "served-elsewhere" in _models, str(sorted(_models)))

    # -- the window rebuilt from the archive --------------------------------
    # A deque that starts empty on deploy has the bot rejoin conversations it was
    # part of thirty seconds earlier knowing nothing - while the archive holds
    # every message, its own replies included. So the window comes back from the
    # archive.
    from qqbot.core.state import GroupState
    from qqbot.gateway.ingest import ingestor as _ing

    await say(G1, "u1", "阿强", "昨天的图在这 https://x.example/cat.jpg")
    await say(G1, "u2", "阿花", "收到了")
    await _ing().record_own_reply(group_id=G1, self_id="999", message_id="bot-r1",
                                  text="我也看看", at=now_local(), name="小X")

    rows = await repo.recent_messages(G1, limit=50)
    _stamps = [r["occurred_at"] for r in rows]
    check("recent_messages returns oldest first", _stamps == sorted(_stamps), "")
    check("and only this group's",
          not any(r for r in await repo.recent_messages(G2, limit=50)
                  if "阿强" in str(r["payload"])), "")

    st = GroupState(group_id=str(G1))
    await st.load_history(self_id="999", owners={"u1"})
    texts = [m.text for m in st.recent]
    check("the window is rebuilt from the archive",
          "收到了" in texts and any("cat.jpg" in t for t in texts), str(texts[-4:]))
    by_text = {m.text: m for m in st.recent}
    check("the bot's own replies come back marked as its own",
          by_text["我也看看"].is_bot and by_text["我也看看"].nickname == "小X")
    check("and an owner comes back marked as an owner",
          by_text["收到了"].is_owner is False
          and any(m.is_owner for m in st.recent if m.user_id == "u1"))
    n_before = len(st.recent)
    await st.load_history(self_id="999", owners=set())
    check("loading twice does not double the window", len(st.recent) == n_before)

    # Console output is on the record too: a /who card the model never saw made
    # the very next question about it unanswerable - the one speaker in the room
    # whose words vanished.
    from qqbot.core.pipeline import note_console_reply
    from qqbot.core.state import REGISTRY as _REG

    _REG._groups[str(G1)] = st
    await note_console_reply(group_id=G1, self_id="999",
                             text="阿强：住在苏州；备注：只在周末上线", name="小X")
    check("a command answer lands in the window as the bot's own line",
          any(m.text.startswith("阿强：住在苏州") and m.is_bot for m in st.recent))
    st_rebuilt = GroupState(group_id=str(G1))
    await st_rebuilt.load_history(self_id="999", owners=set())
    check("and survives into a rebuilt window",
          any(m.text.startswith("阿强：住在苏州") and m.is_bot
              for m in st_rebuilt.recent))
    _REG._groups.pop(str(G1), None)

    # -- the archive, searched ----------------------------------------------
    # The pull half of context: the prompt pushes a fixed window, and everything behind
    # it was unreachable - a link posted yesterday might as well not have existed.
    from qqbot.core.tools import search_history
    hit = await search_history(G1, "cat.jpg")
    check("the archive answers a keyword", "https://x.example/cat.jpg" in hit, hit)
    check("with when and who", "阿强" in hit and "]" in hit, hit)
    check("all keywords must hit, not any",
          "没有搜到" in await search_history(G1, "阿强 不存在的词"))
    check("another group's archive is out of reach",
          "没有搜到" in await search_history(G2, "cat.jpg"))
    check("LIKE pattern characters are literal, not wildcards",
          "没有搜到" in await search_history(G1, "%"),
          "a bare % must not match everything")
    check("an empty query is refused", "关键词为空" in await search_history(G1, "  "))

    # -- hits wrapped in their surroundings ----------------------------------
    # Chat is fragments: the line after the link is part of the story. Close
    # hits merge into one block; far-apart hits stay apart with an ellipsis
    # line between them, and the window is exactly history_context each way.
    check("a hit carries the lines around it",
          "收到了" in hit and "我也看看" in hit, hit)
    await say(G1, "u1", "阿强", "上次说的螺丝刀在哪")
    for i in range(12):
        await say(G1, "u2", "阿花", f"填充话题第{i}句")
    await say(G1, "u2", "阿花", "螺丝刀在工具箱第二层")
    two = await search_history(G1, "螺丝刀")
    check("far-apart hits render as separate blocks", "……" in two, two)
    check("each block shows its own surroundings, cut at the window",
          "填充话题第0句" in two and "填充话题第11句" in two
          and "填充话题第5句" not in two, two)
    await say(G1, "u1", "阿强", "今晚麻辣香锅怎么样")
    await say(G1, "u2", "阿花", "麻辣香锅可以")
    one = await search_history(G1, "麻辣香锅")
    check("adjacent hits merge into one block", "……" not in one
          and "麻辣香锅怎么样" in one and "麻辣香锅可以" in one, one)
    _rcfg = _config().default.retrieval
    _saved_ctx = _rcfg.history_context
    _rcfg.history_context = 0
    bare = await search_history(G1, "cat.jpg")
    check("history_context 0 restores bare hits",
          "cat.jpg" in bare and "收到了" not in bare, bare)

    # -- boolean queries ------------------------------------------------------
    # Lucene syntax through luqum: juxtaposition stays AND, OR groups the
    # synonyms colloquial chat actually needs, - excludes, quoted phrases match
    # whole. Bare-hit mode, so the pins are about which lines are hits.
    await say(G1, "u1", "阿强", "咖啡机到货了，明天开箱")
    await say(G1, "u2", "阿花", "复印机又坏了，打印机也别想跑")
    await say(G1, "u2", "阿花", "打印机换了新喷头 效果不错")
    b1 = await search_history(G1, "(咖啡机 OR 打印机) -复印")
    check("an OR group hits either word and the exclusion drops its row",
          "到货" in b1 and "喷头" in b1 and "别想跑" not in b1, b1)
    b2 = await search_history(G1, "（咖啡机 OR 复印机） 坏了")
    check("full-width parentheses parse and compose with AND",
          "别想跑" in b2 and "到货" not in b2, b2)
    b3 = await search_history(G1, '"新喷头 效果"')
    check("a quoted phrase matches whole, space included",
          "喷头" in b3 and "到货" not in b3, b3)
    # A hit comes back whole, and so does the result. The long messages are the
    # substantial ones - a summary, an argument, a piece of writing - and a fixed
    # width cut exactly the part worth searching for, silently and mid-word. There
    # is no length quota in its place either: money already bounds what a reply may
    # spend, and a character budget would be a second, blinder bound on the same thing.
    _long = "螺丝刀的来历要从头说起，" + "这段话很长很长，".join(str(i) for i in range(60))
    await say(G1, "u1", "阿强", _long)
    _wide = await search_history(G1, "螺丝刀的来历")
    _widest = max(len(line) for line in _wide.splitlines())
    check("a long message comes back whole, not cut mid-word",
          _long in _wide, f"{len(_long)} chars in, longest line {_widest}")
    check("a broken expression is answered in words, not raised",
          "检索式有误" in await search_history(G1, "(("))
    check("lucene features outside the boolean subset are refused in words",
          "检索式有误" in await search_history(G1, "标签:值"))
    _rcfg.history_context = _saved_ctx

    # -- schema self-check ---------------------------------------------------
    await repo.ensure_schema()
    check("ensure_schema passes on a live schema", True)

    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
