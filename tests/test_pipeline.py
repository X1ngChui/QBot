"""End-to-end pipeline test: fake protocol side, stubbed LLM, real DB."""
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
import types


from qqbot.db import init_pool, close_pool, pool
from qqbot.settings import config
from _db import reset
from _stubs import FakeEmbedding

#: One stub for every bundle in this suite.
_EMBED = FakeEmbedding()
from qqbot.core import prompt as prompt_mod
from qqbot.core.budget import BUDGET
from qqbot.core.pipeline import GATEWAY
from qqbot.core.state import REGISTRY, ChatMsg as _CM0
from qqbot.providers import (AsrModel, ChatResult, Providers, SearchEngine,
                             TextModel, VisionModel, set_providers)
from qqbot.providers.base import QuotaExhausted, Rate
from qqbot.util import now_local as _nl0, today_local as _today

fails = []
LLM_CALLS = []


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


# ---- stubs ---------------------------------------------------------------
REPLY_TEXT = {"v": "**行啊**，我看看"}
KNOWLEDGE = {"v": "群主是老王，外号王哥。"}


class FakeText(TextModel):
    """A real subclass of the ABC, so this test breaks if the contract changes - which is
    exactly what happened when the deliberation flag was added."""

    MODEL = "fake-light"
    RATE = Rate("Mtoken", in_hit=0.02, in_miss=1.0, out=2.0, source="fake")

    def rate_for(self, model):
        return self.RATE

    async def chat(self, messages, *, cfg, tools=None,
                   max_tokens=None, effort=None, kind="reply", group_id=None):
        LLM_CALLS.append({"kind": kind, "messages": messages, "tools": tools,
                          "effort": effort, "max_tokens": max_tokens,
                          "grade": cfg.reasoning_effort, "timeout": cfg.timeout_sec})
        # No production path overrides the grade at the call: each use of the text
        # model carries its settings in its own config. Pinned so the call-site
        # exception does not creep back.
        assert effort is None, "the grade belongs to the config, not the call"
        if kind == "extract":
            text = ""
        elif kind == "knowledge":
            text = KNOWLEDGE["v"]
        else:
            text = REPLY_TEXT["v"]
        # priced through the capability's own rate, the way a real backend does
        await BUDGET.record(kind=kind, model=self.MODEL,
                            cny=self.rate_for(self.MODEL).tokens(100, 10, 20),
                            in_hit=100, in_miss=10, out=20, group_id=group_id)
        return ChatResult(text=text, model=self.MODEL, in_hit=100, in_miss=10, out=20)

    async def upload(self, data, *, cfg, mime="image/jpeg"):
        # The model that reads the picture is the one that holds it, so the id
        # comes from here - the same place a multimodal backend puts it.
        return f"file-api-fake{len(data)}"

    async def aclose(self):
        pass


class UnusedVision(VisionModel):
    """A capability this test must not reach. Being called is itself the failure."""

    name = "unused"

    def rate_for(self, model):
        return Rate("Mtoken")

    async def describe(self, data, *, cfg, prompt="", mime="image/jpeg", group_id=None):
        raise AssertionError("vision should not be reached in this test")

    async def aclose(self):
        pass


class UnusedAsr(AsrModel):
    name = "unused"

    def rate_for(self, model):
        return Rate("second")

    async def transcribe(self, data, *, cfg, fmt="wav", seconds=None, group_id=None):
        raise AssertionError("asr should not be reached in this test")

    async def aclose(self):
        pass


class UnusedSearch(SearchEngine):
    name = "unused"

    def rate_for(self, model):
        return Rate("call")

    async def search(self, query, *, cfg, group_id=None):
        raise AssertionError("search should not be reached in this test")

    async def aclose(self):
        pass


providers_bundle = Providers(text=FakeText(), vision=UnusedVision(),
                             asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch())
set_providers(providers_bundle)


class Seg:
    def __init__(self, type_, data):
        self.type, self.data = type_, data


class FakeEvent:
    _ids = itertools.count(1000)

    def __init__(self, text=None, segments=None, user_id="u1", nickname="阿强", group_id=123,
                 to_me=False, reply_id=None, reply_from=None):
        self.group_id = group_id
        self.user_id = user_id
        self.message_id = next(self._ids)
        self._segs = segments or [Seg("text", {"text": text})]
        # The onebot adapter removes a leading/trailing @me segment and reports it here
        # instead, so a real @ arrives with to_me=True and no at segment at all.
        self.to_me = to_me
        # It does the same to a quote: _check_reply resolves it, deletes the segment, and
        # leaves this behind. An event built without it is not the shape production sees,
        # which is how reply_to came to be NULL for every message ever archived while the
        # tests passed - they were the only place a reply segment survived.
        self.reply = (
            types.SimpleNamespace(
                message_id=reply_id,
                sender=types.SimpleNamespace(user_id=reply_from),
            )
            if reply_id is not None else None
        )
        self.sender = types.SimpleNamespace(card="", nickname=nickname, role="member")

    def get_message(self):
        return self._segs


class FakeBot:
    self_id = "999"

    def __init__(self):
        self.sent = []
        self.quoted = []
        self.ats = []
        self.member_list_calls = 0
        self.members = [
            {"user_id": 24680, "card": "群里的阿明", "nickname": "阿明"},
            {"user_id": 7, "card": "小南", "nickname": "dong"}]

    async def send_group_msg(self, *, group_id, message):
        # The engine sends segment arrays; the tests assert on words, so keep the
        # text flat here and remember separately which message each reply quoted
        # and whom it @-ed.
        if isinstance(message, list):
            self.quoted.append(next((s["data"]["id"] for s in message
                                     if s["type"] == "reply"), None))
            self.ats.append(next((s["data"]["qq"] for s in message
                                  if s["type"] == "at"), None))
            message = "".join(s["data"].get("text", "") for s in message
                              if s["type"] == "text").lstrip()
        self.sent.append((group_id, message))
        return {"message_id": 90000 + len(self.sent)}

    async def call_api(self, api, **kw):
        if api == "get_image":
            return {"url": "", "file": ""}
        if api == "get_msg":
            return {"sender": {"nickname": "阿花"},
                    "message": [{"type": "text", "data": {"text": "昨天那张图"}}]}
        if api == "get_forward_msg":
            return {"messages": [
                {"type": "node",
                 "data": {"sender": {"nickname": "阿强"},
                          "message": [{"type": "text", "data": {"text": "第一条"}}]}},
                {"type": "node",
                 "data": {"sender": {"nickname": "阿花"},
                          "message": [{"type": "text", "data": {"text": "第二条"}}]}}]}
        if api == "get_group_member_list":
            self.member_list_calls += 1
            return list(self.members)
        return {}


async def drain(seconds=1.2):
    await asyncio.sleep(seconds)


#: Groups used by the roster checks. Numeric because a group id is a number everywhere
#: below the gateway - raw_event.group_id is a bigint, and every repository takes an int.
ORD = 4001
OTHER = 4002
_SEQ = itertools.count(1)


async def seed(group, uid, name, *, n=1, text="随便说说"):
    """Put an account into a group's history through the real inbound path.

    Through the Ingestor rather than an INSERT: what is being relied on downstream is
    that speaking creates a person, files the group card as an alias, and counts towards
    the roster. A test that wrote the row itself would assert none of that.
    """
    from qqbot.gateway.ingest import ingestor
    from qqbot.gateway.onebot import GroupMessage, Sender
    for _ in range(n):
        await ingestor().ingest(
            GroupMessage(
                message_id=f"seed-{group}-{uid}-{next(_SEQ)}", group_id=group,
                sender=Sender(user_id=uid, card=name), segments=[],
                self_id="999", occurred_at=_nl0(), plain_text=text,
            ),
        )


async def main():
    await init_pool()
    await reset()

    # The whole cast accepts the user agreement up front - consent is its own
    # section; everywhere else a reply is the thing under test.
    from qqbot.db import repo as _repo_seed
    for _uid in ("u1", "u7", "u9", "u404", "bad1", "1", "7", "999"):
        await _repo_seed.record_agreement(123, _uid, 1)

    cfg = config().default
    # The reply path will not start without one, which is the point: a half-wired
    # deployment must fail at boot, not quietly degrade recall.
    cfg.trigger.nicknames = ["小X", "X酱"]
    from qqbot.core import nickname
    nickname.initialize()
    nickname.register(cfg.trigger.nicknames)   # the plugin does this at startup
    bot = FakeBot()

    # 1. direct mention -> must answer, markdown stripped
    ev0 = FakeEvent("小X 在吗")
    await GATEWAY.handle(bot, ev0)
    await drain()
    check("direct mention replies", len(bot.sent) == 1, str(bot.sent))
    check("markdown stripped before send",
          bot.sent and "**" not in bot.sent[0][1], str(bot.sent[:1]))
    # The protocol side is configured not to report the bot's own messages, so nothing
    # else writes them down. Without this they lived only in the in-memory deque - and the
    # group card, which is distilled from the archive, was summarising one side of a
    # conversation.
    own = await pool().fetch(
        """SELECT plain_text, payload FROM raw_event
            WHERE group_id=123 AND platform_user_id=$1""",
        str(bot.self_id))
    check("the bot's own reply is archived too", len(own) == 1, str([dict(r) for r in own]))
    check("and it is the text that was actually sent",
          own and own[0]["plain_text"] == bot.sent[0][1], str(bot.sent[:1]))
    # The answer is anchored to its cause: sent as a quote of the message that
    # called, and the same pointer goes into the archive so the rebuilt window
    # renders the bot's line with the ordinary quote mark.
    check("the reply quotes the message that called",
          bot.quoted and bot.quoted[0] == str(ev0.message_id), str(bot.quoted[:1]))
    check("and @-es its sender, the way QQ's own reply button does",
          bot.ats and bot.ats[0] == "u1", str(bot.ats[:1]))
    check("and the archive keeps the quote pointer",
          own and (own[0]["payload"] or {}).get("reply_to") == str(ev0.message_id),
          str((own[0]["payload"] or {}).get("reply_to")) if own else "no row")

    check("every reply is offered the tools - there is one model and no tier",
          all(c["tools"] for c in LLM_CALLS if c["kind"] == "reply"))

    sys_prompt = [c for c in LLM_CALLS if c["kind"] == "reply"][0]["messages"][0]["content"]
    check("global constants lead the prompt, so every group shares that span",
          sys_prompt.startswith("【消息标记说明】"), sys_prompt[:24])
    # Without this the bot denies seeing an image while holding its description - it does
    # not know that the picture marker is its own eyesight rather than something a person
    # typed.
    check("the reply prompt explains the numbered markers",
          "⟦图片N:描述⟧" in sys_prompt and "open_image" in sys_prompt)
    # The transcript carries names but never account ids, so two people with similar names
    # are indistinguishable to the model - three members rearranging the same joke
    # nickname read as one person renaming himself, and it said so out loud. The system
    # knows better on both counts, and now says so.
    check("the prompt says similar names are still different people",
          "昵称不同按不同账号处理" in sys_prompt)
    check("and that a rename is only a rename when it was recorded",
          "断言该账号改过或没改过名" in sys_prompt,
          sys_prompt[sys_prompt.find("曾用名"):][:120])
    # A name the account displayed and a name the group calls him are different claims,
    # and the prompt has to say so or the second gets reported as the first.
    check("and that a registered alias is not a name he used to display",
          "不要说成「他以前叫」" in sys_prompt)
    # These hold with or without a roster, so they ship apart from the lists: this group
    # has no members configured and still gets them.
    check("reading rules ship whether or not anyone is configured",
          "【信息解读规则】" in sys_prompt)
    # Account, name and person are three layers. The code is certain only about accounts:
    # one person can hold several, so declaring two accounts to be different people states
    # something unknown in the same confident voice as something known.
    check("the rules separate account from person",
          "一个人可持有多个账号" in sys_prompt
          and "不得断言二者一定不属于同一人" in sys_prompt)
    check("and stop short of what is not known",
          "身份确认能力止于账号一级" in sys_prompt
          and "未告知的信息即为未知信息" in sys_prompt)
    check("the persona follows the constants",
          sys_prompt.index("【信息解读规则】") < sys_prompt.index("【你的身份】"))
    # The whole point of a code-supplied fact is that it is certain. That is worth nothing
    # unless the certain lines are marked apart from the guessed ones - the model had been
    # repeating its own inferences back as if someone had told it.
    # Headings come from the module rather than being spelled out here: a rename that
    # broke every section boundary should fail loudly, not be quietly re-typed in a test.
    # A transcript is not a document. Every prompt was written as if it meant what it said,
    # which is how a member's throwaway boast about their ancestry became a recorded trait.
    check("the reply path is told how a group chat reads",
          prompt_mod.H_TONE in sys_prompt and "不能按字面当真" in sys_prompt)
    check("and told not to correct a joke",
          "不要一本正经地解释或纠正" in sys_prompt)
    from qqbot.services import MemoryExtractor
    from qqbot.workers.memory import transcript_legend
    _CP = MemoryExtractor(cfg, legend=transcript_legend()).prompt
    check("the memory path gets the same reading, minus the part about speaking",
          "不能按字面当真" in _CP and "不要一本正经地解释或纠正" not in _CP)
    # One discernment, two consequences: the judgment half is the shared
    # tone_rules, and extraction's own note says what not to record.
    check("with its own instruction for what to do with a joke",
          "没有被当真的话不产生任何候选" in _CP)

    check("the sections are marked",
          all(h in sys_prompt for h in
              (prompt_mod.H_PERSONA, prompt_mod.H_LEGEND, prompt_mod.H_RULES)))
    check("and says not to deny having them", "不要声称看不到图" in sys_prompt)
    from qqbot.settings import ptext as _ptext
    _LEG = _ptext("legend")
    check("one legend, shared by the reply and memory paths",
          _LEG in sys_prompt and _LEG in _CP)

    # 1b. A real @ arrives as to_me with the segment already stripped by the adapter.
    # Relying on the at segment alone means the must-answer path never fires for @.
    await REGISTRY.get("123")
    n_at = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("你好，介绍一下自己", to_me=True))
    await drain()
    check("@ mention replies even with the at segment stripped", len(bot.sent) == n_at + 1,
          str(bot.sent[n_at:]))
    at_call = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]
    check("prompt shows the bot was addressed", "@我" in at_call["messages"][-1]["content"])

    # 1c. Owner recognition. The prompt only ever carries a display name, so without a
    # tag derived from the account id the bot cannot tell who its owner is the moment
    # they change their group card.
    cfg.owners = ["u9"]
    await REGISTRY.get("123")
    await GATEWAY.handle(bot, FakeEvent("在吗", to_me=True, user_id="u9", nickname="随便改的名字"))
    await drain()
    own_tail = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"][-1]["content"]
    check("owner is tagged in the prompt", "随便改的名字⟦拥有者⟧" in own_tail, own_tail[-90:])
    await GATEWAY.handle(bot, FakeEvent("在吗", to_me=True, user_id="u1", nickname="阿强"))
    await drain()
    plain_tail = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"][-1]["content"]
    check("a non-owner is not tagged", "阿强（拥有者）" not in plain_tail)
    cfg.owners = []

    # 2. A nickname inside a longer word is not being addressed. This is the whole
    # trigger now, so the boundary rule is the only thing standing between the bot and
    # answering a message about somebody's games console.
    n0 = len(bot.sent)
    st = await REGISTRY.get("123")
    await GATEWAY.handle(bot, FakeEvent("这个小Xbox游戏机不错"))
    await drain()
    check("substring-only hit does not answer", len(bot.sent) == n0, str(bot.sent[n0:]))

    # 3. Not being addressed at all costs nothing: no reply, and no model call of
    # any kind - being addressed is the entire trigger, and every other message
    # must stay free.
    calls_before = len(LLM_CALLS)
    await GATEWAY.handle(bot, FakeEvent("今天天气不错"))
    await drain()
    check("an unaddressed message draws no reply", len(bot.sent) == n0)
    check("and costs no model call at all", len(LLM_CALLS) == calls_before,
          str([c["kind"] for c in LLM_CALLS[calls_before:]]))

    # 4. Being addressed is the only way in, and it goes straight to the reply.
    await GATEWAY.handle(bot, FakeEvent("小X 有人打游戏吗"))
    await drain()
    check("being addressed answers", len(bot.sent) == n0 + 1, str(bot.sent[n0:]))
    last_reply = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]
    # Deliberation for replies is a config grade (reasoning_effort), so the call
    # itself carries no override - the backend reads the grade from cfg.
    check("replies leave deliberation to config", last_reply["effort"] is None)

    # 5. Muting outranks it. This is the only remaining way to make the bot ignore an @.
    n1 = len(bot.sent)
    st.muted = True
    await GATEWAY.handle(bot, FakeEvent("小X 在吗"))
    await drain()
    check("a muted group stays silent even when addressed", len(bot.sent) == n1)
    st.muted = False

    # 6. one message, one verdict: in a burst only the addressed fragment draws a
    # reply, immediately - later fragments are ordinary background, not merged in.
    await REGISTRY.get("123")
    n2 = len(bot.sent)
    ev6 = FakeEvent("小X 你看")
    for e in (ev6, FakeEvent("这个"), FakeEvent("怎么样")):
        await GATEWAY.handle(bot, e)
        await asyncio.sleep(0.05)
    await drain()
    check("only the addressed fragment replies", len(bot.sent) == n2 + 1,
          f"{len(bot.sent) - n2} replies")
    check("and the reply quotes exactly that fragment",
          bot.quoted[-1] == str(ev6.message_id), f"quoted {bot.quoted[-1]}")
    tail6 = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"][-1]["content"]
    check("the slice ends at the addressed message - later fragments stay out",
          "小X 你看" in tail6 and "怎么样" not in tail6, tail6[-80:])

    # 6b. two people asking almost at once each get their own reply, each quoting
    # its own asker - the misdirected-reply failure this design exists to end.
    n2b = len(bot.sent)
    ev_a = FakeEvent("小X 第一个问", user_id="u1", nickname="阿强", to_me=True)
    ev_b = FakeEvent("小X 第二个问", user_id="u7", nickname="小南", to_me=True)
    await GATEWAY.handle(bot, ev_a)
    await GATEWAY.handle(bot, ev_b)
    await drain()
    check("two near-simultaneous askers get two replies",
          len(bot.sent) == n2b + 2, f"{len(bot.sent) - n2b} replies")
    check("each reply quotes and @s its own asker, whatever the finish order",
          dict(zip(bot.quoted[-2:], bot.ats[-2:], strict=True))
          == {str(ev_a.message_id): "u1", str(ev_b.message_id): "u7"},
          f"{bot.quoted[-2:]} {bot.ats[-2:]}")

    # 7. no rate cap: every addressed message earns its one reply attempt -
    # money and the provider concurrency semaphore are what bound the pace.
    n3 = len(bot.sent)
    for i in range(5):
        await GATEWAY.handle(bot, FakeEvent(f"小X 第{i}次"))
    await drain()
    check("every ask is answered, none rate-dropped", len(bot.sent) - n3 == 5,
          f"{len(bot.sent) - n3} sent")

    # 8. dedup
    ev = FakeEvent("小X 重复消息")
    await GATEWAY.handle(bot, ev)
    await GATEWAY.handle(bot, ev)
    await drain()
    rows = await pool().fetchval(
        "SELECT count(*) FROM raw_event WHERE platform_event_id=$1", str(ev.message_id))
    check("duplicate message archived once", rows == 1, str(rows))

    # 9. blocklist: blocked means unanswered, nothing more - the message still
    # archives so the window stays coherent; the reply is what is withheld.
    from qqbot.db import repo as _repo
    st9 = await REGISTRY.get("123")
    st9.blocked["u9"] = None
    await _repo.block(123, "u9")
    n5 = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("小X 在吗", user_id="u9"))
    await drain()
    check("a blocked user is dropped", len(bot.sent) == n5)
    st9.blocked.pop("u9", None)
    await _repo.unblock(123, "u9")

    # 11. nothing that changes every turn may sit in the cached system block.
    await REGISTRY.get("123")
    await GATEWAY.handle(bot, FakeEvent("小X 显卡现在多少钱"))
    await drain()
    msgs = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"]
    check("the message being answered is in the tail", "显卡现在多少钱" in msgs[-1]["content"])
    check("and not in the cached system block", "显卡现在多少钱" not in msgs[0]["content"])

    # 12. The daily cap stops the bot, including when it is addressed.
    #
    # No mention exemption: every reply is a direct one, so a cap that spared
    # mentions could not stop anything, and a configured daily ceiling would have
    # been decorative for as long as the proactive path had been gone.
    cfg.budget.daily_cny_cap = 0.0000001
    BUDGET._loaded = False
    n6 = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("随便说点什么"))
    await drain()
    check("over budget: an unaddressed message draws nothing", len(bot.sent) == n6)
    await GATEWAY.handle(bot, FakeEvent("小X 你还在吗"))
    await drain()
    check("over budget: being addressed does not buy an exemption",
          len(bot.sent) == n6, f"{len(bot.sent) - n6} replies past the cap")

    # 12b. What the bot knows about the group itself. These are facts whose subject is the
    # group's own entity, learned by the same extraction pass that learns everything else -
    # so they carry evidence, they supersede rather than pile up, and they age out.
    #
    # What they replaced was a paragraph of prose that a model rewrote every night from
    # its own previous paragraph. Nothing in it could be checked, deleted or forgotten,
    # and it had a loop: an in-joke written down got used in a reply, the reply was
    # archived, and the next rewrite read the bot's own words as evidence that the group
    # still said it.
    from qqbot.core import retrieval as _r
    from qqbot.domain.memory import Fact as _F, MemoryType as _MT
    from qqbot.repositories import IdentityRepository as _IR, MemoryRepository as _MR
    _gid = 123
    _subject = await _IR().group_entity(_gid)
    for _pred, _key, _val in (("topic", None, "做音乐的群"),
                              ("term", "切片", "把采样切成小段再重排")):
        await _MR().supersede(
            _F(subject_entity_id=_subject, predicate=_pred, object_key=_key,
               object_value=_val,
               group_id=_gid, memory_type=_MT.GROUP, confidence=0.5),
            [], when=_nl0())
    _facts = await _r.group_knowledge(str(_gid))
    check("the group's own facts read back",
          _facts == ["做音乐的群", "切片：把采样切成小段再重排"], str(_facts))
    check("the topic comes first", _facts[0] == "做音乐的群", str(_facts))

    _, persona_k = config().for_group("123")
    _split = prompt_mod.build_system(persona_k, [], _facts)
    check("they reach the prompt as the bot's own summary",
          "未确认（你自行归纳的印象" in _split and "切片：把采样切成小段再重排" in _split,
          _split[-200:])
    check("and the hand-written material is kept above them",
          "已确认（固定资料）" in _split
          and _split.index("已确认（固定资料）") < _split.index("未确认（你自行归纳的印象"),
          _split[-200:])

    # A group is an entity, which is what lets its facts use the whole model. Two accounts
    # merging must never reach it, and it must not turn up in anybody's roster.
    check("the group's entity is not an account in the roster",
          all(c.user_id != str(_gid) for c in await _r.directory().roster(_gid)))
    # Redefining a word supersedes; a second word is a second row. Without the word in the
    # predicate a group could hold exactly one term.
    await _MR().supersede(
        _F(subject_entity_id=_subject, predicate="term", object_key="切片",
           object_value="改了个说法",
           group_id=_gid, memory_type=_MT.GROUP, confidence=0.5),
        [], when=_nl0())
    await _MR().supersede(
        _F(subject_entity_id=_subject, predicate="term", object_key="干声",
           object_value="没加效果的人声",
           group_id=_gid, memory_type=_MT.GROUP, confidence=0.5),
        [], when=_nl0())
    _facts2 = await _r.group_knowledge(str(_gid))
    check("redefining a term replaces it rather than adding one",
          "切片：改了个说法" in _facts2 and "切片：把采样切成小段再重排" not in _facts2,
          str(_facts2))
    check("and a different term is a separate entry",
          "干声：没加效果的人声" in _facts2, str(_facts2))

    # 12d. Pictures are understood on arrival: the CDN link is freshest then, the
    # describing call is cached per unique picture, and a group the bot never answers
    # still gets a readable archive. Deferring it to reply time would leave a picture
    # posted on its own a bare marker until something drew a reply - by which time the
    # link has expired.
    cfg.budget.daily_cny_cap = 5.0      # test 12 left the cap at ~zero
    BUDGET._loaded = False
    VISION_SEEN = []

    class SeeingVision(VisionModel):
        name = "seeing"

        def rate_for(self, model):
            return Rate("Mtoken", in_miss=1.2, out=7.2)

        async def describe(self, data, *, cfg, prompt="", mime="image/jpeg", group_id=None):
            VISION_SEEN.append(len(data))
            return "一只橘猫在键盘上打滚"

        async def aclose(self):
            pass

    prev = providers_bundle.vision
    set_providers(Providers(text=FakeText(), vision=SeeingVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()))
    from qqbot.core.media import MEDIA as _MEDIA
    async def _fake_fetch(url, max_bytes):
        return b"x" * 2048

    _MEDIA._fetch = _fake_fetch
    _MEDIA._local = staticmethod(lambda path, max_bytes: None)
    _MEDIA._img_windows.clear()      # earlier sections consumed per-minute slots

    st_m = await REGISTRY.get("123")
    n_before = len(bot.sent)
    img_event = FakeEvent(segments=[
        Seg("at", {"qq": "24680"}),
        Seg("image", {"file": "A" * 32 + ".png", "url": "http://x/y.png", "summary": ""}),
    ])
    await GATEWAY.handle(bot, img_event)
    await drain(2.0)
    check("the bot did not answer it", len(bot.sent) == n_before)
    check("but the picture was understood the moment it arrived",
          len(VISION_SEEN) == 1, str(VISION_SEEN))
    stored = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(img_event.message_id))
    check("and the archive carries the description straight away",
          "橘猫" in (stored or ""), repr(stored))
    # But the free half still runs on arrival - an unresolved mention would archive as an
    # account number, which is the one thing the roster exists to keep out of the prompt.
    check("while the mention was still resolved to a name",
          "群里的阿明" in (stored or "") and "24680" not in (stored or ""), repr(stored))

    # A question about it later pays nothing more: the description was bought when the
    # picture arrived and has been sitting in the history since.
    await GATEWAY.handle(bot, FakeEvent("小X 刚那张图是什么"))
    await drain(2.0)
    check("a question about it pays nothing more",
          len(VISION_SEEN) == 1, str(VISION_SEEN))
    hist = "\n".join(
        m["content"] if isinstance(m["content"], str)
        else " ".join(b["text"] for b in m["content"] if b.get("type") == "text")
        for m in [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"])
    check("and the description lands in the history, where the question points",
          "橘猫" in hist and "群里的阿明 ⟦图片⟧" not in hist, hist[-200:])
    backfilled = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(img_event.message_id))
    check("the archive is corrected too, so memory keeps the description",
          "橘猫" in (backfilled or ""), repr(backfilled))
    await GATEWAY.handle(bot, FakeEvent("小X 那图呢"))
    await drain(2.0)
    check("and asking again does not pay for it a second time",
          len(VISION_SEEN) == 1, str(VISION_SEEN))

    # Nor does reposting it: the cache keys on the picture, not the message.
    await GATEWAY.handle(bot, FakeEvent(segments=[
        Seg("image", {"file": "A" * 32 + ".png", "url": "http://x/y2.png", "summary": ""}),
    ]))
    await drain(2.0)
    check("a repost of the same picture is answered from the cache",
          len(VISION_SEEN) == 1, str(VISION_SEEN))

    # A picture in the message that draws the reply is likewise understood on arrival -
    # in the prompt in time for the answer.
    img2 = FakeEvent(segments=[
        Seg("text", {"text": "小X 这图什么意思"}),
        Seg("image", {"file": "B" * 32 + ".png", "url": "http://x/z.png", "summary": ""}),
    ])
    await GATEWAY.handle(bot, img2)
    await drain(2.0)
    check("a picture that draws a reply is understood", len(VISION_SEEN) == 2,
          str(VISION_SEEN))
    tail_i = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"][-1]["content"]
    # The backend keeps files (upload returned an id) and the picture is in the batch,
    # so the tail is the multimodal form: the text block first, its pictures behind it.
    check("the original pixels ride behind the message that posted them",
          isinstance(tail_i, list)
          and tail_i[0].get("type") == "text"
          and any(b.get("type") == "image" and b.get("id", "").startswith("file-api-")
                  for b in tail_i[1:]), str(tail_i)[:160])
    tail_text = tail_i[0]["text"] if isinstance(tail_i, list) else tail_i
    check("and its description reaches the model", "橘猫" in tail_text, tail_text[-120:])
    stored2 = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(img2.message_id))
    check("and is backfilled into the archive", "橘猫" in (stored2 or ""), repr(stored2))

    # The money guard on this path is the daily cap, asked inside the describing call
    # itself, because nothing upstream of arrival gates spending any more.
    seen_now = len(VISION_SEEN)
    cap_was = cfg.budget.daily_cny_cap
    cfg.budget.daily_cny_cap = 0.000001
    BUDGET._loaded = False
    capped = FakeEvent(segments=[
        Seg("image", {"file": "E" * 32 + ".png", "url": "http://x/e.png", "summary": ""})])
    await GATEWAY.handle(bot, capped)
    await drain()
    check("past the daily cap a fresh picture is not described",
          len(VISION_SEEN) == seen_now, str(VISION_SEEN))
    stored_e = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(capped.message_id))
    check("and its marker stays bare, ready for a cheaper day",
          "⟦图片⟧" in (stored_e or "") and "橘猫" not in (stored_e or ""), repr(stored_e))
    cfg.budget.daily_cny_cap = cap_was
    BUDGET._loaded = False

    # "A cheaper day" must actually work: the cap turn-away is transient (Unsettled),
    # so the message's pending work survives, and money returning is all it takes.
    # Before the Unsettled distinction, the paid pass cleared pending on the fallback
    # and a cap-blocked picture could never be described again (a regression this
    # pins).
    await GATEWAY.handle(bot, FakeEvent("小X 刚才那张图是什么"))
    await drain(2.0)
    check("with money back, a cap-blocked picture is described on the next ask",
          len(VISION_SEEN) == seen_now + 1, str(VISION_SEEN))
    stored_e2 = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(capped.message_id))
    check("and its archive line is healed",
          "橘猫" in (stored_e2 or ""), repr(stored_e2))

    # A picture nobody addresses costs exactly one describing call - the whole rule is
    # now pay per unique picture, not per reply that happens to read one.
    n_sent, n_seen = len(bot.sent), len(VISION_SEEN)
    img3 = FakeEvent(segments=[
        Seg("text", {"text": "随手发张图"}),
        Seg("image", {"file": "C" * 32 + ".png", "url": "http://x/w.png", "summary": ""}),
    ])
    await GATEWAY.handle(bot, img3)
    await drain(2.0)
    check("a picture nobody asks about is described exactly once",
          len(VISION_SEEN) == n_seen + 1, str(VISION_SEEN))
    check("and draws no reply", len(bot.sent) == n_sent)
    # But the free half still ran, so the message is in the history as something with a
    # picture in it, ready to be understood if the next line asks about it.
    check("while the message itself is still recorded",
          any(m.msg_id == str(img3.message_id) for m in st_m.recent))

    set_providers(Providers(text=FakeText(), vision=prev,
                            asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()))
    # A describe slower than every wait window must still land: the waiters give up,
    # the flight is not cancelled, and the description reaches the cache and the
    # message for the next turn. The old shape cancelled the task on timeout, which
    # left empty cache rows and images that could never be described (a real
    # incident, not a hypothetical). Two sightings of the same key during the flight
    # must also share it - one paid call, not two.
    # The wait is per-group config, so shorten the one this group resolves to.
    _cfg_media = config().for_group("123")[0]
    _waits = _cfg_media.gateway.media_wait_sec
    _cfg_media.gateway.media_wait_sec = 0.2

    class SlowVision(SeeingVision):
        async def describe(self, data, *, cfg, prompt="", mime="image/jpeg", group_id=None):
            VISION_SEEN.append(len(data))
            await asyncio.sleep(1.0)
            return "慢速描述完成"

    set_providers(Providers(text=FakeText(), vision=SlowVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()))
    _slow_seen = len(VISION_SEEN)
    slow1 = FakeEvent(segments=[
        Seg("image", {"file": "D" * 32 + ".png", "url": "http://x/slow.png", "summary": ""})])
    slow2 = FakeEvent(segments=[
        Seg("image", {"file": "D" * 32 + ".png", "url": "http://x/slow2.png", "summary": ""})])
    await GATEWAY.handle(bot, slow1)
    await asyncio.sleep(0.1)
    await GATEWAY.handle(bot, slow2)      # same picture, mid-flight
    await drain(0.5)                      # every wait window has expired by now
    check("the slow flight is still in the air, uncancelled",
          len(VISION_SEEN) == _slow_seen + 1, str(VISION_SEEN[_slow_seen:]))
    await drain(1.2)
    check("and its description lands after every waiter gave up",
          await _repo.image_cache_get("d" * 32) == "⟦图片:慢速描述完成⟧",
          str(await _repo.image_cache_get("d" * 32)))
    _slow_stored = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(slow1.message_id))
    check("the archive is patched by the task itself, no reply needed",
          "慢速描述完成" in (_slow_stored or ""), repr(_slow_stored))
    check("two sightings during the flight paid for one call",
          len(VISION_SEEN) == _slow_seen + 1, str(VISION_SEEN[_slow_seen:]))
    _cfg_media.gateway.media_wait_sec = _waits

    # A media patch must not undo the arrival truncation: pm.parts holds the full
    # original text, so an unbounded re-render would put the whole thing back into
    # the window and the archive - past the one per-line bound the no-token-budget
    # prompt layout relies on (a regression this pins).
    _cap_len = cfg.gateway.max_msg_len

    class LongVision(SeeingVision):
        async def describe(self, data, *, cfg, prompt="", mime="image/jpeg", group_id=None):
            VISION_SEEN.append(len(data))
            return "长" * (_cap_len + 500)

    set_providers(Providers(text=FakeText(), vision=LongVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()))
    _MEDIA._img_windows.clear()
    longe = FakeEvent(segments=[
        Seg("text", {"text": "看这张超长描述的图"}),
        Seg("image", {"file": "F" * 32 + ".png", "url": "http://x/f.png", "summary": ""}),
    ])
    await GATEWAY.handle(bot, longe)
    await drain(2.0)
    _lm = next((m for m in st_m.recent if m.msg_id == str(longe.message_id)), None)
    check("a media patch respects the per-line bound in the window",
          _lm is not None and len(_lm.text) <= _cap_len,
          str(_lm and len(_lm.text)))
    _lstored = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(longe.message_id))
    check("and in the archive",
          _lstored and len(_lstored) <= _cap_len, str(len(_lstored or "")))

    # open_image: the reply model reads pictures itself, so the tool hands it one by
    # number rather than asking a second model to look. Free - bytes and an upload -
    # and it answers with a content array carrying the file block, which is what lets
    # the picture arrive as the answer to the call instead of a turn appended behind it.
    import json as _json
    from qqbot.core.segments import ImageRef as _IRef
    from qqbot.core.tools import Attachment, ToolCtx, execute as _texec
    URL_CONTENT_CHARS = config().default.retrieval.url_content_chars

    def _tcall(name, **kw):
        return {"id": "t1", "function": {
            "name": name, "arguments": _json.dumps(kw, ensure_ascii=False)}}

    set_providers(Providers(text=FakeText(), vision=SeeingVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()))
    _imsg = _CM0(msg_id="ins1", user_id="1", nickname="某人", text="⟦图片⟧", ts=_nl0(),
                 image_refs=[_IRef(slot=0, key="9" * 32, url="http://x/ins.png")])
    _ictx = ToolCtx(bot=bot, by_pic={4: (_imsg, 0)})
    _seen0 = len(VISION_SEEN)
    out_i = await _texec(_tcall("open_image", n=4), cfg=cfg, group_id="123", ctx=_ictx)
    check("open_image hands back the original as a picture part",
          isinstance(out_i, Attachment)
          and any(b.get("type") == "image" for b in out_i.blocks), str(out_i.blocks))
    check("and costs no model call - it is a fetch, not a second opinion",
          len(VISION_SEEN) == _seen0, str(len(VISION_SEEN) - _seen0))
    check("the tool message carries the picture beside its text",
          [b["type"] for b in out_i.content()] == ["image", "text"],
          str(out_i.content()))
    # The neutral part never reaches a vendor: each backend renders it into its own
    # wire shape on the way out, and they really do differ - DeepSeek takes a flat
    # file_id and rejects the nesting OpenAI documents.
    from qqbot.providers.deepseek import DeepSeekChat as _DSC
    from qqbot.providers.openai_compat import OpenAICompatChat as _OAC
    _neutral = [{"role": "user", "content": out_i.content()}]
    check("the picture part is translated per backend, not shipped as written",
          _OAC()._wire(_neutral)[0]["content"][0]
          == {"type": "file", "file": {"file_id": "file-api-fake2048"}}
          and _DSC()._wire(_neutral)[0]["content"][0]
          == {"type": "file", "file_id": "file-api-fake2048"},
          str(_DSC()._wire(_neutral)[0]["content"][0]))
    check("and the caller's own messages are left as they were",
          _neutral[0]["content"][0] == {"type": "image", "id": "file-api-fake2048"})
    check("a number outside the prompt is answered, not crashed",
          "99" in await _texec(_tcall("open_image", n=99),
                               cfg=cfg, group_id="123", ctx=_ictx))
    check("a missing number is answered too",
          "编号" in await _texec(_tcall("open_image"), cfg=cfg, group_id="123",
                                 ctx=_ictx))
    # "null", "[]" and "42" are valid JSON: a degenerate argument string must get
    # the same in-band answer as an unparsable one, not crash the whole reply.
    out_n = await _texec(
        {"id": "t2", "function": {"name": "web_search", "arguments": "null"}},
        cfg=cfg, group_id="123", ctx=_ictx)
    check("non-object tool arguments are answered in-band",
          out_n.startswith("（工具参数解析失败"), repr(out_n))

    # read_url: page text through the search backend's extract, capped before it
    # reaches the prompt - pages are unbounded, replies are not.
    class ReadingSearch(UnusedSearch):
        async def extract(self, url, *, cfg, group_id=None):
            return "页 " * 4000

    set_providers(Providers(text=FakeText(), vision=SeeingVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=ReadingSearch()))
    out_u = await _texec(_tcall("read_url", url="https://a.example/x"),
                         cfg=cfg, group_id="123")
    check("read_url returns page text capped at the limit",
          out_u.startswith("页")
          and len(out_u) <= URL_CONTENT_CHARS + 40, str(len(out_u)))
    check("a non-http url is refused in words",
          "http" in await _texec(_tcall("read_url", url="ftp://x"),
                                 cfg=cfg, group_id="123"))
    set_providers(Providers(text=FakeText(), vision=SeeingVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()))

    # The window still remembers where this section's pictures were filed, and every
    # later reply in the group would attach them; the sections below assert on plain
    # string tails, so put the fixture world back the way they expect it.
    for m in st_m.recent:
        m.images = []
        m.pending = None      # or the backlog pass re-files them on the next reply

    # 12e. reply and forward segments reach the model as content
    await REGISTRY.get("123")
    # Addressed, because that is the only way a reply happens now - and quoting somebody
    # while asking the bot about it is exactly the shape this checks.
    await GATEWAY.handle(bot, FakeEvent(segments=[
        Seg("at", {"qq": "24680"}),
        Seg("text", {"text": "小X 这个怎么说"}),
    ], reply_id=555, reply_from="24680"))
    await drain(2.0)
    tail_r = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"][-1]["content"]
    # Message 555 is not one the bot has seen, so the quote is reported as unavailable
    # rather than fetched: text on screen belonging to no visible line is the ambiguity
    # numbering replaced.
    check("a quote of something off screen says so", "⟦回复更早的消息⟧" in tail_r,
          tail_r[-140:])
    check("@someone resolves to a name, not a number",
          "群里的阿明" in tail_r and "@24680" not in tail_r, tail_r[-120:])
    # No name cache of our own any more: the protocol side is asked for the group's
    # member list, which is both simpler and answers for people who never spoke.
    from qqbot.core.media import MEDIA as _M
    check("media keeps no name cache of its own", not hasattr(_M, "_names"))
    await GATEWAY.handle(bot, FakeEvent(segments=[
        Seg("forward", {"id": "ff"}),
        Seg("text", {"text": "小X 看看这个"}),
    ]))
    await drain(2.0)
    tail_f = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["messages"][-1]["content"]
    check("a forwarded bundle is expanded", "第一条" in tail_f and "第二条" in tail_f,
          tail_f[-140:])

    # The trigger reads the text as it arrived, never the resolved form: a forwarded
    # conversation that merely *mentions* the bot's name inside is nothing anybody
    # typed at the bot, and must not draw a reply. (Being spoken to is a typed @ or
    # a typed nickname; resolved media text is neither.)
    class _NameDropBot(bot.__class__):
        async def call_api(self, api, **kw):
            if api == "get_forward_msg":
                return {"messages": [
                    {"type": "node",
                     "data": {"sender": {"nickname": "阿强"},
                              "message": [{"type": "text",
                                           "data": {"text": "上次小X说得真对"}}]}}]}
            return await super().call_api(api, **kw)
    _nb = _NameDropBot()
    await GATEWAY.handle(_nb, FakeEvent(segments=[Seg("forward", {"id": "ffn"})]))
    await drain(2.0)
    check("a forward that names the bot inside does not trigger",
          len(_nb.sent) == 0, str(_nb.sent))

    # The roster is the whole group in a fixed order, and the reason is the prefix cache:
    # it sits in the system block ahead of the history, and a cache matches from the
    # beginning, so anything that reorders per turn invalidates the history behind it every
    # turn - so it must never be built from whoever spoke most recently.
    from qqbot.core import retrieval as _retr
    _DIR = _retr.directory()
    for i, uid in enumerate(["a", "b", "c", "d", "e", "f"]):
        await seed(ORD, uid, uid, n=6 - i)           # a is busiest, f quietest
        await _DIR.note(ORD, uid, f"note about {uid}")
    await seed(ORD, "tie1", "tie1")                  # equal counts, must not swap
    await seed(ORD, "tie2", "tie2")
    await _DIR.note(ORD, "tie1", "first of the pair")
    await _DIR.note(ORD, "tie2", "second of the pair")

    _roster = [r["user_id"] for r in await _retr.gather(group_id=str(ORD), bot=bot)]
    check("the roster is ordered by activity, not by recency",
          _roster[:6] == ["a", "b", "c", "d", "e", "f"], str(_roster))
    check("and equal counts keep a stable order",
          _roster[6:] == ["tie1", "tie2"], str(_roster[6:]))
    check("the same call twice gives the same order",
          [r["user_id"] for r in await _retr.gather(group_id=str(ORD), bot=bot)] == _roster)

    # Someone who has not spoken this turn is in it too - which is what removes the need
    # to work out who a message is about before deciding whose record to load.
    check("everyone with a record is in it, whoever is speaking", len(_roster) == 8,
          str(len(_roster)))

    # Isolation, at the level the whole design turns on: a group's roster is built from
    # events in that group, so somebody talkative elsewhere is simply not here.
    await seed(OTHER, "a", "a", n=9)
    await _DIR.note(OTHER, "a", "known only in the other group")
    _elsewhere = await _retr.gather(group_id=str(OTHER), bot=bot)
    check("a roster does not reach into another group",
          [r["user_id"] for r in _elsewhere] == ["a"], str(_elsewhere))
    check("and a note written there stays there",
          _elsewhere[0]["manual_note"] == "known only in the other group"
          and next(r for r in await _retr.gather(group_id=str(ORD), bot=bot)
                   if r["user_id"] == "a")["manual_note"] == "note about a")

    # What an owner writes by hand and what the model worked out are two different kinds
    # of claim, and the prompt says so: one undifferentiated list teaches the model
    # to repeat its own inferences back as though somebody had told it.
    from qqbot.domain.memory import Fact, MemoryType
    from qqbot.repositories import MemoryRepository
    _MEMREPO = MemoryRepository()
    _eid = (await _DIR.person(ORD, "a")).entity_id
    await _MEMREPO.supersede(
        Fact(subject_entity_id=_eid, predicate="likes", object_value="打游戏",
             memory_type=MemoryType.PREFERENCE, group_id=ORD, confidence=0.8),
        [], when=_nl0())
    _card = await _DIR.person(ORD, "a")
    check("a hand-written note is kept apart from what was extracted",
          _card.note == "note about a" and _card.summary == "喜欢打游戏",
          f"{_card.note!r} / {_card.summary!r}")

    _, _p_o = config().for_group(str(ORD))
    _sys = prompt_mod.build_system(_p_o, await _retr.gather(group_id=str(ORD)), "")
    check("the certain half is labelled certain", "已确认（系统记录的名字" in _sys, _sys[-200:])
    check("and the guessed half is labelled guessed",
          "未确认（你自行归纳的印象" in _sys and "喜欢打游戏" in _sys, _sys[-200:])
    check("certain sits above guessed",
          _sys.index("已确认（系统记录的名字") < _sys.index("未确认（你自行归纳的印象"))
    # Stable first: a daily card rewrite must not also invalidate the constants above it.
    check("constants sit above everything that changes",
          _sys.index("【信息解读规则】") < _sys.index("【群成员名册】"))

    # A renamed account keeps the name it used to go by. That is not a column any more -
    # a group card is one alias among several, and changing it only adds another - which
    # is why "what was he called before" is answerable at all.
    # The old card has to have endured into a second day to count as a durable former
    # name - a card worn for one day stays a candidate, which is what keeps one evening
    # of joke renames out of the roster. Endurance is simulated by backdating the
    # evidence trail, which is also what decides it in production.
    await seed(ORD, "g", "旧名字")
    await pool().execute(
        """UPDATE alias_evidence SET created_at = created_at - INTERVAL '1 day'
            WHERE alias_id IN (SELECT id FROM alias WHERE group_id=$1
                                AND alias_text='旧名字')""", ORD)
    await seed(ORD, "g", "旧名字")
    await seed(ORD, "g", "新名字")
    _renamed = await _DIR.person(ORD, "g")
    check("a rename leaves the old name behind rather than overwriting it",
          set(_renamed.other_names) == {"旧名字"} and _renamed.display == "新名字",
          f"{_renamed.display} / {_renamed.other_names}")
    check("but a card worn only today is not yet a durable name",
          "新名字" in [n.text for n in _renamed.candidates],
          str([n.text for n in _renamed.candidates]))

    # A name the account displayed and a name the group uses are different claims, and the
    # prompt says which is which. Run together, a hand-registered nickname was presented
    # as a name the account had once carried - the opposite of the truth.
    await _DIR.name(ORD, "g", "老哥")
    _named = await _DIR.person(ORD, "g")
    check("names the account displayed are kept apart from what people call him",
          set(_named.displayed_names) == {"旧名字"} and set(_named.nicknames) == {"老哥"},
          f"{_named.displayed_names} / {_named.nicknames}")
    _row = next(r for r in await _retr.gather(group_id=str(ORD)) if r["user_id"] == "g")
    _one = prompt_mod.build_system(_p_o, [_row], "")
    check("and the prompt labels them separately",
          "曾用名：旧名字" in _one and "别名：老哥" in _one, _one[-200:])

    # Two accounts, one person: the whole reason the identity layer exists. After a merge
    # the roster has one line, not two, and the message counts add up.
    await seed(ORD, "alt", "小号")
    await _DIR.merge("alt", "g")
    _merged = await _DIR.person(ORD, "alt")
    check("a merged account resolves to the same person",
          _merged.entity_id == _renamed.entity_id and _merged.merged, str(_merged.accounts))
    check("and the roster shows them once, with the counts added",
          len([r for r in await _retr.gather(group_id=str(ORD), bot=bot)
               if r["user_id"] in ("g", "alt")]) == 1)

    # And the undo, which is the reason evidence is kept on everything.
    await _DIR.split("alt")
    _after = await _DIR.person(ORD, "alt")
    check("splitting gives the account its own person again",
          _after.entity_id != _renamed.entity_id and not _after.merged, str(_after.accounts))
    check("names it produced itself go with it",
          "小号" in _after.other_names + (_after.display,), str(_after.other_names))
    check("names belonging to the other account stay behind",
          "新名字" not in _after.other_names + (_after.display,), str(_after.other_names))

    # The protocol side already knows every group card, including for people who have
    # never spoken - so ask it once for the whole group instead of keeping a second cache.
    from qqbot.core.members import MEMBERS as _MEM
    _MEM.forget()
    before = bot.member_list_calls
    live = await _MEM.names_of(bot, "123", ["7", "24680"])
    check("the member list answers with current group cards",
          live == {"7": "小南", "24680": "群里的阿明"}, str(live))
    check("one call covers the whole group", bot.member_list_calls == before + 1,
          f"{before} -> {bot.member_list_calls}")
    await _MEM.names_of(bot, "123", ["7"])
    check("a second lookup reuses it", bot.member_list_calls == before + 1)

    # Identity is in the cached prefix, never the per-turn tail: someone can be talked
    # about while absent, and a tail rebuilt every turn would pay for it every turn.
    _one = [_CM0(msg_id="x", user_id="u7", nickname="小南", text="在", ts=_nl0())]
    check("identity is in the system block, not the tail",
          "曾用名" not in prompt_mod.build_tail(batch=_one))

    # The tail carries nothing but the clock and the message on purpose: whatever
    # sits here is the nearest context the incoming message has, and a pushed block
    # of past events once captured an elliptical question that referred to the
    # conversation. The past is pulled through recall_events, never pushed.
    _tail = prompt_mod.build_tail(batch=_one)
    check("the tail is the clock and the message, nothing pushed beside them",
          _tail.index("当前时间") < _tail.index("下面是刚收到的消息")
          and "相关的事" not in _tail, _tail[:120])
    # The tail has to say which message is the question. Without that, "reply with a
    # message" and "reply to one of the messages" are the same sentence in Chinese, and
    # the model answered whichever thread in the history looked livelier - leaving the
    # person who had actually addressed it with no answer.
    check("and the tail says which message to answer",
          "只回复「刚收到的消息」下面的那一条" in _tail and "背景" in _tail,
          _tail[-140:])
    # And says it without pointing two ways at once: the messages are introduced as being
    # below, so the instruction must not then refer to them as being above.
    check("and refers to it by description, not by direction",
          "上面这条" not in _tail, _tail[-140:])
    # The exception matters as much as the rule. Being asked to answer something raised
    # earlier is ordinary, and a flat ban on the history would refuse it.
    check("while still allowing a question the message points at",
          "除非这条刚收到的消息明确要你代答" in _tail, _tail[-140:])
    check("but never in the cached system block",
          "老周答应周末把切片做完" not in prompt_mod.build_system(persona_k, [], []))

    # 12g. A name is captured when a message arrives, so history would otherwise keep
    # showing whoever renamed themselves under their old name while the roster, the
    # mentions and the group card all moved on - the same person under two names.
    _hist = [
        _CM0(msg_id="r1", user_id="7", nickname="以前的名字", text="早", ts=_nl0()),
        _CM0(msg_id="r2", user_id="999", nickname="", text="早啊", ts=_nl0(), is_bot=True),
        _CM0(msg_id="r3", user_id="u404", nickname="没在群里", text="?", ts=_nl0()),
    ]
    n = await _MEM.relabel(bot, "123", _hist)
    check("a renamed speaker is relabelled in history", n == 1 and _hist[0].nickname == "小南",
          f"{n} {[m.nickname for m in _hist]}")
    check("someone the member list does not cover keeps their captured name",
          _hist[2].nickname == "没在群里")
    check("the bot's own lines are left alone", _hist[1].nickname == "")
    check("re-running it changes nothing", await _MEM.relabel(bot, "123", _hist) == 0)

    # 13. a group nobody configured. There is no allowlist: the account is a dedicated
    # bot account, so a group it should not serve is a group it is not in. A new group
    # therefore has to work with zero configuration - default persona, served from the
    # first message - and has to be *noticed*, since nothing else records that it exists.
    # Last, because it puts traffic through a second group.
    cfg.budget.daily_cny_cap = 5.0
    BUDGET._loaded = False
    n_new = len(bot.sent)
    fresh_gid = 4242
    check("a group with no config file falls back to the default persona",
          config().persona_for(str(fresh_gid)).name
          == config().persona_for("no-such-group").name)
    check("and nothing has claimed it yet",
          await _repo.note_group_seen(fresh_gid) is True)
    check("but claiming it twice reports it only once",
          await _repo.note_group_seen(fresh_gid) is False)
    check("the day it appeared is on the record for the daily report",
          fresh_gid in await _repo.groups_first_seen_on(_today()),
          str(await _repo.groups_first_seen_on(_today())))

    # An unconfigured group is served, archived and answered like any other.
    unconfigured = FakeEvent("小X 在吗", group_id=999)
    await GATEWAY.handle(bot, unconfigured)
    await drain()
    check("an unconfigured group gets a reply", len(bot.sent) == n_new + 1)
    check("and its messages are archived",
          await pool().fetchval(
              "SELECT count(*) FROM raw_event WHERE platform_event_id=$1",
              str(unconfigured.message_id)) == 1)
    check("first sight of it was recorded when its state loaded",
          999 in await _repo.groups_with_state())

    # 14. the money-bounded agent loop. There is no round count and no per-tool quota:
    # every round is a paid model call charged to the reply's scope, the first round
    # always runs, and the budget gate sits between a round's tool requests and their
    # execution - once the cap has been spent, tools whose results no further round
    # could read never run. The gate reads money already spent, never a forecast of
    # the next round: a forecast needs a price for the model, and an unpriced model
    # then fails it forever, taking the tool loop dark at zero spend.
    # Either limit (the purse, the search allowance) ends the *spending*: one tool-less
    # wrap-up round then answers from what the paid rounds already fetched. Every
    # number below is arranged so the arithmetic is checkable by hand.
    from qqbot.core import engine as _eng

    ROUND_CHARGE = 0.02    # what the scripted model books per round
    cfg.budget.per_reply_cny = 0.10
    # With a 0.10 purse and 0.02 a round, the money never binds in this scenario:
    # round one executes two searches, round two's fresh search is the third call
    # and the scripted allowance dies there.

    class CountingSearch(SearchEngine):
        """Free like the real one, and out of allowance from the third call on."""

        name = "counting"
        calls: list = []

        def rate_for(self, model):
            return Rate("call", per_unit=0.0)

        async def search(self, query, *, cfg, group_id=None):
            if len(self.calls) >= 2:
                raise QuotaExhausted("out of allowance")
            self.calls.append(query)
            await BUDGET.record(kind="search", model="counting", cny=0.0,
                                group_id=group_id)
            return [{"title": "T", "content": "C"}]

        async def aclose(self):
            pass

    def _tc(q):
        import json as _json
        return {"id": f"t{len(LLM_CALLS)}", "type": "function",
                "function": {"name": "web_search",
                             "arguments": _json.dumps({"query": q})}}

    class ScriptedText(FakeText):
        """Asks for searches on every round - the same word twice, plus a fresh one -
        so only a limit can end the reply."""

        async def chat(self, messages, *, cfg, tools=None,
                       max_tokens=None, effort=None, kind="reply",
                       group_id=None):
            LLM_CALLS.append({"kind": kind, "messages": messages,
                              "tools": tools, "effort": effort,
                              "max_tokens": max_tokens,
                              "grade": cfg.reasoning_effort,
                              "timeout": cfg.timeout_sec})
            await BUDGET.record(kind=kind, model=self.MODEL, cny=ROUND_CHARGE,
                                group_id=group_id)
            if tools is None:   # the wrap-up round: no tools offered, answer given
                return ChatResult(text="就查到这些了", model=self.MODEL)
            return ChatResult(text="", model=self.MODEL,
                              tool_calls=[_tc("话题A"), _tc("话题A"), _tc(f"话题{len(LLM_CALLS)}")])

    set_providers(Providers(text=ScriptedText(), vision=UnusedVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=CountingSearch()))
    n_llm = len(LLM_CALLS)
    st13 = await REGISTRY.get("123")
    text, _prov1, _tr1 = await _eng.generate(
        bot=bot, st=st13, cfg=cfg, persona=config().for_group("123")[1],
        batch=[_CM0(msg_id="loop1", user_id="u1", nickname="阿强",
                    text="帮我查个东西", ts=_nl0())])
    # The scripted allowance dies on the third search, in round two: the limit ends
    # the spending, and a tool-less wrap-up round answers from what rounds one and
    # two already fetched (the daily cap, checked before anything is spent, still
    # means silence).
    check("a reply that hits the search allowance wraps up with an answer",
          text == "就查到这些了", repr(text))
    check("the wrap-up is one extra round, offered no tools",
          len(LLM_CALLS) - n_llm == 3 and LLM_CALLS[-1]["tools"] is None,
          f"{len(LLM_CALLS) - n_llm} rounds, tools={LLM_CALLS[-1]['tools']!r}")
    check("the wrap-up round is told the allowance is gone",
          any("额度已用完" in (m.get("content") or "")
              for m in LLM_CALLS[-1]["messages"] if m.get("role") == "user"))
    check("free searches are not gated by the reply's purse",
          len(CountingSearch.calls) == 2,
          f"{len(CountingSearch.calls)} calls: {CountingSearch.calls}")
    check("a repeated identical call is not re-executed",
          CountingSearch.calls.count("话题A") == 1, str(CountingSearch.calls))
    tool_texts = [m.get("content") or "" for m in LLM_CALLS[-1]["messages"]
                  if m.get("role") == "tool"]
    check("the model is told about the duplicate in words",
          any("刚执行过" in t for t in tool_texts))
    check("the unexecuted request got its placeholder result",
          any("没有执行" in t for t in tool_texts))

    # And the other limit the same way: with the allowance out of the picture and the
    # purse shrunk to 0.04, rounds one and two run and spend it exactly - the gate
    # then reads the purse as empty, the pending tool requests never run, and the
    # wrap-up answers from what the first two rounds fetched.
    class EndlessSearch(CountingSearch):
        """CountingSearch without the allowance: only the purse can end this one."""

        calls = []

        async def search(self, query, *, cfg, group_id=None):
            self.calls.append(query)
            return [{"title": "T", "content": "C"}]

    cfg.budget.per_reply_cny = 0.04
    set_providers(Providers(text=ScriptedText(), vision=UnusedVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=EndlessSearch()))
    n_llm2 = len(LLM_CALLS)
    text2, _prov2, _tr2 = await _eng.generate(
        bot=bot, st=st13, cfg=cfg, persona=config().for_group("123")[1],
        batch=[_CM0(msg_id="loop2", user_id="u1", nickname="阿强",
                    text="再查个东西", ts=_nl0())])
    check("a reply that runs out of money wraps up the same way",
          text2 == "就查到这些了" and len(LLM_CALLS) - n_llm2 == 3
          and LLM_CALLS[-1]["tools"] is None,
          f"{text2!r}, {len(LLM_CALLS) - n_llm2} rounds")
    check("and the unaffordable round's tools were never executed",
          len(EndlessSearch.calls) == 2, str(EndlessSearch.calls))

    # A reply that searched leaves its provenance on the archived line: what the
    # group read carries no marker, what the bot remembers does. credibility_rules then
    # lets a later turn cite the marked line instead of re-searching, and treats
    # unmarked lines as improvised off the context.
    from qqbot.core.engine import _provenance as _pvfn
    from qqbot.core.output import clean_reply as _cr
    check("provenance names the tool and the query",
          _pvfn([("web_search", {"query": "明天 天气"}, "1. T C")])
          == "⟦依据:搜索“明天 天气”⟧",
          _pvfn([("web_search", {"query": "明天 天气"}, "1. T C")]))
    check("no tools means no marker", _pvfn([]) == "")
    from qqbot.core.engine import _trace as _trfn
    check("no tools means no trace either", _trfn([]) == "")

    class OneSearchText(FakeText):
        _asked = False

        async def chat(self, messages, *, cfg, tools=None, max_tokens=None,
                       effort=None, kind="reply", group_id=None):
            LLM_CALLS.append({"kind": kind, "messages": messages, "tools": tools,
                              "effort": effort, "max_tokens": max_tokens,
                              "grade": cfg.reasoning_effort,
                              "timeout": cfg.timeout_sec})
            if not type(self)._asked:
                type(self)._asked = True
                return ChatResult(text="", model=self.MODEL,
                                  tool_calls=[_tc("明天 天气")])
            return ChatResult(text="明天多云", model=self.MODEL)

    cfg.budget.per_reply_cny = 0.30
    set_providers(Providers(text=OneSearchText(), vision=UnusedVision(),
                            asr=UnusedAsr(), embedding=_EMBED, search=EndlessSearch()))
    st_pv = await REGISTRY.get("123")
    ok_pv = await _eng.respond(
        bot=bot, st=st_pv, cfg=cfg, persona=config().for_group("123")[1],
        batch=[_CM0(msg_id="pv1", user_id="u1", nickname="阿强",
                    text="明天天气怎样", ts=_nl0())])
    check("the searched reply is sent without the marker",
          ok_pv and bot.sent[-1][1] == "明天多云", str(bot.sent[-1:]))
    _pv_line = st_pv.recent[-1]
    check("but the window remembers what it rested on",
          _pv_line.is_bot and _pv_line.text == "明天多云 ⟦依据:搜索“明天 天气”⟧",
          repr(_pv_line.text))
    _pv_row = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", _pv_line.msg_id)
    check("and so does the archive",
          "⟦依据:搜索“明天 天气”⟧" in (_pv_row or ""), repr(_pv_row))
    check("an imitated provenance marker never reaches the group",
          _cr("明天多云 [依据:搜索“天气”]") == "明天多云",
          repr(_cr("明天多云 [依据:搜索“天气”]")))
    # The quote pointer is transcript notation too - the real quote is the reply
    # segment the send path attaches. Asking in the prompt did not hold, so the
    # output layer strips it like every other imitated marker - but only at line
    # starts, where format imitation lives; mid-sentence it is likelier the
    # reply's own content, and where the readings collide the guard declines.
    check("an imitated quote pointer never reaches the group",
          _cr("[回复 #26] 这波我不评价") == "这波我不评价",
          repr(_cr("[回复 #26] 这波我不评价")))
    check("its lost-message form too, on its own line",
          _cr("好的\n[回复更早的消息] 我看看") == "好的\n我看看",
          repr(_cr("好的\n[回复更早的消息] 我看看")))
    check("but a mid-sentence mention is content and stays",
          _cr("他原话就带着 [回复 #3] 这几个字") == "他原话就带着 [回复 #3] 这几个字",
          repr(_cr("他原话就带着 [回复 #3] 这几个字")))
    # Every strip is counted: the guard doubles as the online sensor, and the
    # daily report reads these to show format discipline regressing at the source.
    from qqbot.core import output as _out
    _n0 = _out.STRIPPED["quote_mark"]
    _cr("[回复 #5] 好")
    check("a stripper hit is counted for the daily report",
          _out.STRIPPED["quote_mark"] == _n0 + 1, str(dict(_out.STRIPPED)))
    _n1 = _out.STRIPPED["quote_mark"]
    _cr("干净的正文")
    check("a clean reply counts nothing", _out.STRIPPED["quote_mark"] == _n1)
    # Order matters: the quote mark is stripped first, or a line imitating both
    # markers would shed the quote and keep the uncovered line number.
    check("a quote mark hiding a line number uncovers nothing",
          _cr("[回复 #2] #3 阿强: 都别吵了") == "都别吵了",
          repr(_cr("[回复 #2] #3 阿强: 都别吵了")))

    # The trajectory lives in reply_trace and nowhere else: the deque holds only
    # conversation, and prompt assembly queries the table for the window's replies
    # and seats each entry directly before the reply it fed - which also covers
    # the restart case with no re-seating logic at all.
    _expected_trace = "⟦检索记录⟧\n搜索“明天 天气”：1. T C"
    check("the trajectory persists in its own table",
          await pool().fetchval(
              "SELECT content FROM reply_trace WHERE reply_event_id=$1",
              _pv_line.msg_id) == _expected_trace,
          str(await pool().fetchval(
              "SELECT content FROM reply_trace WHERE reply_event_id=$1",
              _pv_line.msg_id)))
    check("and the deque holds only conversation",
          not any(m.text.startswith("[检索记录]") for m in st_pv.recent))
    _t2, _p2, _tr2b = await _eng.generate(
        bot=bot, st=st_pv, cfg=cfg, persona=config().for_group("123")[1],
        batch=[_CM0(msg_id="pv2", user_id="u1", nickname="阿强",
                    text="后天呢", ts=_nl0())])
    _msgs2 = list(LLM_CALLS[-1]["messages"])
    _ri = next(i for i, m in enumerate(_msgs2)
               if m.get("role") == "assistant"
               and isinstance(m.get("content"), str)
               and "⟦依据:搜索“明天 天气”⟧" in m["content"])
    check("assembly seats the stored trace directly before its reply",
          _msgs2[_ri - 1].get("role") == "assistant"
          and _msgs2[_ri - 1].get("content") == _expected_trace,
          str(_msgs2[_ri - 1])[:160])
    check("an imitated trace marker line never reaches the group",
          _cr("[检索记录]\n搜索“x”：y\n好的") == "搜索“x”：y\n好的",
          repr(_cr("[检索记录]\n搜索“x”：y\n好的")))
    set_providers(providers_bundle)

    # 15. the per-group blocklist, end to end. Blocked means unanswered and
    # nothing more: the message still archives - a hole where a person used to
    # be reads as broken context - and the list survives a restart via its own
    # table. The accepted price is that a blocked account still feeds memory.
    st15 = await REGISTRY.get("123")
    st15.blocked["bad1"] = None
    await _repo.block(123, "bad1")
    n_sent15 = len(bot.sent)
    ev_blocked = FakeEvent("小X 在吗", user_id="bad1", nickname="捣乱的", to_me=True)
    await GATEWAY.handle(bot, ev_blocked)
    await drain()
    check("a blocked account draws no reply even when it @s the bot",
          len(bot.sent) == n_sent15, f"{len(bot.sent) - n_sent15} sent")
    check("but its message still archives, keeping the window coherent",
          await pool().fetchval(
              "SELECT count(*) FROM raw_event WHERE platform_event_id=$1",
              str(ev_blocked.message_id)) == 1)
    fresh15 = type(st15)(group_id="123")
    await fresh15.load()
    check("the blocklist survives a restart", "bad1" in fresh15.blocked,
          str(fresh15.blocked))
    st15.blocked.pop("bad1", None)
    await _repo.unblock(123, "bad1")
    await GATEWAY.handle(bot, FakeEvent("小X 还在吗", user_id="bad1",
                                        nickname="捣乱的", to_me=True))
    await drain()
    check("unblocking restores replies", len(bot.sent) == n_sent15 + 1)

    # A timed block lapses by being noticed: the first message past its expiry
    # answers normally, prunes the in-memory entry and sweeps the DB row.
    from datetime import timedelta as _btd
    st15.blocked["bad1"] = _nl0() - _btd(seconds=1)
    await _repo.block(123, "bad1", until=_nl0() - _btd(seconds=1))
    n_lapse = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("小X 醒了吗", user_id="bad1",
                                        nickname="捣乱的", to_me=True))
    await drain()
    check("a lapsed timed block no longer blocks", len(bot.sent) == n_lapse + 1,
          f"{len(bot.sent) - n_lapse} sent")
    check("and its entry is gone from memory", "bad1" not in st15.blocked,
          str(st15.blocked))
    check("and its row is gone from the table",
          await pool().fetchval(
              "SELECT count(*) FROM group_blocklist WHERE group_id=123"
              " AND user_id='bad1'") == 0)
    # A still-running timed block behaves like any block.
    st15.blocked["bad1"] = _nl0() + _btd(hours=1)
    await _repo.block(123, "bad1", until=_nl0() + _btd(hours=1))
    n_live = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("小X 在么", user_id="bad1",
                                        nickname="捣乱的", to_me=True))
    await drain()
    check("a running timed block still blocks", len(bot.sent) == n_live)
    st15.blocked.pop("bad1", None)
    await _repo.unblock(123, "bad1")

    # 16. the consent gate: a member who never accepted the user agreement is
    # not replied to - they get the agreement text instead, once per cooldown,
    # with no model call and nothing spent - and /agree opens the door. Their
    # messages archive like anyone's; only the reply is withheld.
    from qqbot.core import agreement as _agree
    await REGISTRY.get("123")
    n16 = len(bot.sent)
    calls16 = len(LLM_CALLS)
    ev16 = FakeEvent("小X 在吗", user_id="newbie", nickname="新人", to_me=True)
    await GATEWAY.handle(bot, ev16)
    await drain()
    check("an unconsenting member draws a one-line pointer, not a reply",
          len(bot.sent) == n16 + 1 and "/agree" in str(bot.sent[-1])
          and "/terms" in str(bot.sent[-1])
          and "【用户协议】" not in str(bot.sent[-1])  # the pointer, never the full text
          and len(LLM_CALLS) == calls16, str(bot.sent[n16:])[:160])
    check("and their message still archives",
          await pool().fetchval(
              "SELECT count(*) FROM raw_event WHERE platform_event_id=$1",
              str(ev16.message_id)) == 1)
    await GATEWAY.handle(bot, FakeEvent("小X 在吗", user_id="newbie",
                                        nickname="新人", to_me=True))
    await drain()
    check("the agreement prompt respects its cooldown",
          len(bot.sent) == n16 + 1, f"{len(bot.sent) - n16} sent")
    check("/agree records a first acceptance as first",
          await _agree.accept("123", "newbie") is True)
    check("and a repeat as a repeat", await _agree.accept("123", "newbie") is False)
    check("consent in one group says nothing about another",
          not await _agree.ok("456", "newbie"))
    # A version bump voids old acceptances: everyone re-consents to the new
    # text, and re-accepting registers as a change, not a repeat.
    _orig_version = _agree.version
    _agree.version = lambda: 2
    check("a version bump voids the old acceptance",
          not await _agree.ok("123", "newbie"))
    check("re-accepting the new version counts as a change",
          await _agree.accept("123", "newbie") is True)
    check("and satisfies the gate again", await _agree.ok("123", "newbie"))
    _agree.version = _orig_version
    await GATEWAY.handle(bot, FakeEvent("小X 在吗", user_id="newbie",
                                        nickname="新人", to_me=True))
    await drain()
    check("after /agree the reply path opens",
          len(bot.sent) == n16 + 2 and len(LLM_CALLS) > calls16)

    # 17. group notices become transcript lines: archived, in the window, and
    # never a reply - even a poke aimed at the bot itself only transcribes.
    st17 = await REGISTRY.get("123")
    n17 = len(bot.sent)
    _t17 = int(_nl0().timestamp())
    ev_note = types.SimpleNamespace(
        group_id=123, user_id="u9", notice_type="group_recall",
        operator_id="u9", time=_t17)
    await GATEWAY.handle_notice(bot, ev_note)
    await drain(0.3)
    check("a recall becomes a window line",
          any(m.text == "⟦撤回了自己的一条消息⟧" and m.user_id == "u9"
              for m in st17.recent))
    check("and is archived once",
          await pool().fetchval(
              "SELECT count(*) FROM raw_event WHERE platform_event_id"
              " LIKE 'notice-group_recall-123-u9-%'") == 1)
    await GATEWAY.handle_notice(bot, ev_note)   # the adapter replays; dedup eats it
    await drain(0.2)
    check("a replayed notice lands only once",
          sum(1 for m in st17.recent
              if m.msg_id.startswith("notice-group_recall")) == 1)
    await GATEWAY.handle_notice(bot, types.SimpleNamespace(
        group_id=123, user_id="u1", notice_type="notify", sub_type="poke",
        target_id="999", time=_t17 + 1))
    await GATEWAY.handle_notice(bot, types.SimpleNamespace(
        group_id=123, user_id="u7", notice_type="group_increase",
        sub_type="approve", time=_t17 + 2))
    await GATEWAY.handle_notice(bot, types.SimpleNamespace(
        group_id=123, user_id="u7", notice_type="group_ban",
        sub_type="ban", duration=600, time=_t17 + 3))
    await drain(0.3)
    check("a poke at the bot is transcribed, never answered",
          any(m.text == "⟦戳了戳你⟧" for m in st17.recent)
          and len(bot.sent) == n17)
    check("a join is transcribed",
          any(m.text == "⟦加入了本群⟧" for m in st17.recent))
    check("a ban is transcribed with its span",
          any(m.text == "⟦被禁言 10 分钟⟧" for m in st17.recent))
    for mid in ("r1", "r2"):   # an admin mass-recall: one author, same second
        await GATEWAY.handle_notice(bot, types.SimpleNamespace(
            group_id=123, user_id="u9", operator_id="u1", message_id=mid,
            notice_type="group_recall", time=_t17 + 4))
    # Mute-all arrives as user_id 0 with duration -1: group state, no member.
    await GATEWAY.handle_notice(bot, types.SimpleNamespace(
        group_id=123, user_id=0, operator_id="u1", notice_type="group_ban",
        sub_type="ban", duration=-1, time=_t17 + 5))
    # The bot's own message recalled by an admin: not transcribed - archiving
    # would mint an identity entity for the bot.
    await GATEWAY.handle_notice(bot, types.SimpleNamespace(
        group_id=123, user_id="999", operator_id="u1", message_id="r3",
        notice_type="group_recall", time=_t17 + 6))
    await drain(0.3)
    check("same-second recalls of one author each get their line",
          sum(1 for m in st17.recent
              if m.text == "⟦一条消息被管理员撤回⟧" and m.user_id == "u9") == 2)
    check("mute-all credits no phantom account",
          not any(m.user_id == "0" for m in st17.recent))
    check("the bot's own events are not transcribed",
          not any(m.msg_id.startswith("notice") and m.user_id == "999"
                  for m in st17.recent))

    # 17b. quoting the bot's own line is being addressed, even with the reply
    # button's auto-@ stripped off by hand; quoting anybody else without an @
    # stays silence. Window-scoped: the check must stay synchronous.
    st17b = await REGISTRY.get("123")
    bot_line = next(m for m in reversed(st17b.recent) if m.is_bot)
    user_line = next(m for m in reversed(st17b.recent)
                     if not m.is_bot and m.user_id)
    n17b = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("你确定吗", user_id="u1",
                                        reply_id=bot_line.msg_id,
                                        reply_from="999"))
    await drain()
    check("a de-@'d quote of the bot's line still draws a reply",
          len(bot.sent) == n17b + 1, f"{len(bot.sent) - n17b} sent")
    await GATEWAY.handle(bot, FakeEvent("你确定吗", user_id="u1",
                                        reply_id=user_line.msg_id,
                                        reply_from=user_line.user_id))
    await drain()
    check("quoting anybody else without an @ stays silence",
          len(bot.sent) == n17b + 1, f"{len(bot.sent) - n17b - 1} extra")

    # 18. namesakes: two members sharing a card are told apart by a permanent
    # serial - renames dissolve and restore the suffix, never the number.
    from qqbot.core import tools as _tools
    from qqbot.core.members import MEMBERS as _MEM18
    bot.members += [{"user_id": "u31", "card": "张伟", "nickname": "zw"},
                    {"user_id": "u32", "card": "张伟", "nickname": "wei"}]
    _MEM18.forget("123")
    named = await _MEM18.names_of(bot, "123", ["u31", "u32"])
    check("namesakes render as distinct numbered names",
          named.get("u31", "").startswith("张伟⟦同名")
          and named.get("u32", "").startswith("张伟⟦同名")
          and named["u31"] != named["u32"], str(named))
    first = dict(named)
    for r in bot.members:
        if r["user_id"] == "u32":
            r["card"] = "李芳"
    _MEM18.forget("123")
    named = await _MEM18.names_of(bot, "123", ["u31", "u32"])
    check("a rename dissolves the clash and the suffixes go",
          named.get("u31") == "张伟" and named.get("u32") == "李芳", str(named))
    for r in bot.members:
        if r["user_id"] == "u32":
            r["card"] = "张伟"
    _MEM18.forget("123")
    named = await _MEM18.names_of(bot, "123", ["u31", "u32"])
    check("re-clashing restores the very same serials", named == first,
          f"{named} vs {first}")
    line18 = _CM0(msg_id="ns-1", user_id="u31", nickname="张伟",
                  text="改锥在我这", ts=_nl0())
    await _MEM18.relabel(bot, "123", [line18])
    check("relabel carries the numbered name onto window lines",
          line18.nickname == first["u31"], line18.nickname)
    await seed(123, "u31", "张伟", text="改锥昨天借给阿强了")
    await seed(123, "u32", "张伟", text="改锥我根本没见过")
    n31 = int(first["u31"].split("⟦同名")[1].rstrip("⟧"))
    # Bare-hit mode: the pin is about which lines are *hits* - with context on,
    # the other namesake's line would legitimately appear as surroundings.
    _rcfg18 = config().default.retrieval
    _ctx18, _rcfg18.history_context = _rcfg18.history_context, 0
    got = await _tools.search_history(123, "改锥", speaker=f"张伟⟦同名{n31}⟧")
    got_legacy = await _tools.search_history(123, "改锥", speaker=f"张伟({n31})")
    _rcfg18.history_context = _ctx18
    check("search_history narrows by the serial, not the shared name",
          "借给阿强" in got and "没见过" not in got, got)
    # Old transcripts and archived @-resolutions still carry name(N); a speaker
    # copied from one must keep narrowing by the same serial.
    check("the legacy namesake form still narrows",
          "借给阿强" in got_legacy and "没见过" not in got_legacy, got_legacy)

    # Last, so every kind of memory write has actually happened by now. Reasoning
    # models bill deliberation as output, so a memory call must ask for a terse
    # direct answer - deliberating under a word limit truncates the answer itself,
    # cutting it off mid-sentence.
    check("the memory path never asks the model to write prose",
          not any(c["kind"] in ("knowledge", "summary") for c in LLM_CALLS),
          str({c["kind"] for c in LLM_CALLS}))
    # Each use of the text model brings its own settings: extraction reads a whole
    # chunk of transcript, so it carries a grade and a patience the reply path would
    # never grant, and neither is a call-site exception.
    _txtcfg = config().default.llm.text
    check("extraction and replies each run on their own configured settings",
          all(c["grade"] == _txtcfg.extract.reasoning_effort
              and c["timeout"] == _txtcfg.extract.timeout_sec
              for c in LLM_CALLS if c["kind"] == "extract")
          and all(c["grade"] == _txtcfg.reasoning_effort
                  and c["timeout"] == _txtcfg.timeout_sec
                  for c in LLM_CALLS if c["kind"] == "reply"))

    await GATEWAY.shutdown()
    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
