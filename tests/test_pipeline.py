"""End-to-end pipeline test: fake protocol side, stubbed LLM, real DB."""

import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))
from _db import configure_test_database

configure_test_database()
import asyncio
import itertools
import re
import types


from qqbot.db import init_pool, close_pool, pool
from qqbot.domain.ids import GroupId
from qqbot.settings import ConfigBundle, config
import qqbot.settings as _settings
from _db import reset
from _stubs import FakeEmbedding, LegacyTextSession, function_call, response

#: One stub for every bundle in this suite.
_EMBED = FakeEmbedding()
from qqbot.core import engine as _engine_module
from qqbot.core import prompt as prompt_mod
from qqbot.core.budget import BUDGET
from qqbot.core.commands import CommandRouter
from qqbot.core.delivery import GroupDelivery
from qqbot.core.media import MediaCoordinator, MediaProcessor
from qqbot.core.pipeline import Gateway
from qqbot.core.retrieval import build_directory
from qqbot.core.state import ChatMsg as _CM0, GroupState, Registry
from qqbot.gateway.ingest import Ingestor
from qqbot.repositories import IdentityLinkRepository, IdentityRepository
from qqbot.services import IdentityLinkService, IdentityResolver
from qqbot.providers import AsrModel, Providers, SearchEngine, TextModel, VisionModel
from qqbot.providers.base import QuotaExhausted, Rate
from qqbot.providers.contracts import StoredImage
from qqbot.util import now_local as _nl0, today_local as _today

fails = []
LLM_CALLS = []


def install_settings(settings):
    """Replace the immutable configuration snapshot used by this test process."""
    bundle = config()
    personas = {"default": bundle._default_persona, **bundle.personas}
    _settings._bundle = ConfigBundle(
        settings.model_dump(),
        personas,
        bundle.prompts,
        bundle.agreement_text,
        bundle.predicates,
    )
    return _settings._bundle.default


def install_budget(settings, **changes):
    return install_settings(
        settings.model_copy(
            update={
                "budget": settings.budget.model_copy(update=changes),
            }
        )
    )


def install_tools(settings, **changes):
    return install_settings(
        settings.model_copy(
            update={
                "tools": settings.tools.model_copy(update=changes),
            }
        )
    )


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


# ---- stubs ---------------------------------------------------------------
REPLY_TEXT = {"v": "**行啊**，我看看"}
KNOWLEDGE = {"v": "群主是老王，外号王哥。"}


#: The message being answered, as the reply prompt's tail shows it: its line number,
#: then its speaker's member number behind the name.
_TAIL_LINE = re.compile(r"下面是刚收到的消息：\n#(\d+) ⟦[^⟧]*⟧ [^\n]*?⟦(\d+)⟧")


def send_to_asker(messages, text):
    """The send call a well-behaved model makes: the text, replying to the message
    being answered and @-ing its sender, both named by the numbers the prompt shows."""
    content = [{"type": "text", "data": {"text": text}}]
    for m in reversed(messages):
        found = (
            _TAIL_LINE.search(m["content"])
            if m.get("role") == "user" and isinstance(m.get("content"), str)
            else None
        )
        if found:
            content[:0] = [
                {"type": "reply", "data": {"line": int(found.group(1))}},
                {"type": "at", "data": {"member": int(found.group(2))}},
            ]
            break
    return [
        function_call(
            "send_messages",
            {"messages": [{"content": content}]},
            call_id=f"send-{len(LLM_CALLS)}",
        )
    ]


class FakeAttachments:
    async def store(self, data, media_type):
        return StoredImage("fake-light", f"file-api-fake{len(data)}")

    async def aclose(self):
        pass


class FakeText(TextModel):
    """A real subclass of the ABC, so this test breaks if the contract changes - which is
    exactly what happened when the deliberation flag was added. On the reply path it
    answers the way a well-behaved model does: through the send tool."""

    MODEL = "fake-light"
    RATE = Rate("Mtoken", in_hit=0.02, in_miss=1.0, out=2.0, source="fake")

    def rate_for(self, model):
        return self.RATE

    def open_session(self, request):
        return LegacyTextSession(self, request)

    async def respond(
        self, input, *, cfg, tools=None, max_tokens=None, effort=None, kind="reply", group_id=None
    ):
        LLM_CALLS.append(
            {
                "kind": kind,
                "input": input,
                "tools": tools,
                "effort": effort,
                "max_tokens": max_tokens,
                "grade": cfg.reasoning_effort,
                "timeout": cfg.timeout_sec,
            }
        )
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
        await BUDGET.record(
            kind=kind,
            model=self.MODEL,
            cny=self.rate_for(self.MODEL).tokens(100, 10, 20),
            in_hit=100,
            in_miss=10,
            out=20,
            group_id=group_id,
        )
        if kind == "reply" and tools:
            return response(
                model=self.MODEL,
                in_hit=100,
                in_miss=10,
                out=20,
                tool_calls=send_to_asker(input, text),
            )
        return response(text=text, model=self.MODEL, in_hit=100, in_miss=10, out=20)

    attachments = FakeAttachments()

    async def aclose(self):
        pass


class UnusedVision(VisionModel):
    """A capability this test must not reach. Being called is itself the failure."""

    name = "unused"

    def rate_for(self, model):
        return Rate("Mtoken")

    async def describe(self, data, *, prompt="", mime="image/jpeg", group_id=None):
        raise AssertionError("vision should not be reached in this test")

    async def aclose(self):
        pass


class UnusedAsr(AsrModel):
    name = "unused"

    def rate_for(self, model):
        return Rate("second")

    async def transcribe(self, data, *, fmt="wav", seconds=None, group_id=None):
        raise AssertionError("asr should not be reached in this test")

    async def aclose(self):
        pass


class UnusedSearch(SearchEngine):
    name = "unused"

    def rate_for(self, model):
        return Rate("call")

    async def search(self, query, *, options, group_id=None):
        raise AssertionError("search should not be reached in this test")

    async def aclose(self):
        pass


providers_bundle = Providers(
    text=FakeText(), vision=UnusedVision(), asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()
)
_IDS = IdentityRepository()
_RESOLVER = IdentityResolver(_IDS)
_DIRECTORY = build_directory(identities=_IDS, resolver=_RESOLVER)
_LINKS = IdentityLinkService(
    config().default.identity_link,
    _RESOLVER,
    _IDS,
    IdentityLinkRepository(),
)
REGISTRY = Registry()
_DELIVERY = GroupDelivery()
_MEDIA_PROCESSOR = MediaProcessor(config().default.media, providers_bundle, _DIRECTORY)
MEDIA = MediaCoordinator(_MEDIA_PROCESSOR)
_ROUTER = CommandRouter(_DELIVERY, REGISTRY, _DIRECTORY, _LINKS, providers_bundle)
GATEWAY = Gateway(
    ingestor=Ingestor(IdentityResolver(IdentityRepository())),
    registry=REGISTRY,
    router=_ROUTER,
    delivery=_DELIVERY,
    media=MEDIA,
    providers=providers_bundle,
    directory=_DIRECTORY,
)


def set_providers(bundle):
    global providers_bundle
    providers_bundle = bundle
    _MEDIA_PROCESSOR._providers = bundle
    _ROUTER._providers = bundle
    GATEWAY._providers = bundle


def new_gateway():
    return Gateway(
        ingestor=GATEWAY._ingestor,
        registry=REGISTRY,
        router=_ROUTER,
        delivery=_DELIVERY,
        media=MEDIA,
        providers=providers_bundle,
        directory=_DIRECTORY,
    )


async def _respond(**kwargs):
    return await _engine_module.respond(
        providers=providers_bundle,
        media=_MEDIA_PROCESSOR,
        delivery=_DELIVERY,
        directory=_DIRECTORY,
        **kwargs,
    )


class Seg:
    def __init__(self, type_, data):
        self.type, self.data = type_, data


class FakeEvent:
    _ids = itertools.count(1000)

    def __init__(
        self,
        text=None,
        segments=None,
        user_id="u1",
        nickname="阿强",
        group_id=123,
        to_me=False,
        reply_id=None,
        reply_from=None,
        message_id=None,
    ):
        self.group_id = group_id
        self.user_id = user_id
        self.message_id = message_id if message_id is not None else next(self._ids)
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
            if reply_id is not None
            else None
        )
        self.sender = types.SimpleNamespace(card="", nickname=nickname, role="member")

    def get_message(self):
        return self._segs


class FakeBot:
    self_id = "999"
    _message_ids = itertools.count(90001)

    def __init__(self):
        self.sent = []
        self.quoted = []
        self.ats = []
        self.member_list_calls = 0
        self.members = [
            {"user_id": "u1", "card": "阿强", "nickname": "阿强"},
            {"user_id": 24680, "card": "群里的阿明", "nickname": "阿明"},
            {"user_id": 7, "card": "小南", "nickname": "dong"},
        ]

    async def send_group_msg(self, *, group_id, message):
        # Delivery and self-observation are separate protocol events. The fake emits
        # both so the production receive path, rather than the engine, owns state and
        # archival in this end-to-end suite.
        segments = (
            list(message)
            if isinstance(message, list)
            else [{"type": "text", "data": {"text": str(message)}}]
        )
        reply_id = next((s["data"]["id"] for s in segments if s["type"] == "reply"), None)
        if isinstance(message, list):
            self.quoted.append(reply_id)
            self.ats.append(next((s["data"]["qq"] for s in segments if s["type"] == "at"), None))
            shown = "".join(
                s["data"].get("text", "") for s in segments if s["type"] == "text"
            ).lstrip()
        else:
            shown = str(message)
        self.sent.append((group_id, shown))
        message_id = next(self._message_ids)
        reported = [
            Seg(s["type"], dict(s.get("data") or {})) for s in segments if s["type"] != "reply"
        ]
        await GATEWAY.handle(
            self,
            FakeEvent(
                segments=reported,
                user_id=str(self.self_id),
                nickname="小X",
                group_id=group_id,
                reply_id=reply_id,
                message_id=message_id,
            ),
        )
        media_tasks = {ticket.task for ticket in MEDIA._tickets.values() if ticket.task is not None}
        if media_tasks:
            await asyncio.gather(*media_tasks)
        return {"message_id": message_id}

    async def call_api(self, api, **kw):
        if api == "get_image":
            return {"url": "", "file": ""}
        if api == "get_msg":
            return {
                "sender": {"nickname": "阿花"},
                "message": [{"type": "text", "data": {"text": "昨天那张图"}}],
            }
        if api == "get_group_member_list":
            self.member_list_calls += 1
            return list(self.members)
        return {}


async def drain(seconds=1.2):
    await asyncio.sleep(seconds)


GROUP = GroupId("123")
ORD = GroupId("4001")
OTHER = GroupId("4002")
_SEQ = itertools.count(1)


async def seed(group, uid, name, *, n=1, text="随便说说"):
    """Put an account into a group's history through the real inbound path.

    Through the Ingestor rather than an INSERT: what is being relied on downstream is
    that speaking creates a person, files the group card as an alias, and counts towards
    the roster. A test that wrote the row itself would assert none of that.
    """
    from qqbot.gateway.onebot import GroupMessage, Sender

    for _ in range(n):
        await GATEWAY._ingestor.ingest(
            GroupMessage(
                message_id=f"seed-{group}-{uid}-{next(_SEQ)}",
                group_id=group,
                sender=Sender(user_id=uid, card=name),
                segments=[],
                self_id="999",
                occurred_at=_nl0(),
                plain_text=text,
            ),
        )


async def main():
    await init_pool()
    await reset()

    # The whole cast accepts the user agreement up front - consent is its own
    # section; everywhere else a reply is the thing under test.
    from qqbot.db import repo as _repo_seed

    for _uid in ("u1", "u7", "u9", "u404", "bad1", "1", "7", "999"):
        await _repo_seed.record_agreement(GROUP, _uid, 1)

    cfg = config().default
    # The reply path will not start without one, which is the point: a half-wired
    # deployment must fail at boot, not quietly degrade recall.
    from qqbot.core import nickname

    nickname.initialize()
    nickname.register(cfg.trigger.nicknames)  # the plugin does this at startup
    bot = FakeBot()

    # 1. direct mention -> must answer, markdown stripped
    ev0 = FakeEvent("小X 在吗")
    await GATEWAY.handle(bot, ev0)
    await drain()
    check("direct mention replies", len(bot.sent) == 1, str(bot.sent))
    check(
        "markdown stripped before send", bot.sent and "**" not in bot.sent[0][1], str(bot.sent[:1])
    )
    # NapCat reports the bot's displayed message back through the same gateway, so the
    # receive path owns the window and archive copy.
    own = await pool().fetch(
        """SELECT plain_text, payload FROM raw_event
            WHERE group_id=123 AND platform_user_id=$1""",
        str(bot.self_id),
    )
    check("the bot's own reply is archived too", len(own) == 1, str([dict(r) for r in own]))
    check(
        "and it is the text that was actually sent, opened by the @ it carried",
        own and own[0]["plain_text"] == "@阿强 " + bot.sent[0][1],
        f"{bot.sent[:1]} {own[0]['plain_text'] if own else None!r}",
    )
    # The answer is anchored to its cause: sent as a quote of the message that
    # called, and the same pointer goes into the archive so the rebuilt window
    # renders the bot's line with the ordinary quote mark.
    check(
        "the reply quotes the message that called",
        bot.quoted and bot.quoted[0] == str(ev0.message_id),
        str(bot.quoted[:1]),
    )
    check(
        "and @-es its sender, the way QQ's own reply button does",
        bot.ats and bot.ats[0] == "u1",
        str(bot.ats[:1]),
    )
    check(
        "and the archive keeps the quote pointer",
        own and (own[0]["payload"] or {}).get("reply_to") == str(ev0.message_id),
        str((own[0]["payload"] or {}).get("reply_to")) if own else "no row",
    )

    _sent_before_self = len(bot.sent)
    _calls_before_self = len(LLM_CALLS)
    await GATEWAY.handle(
        bot,
        FakeEvent(
            "/help",
            user_id=str(bot.self_id),
            nickname="小X",
            message_id=99001,
        ),
    )
    await drain(0.05)
    check(
        "a command-shaped self event is observed without replying",
        len(bot.sent) == _sent_before_self and len(LLM_CALLS) == _calls_before_self,
    )
    check(
        "self-observation never creates a member identity for the bot",
        await pool().fetchval(
            "SELECT count(*) FROM identity_account WHERE platform_user_id=$1",
            str(bot.self_id),
        )
        == 0,
    )
    await GATEWAY.handle(
        bot,
        FakeEvent(
            segments=[Seg("dice", {"result": "4"})],
            user_id=str(bot.self_id),
            nickname="小X",
            message_id=99002,
        ),
    )
    await drain(0.05)
    _dice_row = await pool().fetchrow(
        "SELECT plain_text, payload FROM raw_event WHERE platform_event_id='99002'"
    )
    check(
        "a reported self dice result reaches the window and archive",
        (await REGISTRY.get(GROUP)).recent[-1].text == "⟦骰子:4点⟧"
        and _dice_row["plain_text"] == "⟦骰子:4点⟧"
        and _dice_row["payload"]["segments"][0]["data"]["result"] == "4",
        repr(dict(_dice_row) if _dice_row else None),
    )

    check(
        "every reply is offered the tools - there is one model and no tier",
        all(c["tools"] for c in LLM_CALLS if c["kind"] == "reply"),
    )

    first_reply = [c for c in LLM_CALLS if c["kind"] == "reply"][0]
    sys_prompt = first_reply["input"][0]["content"]
    developer_prompt = first_reply["input"][1]["content"]
    check(
        "global constants lead the prompt, so every group shares that span",
        sys_prompt.startswith(prompt_mod.H_SEND),
        sys_prompt[:24],
    )
    # The one thing the model does comes before everything it reads: its words reach
    # the group only through the send tool.
    check(
        "and the first of them is how to speak",
        "唯一方式是调用 send_messages" in sys_prompt[:200],
        sys_prompt[:200],
    )
    # Without this the bot denies seeing an image while holding its description - it does
    # not know that the picture marker is its own eyesight rather than something a person
    # typed.
    check(
        "the reply prompt explains the numbered markers",
        "⟦图片N:描述⟧" in sys_prompt and "open_images" in sys_prompt,
    )
    # The transcript carries names but never account ids, so two people with similar names
    # are indistinguishable to the model - three members rearranging the same joke
    # nickname read as one person renaming himself, and it said so out loud. The system
    # knows better on both counts, and now says so.
    check(
        "the prompt says identity is judged by member number, not by name",
        "名字后的 ⟦N⟧ 是本次请求分配的正整数目标编号" in sys_prompt
        and "本身不带编号，不构成身份判断" in sys_prompt,
    )
    check(
        "and that a rename is only a rename when it was recorded",
        "不能据此断言对方从未改名" in sys_prompt,
        sys_prompt[sys_prompt.find("曾用名") :][:120],
    )
    # A name the account displayed and a name the group calls him are different claims,
    # and the prompt has to say so or the second gets reported as the first.
    check(
        "and that a registered alias is not a name he used to display",
        "不要说成「他以前叫」" in sys_prompt,
    )
    # These hold with or without a roster, so they ship apart from the lists: this group
    # has no members configured and still gets them.
    check(
        "reading rules ship whether or not anyone is configured",
        prompt_mod.H_CREDIBILITY in sys_prompt,
    )
    # Reply and extraction deliberately expose different identity targets. A reply
    # number names the current linked holder, while extraction records exact accounts.
    check(
        "the rules separate linked reply targets from exact extraction accounts",
        "回复路径按当前关联账号集合编号" in sys_prompt and "提取路径按精确账号编号" in sys_prompt,
    )
    check(
        "and stop short of what is not known",
        "系统没有告知的身份关系" in sys_prompt and "都是未知的，不猜" in sys_prompt,
    )
    check(
        "the group-scoped persona follows global policy as developer context",
        prompt_mod.H_PERSONA in developer_prompt and prompt_mod.H_PERSONA not in sys_prompt,
    )
    # The whole point of a code-supplied fact is that it is certain. That is worth nothing
    # unless the certain lines are marked apart from the guessed ones - the model had been
    # repeating its own inferences back as if someone had told it.
    # Headings come from the module rather than being spelled out here: a rename that
    # broke every section boundary should fail loudly, not be quietly re-typed in a test.
    # A transcript is not a document. Every prompt was written as if it meant what it said,
    # which is how a member's throwaway boast about their ancestry became a recorded trait.
    check(
        "the reply path is told how a group chat reads",
        prompt_mod.H_TONE in sys_prompt and "不只看字面" in sys_prompt,
    )
    check("and told not to correct a joke", "不要无故上纲上线或一本正经纠正" in sys_prompt)
    from qqbot.services import MemoryExtractor

    _CP = MemoryExtractor(cfg, providers_bundle.text).prompt
    check(
        "the memory path gets the same reading, minus the part about speaking",
        "不只看字面" in _CP and "不要无故上纲上线" not in _CP,
    )
    # One discernment, two consequences: the judgment half is the shared
    # tone_rules, and extraction's own note says what not to record.
    check(
        "with its own instruction for what to do with a joke", "没有被当真的话不产生任何候选" in _CP
    )

    check(
        "the authority sections are marked",
        all(
            h in sys_prompt
            for h in (
                prompt_mod.H_SEND,
                prompt_mod.H_LEGEND,
                prompt_mod.H_IDENTITY,
                prompt_mod.H_CREDIBILITY,
                prompt_mod.H_PRIVATE,
                prompt_mod.H_TONE,
            )
        )
        and prompt_mod.H_PERSONA in developer_prompt,
    )
    check("and says to use visible media directly", "应直接使用" in sys_prompt)
    from qqbot.prompting import PromptKey
    from qqbot.settings import prompt_catalog

    _LEG = prompt_catalog().source(PromptKey.SHARED_LEGEND)
    check("one legend, shared by the reply and memory paths", _LEG in sys_prompt and _LEG in _CP)

    # 1b. A real @ arrives as to_me with the segment already stripped by the adapter.
    # Relying on the at segment alone means the must-answer path never fires for @.
    await REGISTRY.get(GROUP)
    n_at = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("你好，介绍一下自己", to_me=True))
    await drain()
    check(
        "@ mention replies even with the at segment stripped",
        len(bot.sent) == n_at + 1,
        str(bot.sent[n_at:]),
    )
    at_call = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]
    check(
        "prompt shows the bot was addressed",
        "⟦0⟧" in at_call["input"][-1]["content"] and "@我" not in at_call["input"][-1]["content"],
    )

    # 1c. Owner recognition. The prompt only ever carries a display name, so without a
    # tag derived from the account id the bot cannot tell who its owner is the moment
    # they change their group card.
    cfg = install_settings(cfg.model_copy(update={"owners": ["u9"]}))
    await REGISTRY.get(GROUP)
    await GATEWAY.handle(bot, FakeEvent("在吗", to_me=True, user_id="u9", nickname="随便改的名字"))
    await drain()
    own_tail = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"][-1]["content"]
    check(
        "owner is tagged in the prompt, behind the member number",
        re.search(r"随便改的名字⟦\d+⟧⟦拥有者⟧", own_tail),
        own_tail[-90:],
    )
    await GATEWAY.handle(bot, FakeEvent("在吗", to_me=True, user_id="u1", nickname="阿强"))
    await drain()
    plain_tail = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"][-1]["content"]
    check("a non-owner is not tagged", "阿强（拥有者）" not in plain_tail)
    cfg = install_settings(cfg.model_copy(update={"owners": []}))

    # 2. A nickname inside a longer word is not being addressed. This is the whole
    # trigger now, so the boundary rule is the only thing standing between the bot and
    # answering a message about somebody's games console.
    n0 = len(bot.sent)
    st = await REGISTRY.get(GROUP)
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
    check(
        "and costs no model call at all",
        len(LLM_CALLS) == calls_before,
        str([c["kind"] for c in LLM_CALLS[calls_before:]]),
    )

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
    await REGISTRY.get(GROUP)
    n2 = len(bot.sent)
    ev6 = FakeEvent("小X 你看")
    for e in (ev6, FakeEvent("这个"), FakeEvent("怎么样")):
        await GATEWAY.handle(bot, e)
        await asyncio.sleep(0.05)
    await drain()
    check(
        "only the addressed fragment replies",
        len(bot.sent) == n2 + 1,
        f"{len(bot.sent) - n2} replies",
    )
    check(
        "and the reply quotes exactly that fragment",
        bot.quoted[-1] == str(ev6.message_id),
        f"quoted {bot.quoted[-1]}",
    )
    tail6 = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"][-1]["content"]
    check(
        "the slice ends at the addressed message - later fragments stay out",
        "小X 你看" in tail6 and "怎么样" not in tail6,
        tail6[-80:],
    )

    # 6b. two people asking almost at once each get their own reply, each quoting
    # its own asker - the misdirected-reply failure this design exists to end.
    n2b = len(bot.sent)
    ev_a = FakeEvent("小X 第一个问", user_id="u1", nickname="阿强", to_me=True)
    ev_b = FakeEvent("小X 第二个问", user_id="u7", nickname="小南", to_me=True)
    await GATEWAY.handle(bot, ev_a)
    await GATEWAY.handle(bot, ev_b)
    await drain()
    check(
        "two near-simultaneous askers get two replies",
        len(bot.sent) == n2b + 2,
        f"{len(bot.sent) - n2b} replies",
    )
    check(
        "each reply quotes and @s its own asker, whatever the finish order",
        dict(zip(bot.quoted[-2:], bot.ats[-2:], strict=True))
        == {str(ev_a.message_id): "u1", str(ev_b.message_id): "u7"},
        f"{bot.quoted[-2:]} {bot.ats[-2:]}",
    )

    # 7. no rate cap: every addressed message earns its one reply attempt -
    # money and the provider concurrency semaphore are what bound the pace.
    n3 = len(bot.sent)
    for i in range(5):
        await GATEWAY.handle(bot, FakeEvent(f"小X 第{i}次"))
    await drain()
    check(
        "every ask is answered, none rate-dropped",
        len(bot.sent) - n3 == 5,
        f"{len(bot.sent) - n3} sent",
    )

    # 8. Database append-once admission survives process-local gateway replacement.
    ev = FakeEvent("小X 重复消息")
    before_duplicate = len(bot.sent)
    replay_gateway = new_gateway()
    await GATEWAY.handle(bot, ev)
    await replay_gateway.handle(bot, ev)
    await drain()
    rows = await pool().fetchval(
        "SELECT count(*) FROM raw_event WHERE platform_event_id=$1", str(ev.message_id)
    )
    check("duplicate message archived once", rows == 1, str(rows))
    check(
        "a duplicate across gateway instances causes only one reply",
        len(bot.sent) == before_duplicate + 1,
        f"{len(bot.sent) - before_duplicate} replies",
    )
    await replay_gateway.shutdown()

    # Admission failure is terminal: no command mutation, live line, output or model call.
    admission = GATEWAY._ingestor
    original_ingest = admission.ingest

    async def fail_ingest(*_args, **_kwargs):
        raise RuntimeError("scripted archive failure")

    admission.ingest = fail_ingest
    cfg = install_settings(cfg.model_copy(update={"owners": ["10000"]}))
    failed_event = FakeEvent("/mute on", user_id="10000", nickname="拥有者")
    failed_state = await REGISTRY.get(GROUP)
    before_failed_sent = len(bot.sent)
    before_failed_calls = len(LLM_CALLS)
    before_failed_muted = failed_state.muted
    try:
        try:
            await GATEWAY.handle(bot, failed_event)
        except RuntimeError as exc:
            failed_raised = str(exc) == "scripted archive failure"
        else:
            failed_raised = False
    finally:
        admission.ingest = original_ingest
    check(
        "archive failure stops every downstream side effect",
        failed_raised
        and failed_state.muted == before_failed_muted
        and len(bot.sent) == before_failed_sent
        and len(LLM_CALLS) == before_failed_calls
        and not any(m.msg_id == str(failed_event.message_id) for m in failed_state.recent),
    )

    command_event = FakeEvent("/mute on", user_id="10000", nickname="拥有者")
    command_replay = new_gateway()
    await GATEWAY.handle(bot, command_event)
    await command_replay.handle(bot, command_event)
    await command_replay.shutdown()
    command_input = await pool().fetchrow(
        """SELECT id, plain_text FROM raw_event
            WHERE platform_event_id=$1""",
        str(command_event.message_id),
    )
    command_output = await pool().fetchrow(
        """SELECT plain_text, payload FROM raw_event
            WHERE payload->>'reply_to'=$1 AND payload->>'author_kind'='bot'
            ORDER BY occurred_at DESC, id DESC LIMIT 1""",
        str(command_event.message_id),
    )
    check(
        "a command is admitted before its handler mutates state",
        command_input is not None
        and command_input["plain_text"] == "/mute on"
        and failed_state.muted,
        repr(command_input),
    )
    command_output_count = await pool().fetchval(
        """SELECT count(*) FROM raw_event
            WHERE payload->>'reply_to'=$1 AND payload->>'author_kind'='bot'""",
        str(command_event.message_id),
    )
    check(
        "command input and self-observed output form one archived pair across replays",
        command_output is not None
        and "已静音" in command_output["plain_text"]
        and command_output_count == 1,
        f"{command_output!r} count={command_output_count}",
    )
    await GATEWAY.handle(
        bot,
        FakeEvent("/mute off", user_id="10000", nickname="拥有者"),
    )
    cfg = install_settings(cfg.model_copy(update={"owners": []}))

    # 9. blocklist: blocked means unanswered, nothing more - the message still
    # archives so the window stays coherent; the reply is what is withheld.
    from qqbot.db import repo as _repo

    await REGISTRY.get(GROUP)
    await _repo.block(GROUP, "u9")
    n5 = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("小X 在吗", user_id="u9"))
    await drain()
    check("a blocked user is dropped", len(bot.sent) == n5)
    await _repo.unblock(GROUP, "u9")

    # 11. nothing that changes every turn may sit in the cached system block.
    await REGISTRY.get(GROUP)
    await GATEWAY.handle(bot, FakeEvent("小X 显卡现在多少钱"))
    await drain()
    msgs = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"]
    check("the message being answered is in the tail", "显卡现在多少钱" in msgs[-1]["content"])
    check("and not in the cached system block", "显卡现在多少钱" not in msgs[0]["content"])

    # 12. The daily cap stops the bot, including when it is addressed.
    #
    # No mention exemption: every reply is a direct one, so a cap that spared
    # mentions could not stop anything, and a configured daily ceiling would have
    # been decorative for as long as the proactive path had been gone.
    cfg = install_budget(cfg, daily_cny_cap=0.0000001)
    BUDGET._loaded = False
    n6 = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("随便说点什么"))
    await drain()
    check("over budget: an unaddressed message draws nothing", len(bot.sent) == n6)
    await GATEWAY.handle(bot, FakeEvent("小X 你还在吗"))
    await drain()
    check(
        "over budget: being addressed does not buy an exemption",
        len(bot.sent) == n6,
        f"{len(bot.sent) - n6} replies past the cap",
    )

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

    _gid = GROUP
    _subject = await _IR().group_entity(_gid)
    for _pred, _key, _val in (
        ("topic", None, "做音乐的群"),
        ("term", "切片", "把采样切成小段再重排"),
    ):
        await _MR().supersede(
            _F(
                subject_entity_id=_subject,
                predicate=_pred,
                object_key=_key,
                object_value=_val,
                group_id=_gid,
                memory_type=_MT.GROUP,
                confidence=0.5,
            ),
            [],
            when=_nl0(),
        )
    _facts = await _r.group_knowledge(_gid)
    check(
        "the group's own facts read back",
        _facts == ["做音乐的群", "切片：把采样切成小段再重排"],
        str(_facts),
    )
    check("the topic comes first", _facts[0] == "做音乐的群", str(_facts))

    _, persona_k = config().for_group(GROUP)
    _split = prompt_mod.build_system(persona_k, [], _facts)
    check(
        "they reach the prompt as the bot's own summary",
        "未确认（你自行归纳的印象" in _split and "切片：把采样切成小段再重排" in _split,
        _split[-200:],
    )
    check(
        "and the hand-written material is kept above them",
        "已确认（固定资料）" in _split
        and _split.index("已确认（固定资料）") < _split.index("未确认（你自行归纳的印象"),
        _split[-200:],
    )

    # A group is an entity, which is what lets its facts use the whole model. Two accounts
    # merging must never reach it, and it must not turn up in anybody's roster.
    check(
        "the group's entity is not an account in the roster",
        all(c.user_id != str(_gid) for c in await _DIRECTORY.roster(_gid)),
    )
    # Redefining a word supersedes; a second word is a second row. Without the word in the
    # predicate a group could hold exactly one term.
    await _MR().supersede(
        _F(
            subject_entity_id=_subject,
            predicate="term",
            object_key="切片",
            object_value="改了个说法",
            group_id=_gid,
            memory_type=_MT.GROUP,
            confidence=0.5,
        ),
        [],
        when=_nl0(),
    )
    await _MR().supersede(
        _F(
            subject_entity_id=_subject,
            predicate="term",
            object_key="干声",
            object_value="没加效果的人声",
            group_id=_gid,
            memory_type=_MT.GROUP,
            confidence=0.5,
        ),
        [],
        when=_nl0(),
    )
    _facts2 = await _r.group_knowledge(_gid)
    check(
        "redefining a term replaces it rather than adding one",
        "切片：改了个说法" in _facts2 and "切片：把采样切成小段再重排" not in _facts2,
        str(_facts2),
    )
    check(
        "and a different term is a separate entry", "干声：没加效果的人声" in _facts2, str(_facts2)
    )

    # 12d. Pictures are understood on arrival: the CDN link is freshest then, the
    # describing call is cached per unique picture, and a group the bot never answers
    # still gets a readable archive. Deferring it to reply time would leave a picture
    # posted on its own a bare marker until something drew a reply - by which time the
    # link has expired.
    cfg = install_budget(cfg, daily_cny_cap=5.0)  # test 12 left the cap at ~zero
    BUDGET._loaded = False
    VISION_SEEN = []

    class SeeingVision(VisionModel):
        name = "seeing"

        def rate_for(self, model):
            return Rate("Mtoken", in_miss=1.2, out=7.2)

        async def describe(self, data, *, prompt="", mime="image/jpeg", group_id=None):
            VISION_SEEN.append(len(data))
            return "一只橘猫在键盘上打滚"

        async def aclose(self):
            pass

    prev = providers_bundle.vision
    set_providers(
        Providers(
            text=FakeText(),
            vision=SeeingVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=UnusedSearch(),
        )
    )
    _MEDIA = _MEDIA_PROCESSOR

    async def _fake_fetch(url, max_bytes):
        return b"x" * 2048

    _MEDIA._fetch = _fake_fetch
    _MEDIA._local = staticmethod(lambda path, max_bytes: None)
    _MEDIA._img_windows.clear()  # earlier sections consumed per-minute slots

    st_m = await REGISTRY.get(GROUP)
    n_before = len(bot.sent)
    img_event = FakeEvent(
        segments=[
            Seg("at", {"qq": "24680"}),
            Seg("image", {"file": "A" * 32 + ".png", "url": "http://x/y.png", "summary": ""}),
        ]
    )
    await GATEWAY.handle(bot, img_event)
    await drain(2.0)
    check("the bot did not answer it", len(bot.sent) == n_before)
    check(
        "but the picture was understood the moment it arrived",
        len(VISION_SEEN) == 1,
        str(VISION_SEEN),
    )
    stored = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(img_event.message_id)
    )
    check(
        "and the archive carries the description straight away",
        "橘猫" in (stored or ""),
        repr(stored),
    )
    # But the free half still runs on arrival - an unresolved mention would archive as an
    # account number, which is the one thing the roster exists to keep out of the prompt.
    check(
        "while the mention was still resolved to a name",
        "群里的阿明" in (stored or "") and "24680" not in (stored or ""),
        repr(stored),
    )

    # A question about it later pays nothing more: the description was bought when the
    # picture arrived and has been sitting in the history since.
    await GATEWAY.handle(bot, FakeEvent("小X 刚那张图是什么"))
    await drain(2.0)
    check("a question about it pays nothing more", len(VISION_SEEN) == 1, str(VISION_SEEN))
    hist = "\n".join(
        content
        if isinstance((content := m.get("content")), str)
        else " ".join(
            b.get("text", "")
            for b in content or ()
            if isinstance(b, dict) and b.get("type") in ("text", "input_text")
        )
        for m in [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"]
    )
    check(
        "and the description lands in the history, where the question points",
        "橘猫" in hist and "群里的阿明 ⟦图片⟧" not in hist,
        hist[-200:],
    )
    backfilled = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(img_event.message_id)
    )
    check(
        "the archive is corrected too, so memory keeps the description",
        "橘猫" in (backfilled or ""),
        repr(backfilled),
    )
    await GATEWAY.handle(bot, FakeEvent("小X 那图呢"))
    await drain(2.0)
    check(
        "and asking again does not pay for it a second time",
        len(VISION_SEEN) == 1,
        str(VISION_SEEN),
    )

    # Nor does reposting it: the cache keys on the picture, not the message.
    await GATEWAY.handle(
        bot,
        FakeEvent(
            segments=[
                Seg("image", {"file": "A" * 32 + ".png", "url": "http://x/y2.png", "summary": ""}),
            ]
        ),
    )
    await drain(2.0)
    check(
        "a repost of the same picture is answered from the cache",
        len(VISION_SEEN) == 1,
        str(VISION_SEEN),
    )

    # A picture in the message that draws the reply is likewise understood on arrival -
    # in the prompt in time for the answer.
    img2 = FakeEvent(
        segments=[
            Seg("text", {"text": "小X 这图什么意思"}),
            Seg("image", {"file": "B" * 32 + ".png", "url": "http://x/z.png", "summary": ""}),
        ]
    )
    await GATEWAY.handle(bot, img2)
    await drain(2.0)
    check("a picture that draws a reply is understood", len(VISION_SEEN) == 2, str(VISION_SEEN))
    tail_i = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"][-1]["content"]
    # Nothing is pushed: the tail stays text, the description line carries a number,
    # and the model opens the original by that number if it wants the pixels.
    check(
        "the tail stays plain text - no original is pushed",
        isinstance(tail_i, str),
        str(tail_i)[:160],
    )
    tail_text = tail_i if isinstance(tail_i, str) else ""
    check(
        "and its description reaches the model, numbered",
        "橘猫" in tail_text
        and "⟦图片" in tail_text
        and tail_text.split("⟦图片", 1)[1][:1].isdigit(),
        tail_text[-120:],
    )
    stored2 = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(img2.message_id)
    )
    check("and is backfilled into the archive", "橘猫" in (stored2 or ""), repr(stored2))

    # The money guard on this path is the daily cap, asked inside the describing call
    # itself, because nothing upstream of arrival gates spending any more.
    seen_now = len(VISION_SEEN)
    cap_was = cfg.budget.daily_cny_cap
    cfg = install_budget(cfg, daily_cny_cap=0.000001)
    BUDGET._loaded = False
    capped = FakeEvent(
        segments=[Seg("image", {"file": "E" * 32 + ".png", "url": "http://x/e.png", "summary": ""})]
    )
    await GATEWAY.handle(bot, capped)
    await drain()
    check(
        "past the daily cap a fresh picture is not described",
        len(VISION_SEEN) == seen_now,
        str(VISION_SEEN),
    )
    stored_e = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(capped.message_id)
    )
    check(
        "and its marker stays bare, ready for a cheaper day",
        "⟦图片⟧" in (stored_e or "") and "橘猫" not in (stored_e or ""),
        repr(stored_e),
    )
    cfg = install_budget(cfg, daily_cny_cap=cap_was)
    BUDGET._loaded = False

    # "A cheaper day" must actually work: the cap turn-away is transient (Unsettled),
    # so the message's pending work survives, and money returning is all it takes.
    # Before the Unsettled distinction, the paid pass cleared pending on the fallback
    # and a cap-blocked picture could never be described again (a regression this
    # pins).
    await GATEWAY.handle(bot, FakeEvent("小X 刚才那张图是什么"))
    await drain(2.0)
    check(
        "with money back, a cap-blocked picture is described on the next ask",
        len(VISION_SEEN) == seen_now + 1,
        str(VISION_SEEN),
    )
    stored_e2 = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(capped.message_id)
    )
    check("and its archive line is healed", "橘猫" in (stored_e2 or ""), repr(stored_e2))

    # A picture nobody addresses costs exactly one describing call - the whole rule is
    # now pay per unique picture, not per reply that happens to read one.
    n_sent, n_seen = len(bot.sent), len(VISION_SEEN)
    img3 = FakeEvent(
        segments=[
            Seg("text", {"text": "随手发张图"}),
            Seg("image", {"file": "C" * 32 + ".png", "url": "http://x/w.png", "summary": ""}),
        ]
    )
    await GATEWAY.handle(bot, img3)
    await drain(2.0)
    check(
        "a picture nobody asks about is described exactly once",
        len(VISION_SEEN) == n_seen + 1,
        str(VISION_SEEN),
    )
    check("and draws no reply", len(bot.sent) == n_sent)
    # But the free half still ran, so the message is in the history as something with a
    # picture in it, ready to be understood if the next line asks about it.
    check(
        "while the message itself is still recorded",
        any(m.msg_id == str(img3.message_id) for m in st_m.recent),
    )

    set_providers(
        Providers(
            text=FakeText(), vision=prev, asr=UnusedAsr(), embedding=_EMBED, search=UnusedSearch()
        )
    )
    # A describe slower than every wait window must still land: the waiters give up,
    # the flight is not cancelled, and the description reaches the cache and the
    # message for the next turn. The old shape cancelled the task on timeout, which
    # left empty cache rows and images that could never be described (a real
    # incident, not a hypothetical). Two sightings of the same key during the flight
    # must also share it - one paid call, not two.
    # The wait is per-group config, so shorten the one this group resolves to.
    _waits = cfg.media.wait_sec
    cfg = install_settings(
        cfg.model_copy(
            update={
                "media": cfg.media.model_copy(update={"wait_sec": 0.2}),
            }
        )
    )

    class SlowVision(SeeingVision):
        async def describe(self, data, *, prompt="", mime="image/jpeg", group_id=None):
            VISION_SEEN.append(len(data))
            await asyncio.sleep(1.0)
            return "慢速描述完成"

    set_providers(
        Providers(
            text=FakeText(),
            vision=SlowVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=UnusedSearch(),
        )
    )
    _slow_seen = len(VISION_SEEN)
    slow1 = FakeEvent(
        segments=[
            Seg("image", {"file": "D" * 32 + ".png", "url": "http://x/slow.png", "summary": ""})
        ]
    )
    slow2 = FakeEvent(
        segments=[
            Seg("image", {"file": "D" * 32 + ".png", "url": "http://x/slow2.png", "summary": ""})
        ]
    )
    await GATEWAY.handle(bot, slow1)
    await asyncio.sleep(0.1)
    await GATEWAY.handle(bot, slow2)  # same picture, mid-flight
    await drain(0.5)  # every wait window has expired by now
    check(
        "the slow flight is still in the air, uncancelled",
        len(VISION_SEEN) == _slow_seen + 1,
        str(VISION_SEEN[_slow_seen:]),
    )
    await drain(1.2)
    check(
        "and its description lands after every waiter gave up",
        await _repo.image_cache_get("d" * 32) == "⟦图片:慢速描述完成⟧",
        str(await _repo.image_cache_get("d" * 32)),
    )
    _slow_stored = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(slow1.message_id)
    )
    check(
        "the archive is patched by the task itself, no reply needed",
        "慢速描述完成" in (_slow_stored or ""),
        repr(_slow_stored),
    )
    check(
        "two sightings during the flight paid for one call",
        len(VISION_SEEN) == _slow_seen + 1,
        str(VISION_SEEN[_slow_seen:]),
    )
    cfg = install_settings(
        cfg.model_copy(
            update={
                "media": cfg.media.model_copy(update={"wait_sec": _waits}),
            }
        )
    )

    # A media patch must not undo the arrival truncation: pm.parts holds the full
    # original text, so an unbounded re-render would put the whole thing back into
    # the window and the archive - past the one per-line bound the no-token-budget
    # prompt layout relies on (a regression this pins).
    _cap_len = cfg.tools.send_messages.max_text_chars_per_message

    class LongVision(SeeingVision):
        async def describe(self, data, *, prompt="", mime="image/jpeg", group_id=None):
            VISION_SEEN.append(len(data))
            return "长" * (_cap_len + 500)

    set_providers(
        Providers(
            text=FakeText(),
            vision=LongVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=UnusedSearch(),
        )
    )
    _MEDIA._img_windows.clear()
    longe = FakeEvent(
        segments=[
            Seg("text", {"text": "看这张超长描述的图"}),
            Seg("image", {"file": "F" * 32 + ".png", "url": "http://x/f.png", "summary": ""}),
        ]
    )
    await GATEWAY.handle(bot, longe)
    await drain(2.0)
    _lm = next((m for m in st_m.recent if m.msg_id == str(longe.message_id)), None)
    check(
        "a media patch respects the per-line bound in the window",
        _lm is not None and len(_lm.text) <= _cap_len,
        str(_lm and len(_lm.text)),
    )
    _lstored = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(longe.message_id)
    )
    check("and in the archive", _lstored and len(_lstored) <= _cap_len, str(len(_lstored or "")))

    # open_images: the reply model reads pictures itself, so the tool hands it one by
    # number rather than asking a second model to look. Free - bytes and an upload -
    # and it answers with a content array carrying the file block, which is what lets
    # the picture arrive as the answer to the call instead of a turn appended behind it.
    from qqbot.core.segments import ImageRef as _IRef
    from qqbot.core.tools import Attachment, ToolCtx, execute as _texec

    URL_CONTENT_CHARS = config().default.tools.read_url.max_content_chars

    def _tcall(name, **kw):
        return function_call(name, kw, call_id="t1")

    set_providers(
        Providers(
            text=FakeText(),
            vision=SeeingVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=UnusedSearch(),
        )
    )
    _imsg = _CM0(
        msg_id="ins1",
        user_id="1",
        nickname="某人",
        text="⟦图片⟧",
        ts=_nl0(),
        image_refs=[_IRef(slot=0, key="9" * 32, url="http://x/ins.png")],
    )
    _ictx = ToolCtx(
        providers=providers_bundle,
        media=_MEDIA_PROCESSOR,
        bot=bot,
        by_pic={4: (_imsg, 0)},
    )
    _seen0 = len(VISION_SEEN)
    out_i = await _texec(_tcall("open_images", ns=[4]), cfg=cfg, group_id=GROUP, ctx=_ictx)
    check(
        "open_images hands back the original as a picture part",
        isinstance(out_i, Attachment)
        and any(isinstance(part, StoredImage) for part in out_i.parts),
        str(out_i.parts),
    )
    check(
        "and costs no model call - it is a fetch, not a second opinion",
        len(VISION_SEEN) == _seen0,
        str(len(VISION_SEEN) - _seen0),
    )
    from qqbot.providers.contracts import Message as _Message, Role as _Role, TextPart as _TextPart

    _content = out_i.content()
    check(
        "the tool message labels the picture with its number, then the text",
        [type(part) for part in _content] == [_TextPart, StoredImage, _TextPart]
        and _content[0].text == "图片4：",
        str(_content),
    )
    # The neutral part never reaches a vendor: the codec renders it at the adapter edge.
    from qqbot.providers.openai_responses import ResponsesCodec as _ResponsesCodec

    _neutral = (
        _Message(
            _Role.USER,
            (_TextPart("图片4："), StoredImage("openai_responses", "file-api-fake2048")),
        ),
    )
    _wired = _ResponsesCodec().encode_items(_neutral)
    check(
        "the picture part is translated to a Responses input image",
        _wired[0]["content"][1] == {"type": "input_image", "file_id": "file-api-fake2048"},
        str(_wired[0]["content"][1]),
    )
    check(
        "and the caller's neutral value is unchanged",
        isinstance(_neutral[0].content[1], StoredImage),
    )
    # Several at once: a question is often about a set, and one round per picture
    # would spend the loop's bound on fetching. Unknown numbers are reported beside
    # the ones that came back, not instead of them.
    _imsg2 = _CM0(
        msg_id="ins2",
        user_id="1",
        nickname="某人",
        text="⟦图片⟧",
        ts=_nl0(),
        image_refs=[_IRef(slot=0, key="8" * 32, url="http://x/ins2.png")],
    )
    _ictx.by_pic[5] = (_imsg2, 0)
    out_m = await _texec(_tcall("open_images", ns=[4, 5, 99]), cfg=cfg, group_id=GROUP, ctx=_ictx)
    check(
        "open_images fetches several pictures in one call",
        isinstance(out_m, Attachment)
        and [type(part) for part in out_m.content()]
        == [_TextPart, StoredImage, _TextPart, StoredImage, _TextPart]
        and "图片5：" in str(out_m.content())
        and "99" in str(out_m),
        str(out_m.content()),
    )
    check(
        "a number outside the prompt is answered, not crashed",
        "99" in await _texec(_tcall("open_images", ns=[99]), cfg=cfg, group_id=GROUP, ctx=_ictx),
    )
    check(
        "a missing number is answered too",
        "编号" in await _texec(_tcall("open_images"), cfg=cfg, group_id=GROUP, ctx=_ictx),
    )
    # "null", "[]" and "42" are valid JSON: a degenerate argument string must get
    # the same in-band answer as an unparsable one, not crash the whole reply.
    out_n = await _texec(
        function_call("web_search", "null", call_id="t2"), cfg=cfg, group_id=GROUP, ctx=_ictx
    )
    check(
        "non-object tool arguments are answered in-band",
        out_n.startswith("（工具参数解析失败"),
        repr(out_n),
    )

    # read_url: page text through the optional page-reader capability, capped before it
    # reaches the prompt - pages are unbounded, replies are not.
    class ReadingSearch(UnusedSearch):
        async def read_page(self, url, *, group_id=None):
            return "页 " * 4000

    reading_search = ReadingSearch()
    set_providers(
        Providers(
            text=FakeText(),
            vision=SeeingVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=reading_search,
            page_reader=reading_search,
        )
    )
    _ictx.providers = providers_bundle
    out_u = await _texec(
        _tcall("read_url", url="https://a.example/x"),
        cfg=cfg,
        group_id=GROUP,
        ctx=_ictx,
    )
    check(
        "read_url returns page text capped at the limit",
        out_u.startswith("页") and len(out_u) <= URL_CONTENT_CHARS + 40,
        str(len(out_u)),
    )
    check(
        "a non-http url is refused in words",
        "http" in await _texec(_tcall("read_url", url="ftp://x"), cfg=cfg, group_id=GROUP),
    )
    set_providers(
        Providers(
            text=FakeText(),
            vision=SeeingVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=UnusedSearch(),
        )
    )

    # Later sections count calls, so discard retryable fixture tickets first.
    MEDIA._tickets.clear()

    # 12e. reply and forward segments reach the model as content
    await REGISTRY.get(GROUP)
    # Addressed, because that is the only way a reply happens now - and quoting somebody
    # while asking the bot about it is exactly the shape this checks.
    await GATEWAY.handle(
        bot,
        FakeEvent(
            segments=[
                Seg("at", {"qq": "24680"}),
                Seg("text", {"text": "小X 这个怎么说"}),
            ],
            reply_id=555,
            reply_from="24680",
        ),
    )
    await drain(2.0)
    tail_r = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"][-1]["content"]
    # Message 555 is not one the bot has seen, so the quote is reported as unavailable
    # rather than fetched: text on screen belonging to no visible line is the ambiguity
    # numbering replaced.
    check("a quote of something off screen says so", "⟦回复更早的消息⟧" in tail_r, tail_r[-140:])
    check(
        "@someone resolves to a name, not a number",
        "群里的阿明" in tail_r and "@24680" not in tail_r,
        tail_r[-120:],
    )
    # No name cache of our own any more: the protocol side is asked for the group's
    # member list, which is both simpler and answers for people who never spoke.
    _M = _MEDIA_PROCESSOR
    check("media keeps no name cache of its own", not hasattr(_M, "_names"))
    # The record arrives inline in the segment: no fetch, and the pictures inside
    # are numbered with the carrying message and openable by that number.
    _t12 = int(_nl0().timestamp())
    await GATEWAY.handle(
        bot,
        FakeEvent(
            segments=[
                Seg(
                    "forward",
                    {
                        "id": "fi",
                        "content": [
                            {
                                "time": _t12 - 60,
                                "sender": {"nickname": "阿花"},
                                "message": [{"type": "text", "data": {"text": "内嵌第一条"}}],
                            },
                            {
                                "time": _t12 - 30,
                                "sender": {"nickname": "阿花"},
                                # This is a distinct key. An earlier check gave the picture
                                # keyed by "f" * 32 a long description that would truncate this.
                                "message": [
                                    {
                                        "type": "image",
                                        "data": {
                                            "file": "e" * 32 + ".png",
                                            "url": "http://x/fwd.png",
                                        },
                                    }
                                ],
                            },
                        ],
                    },
                ),
                Seg("text", {"text": "小X 里面的图是啥"}),
            ]
        ),
    )
    await drain(2.0)
    tail_i = [c for c in LLM_CALLS if c["kind"] == "reply"][-1]["input"][-1]["content"]
    check(
        "an inline forward renders as an indented block without a fetch",
        "内嵌第一条" in tail_i and "\n  ⟦" in tail_i,
        tail_i[-200:],
    )
    _fw_line = next(
        m for m in reversed((await REGISTRY.get(GROUP)).recent) if "内嵌第一条" in m.text
    )
    check(
        "the forwarded picture is the message's own reference",
        len(_fw_line.image_refs) == 1 and _fw_line.image_refs[0].nested,
        str(_fw_line.image_refs),
    )
    check(
        "and its marker is numbered in the prompt",
        re.search(r"阿花: ⟦图片\d+", tail_i) is not None,
        tail_i[-200:],
    )

    # The trigger reads the text as it arrived, never the resolved form: a forwarded
    # conversation that merely *mentions* the bot's name inside is nothing anybody
    # typed at the bot, and must not draw a reply. (Being spoken to is a typed @ or
    # a typed nickname; resolved media text is neither.)
    _nb = bot.__class__()
    await GATEWAY.handle(
        _nb,
        FakeEvent(
            segments=[
                Seg(
                    "forward",
                    {
                        "id": "ffn",
                        "content": [
                            {
                                "time": _t12,
                                "sender": {"nickname": "阿强"},
                                "message": [{"type": "text", "data": {"text": "上次小X说得真对"}}],
                            }
                        ],
                    },
                )
            ]
        ),
    )
    await drain(2.0)
    check("a forward that names the bot inside does not trigger", len(_nb.sent) == 0, str(_nb.sent))

    # The roster is the whole group in a fixed order, and the reason is the prefix cache:
    # it sits in the system block ahead of the history, and a cache matches from the
    # beginning, so anything that reorders per turn invalidates the history behind it every
    # turn - so it must never be built from whoever spoke most recently. The order is
    # first appearance, because it is also the member numbering: a newcomer joins at
    # the end and nobody else's number moves.
    from qqbot.core import retrieval as _retr

    _DIR = _DIRECTORY
    for i, uid in enumerate(["a", "b", "c", "d", "e", "f"]):
        await seed(ORD, uid, uid, n=6 - i)  # a is busiest, f quietest
        await _DIR.note(ORD, uid, f"note about {uid}")
    await seed(ORD, "tie1", "tie1")  # equal counts, must not swap
    await seed(ORD, "tie2", "tie2")
    await _DIR.note(ORD, "tie1", "first of the pair")
    await _DIR.note(ORD, "tie2", "second of the pair")
    await seed(ORD, "zz", "zz", n=9)  # busiest, last to appear, no record

    _roster_rows = await _retr.gather(
        group_id=ORD,
        directory=_DIRECTORY,
        bot=bot,
    )
    _roster = [row["user_id"] for row in _roster_rows]
    check(
        "the roster is ordered by first appearance, not by activity or recency",
        _roster == ["a", "b", "c", "d", "e", "f", "tie1", "tie2", "zz"],
        str(_roster),
    )
    check(
        "the same call twice gives the same order",
        [
            row["user_id"]
            for row in await _retr.gather(
                group_id=ORD,
                directory=_DIRECTORY,
                bot=bot,
            )
        ]
        == _roster,
    )

    # Someone who has not spoken this turn is in it too - which is what removes the need
    # to work out who a message is about before deciding whose record to load - and so
    # is someone nothing is known about: the roster is where every member number lives.
    check(
        "everyone who has appeared is in it, known about or not",
        len(_roster) == 9 and "zz" in _roster,
        str(_roster),
    )
    _numbered = prompt_mod.build_system(
        config().for_group(ORD)[1],
        await _retr.gather(group_id=ORD, directory=_DIRECTORY, bot=bot),
        "",
    )
    check(
        "the roster carries the member numbers, in its own order",
        "- a⟦1⟧，note about a" in _numbered and "\n- zz⟦9⟧\n" in _numbered + "\n",
        _numbered[_numbered.rfind("【群成员名册】") :][:400],
    )

    # Isolation, at the level the whole design turns on: a group's roster is built from
    # events in that group, so somebody talkative elsewhere is simply not here.
    await seed(OTHER, "a", "a", n=9)
    await _DIR.note(OTHER, "a", "known only in the other group")
    _elsewhere = await _retr.gather(group_id=OTHER, directory=_DIRECTORY, bot=bot)
    check(
        "a roster does not reach into another group",
        [r["user_id"] for r in _elsewhere] == ["a"],
        str(_elsewhere),
    )
    check(
        "and a note written there stays there",
        _elsewhere[0]["manual_note"] == "known only in the other group"
        and next(
            r
            for r in await _retr.gather(group_id=ORD, directory=_DIRECTORY, bot=bot)
            if r["user_id"] == "a"
        )["manual_note"]
        == "note about a",
    )

    # What an owner writes by hand and what the model worked out are two different kinds
    # of claim, and the prompt says so: one undifferentiated list teaches the model
    # to repeat its own inferences back as though somebody had told it.
    from qqbot.domain.memory import Fact, MemoryType
    from qqbot.repositories import MemoryRepository

    _MEMREPO = MemoryRepository()
    _eid = (await _DIR.holder_card(ORD, "a")).entity_id
    await _MEMREPO.supersede(
        Fact(
            subject_entity_id=_eid,
            predicate="likes",
            object_value="打游戏",
            memory_type=MemoryType.PREFERENCE,
            group_id=ORD,
            confidence=0.8,
        ),
        [],
        when=_nl0(),
    )
    _card = await _DIR.holder_card(ORD, "a")
    check(
        "a hand-written note is kept apart from what was extracted",
        _card.note == "note about a" and _card.summary == "喜欢打游戏",
        f"{_card.note!r} / {_card.summary!r}",
    )

    _, _p_o = config().for_group(ORD)
    _sys = prompt_mod.build_system(_p_o, await _retr.gather(group_id=ORD, directory=_DIRECTORY), "")
    check("the certain half is labelled certain", "已确认（系统记录的名字" in _sys, _sys[-200:])
    check(
        "and the guessed half is labelled guessed",
        "未确认（你自行归纳的印象" in _sys and "喜欢打游戏" in _sys,
        _sys[-200:],
    )
    check(
        "certain sits above guessed",
        _sys.index("已确认（系统记录的名字") < _sys.index("未确认（你自行归纳的印象"),
    )
    # Stable first: a daily card rewrite must not also invalidate the constants above it.
    check(
        "constants sit above everything that changes",
        _sys.index(prompt_mod.H_CREDIBILITY) < _sys.index(prompt_mod.H_WHO),
    )

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
                                AND alias_text='旧名字')""",
        ORD.to_db(),
    )
    await seed(ORD, "g", "旧名字")
    await seed(ORD, "g", "新名字")
    _renamed = await _DIR.holder_card(ORD, "g")
    check(
        "a rename leaves the old name behind rather than overwriting it",
        set(_renamed.other_names) == {"旧名字"} and _renamed.display == "新名字",
        f"{_renamed.display} / {_renamed.other_names}",
    )
    check(
        "but a card worn only today is not yet a durable name",
        "新名字" in [n.text for n in _renamed.candidates],
        str([n.text for n in _renamed.candidates]),
    )

    # A name the account displayed and a name the group uses are different claims, and the
    # prompt says which is which. Run together, a hand-registered nickname was presented
    # as a name the account had once carried - the opposite of the truth.
    await _DIR.name(ORD, "g", "老哥")
    _named = await _DIR.holder_card(ORD, "g")
    check(
        "names the account displayed are kept apart from what people call him",
        set(_named.displayed_names) == {"旧名字"} and set(_named.nicknames) == {"老哥"},
        f"{_named.displayed_names} / {_named.nicknames}",
    )
    _row = next(
        row
        for row in await _retr.gather(group_id=ORD, directory=_DIRECTORY)
        if row["user_id"] == "g"
    )
    _one = prompt_mod.build_system(_p_o, [_row], "")
    check(
        "and the prompt labels them separately",
        "曾用名：旧名字" in _one and "别名：老哥" in _one,
        _one[-200:],
    )

    # Two accounts, one person: the whole reason the identity layer exists. After a merge
    # the roster has one line, not two, and the message counts add up.
    await seed(ORD, "alt", "小号")
    await _DIR.merge("alt", "g")
    _merged = await _DIR.holder_card(ORD, "alt")
    check(
        "a merged account resolves to the same person",
        _merged.entity_id == _renamed.entity_id and _merged.merged,
        str(_merged.accounts),
    )
    check(
        "and the roster shows them once, with the counts added",
        len(
            [
                r
                for r in await _retr.gather(group_id=ORD, directory=_DIRECTORY, bot=bot)
                if r["user_id"] in ("g", "alt")
            ]
        )
        == 1,
    )

    # And the undo, which is the reason evidence is kept on everything.
    await _DIR.split("alt")
    _after = await _DIR.holder_card(ORD, "alt")
    check(
        "splitting gives the account its own person again",
        _after.entity_id != _renamed.entity_id and not _after.merged,
        str(_after.accounts),
    )
    check(
        "names it produced itself go with it",
        "小号" in _after.other_names + (_after.display,),
        str(_after.other_names),
    )
    check(
        "names belonging to the other account stay behind",
        "新名字" not in _after.other_names + (_after.display,),
        str(_after.other_names),
    )

    # The protocol side already knows every group card, including for people who have
    # never spoken - so ask it once for the whole group instead of keeping a second cache.
    from qqbot.core.members import MEMBERS as _MEM

    _MEM.forget()
    before = bot.member_list_calls
    live = await _MEM.names_of(bot, GROUP, ["7", "24680"])
    check(
        "the member list answers with current group cards",
        live == {"7": "小南", "24680": "群里的阿明"},
        str(live),
    )
    check(
        "one call covers the whole group",
        bot.member_list_calls == before + 1,
        f"{before} -> {bot.member_list_calls}",
    )
    await _MEM.names_of(bot, GROUP, ["7"])
    check("a second lookup reuses it", bot.member_list_calls == before + 1)

    # Identity is in the cached prefix, never the per-turn tail: someone can be talked
    # about while absent, and a tail rebuilt every turn would pay for it every turn.
    _one = [_CM0(msg_id="x", user_id="u7", nickname="小南", text="在", ts=_nl0())]
    check(
        "identity is in the system block, not the tail",
        "曾用名" not in prompt_mod.build_tail(msg=_one[0]),
    )

    # The tail carries nothing but the clock and the message on purpose: whatever
    # sits here is the nearest context the incoming message has, and a pushed block
    # of past events once captured an elliptical question that referred to the
    # conversation. The past is pulled through recall_events, never pushed.
    _tail = prompt_mod.build_tail(msg=_one[0])
    check(
        "the tail is the clock and the message, nothing pushed beside them",
        _tail.index("下面是刚收到的消息") > 0 and "相关的事" not in _tail,
        _tail[:120],
    )
    # The tail has to say which message is the question. The send mechanism stays in the
    # system template instead of being repeated here; this nearest context only anchors the
    # one message the run must answer.
    check(
        "and the tail says which message to answer",
        "下面是刚收到的消息" in _tail and "只回应" in _tail,
        _tail[-140:],
    )
    check(
        "and gives the current message exactly one task anchor",
        _tail.count("下面是刚收到的消息") == 1,
        _tail[-140:],
    )
    # The exception matters as much as the rule. Being asked to answer something raised
    # earlier is ordinary, and a flat ban on the history would refuse it.
    # The exception lives with the rule, in the fixed rules that open the prompt.
    check(
        "while still allowing a question the message points at",
        "除非刚收到的消息明确要求你代答" in prompt_mod.build_system(persona_k, [], []),
    )
    check(
        "but never in the cached system block",
        "老周答应周末把切片做完" not in prompt_mod.build_system(persona_k, [], []),
    )

    # 12g. A name is captured when a message arrives, so history would otherwise keep
    # showing whoever renamed themselves under their old name while the roster, the
    # mentions and the group card all moved on - the same person under two names.
    _hist = [
        _CM0(msg_id="r1", user_id="7", nickname="以前的名字", text="早", ts=_nl0()),
        _CM0(msg_id="r2", user_id="999", nickname="", text="早啊", ts=_nl0(), is_bot=True),
        _CM0(msg_id="r3", user_id="u404", nickname="没在群里", text="?", ts=_nl0()),
    ]
    n = await _MEM.relabel(bot, GROUP, _hist)
    check(
        "a renamed speaker is relabelled in history",
        n == 1 and _hist[0].nickname == "小南",
        f"{n} {[m.nickname for m in _hist]}",
    )
    check(
        "someone the member list does not cover keeps their captured name",
        _hist[2].nickname == "没在群里",
    )
    check("the bot's own lines are left alone", _hist[1].nickname == "")
    check("re-running it changes nothing", await _MEM.relabel(bot, GROUP, _hist) == 0)

    # 13. a group nobody configured. There is no allowlist: the account is a dedicated
    # bot account, so a group it should not serve is a group it is not in. A new group
    # therefore has to work with zero configuration - default persona, served from the
    # first message - and has to be *noticed*, since nothing else records that it exists.
    # Last, because it puts traffic through a second group.
    cfg = install_budget(cfg, daily_cny_cap=5.0)
    BUDGET._loaded = False
    n_new = len(bot.sent)
    fresh_gid = GroupId("4242")
    check(
        "a group with no config file falls back to the default persona",
        config().persona_for(fresh_gid).name == config().persona_for(GroupId("999999")).name,
    )
    check("and nothing has claimed it yet", await _repo.note_group_seen(fresh_gid) is True)
    check(
        "but claiming it twice reports it only once",
        await _repo.note_group_seen(fresh_gid) is False,
    )
    check(
        "the day it appeared is on the record for the daily report",
        fresh_gid in await _repo.groups_first_seen_on(_today()),
        str(await _repo.groups_first_seen_on(_today())),
    )

    # An unconfigured group is served, archived and answered like any other.
    unconfigured = FakeEvent("小X 在吗", group_id=999)
    await GATEWAY.handle(bot, unconfigured)
    await drain()
    check("an unconfigured group gets a reply", len(bot.sent) == n_new + 1)
    check(
        "and its messages are archived",
        await pool().fetchval(
            "SELECT count(*) FROM raw_event WHERE platform_event_id=$1",
            str(unconfigured.message_id),
        )
        == 1,
    )
    check(
        "first sight of it was recorded when its state loaded",
        GroupId("999") in await _repo.groups_with_state(),
    )

    # 14. the money-bounded agent loop. There is no round count and no per-tool quota:
    # every round is a paid model call charged to the reply's scope, the first round
    # always runs, and the budget gate sits between a round's tool requests and their
    # execution - once the cap has been spent, tools whose results no further round
    # could read never run. The gate reads money already spent, never a forecast of
    # the next round: a forecast needs a price for the model, and an unpriced model
    # then fails it forever, taking the tool loop dark at zero spend.
    # Either limit (the purse, the search allowance) ends the *spending*: one wrap-up
    # round, offered only the send tool, then answers from what the paid rounds
    # already fetched. Every number below is arranged so the arithmetic is checkable
    # by hand.
    import json as _j14
    from qqbot.core import engine as _eng

    ROUND_CHARGE = 0.02  # what the scripted model books per round
    cfg = install_budget(cfg, per_reply_cny=0.10)
    # With a 0.10 purse and 0.02 a round, the money never binds in this scenario:
    # round one executes two searches, round two's fresh search is the third call
    # and the scripted allowance dies there.

    class CountingSearch(SearchEngine):
        """Free like the real one, and out of allowance from the third call on."""

        name = "counting"
        calls: list = []

        def rate_for(self, model):
            return Rate("call", per_unit=0.0)

        async def search(self, query, *, options, group_id=None):
            if len(self.calls) >= 2:
                raise QuotaExhausted("out of allowance")
            self.calls.append(query)
            await BUDGET.record(kind="search", model="counting", cny=0.0, group_id=group_id)
            return [{"title": "T", "content": "C"}]

        async def aclose(self):
            pass

    def _tc(q):
        return function_call("web_search", {"query": q}, call_id=f"t{len(LLM_CALLS)}")

    def _send(*, text="", at=None, reply=None):
        content = []
        if reply is not None:
            content.append({"type": "reply", "data": {"line": reply}})
        content.extend({"type": "at", "data": {"member": member}} for member in (at or ()))
        content.append({"type": "text", "data": {"text": text}})
        return function_call(
            "send_messages",
            {"messages": [{"content": content}]},
            call_id=f"s{len(LLM_CALLS)}",
        )

    def _names(tools):
        return [t["name"] for t in tools or ()]

    class ScriptedText(FakeText):
        """Asks for searches on every round - the same word twice, plus a fresh one -
        so only a limit can end the reply. Offered nothing but the send tool, it
        sends."""

        async def respond(
            self,
            input,
            *,
            cfg,
            tools=None,
            max_tokens=None,
            effort=None,
            kind="reply",
            group_id=None,
        ):
            LLM_CALLS.append(
                {
                    "kind": kind,
                    "input": input,
                    "tools": tools,
                    "effort": effort,
                    "max_tokens": max_tokens,
                    "grade": cfg.reasoning_effort,
                    "timeout": cfg.timeout_sec,
                }
            )
            await BUDGET.record(kind=kind, model=self.MODEL, cny=ROUND_CHARGE, group_id=group_id)
            if _names(tools) == ["send_messages"]:  # the wrap-up round
                return response(model=self.MODEL, tool_calls=[_send(text="就查到这些了")])
            return response(
                model=self.MODEL,
                tool_calls=[_tc("话题A"), _tc("话题A"), _tc(f"话题{len(LLM_CALLS)}")],
            )

    set_providers(
        Providers(
            text=ScriptedText(),
            vision=UnusedVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=CountingSearch(),
        )
    )
    n_llm = len(LLM_CALLS)
    st13 = await REGISTRY.get(GROUP)
    r1 = await _eng.generate(
        providers=providers_bundle,
        media=_MEDIA_PROCESSOR,
        directory=_DIRECTORY,
        bot=bot,
        st=st13,
        cfg=cfg,
        persona=config().for_group(GROUP)[1],
        msg=_CM0(msg_id="loop1", user_id="u1", nickname="阿强", text="帮我查个东西", ts=_nl0()),
    )
    # The scripted allowance dies on the third search, in round two: the limit ends
    # the spending, and a wrap-up round answers from what rounds one and two already
    # fetched (the daily cap, checked before anything is spent, still means silence).
    check(
        "a reply that hits the search allowance wraps up with an answer",
        r1 is not None and r1.messages[0].text == "就查到这些了",
        repr(r1),
    )
    check(
        "the wrap-up is one extra round, offered only the send tool",
        len(LLM_CALLS) - n_llm == 3 and _names(LLM_CALLS[-1]["tools"]) == ["send_messages"],
        f"{len(LLM_CALLS) - n_llm} rounds, tools={_names(LLM_CALLS[-1]['tools'])}",
    )
    check(
        "every ordinary round offers the send tool first",
        _names(LLM_CALLS[n_llm]["tools"])[0] == "send_messages",
        str(_names(LLM_CALLS[n_llm]["tools"])),
    )
    check(
        "the wrap-up round is told the allowance is gone",
        any(
            "额度已用完" in (m.get("content") or "")
            for m in LLM_CALLS[-1]["input"]
            if m.get("role") == "user"
        ),
    )
    check(
        "free searches are not gated by the reply's purse",
        len(CountingSearch.calls) == 2,
        f"{len(CountingSearch.calls)} calls: {CountingSearch.calls}",
    )
    check(
        "a repeated identical call is not re-executed",
        CountingSearch.calls.count("话题A") == 1,
        str(CountingSearch.calls),
    )
    tool_texts = [
        m.get("output") or ""
        for m in LLM_CALLS[-1]["input"]
        if m.get("type") == "function_call_output"
    ]
    check(
        "the model is told about the duplicate in words", any("刚执行过" in t for t in tool_texts)
    )
    check(
        "the unexecuted request got its placeholder result",
        any("没有执行" in t for t in tool_texts),
    )

    # And the other limit the same way: with the allowance out of the picture and the
    # purse shrunk to 0.04, rounds one and two run and spend it exactly - the gate
    # then reads the purse as empty, the pending tool requests never run, and the
    # wrap-up answers from what the first two rounds fetched.
    class EndlessSearch(CountingSearch):
        """CountingSearch without the allowance: only the purse can end this one."""

        calls = []

        async def search(self, query, *, options, group_id=None):
            self.calls.append(query)
            return [{"title": "T", "content": "C"}]

    cfg = install_budget(cfg, per_reply_cny=0.04)
    set_providers(
        Providers(
            text=ScriptedText(),
            vision=UnusedVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=EndlessSearch(),
        )
    )
    n_llm2 = len(LLM_CALLS)
    r2 = await _eng.generate(
        providers=providers_bundle,
        media=_MEDIA_PROCESSOR,
        directory=_DIRECTORY,
        bot=bot,
        st=st13,
        cfg=cfg,
        persona=config().for_group(GROUP)[1],
        msg=_CM0(msg_id="loop2", user_id="u1", nickname="阿强", text="再查个东西", ts=_nl0()),
    )
    check(
        "a reply that runs out of money wraps up the same way",
        r2 is not None
        and r2.messages[0].text == "就查到这些了"
        and len(LLM_CALLS) - n_llm2 == 3
        and _names(LLM_CALLS[-1]["tools"]) == ["send_messages"],
        f"{r2!r}, {len(LLM_CALLS) - n_llm2} rounds",
    )
    check(
        "and the unaffordable round's tools were never executed",
        len(EndlessSearch.calls) == 2,
        str(EndlessSearch.calls),
    )

    from qqbot.core import tools as _loop_tools

    _real_execute = _loop_tools.execute
    _mid_batch_calls: list[str] = []

    async def _spending_execute(call, **kwargs):
        _mid_batch_calls.append(call.name)
        await BUDGET.record(
            kind="search", model="paid-tool", cny=0.06, group_id=kwargs.get("group_id")
        )
        return "result"

    _loop_tools.execute = _spending_execute
    try:
        from qqbot.core import agent as _agent
        from qqbot.core.member_numbers import MemberNumbers as _MemberNumbers

        _mid_run = _agent.AgentRun(
            model=FakeText(),
            request=_agent.request_for_reply((), cfg, group_id=GROUP),
            cfg=cfg,
            state=st13,
            tool_context=_loop_tools.ToolCtx(
                providers=providers_bundle,
                media=_MEDIA_PROCESSOR,
            ),
            people=_MemberNumbers(self_id=str(bot.self_id)),
            lines={},
        )
        with BUDGET.scope(0.05) as _mid_spend:
            _mid_answers, _mid_hit = await _mid_run._execute_round(
                (
                    function_call("recall_events", {"question": "first"}, call_id="mid-1"),
                    function_call("recall_events", {"question": "second"}, call_id="mid-2"),
                ),
                send_notes={},
                spend=_mid_spend,
            )
    finally:
        _loop_tools.execute = _real_execute
    check(
        "a paid tool exhausting the scope blocks later calls in the same batch",
        _mid_batch_calls == ["recall_events"]
        and _mid_hit
        and "没有执行" in str(_mid_answers[1].output),
        f"{_mid_batch_calls} {_mid_answers}",
    )

    _parallel_active = 0
    _parallel_peak = 0
    _parallel_release = asyncio.Event()

    async def _parallel_execute(call, **_kwargs):
        nonlocal _parallel_active, _parallel_peak
        _parallel_active += 1
        _parallel_peak = max(_parallel_peak, _parallel_active)
        if _parallel_active == 2:
            _parallel_release.set()
        await asyncio.wait_for(_parallel_release.wait(), timeout=1)
        _parallel_active -= 1
        return "result"

    _loop_tools.execute = _parallel_execute
    try:
        _parallel_run = _agent.AgentRun(
            model=FakeText(),
            request=_agent.request_for_reply((), cfg, group_id=GROUP),
            cfg=cfg,
            state=st13,
            tool_context=_loop_tools.ToolCtx(
                providers=providers_bundle,
                media=_MEDIA_PROCESSOR,
            ),
            people=_MemberNumbers(self_id=str(bot.self_id)),
            lines={},
        )
        with BUDGET.scope(1) as _parallel_spend:
            _parallel_answers, _ = await _parallel_run._execute_round(
                (
                    function_call("web_search", {"query": "first"}, call_id="par-1"),
                    function_call("read_url", {"url": "https://example.invalid"}, call_id="par-2"),
                ),
                send_notes={},
                spend=_parallel_spend,
            )
    finally:
        _loop_tools.execute = _real_execute
    check(
        "independent free tools in one model turn execute concurrently",
        _parallel_peak == 2
        and [answer.output for answer in _parallel_answers] == ["result", "result"],
        f"peak={_parallel_peak} answers={_parallel_answers}",
    )

    # The per-round cap: the scripted round asks for three calls; capped at two,
    # the third is answered with the overflow note and never reaches the backend.
    class CappedSearch(EndlessSearch):
        calls = []

    _cap0 = cfg.tools.max_calls_per_round
    cfg = install_tools(cfg, max_calls_per_round=2)
    set_providers(
        Providers(
            text=ScriptedText(),
            vision=UnusedVision(),
            asr=UnusedAsr(),
            embedding=_EMBED,
            search=CappedSearch(),
        )
    )
    await _eng.generate(
        providers=providers_bundle,
        media=_MEDIA_PROCESSOR,
        directory=_DIRECTORY,
        bot=bot,
        st=st13,
        cfg=cfg,
        persona=config().for_group(GROUP)[1],
        msg=_CM0(msg_id="loop3", user_id="u1", nickname="阿强", text="再查一次", ts=_nl0()),
    )
    cfg = install_tools(cfg, max_calls_per_round=_cap0)
    _tool_texts3 = [
        m.get("output") or ""
        for m in LLM_CALLS[-1]["input"]
        if m.get("type") == "function_call_output"
    ]
    check(
        "a call past the per-round cap is answered with the overflow note",
        any("次数已达上限" in t for t in _tool_texts3),
        str(_tool_texts3)[:200],
    )
    check(
        "and was never executed",
        all(not q.startswith("话题") or q == "话题A" for q in CappedSearch.calls),
        str(CappedSearch.calls),
    )

    # Tool results are retained only as bounded structured evidence. The visible reply,
    # window and raw archive all keep exactly what the group read.
    from qqbot.core.output import clean_reply as _cr

    _weather = _agent.ToolExecution("web_search", {"query": "明天 天气"}, "1. T C", True)
    from qqbot.core.engine import _evidence_memo as _memo_fn

    check("no tools means no evidence memo", _memo_fn((), cfg) is None)
    _history_execution = _agent.ToolExecution(
        "search_history",
        {"query": "改锥"},
        "⟦09-01 10:00⟧ 张伟⟦3⟧: 改锥在我这",
        True,
    )
    _memo = _memo_fn((_history_execution,), cfg)
    check(
        "evidence keeps no prompt-local member numbers",
        _memo is not None
        and _memo.render() == "⟦检索记录⟧\n查档“改锥”：[09-01 10:00] 张伟: 改锥在我这",
        _memo.render() if _memo else "none",
    )
    _page_memo = _memo_fn(
        (
            _agent.ToolExecution(
                "read_url",
                {"url": "https://user:secret@example.invalid/page?token=hidden#part"},
                "正文",
                True,
            ),
        ),
        cfg,
    )
    check(
        "evidence strips URL credentials, queries and fragments",
        _page_memo is not None
        and "https://example.invalid/page" in _page_memo.render()
        and all(word not in _page_memo.render() for word in ("secret", "hidden", "part")),
        _page_memo.render() if _page_memo else "none",
    )

    class Scripted(FakeText):
        """Answers each round from a script of neutral ModelTurn makers."""

        script: list = []

        async def respond(
            self,
            input,
            *,
            cfg,
            tools=None,
            max_tokens=None,
            effort=None,
            kind="reply",
            group_id=None,
        ):
            LLM_CALLS.append(
                {
                    "kind": kind,
                    "input": list(input),
                    "tools": tools,
                    "effort": effort,
                    "max_tokens": max_tokens,
                    "grade": cfg.reasoning_effort,
                    "timeout": cfg.timeout_sec,
                }
            )
            n = sum(1 for c in LLM_CALLS[self.start :] if c["kind"] == "reply") - 1
            step = self.script[min(n, len(self.script) - 1)]
            return step(input)

    def _use(*steps):
        text_model = Scripted()
        text_model.script = list(steps)
        text_model.start = len(LLM_CALLS)
        set_providers(
            Providers(
                text=text_model,
                vision=UnusedVision(),
                asr=UnusedAsr(),
                embedding=_EMBED,
                search=EndlessSearch(),
            )
        )

    def _calls(*calls):
        return lambda _input: response(model="fake-light", tool_calls=list(calls))

    cfg = install_budget(cfg, per_reply_cny=0.30)
    _use(_calls(_tc("明天 天气")), _calls(_send(text="明天多云")))
    st_pv = await REGISTRY.get(GROUP)
    ok_pv = await _respond(
        bot=bot,
        st=st_pv,
        cfg=cfg,
        persona=config().for_group(GROUP)[1],
        msg=_CM0(msg_id="pv1", user_id="u1", nickname="阿强", text="明天天气怎样", ts=_nl0()),
    )
    check(
        "the searched reply is sent without the marker",
        ok_pv and bot.sent[-1][1] == "明天多云",
        str(bot.sent[-1:]),
    )
    check(
        "a send with neither at nor reply goes out as a plain message",
        bot.quoted[-1] is None and bot.ats[-1] is None,
        f"{bot.quoted[-1]} {bot.ats[-1]}",
    )
    _pv_line = st_pv.recent[-1]
    check(
        "the window keeps only the visible reply",
        _pv_line.is_bot
        and _pv_line.text == "明天多云"
        and _pv_line.at == []
        and _pv_line.reply_to is None,
        repr(_pv_line),
    )
    _pv_row = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", _pv_line.msg_id
    )
    check("the archive also keeps only the visible reply", _pv_row == "明天多云", repr(_pv_row))

    # The send tool: whom to @ and which line to reply to are the model's choice,
    # named by the numbers the prompt showed. Two members share a card here - the
    # member numbers are what keep them apart.
    bot.members += [
        {"user_id": "u1", "card": "阿强", "nickname": "aq"},
        {"user_id": "u61", "card": "李芳", "nickname": "lf1"},
        {"user_id": "u62", "card": "李芳", "nickname": "lf2"},
    ]
    from qqbot.core.members import MEMBERS as _MEMpv

    _MEMpv.forget(GROUP)
    st_s = type(st_pv)(group_id=GroupId("5601"))
    st_s.loaded = st_s.history_loaded = True
    REGISTRY._groups[GroupId("5601")] = st_s
    _w1 = _CM0(msg_id="s-w1", user_id="u61", nickname="李芳", text="我是第一个李芳", ts=_nl0())
    _w2 = _CM0(msg_id="s-w2", user_id="u62", nickname="李芳", text="我是第二个李芳", ts=_nl0())
    _wm = _CM0(
        msg_id="s-wm",
        user_id="u1",
        nickname="阿强",
        text="@李芳 和 @李芳 都看看",
        ts=_nl0(),
        mentions=[("u61", "李芳"), ("u62", "李芳")],
    )
    _ask = _CM0(
        msg_id="s-ask",
        user_id="u1",
        nickname="阿强",
        text="小X 帮我跟第二个李芳打个招呼",
        ts=_nl0(),
    )
    for _m in (_w1, _w2, _wm, _ask):
        st_s.add(_m)
    _persona = config().for_group(GROUP)[1]

    def _prompt_text(messages):
        return "\n".join(m["content"] for m in messages if isinstance(m.get("content"), str))

    _seen: dict = {}

    def _look_then_send(input):
        _seen["prompt"] = _prompt_text(input)
        return response(
            model="fake-light", tool_calls=[_send(text="@李芳 你好呀", at=[2], reply=2)]
        )

    _use(_look_then_send)
    n_sent = len(bot.sent)
    ok_s = await _respond(
        bot=bot, st=st_s, cfg=cfg, persona=_persona, msg=_ask, window=[_w1, _w2, _wm]
    )
    check(
        "two members sharing a card wear different member numbers",
        "李芳⟦1⟧: 我是第一个李芳" in _seen.get("prompt", "")
        and "李芳⟦2⟧: 我是第二个李芳" in _seen.get("prompt", "")
        and "阿强⟦3⟧: 小X 帮我" in _seen.get("prompt", ""),
        _seen.get("prompt", "")[-300:],
    )
    check(
        "same-name @ targets carry the numbers of their accounts",
        "@李芳⟦1⟧ 和 @李芳⟦2⟧ 都看看" in _seen.get("prompt", ""),
        _seen.get("prompt", "")[-400:],
    )
    check(
        "the send @-s the numbered member and replies to the numbered line",
        ok_s and len(bot.sent) == n_sent + 1 and bot.ats[-1] == "u62" and bot.quoted[-1] == "s-w2",
        f"{bot.ats[-1:]} {bot.quoted[-1:]}",
    )
    check(
        "a model-written @ and number are taken off the text",
        bot.sent[-1][1] == "你好呀",
        repr(bot.sent[-1]),
    )
    _s_line = st_s.recent[-1]
    check(
        "the window keeps whom it @-ed and which line it replied to",
        _s_line.is_bot
        and _s_line.text == "@李芳 你好呀"
        and _s_line.at == [("u62", "李芳")]
        and _s_line.reply_to == "s-w2",
        repr(_s_line),
    )
    _s_row = await pool().fetchrow(
        "SELECT plain_text, payload FROM raw_event WHERE platform_event_id=$1", _s_line.msg_id
    )
    _s_payload = (
        _j14.loads(_s_row["payload"]) if isinstance(_s_row["payload"], str) else _s_row["payload"]
    )
    check(
        "the archive reads as the group read it, with the @ as a segment",
        _s_row["plain_text"] == "@李芳 你好呀"
        and {"type": "at", "data": {"qq": "u62"}} in _s_payload["segments"]
        and _s_payload["reply_to"] == "s-w2",
        repr(_s_row),
    )
    # A restart rebuilds the same line from the archive: the @ comes back as data,
    # not as text the next render would show twice.
    from qqbot.core.state import GroupState as _GS14

    _re = _GS14(group_id=GroupId("5601"))
    await _re.load_history(self_id="999", owners=set())
    _back = next((m for m in _re.recent if m.msg_id == _s_line.msg_id), None)
    check(
        "a rebuilt window preserves the structured display and addressee",
        _back is not None
        and _back.text == "@李芳 你好呀"
        and [account for account, _ in _back.at] == ["u62"],
        repr(_back),
    )
    # The next prompt shows that line as the send call that made it, so what the
    # model reads of its own output is the shape it should produce.
    _use(_look_then_send)
    _follow = _CM0(msg_id="s-ask2", user_id="u1", nickname="阿强", text="小X 然后呢", ts=_nl0())
    st_s.add(_follow)
    await _eng.generate(
        providers=providers_bundle,
        media=_MEDIA_PROCESSOR,
        directory=_DIRECTORY,
        bot=bot,
        st=st_s,
        cfg=cfg,
        persona=_persona,
        msg=_follow,
        window=[_w1, _w2, _ask, _s_line],
    )
    _hist = LLM_CALLS[-1]["input"]
    _own = [m for m in _hist if m.get("type") == "function_call"]
    check(
        "the bot's past message is rendered as its send call",
        len(_own) == 1
        and _j14.loads(_own[0]["arguments"])
        == {
            "messages": [
                {
                    "content": [
                        {"type": "reply", "data": {"line": 2}},
                        {"type": "at", "data": {"member": 2}},
                        {"type": "text", "data": {"text": " 你好呀"}},
                    ]
                }
            ]
        },
        repr(_own),
    )
    _own_result = next((m for m in _hist if m.get("type") == "function_call_output"), {})
    check(
        "followed by its result, carrying the line's number",
        _own_result.get("call_id") == _own[0]["call_id"]
        and str(_own_result.get("output", "")).startswith("已发送：#4 "),
        repr(_own_result),
    )

    # A member or line number outside the frozen prompt snapshot invalidates the
    # send. The model gets one correction result and may retry without pretending
    # that a requested control segment was silently delivered.
    _use(
        _calls(_send(text="好的", at=[42], reply=77)),
        _calls(_send(text="好的")),
    )
    ok_u = await _respond(
        bot=bot, st=st_s, cfg=cfg, persona=_persona, msg=_follow, window=[_w1, _w2, _ask]
    )
    _u_tools = [
        m.get("output") for m in LLM_CALLS[-1]["input"] if m.get("type") == "function_call_output"
    ]
    check(
        "unknown snapshot numbers are rejected before a corrected send",
        ok_u
        and bot.sent[-1][1] == "好的"
        and bot.ats[-1] is None
        and bot.quoted[-1] is None
        and any("无效" in str(t) for t in _u_tools),
        f"{bot.sent[-1:]} {_u_tools}",
    )

    # A send that cannot be sent is answered like a failed tool, and the model sends
    # again on the next round.
    _use(_calls(_send(text="  ")), _calls(_send(text="重新发一遍", at=[3])))
    ok_e = await _respond(
        bot=bot, st=st_s, cfg=cfg, persona=_persona, msg=_follow, window=[_w1, _w2, _ask]
    )
    _e_tools = [
        m.get("output") for m in LLM_CALLS[-1]["input"] if m.get("type") == "function_call_output"
    ]
    check(
        "an empty send is refused in words and the next round sends",
        ok_e
        and bot.sent[-1][1] == "重新发一遍"
        and bot.ats[-1] == "u1"
        and any("没有可发送的内容" in str(t) for t in _e_tools),
        f"{bot.sent[-1:]} {_e_tools}",
    )

    _use(
        _calls(
            function_call("send_messages", {"messages": []}, call_id="bad-send"),
            function_call(
                "send_messages",
                {"messages": [{"content": [{"type": "text", "data": {"text": "同轮有效"}}]}]},
                call_id="good-send",
            ),
        )
    )
    n_multi_send = len(LLM_CALLS)
    ok_multi_send = await _respond(
        bot=bot, st=st_s, cfg=cfg, persona=_persona, msg=_follow, window=[_w1, _w2, _ask]
    )
    check(
        "a valid later send in the same response is not hidden by an invalid first send",
        ok_multi_send and bot.sent[-1][1] == "同轮有效" and len(LLM_CALLS) == n_multi_send + 1,
        str(bot.sent[-1:]),
    )

    # A send ends the reply: a search asked for in the same round never runs.
    EndlessSearch.calls.clear()
    _use(_calls(_tc("顺便查查"), _send(text="先这样")))
    ok_b = await _respond(
        bot=bot, st=st_s, cfg=cfg, persona=_persona, msg=_follow, window=[_w1, _w2, _ask]
    )
    check(
        "calls sharing a round with the send are not executed",
        ok_b and bot.sent[-1][1] == "先这样" and EndlessSearch.calls == [],
        f"{bot.sent[-1:]} {EndlessSearch.calls}",
    )

    # The send tool is the only way out: a round that writes its reply out as bare
    # text sends nothing, and no further round is spent on it.
    _use(
        lambda _m: response(text="明天多云", model="fake-light"),
        lambda m: response(model="fake-light", tool_calls=send_to_asker(m, "明天多云")),
    )
    n_bare, n_calls = len(bot.sent), len(LLM_CALLS)
    ok_bare = await _respond(
        bot=bot,
        st=st_pv,
        cfg=cfg,
        persona=config().for_group(GROUP)[1],
        msg=_CM0(msg_id="pv-bare", user_id="u1", nickname="阿强", text="大后天呢", ts=_nl0()),
    )
    check(
        "bare text sends nothing, and ends the reply",
        not ok_bare and len(bot.sent) == n_bare and len(LLM_CALLS) == n_calls + 1,
        f"{bot.sent[n_bare:]} {len(LLM_CALLS) - n_calls} round(s)",
    )

    _use(lambda m: response(model="fake-light", tool_calls=send_to_asker(m, "@阿强 明天多云")))
    ok_at = await _respond(
        bot=bot,
        st=st_pv,
        cfg=cfg,
        persona=config().for_group(GROUP)[1],
        msg=_CM0(msg_id="pv-at", user_id="u1", nickname="阿强", text="后天呢", ts=_nl0()),
    )
    check(
        "a send replying to the asker and @-ing them, the @ not doubled",
        ok_at
        and bot.sent[-1][1] == "明天多云"
        and bot.ats[-1] == "u1"
        and bot.quoted[-1] == "pv-at",
        str(bot.sent[-1:]),
    )
    check(
        "the window remembers the answer with its addressee",
        st_pv.recent[-1].is_bot
        and st_pv.recent[-1].text == "@阿强 明天多云"
        and st_pv.recent[-1].at == [("u1", "阿强")],
        repr(st_pv.recent[-1]),
    )
    _at_row = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", st_pv.recent[-1].msg_id
    )
    check("and the archive reads as the group read it", _at_row == "@阿强 明天多云", repr(_at_row))
    check(
        "an imitated provenance marker never reaches the group",
        _cr("明天多云 ⟦依据:搜索“天气”⟧") == "明天多云",
        repr(_cr("明天多云 ⟦依据:搜索“天气”⟧")),
    )
    check(
        "an imitated member number never reaches the group",
        _cr("李芳⟦2⟧ 你好") == "李芳 你好",
        repr(_cr("李芳⟦2⟧ 你好")),
    )
    # The quote pointer is transcript notation too - the real quote is the reply
    # segment the send path attaches. Asking in the prompt did not hold, so the
    # output layer strips it like every other imitated marker - but only at line
    # starts, where format imitation lives; mid-sentence it is likelier the
    # reply's own content, and where the readings collide the guard declines.
    check(
        "an imitated quote pointer never reaches the group",
        _cr("⟦回复 #26⟧ 这波我不评价") == "这波我不评价",
        repr(_cr("⟦回复 #26⟧ 这波我不评价")),
    )
    check(
        "its lost-message form too, on its own line",
        _cr("好的\n⟦回复更早的消息⟧ 我看看") == "好的\n我看看",
        repr(_cr("好的\n⟦回复更早的消息⟧ 我看看")),
    )
    check(
        "but a mid-sentence mention is content and stays",
        _cr("他原话就带着 ⟦回复 #3⟧ 这几个字") == "他原话就带着 [回复 #3] 这几个字",
        repr(_cr("他原话就带着 ⟦回复 #3⟧ 这几个字")),
    )
    # The square form is what a member's own imitation looks like after defang, and
    # what the model reads in their lines: quoting it back is content, not markup.
    check(
        "a square-bracket form is text and stays",
        _cr("[回复 #26] 这波我不评价") == "[回复 #26] 这波我不评价",
        repr(_cr("[回复 #26] 这波我不评价")),
    )
    # Every strip is counted: the guard doubles as the online sensor, and the
    # daily report reads these to show format discipline regressing at the source.
    from qqbot.core import output as _out

    _n0 = _out.STRIPPED["quote_mark"]
    _cr("⟦回复 #5⟧ 好")
    check(
        "a stripper hit is counted for the daily report",
        _out.STRIPPED["quote_mark"] == _n0 + 1,
        str(dict(_out.STRIPPED)),
    )
    _n1 = _out.STRIPPED["quote_mark"]
    _cr("干净的正文")
    check("a clean reply counts nothing", _out.STRIPPED["quote_mark"] == _n1)
    # Order matters: the quote mark is stripped first, or a line imitating both
    # markers would shed the quote and keep the uncovered line number.
    check(
        "a quote mark hiding a line number uncovers nothing",
        _cr("⟦回复 #2⟧ #3 阿强: 都别吵了") == "都别吵了",
        repr(_cr("⟦回复 #2⟧ #3 阿强: 都别吵了")),
    )

    # Evidence lives in structured, expiring storage rather than in the conversation
    # deque. Prompt assembly renders it immediately before the send call it supported.
    _expected_evidence = "⟦检索记录⟧\n搜索“明天 天气”：1. T C"
    _memo_row = await pool().fetchrow(
        "SELECT memo, expires_at FROM reply_trace WHERE reply_event_id=$1",
        _pv_line.msg_id,
    )
    check(
        "new reply evidence is structured and versioned",
        _memo_row["memo"]["schema"] == 1
        and _memo_row["memo"]["items"][0]["source"] == "web_search"
        and _memo_row["expires_at"] is not None,
        repr(dict(_memo_row)),
    )
    check(
        "and the deque holds only conversation",
        not any(m.text.startswith("⟦检索记录⟧") for m in st_pv.recent),
    )
    _use(_calls(_send(text="后天也多云")))
    await _eng.generate(
        providers=providers_bundle,
        media=_MEDIA_PROCESSOR,
        directory=_DIRECTORY,
        bot=bot,
        st=st_pv,
        cfg=cfg,
        persona=config().for_group(GROUP)[1],
        msg=_CM0(msg_id="pv2", user_id="u1", nickname="阿强", text="后天呢", ts=_nl0()),
    )
    _msgs2 = list(LLM_CALLS[-1]["input"])
    _ri = next(
        (
            i
            for i, m in enumerate(_msgs2)
            if m.get("type") == "function_call_output"
            and str(m.get("output", "")).startswith("已发送：")
            and i > 0
            and _msgs2[i - 1].get("type") == "function_call"
            and "明天多云" in str(_msgs2[i - 1].get("arguments", ""))
        ),
        None,
    )
    check(
        "assembly seats rendered evidence with the send call it fed",
        _ri is not None
        and _msgs2[_ri - 1].get("name") == "send_messages"
        and _msgs2[_ri - 2].get("role") == "assistant"
        and _msgs2[_ri - 2].get("content") == _expected_evidence
        and "依据" not in str(_msgs2[_ri].get("output", "")),
        str(_msgs2[_ri - 2 : _ri + 1] if _ri else _msgs2[-3:])[:200],
    )
    check(
        "an imitated evidence marker line never reaches the group",
        _cr("⟦检索记录⟧\n搜索“x”：y\n好的") == "搜索“x”：y\n好的",
        repr(_cr("⟦检索记录⟧\n搜索“x”：y\n好的")),
    )
    await pool().execute(
        "UPDATE reply_trace SET expires_at=NOW() - INTERVAL '1 second' WHERE reply_event_id=$1",
        _pv_line.msg_id,
    )
    check(
        "expired evidence degrades to absence",
        await _repo.evidence_for(GROUP, [_pv_line.msg_id]) == {},
    )
    set_providers(providers_bundle)

    # 15. the per-group blocklist, end to end. Blocked means unanswered and
    # nothing more: the message still archives - a hole where a person used to
    # be reads as broken context - and the list survives a restart via its own
    # table. The accepted price is that a blocked account still feeds memory.
    st15 = await REGISTRY.get(GROUP)
    await _repo.block(GROUP, "bad1")
    n_sent15 = len(bot.sent)
    ev_blocked = FakeEvent("小X 在吗", user_id="bad1", nickname="捣乱的", to_me=True)
    await GATEWAY.handle(bot, ev_blocked)
    await drain()
    check(
        "a blocked account draws no reply even when it @s the bot",
        len(bot.sent) == n_sent15,
        f"{len(bot.sent) - n_sent15} sent",
    )
    check(
        "but its message still archives, keeping the window coherent",
        await pool().fetchval(
            "SELECT count(*) FROM raw_event WHERE platform_event_id=$1", str(ev_blocked.message_id)
        )
        == 1,
    )
    fresh15 = type(st15)(group_id=GROUP)
    await fresh15.load()
    check("the block rule survives a restart", await fresh15.blocked_now("bad1"))
    await _repo.unblock(GROUP, "bad1")
    await GATEWAY.handle(
        bot, FakeEvent("小X 还在吗", user_id="bad1", nickname="捣乱的", to_me=True)
    )
    await drain()
    check("unblocking restores replies", len(bot.sent) == n_sent15 + 1)

    # A timed block lapses dynamically: the first message past its expiry answers
    # normally. The row remains as bounded per-target audit state and is replaced by a
    # later rule for the same scope.
    from datetime import timedelta as _btd

    await _repo.block(GROUP, "bad1", until=_nl0() - _btd(seconds=1))
    n_lapse = len(bot.sent)
    await GATEWAY.handle(
        bot, FakeEvent("小X 醒了吗", user_id="bad1", nickname="捣乱的", to_me=True)
    )
    await drain()
    check(
        "a lapsed timed block no longer blocks",
        len(bot.sent) == n_lapse + 1,
        f"{len(bot.sent) - n_lapse} sent",
    )
    check("and the dynamic rule is no longer active", not await st15.blocked_now("bad1"))
    check(
        "and its bounded audit row remains inactive",
        await pool().fetchval(
            "SELECT count(*) FROM group_blocklist WHERE group_id=123"
            " AND user_id='bad1' AND blocked_until <= NOW()"
        )
        == 1,
    )
    # A still-running timed block behaves like any block.
    await _repo.block(GROUP, "bad1", until=_nl0() + _btd(hours=1))
    n_live = len(bot.sent)
    await GATEWAY.handle(bot, FakeEvent("小X 在么", user_id="bad1", nickname="捣乱的", to_me=True))
    await drain()
    check("a running timed block still blocks", len(bot.sent) == n_live)
    await _repo.unblock(GROUP, "bad1")

    # 16. the consent gate: a member who never accepted the user agreement is
    # not replied to - they get the agreement text instead, once per cooldown,
    # with no model call and nothing spent - and /agree opens the door. Their
    # messages archive like anyone's; only the reply is withheld.
    from qqbot.core import agreement as _agree

    await REGISTRY.get(GROUP)
    n16 = len(bot.sent)
    calls16 = len(LLM_CALLS)
    ev16 = FakeEvent("小X 在吗", user_id="newbie", nickname="新人", to_me=True)
    await GATEWAY.handle(bot, ev16)
    await drain()
    check(
        "an unconsenting member draws a one-line pointer, not a reply",
        len(bot.sent) == n16 + 1
        and "/agree" in str(bot.sent[-1])
        and "/terms" in str(bot.sent[-1])
        and "【用户协议】" not in str(bot.sent[-1])  # the pointer, never the full text
        and len(LLM_CALLS) == calls16,
        str(bot.sent[n16:])[:160],
    )
    check(
        "and their message still archives",
        await pool().fetchval(
            "SELECT count(*) FROM raw_event WHERE platform_event_id=$1", str(ev16.message_id)
        )
        == 1,
    )
    # The platform-reported pointer is observed like every other bot-authored line.
    st16 = await REGISTRY.get(GROUP)
    _last16 = st16.recent[-1]
    check(
        "the agreement pointer returns through self-observation",
        _last16.is_bot and _last16.user_id == str(bot.self_id) and _agree.POINTER in _last16.text,
        f"{_last16.is_bot} {_last16.user_id} {_last16.text!r}",
    )
    await GATEWAY.handle(bot, FakeEvent("小X 在吗", user_id="newbie", nickname="新人", to_me=True))
    await drain()
    check(
        "the agreement prompt respects its cooldown",
        len(bot.sent) == n16 + 1,
        f"{len(bot.sent) - n16} sent",
    )
    check(
        "/agree records a first acceptance as first", await _agree.accept(GROUP, "newbie") is True
    )
    check("and a repeat as a repeat", await _agree.accept(GROUP, "newbie") is False)
    check(
        "consent in one group says nothing about another",
        not await _agree.ok(GroupId("456"), "newbie"),
    )
    # A version bump voids old acceptances: everyone re-consents to the new
    # text, and re-accepting registers as a change, not a repeat.
    _orig_version = _agree.version
    _agree.version = lambda: 2
    check("a version bump voids the old acceptance", not await _agree.ok(GROUP, "newbie"))
    check(
        "re-accepting the new version counts as a change",
        await _agree.accept(GROUP, "newbie") is True,
    )
    check("and satisfies the gate again", await _agree.ok(GROUP, "newbie"))
    _agree.version = _orig_version
    await GATEWAY.handle(bot, FakeEvent("小X 在吗", user_id="newbie", nickname="新人", to_me=True))
    await drain()
    check(
        "after /agree the reply path opens", len(bot.sent) == n16 + 2 and len(LLM_CALLS) > calls16
    )

    # 17. group notices become transcript lines: archived, in the window, and
    # never a reply - even a poke aimed at the bot itself only transcribes.
    st17 = await REGISTRY.get(GROUP)
    n17 = len(bot.sent)
    _t17 = int(_nl0().timestamp())
    ev_note = types.SimpleNamespace(
        group_id=GROUP, user_id="u9", notice_type="group_recall", operator_id="u9", time=_t17
    )
    await GATEWAY.handle_notice(bot, ev_note)
    await drain(0.3)
    check(
        "a recall becomes a window line",
        any(m.text == "⟦撤回了自己的一条消息⟧" and m.user_id == "u9" for m in st17.recent),
    )
    check(
        "and is archived once",
        await pool().fetchval(
            "SELECT count(*) FROM raw_event WHERE platform_event_id"
            " LIKE 'notice-group_recall-123-u9-%'"
        )
        == 1,
    )
    check(
        "notice rows retain their normalized archive type",
        await pool().fetchval(
            """SELECT event_type FROM raw_event
                WHERE platform_event_id LIKE 'notice-group_recall-123-u9-%'
                LIMIT 1"""
        )
        == "notice",
    )
    await GATEWAY.handle_notice(bot, ev_note)  # the database admission gate rejects it
    await drain(0.2)
    check(
        "a replayed notice lands only once",
        sum(1 for m in st17.recent if m.msg_id.startswith("notice-group_recall")) == 1,
    )
    rebuilt_notice_state = GroupState(group_id=GROUP)
    await rebuilt_notice_state.load_history(self_id=str(bot.self_id), owners=set())
    check(
        "a restart rebuild keeps normalized notice rows in the transcript",
        any(m.text == "⟦撤回了自己的一条消息⟧" for m in rebuilt_notice_state.recent),
    )
    await GATEWAY.handle_notice(
        bot,
        types.SimpleNamespace(
            group_id=GROUP,
            user_id="u1",
            notice_type="notify",
            sub_type="poke",
            target_id="999",
            time=_t17 + 1,
        ),
    )
    await GATEWAY.handle_notice(
        bot,
        types.SimpleNamespace(
            group_id=GROUP,
            user_id="u7",
            notice_type="group_increase",
            sub_type="approve",
            time=_t17 + 2,
        ),
    )
    await GATEWAY.handle_notice(
        bot,
        types.SimpleNamespace(
            group_id=GROUP,
            user_id="u7",
            notice_type="group_ban",
            sub_type="ban",
            duration=600,
            time=_t17 + 3,
        ),
    )
    await drain(0.3)
    check(
        "a poke at the bot is transcribed, never answered",
        any(m.text == "⟦戳了戳你⟧" for m in st17.recent) and len(bot.sent) == n17,
    )
    check("a join is transcribed", any(m.text == "⟦加入了本群⟧" for m in st17.recent))
    check(
        "a ban is transcribed with its span", any(m.text == "⟦被禁言 10 分钟⟧" for m in st17.recent)
    )
    for mid in ("r1", "r2"):  # an admin mass-recall: one author, same second
        await GATEWAY.handle_notice(
            bot,
            types.SimpleNamespace(
                group_id=GROUP,
                user_id="u9",
                operator_id="u1",
                message_id=mid,
                notice_type="group_recall",
                time=_t17 + 4,
            ),
        )
    # Mute-all arrives as user_id 0 with duration -1: group state, no member.
    await GATEWAY.handle_notice(
        bot,
        types.SimpleNamespace(
            group_id=GROUP,
            user_id=0,
            operator_id="u1",
            notice_type="group_ban",
            sub_type="ban",
            duration=-1,
            time=_t17 + 5,
        ),
    )
    # The bot's own message recalled by an admin: not transcribed - archiving
    # would mint an identity entity for the bot.
    await GATEWAY.handle_notice(
        bot,
        types.SimpleNamespace(
            group_id=GROUP,
            user_id="999",
            operator_id="u1",
            message_id="r3",
            notice_type="group_recall",
            time=_t17 + 6,
        ),
    )
    await drain(0.3)
    check(
        "same-second recalls of one author each get their line",
        sum(1 for m in st17.recent if m.text == "⟦一条消息被管理员撤回⟧" and m.user_id == "u9")
        == 2,
    )
    check("mute-all credits no phantom account", not any(m.user_id == "0" for m in st17.recent))
    check(
        "the bot's own events are not transcribed",
        not any(m.msg_id.startswith("notice") and m.user_id == "999" for m in st17.recent),
    )

    # 17b. quoting the bot's own line is being addressed, even with the reply
    # button's auto-@ stripped off by hand; quoting anybody else without an @
    # stays silence. Window-scoped: the check must stay synchronous.
    st17b = await REGISTRY.get(GROUP)
    bot_line = next(m for m in reversed(st17b.recent) if m.is_bot)
    user_line = next(m for m in reversed(st17b.recent) if not m.is_bot and m.user_id)
    n17b = len(bot.sent)
    await GATEWAY.handle(
        bot, FakeEvent("你确定吗", user_id="u1", reply_id=bot_line.msg_id, reply_from="999")
    )
    await drain()
    check(
        "a de-@'d quote of the bot's line still draws a reply",
        len(bot.sent) == n17b + 1,
        f"{len(bot.sent) - n17b} sent",
    )
    await GATEWAY.handle(
        bot,
        FakeEvent(
            "你确定吗", user_id="u1", reply_id=user_line.msg_id, reply_from=user_line.user_id
        ),
    )
    await drain()
    check(
        "quoting anybody else without an @ stays silence",
        len(bot.sent) == n17b + 1,
        f"{len(bot.sent) - n17b - 1} extra",
    )

    # A typed command is routed after archive admission, never through the model reply
    # path. Its line stays on the record, so the command answer's quote has an antecedent.
    n_cmd = len(bot.sent)
    n_cmd_model = len([call for call in LLM_CALLS if call["kind"] == "reply"])
    _cmd_ev = FakeEvent("/who", user_id="u1")
    await GATEWAY.handle(bot, _cmd_ev)
    await drain()
    check(
        "a command receives exactly one routed response",
        len(bot.sent) == n_cmd + 1,
        f"{len(bot.sent) - n_cmd} sent",
    )
    check(
        "a command never enters the model reply path",
        len([call for call in LLM_CALLS if call["kind"] == "reply"]) == n_cmd_model,
    )
    check(
        "but it enters the window", any(m.msg_id == str(_cmd_ev.message_id) for m in st17b.recent)
    )
    _cmd_row = await pool().fetchval(
        "SELECT plain_text FROM raw_event WHERE platform_event_id=$1", str(_cmd_ev.message_id)
    )
    check("and the archive", _cmd_row is not None and "/who" in _cmd_row, repr(_cmd_row))

    # 18. member numbers: members sharing a card are told apart by the number each
    # person wears in one render, not by anything stored against the name.
    from qqbot.core import tools as _tools
    from qqbot.core.member_numbers import MemberNumbers as _MN18
    from qqbot.core.members import MEMBERS as _MEM18

    bot.members += [
        {"user_id": "u31", "card": "张伟", "nickname": "zw"},
        {"user_id": "u32", "card": "张伟", "nickname": "wei"},
    ]
    _MEM18.forget(GROUP)
    named = await _MEM18.names_of(bot, GROUP, ["u31", "u32"])
    check(
        "the member list holds plain names, shared ones included",
        named == {"u31": "张伟", "u32": "张伟"},
        str(named),
    )
    # A merged main and alt are one person: one number between them, while an
    # unrelated member sharing the card gets a number of their own.
    await seed(GROUP, "u41", "王大锤", text="大号在此")
    await seed(GROUP, "u42", "王大锤", text="小号在此")
    await seed(GROUP, "u43", "王大锤", text="我是另一个王大锤")
    await _DIRECTORY.merge("u42", "u41")
    _p18 = _MN18(self_id="999")
    await _p18.learn(["u41", "u42", "u43"])
    n41, n42, n43 = _p18.number("u41"), _p18.number("u42", spoke=True), _p18.number("u43")
    check(
        "a merged person's accounts share one number; a namesake gets another",
        n41 == n42 and n43 != n41,
        f"{n41} {n42} {n43}",
    )
    check(
        "the account to @ for a person is the one that spoke last",
        _p18.account(n41) == "u42" and _p18.accounts(n41) == ["u41", "u42"],
        f"{_p18.account(n41)} {_p18.accounts(n41)}",
    )
    check("the bot is never numbered", _p18.number("999") == 0)
    check("a number no render showed names nobody", _p18.account(99) is None)
    # A rename relabels the window: the speaker's line and the bot's own @ of them.
    _line18 = _CM0(msg_id="ns-1", user_id="u31", nickname="旧名", text="改锥在我这", ts=_nl0())
    _own18 = _CM0(
        msg_id="ns-2",
        user_id="999",
        nickname="小X",
        text="收到",
        ts=_nl0(),
        is_bot=True,
        at=[("u31", "旧名")],
    )
    n18 = await _MEM18.relabel(bot, GROUP, [_line18, _own18])
    check(
        "relabel carries the current card onto lines and onto the bot's @s",
        _line18.nickname == "张伟" and _own18.at == [("u31", "张伟")] and n18 == 2,
        f"{_line18.nickname} {_own18.at} {n18}",
    )
    # A member the table has never heard of forces one refresh inside the TTL -
    # a newcomer @-ed a minute after joining must resolve - and a miss that
    # survives the refresh is remembered, so a departed member's old lines do
    # not cost a fetch on every reply.
    calls18 = bot.member_list_calls
    await _MEM18.names_of(bot, GROUP, ["u31", "u-gone"])
    check(
        "an unknown member forces one refresh inside the TTL",
        bot.member_list_calls == calls18 + 1,
        str(bot.member_list_calls - calls18),
    )
    await _MEM18.names_of(bot, GROUP, ["u31", "u-gone"])
    check(
        "and a miss that survived it does not force another",
        bot.member_list_calls == calls18 + 1,
        str(bot.member_list_calls - calls18),
    )
    bot.members.append({"user_id": "u-new", "card": "新人甲", "nickname": "n"})
    check(
        "a newcomer resolves at once, inside the TTL",
        (await _MEM18.name_of(bot, GROUP, "u-new")) == "新人甲",
    )
    await seed(GROUP, "u31", "张伟", text="改锥昨天借给阿强了")
    await seed(GROUP, "u32", "张伟", text="改锥我根本没见过")
    # Bare-hit mode: the pin is about which lines are *hits* - with context on,
    # the other namesake's line would legitimately appear as surroundings.
    _base_rcfg18 = config().default.tools.search_history
    _ctx18 = _base_rcfg18.context_lines
    _rcfg18 = _base_rcfg18.model_copy(update={"context_lines": 0})
    _p18b = _MN18(self_id="999")
    n31 = _p18b.number("u31", spoke=True)
    got = await _tools.search_history(GROUP, "改锥", speaker=n31, rcfg=_rcfg18, people=_p18b)
    check(
        "search_history narrows by member number, not the shared name",
        "借给阿强" in got and "没见过" not in got,
        got,
    )
    got_nums = await _tools.search_history(GROUP, "改锥", rcfg=_rcfg18, people=_p18b)
    check(
        "search results number both namesakes, the known one as the prompt did",
        f"张伟⟦{n31}⟧: 改锥昨天借给阿强了" in got_nums
        and f"张伟⟦{_p18b.known('u32')}⟧: 改锥我根本没见过" in got_nums
        and _p18b.known("u32") not in (0, n31),
        got_nums,
    )
    got_miss = await _tools.search_history(GROUP, "改锥", speaker=77, people=_p18b)
    check(
        "a member number the prompt never showed is answered in words",
        not _tools.verified(got_miss) and "77" in got_miss,
        got_miss,
    )
    got_name = await _tools.search_history(
        GROUP, "改锥", speaker=77, speaker_name="张伟", people=_p18b
    )
    check(
        "and with a name given, falls back to matching the name",
        "借给阿强" in got_name and "没见过" in got_name,
        got_name,
    )
    got_own = await _tools.search_history(GROUP, "明天多云", self_id="999")
    check(
        "the bot's own archived line wears the self tag in search results",
        "⟦0⟧: " in got_own,
        got_own,
    )
    # The answer as a whole is bounded; a cut answer says so.
    _chars18 = _rcfg18.max_result_chars
    _short_rcfg18 = _rcfg18.model_copy(update={"max_result_chars": 1000})
    got_cut = await _tools.search_history(GROUP, "改锥", rcfg=_short_rcfg18)
    check("a short search answer is not cut", "结果过长" not in got_cut, got_cut[-60:])
    from qqbot.settings import SearchHistoryToolCfg as _RC

    _tiny = _RC(
        **{
            **_rcfg18.model_dump(),
            "max_result_chars": 1000,
            "context_lines": 0,
        }
    )
    await seed(GROUP, "u31", "张伟", text="改锥" + "很长的话" * 300)
    got_cut2 = await _tools.search_history(GROUP, "改锥", rcfg=_tiny)
    check(
        "an over-long search answer is cut at a line and says so",
        "结果过长" in got_cut2 and len(got_cut2) < 1200,
        str(len(got_cut2)),
    )

    # 19. One terminal send call can deliver several independent QQ messages. The
    # delivery lock keeps each batch contiguous while generation remains concurrent.
    from datetime import timedelta as _td19
    from qqbot.core.agent import MessageDraft as _MD19, ReplyDraft as _RD19
    from qqbot.core.outbound import (
        DiceSegment as _D19,
        ReplySegment as _R19,
        TextSegment as _T19,
    )
    from qqbot.domain.evidence import (
        EvidenceItem as _EI19,
        EvidenceMemo as _EM19,
        EvidenceOutcome as _EO19,
        EvidenceSource as _ES19,
    )
    from qqbot.db import repo as _repo19

    _created19 = _nl0()
    _memo19 = _EM19(
        items=(_EI19(_ES19.WEB_SEARCH, "虚构查询", _EO19.VERIFIED, "虚构结果"),),
        created_at=_created19,
        expires_at=_created19 + _td19(days=1),
    )

    def _draft19(*texts, evidence=None):
        return _RD19(
            tuple(_MD19((_T19(text),)) for text in texts),
            evidence=evidence,
        )

    _replies19 = {
        "clean-empty": _draft19("C1", "** **"),
        "partial": _draft19("P1", "P2", "P3", evidence=_memo19),
        "retry": _RD19(
            (
                _MD19((_R19("recalled"), _T19("R1"))),
                _MD19((_T19("R2"),)),
            )
        ),
        "evidence": _RD19(
            (_MD19((_T19("E1"),)), _MD19((_D19(),))),
            evidence=_memo19,
        ),
        "A": _draft19("A1", "A2"),
        "B": _draft19("B1", "B2"),
    }
    _original_generate19 = _eng.generate

    async def _generate19(*, msg, **_kwargs):
        return _replies19[msg.text]

    class ActionFailed(Exception):
        pass

    class _BatchBot19(FakeBot):
        def __init__(self):
            super().__init__()
            self.attempted = []
            self.fail_text = None
            self.fail_reply_text = None
            self.failed_reply = False

        async def send_group_msg(self, *, group_id, message):
            text = "".join(
                segment["data"].get("text", "") for segment in message if segment["type"] == "text"
            )
            self.attempted.append(text)
            await asyncio.sleep(0.01)
            if text == self.fail_text:
                raise RuntimeError("scripted refusal")
            if (
                text == self.fail_reply_text
                and not self.failed_reply
                and any(segment["type"] == "reply" for segment in message)
            ):
                self.failed_reply = True
                raise ActionFailed("quoted message is gone")
            return await super().send_group_msg(group_id=group_id, message=message)

    _batch_bot19 = _BatchBot19()
    _batch_state19 = type(st_pv)(group_id=GroupId("5701"))
    _batch_state19.loaded = _batch_state19.history_loaded = True
    REGISTRY._groups[GroupId("5701")] = _batch_state19
    _eng.generate = _generate19
    try:
        _before_clean19 = len(_batch_bot19.attempted)
        _clean_empty19 = await _respond(
            bot=_batch_bot19,
            st=_batch_state19,
            cfg=cfg,
            persona=_persona,
            msg=_CM0("batch-c", "u1", "阿强", "clean-empty", _nl0()),
        )
        check(
            "the whole batch is cleaned before its first protocol send",
            not _clean_empty19 and len(_batch_bot19.attempted) == _before_clean19,
            repr(_batch_bot19.attempted[_before_clean19:]),
        )

        _batch_bot19.fail_text = "P2"
        _partial19 = await _respond(
            bot=_batch_bot19,
            st=_batch_state19,
            cfg=cfg,
            persona=_persona,
            msg=_CM0("batch-p", "u1", "阿强", "partial", _nl0()),
        )
        check(
            "an irrecoverable batch failure keeps the prefix and stops the suffix",
            _partial19
            and _batch_bot19.attempted[-2:] == ["P1", "P2"]
            and [message.text for message in list(_batch_state19.recent)[-1:]] == ["P1"],
            f"{_batch_bot19.attempted} {list(_batch_state19.recent)[-3:]}",
        )
        _partial_id19 = _batch_state19.recent[-1].msg_id
        check(
            "a delivered prefix is archived before the failed item",
            await pool().fetchval(
                "SELECT plain_text FROM raw_event WHERE platform_event_id=$1",
                _partial_id19,
            )
            == "P1",
        )

        _batch_bot19.fail_text = None
        _batch_bot19.fail_reply_text = "R1"
        _retry19 = await _respond(
            bot=_batch_bot19,
            st=_batch_state19,
            cfg=cfg,
            persona=_persona,
            msg=_CM0("batch-r", "u1", "阿强", "retry", _nl0()),
        )
        _retry_lines19 = list(_batch_state19.recent)[-2:]
        check(
            "reply fallback retries only that item and continues the batch",
            _retry19
            and _batch_bot19.attempted[-3:] == ["R1", "R1", "R2"]
            and [message.text for message in _retry_lines19] == ["R1", "R2"]
            and _retry_lines19[0].reply_to is None,
            f"{_batch_bot19.attempted[-3:]} {_retry_lines19}",
        )
        _batch_bot19.fail_reply_text = None

        _evidence19 = await _respond(
            bot=_batch_bot19,
            st=_batch_state19,
            cfg=cfg,
            persona=_persona,
            msg=_CM0("batch-e", "u1", "阿强", "evidence", _nl0()),
        )
        _evidence_lines19 = list(_batch_state19.recent)[-2:]
        _stored19 = await _repo19.evidence_for(
            GroupId("5701"),
            [message.msg_id for message in _evidence_lines19],
        )
        check(
            "batch evidence is stored only on the first delivered message",
            _evidence19
            and [message.text for message in _evidence_lines19] == ["E1", "⟦骰子⟧"]
            and list(_stored19) == [_evidence_lines19[0].msg_id],
            repr(_stored19),
        )

        _before19 = len(_batch_bot19.attempted)
        _results19 = await asyncio.gather(
            *(
                _respond(
                    bot=_batch_bot19,
                    st=_batch_state19,
                    cfg=cfg,
                    persona=_persona,
                    msg=_CM0(f"batch-{name}", "u1", "阿强", name, _nl0()),
                )
                for name in ("A", "B")
            )
        )
        _order19 = _batch_bot19.attempted[_before19:]
        check(
            "concurrent reply batches do not interleave their messages",
            all(_results19) and _order19 in (["A1", "A2", "B1", "B2"], ["B1", "B2", "A1", "A2"]),
            repr(_order19),
        )
    finally:
        _eng.generate = _original_generate19

    # Last, so every kind of memory write has actually happened by now. Reasoning
    # models bill deliberation as output, so a memory call must ask for a terse
    # direct answer - deliberating under a word limit truncates the answer itself,
    # cutting it off mid-sentence.
    check(
        "the memory path never asks the model to write prose",
        not any(c["kind"] in ("knowledge", "summary") for c in LLM_CALLS),
        str({c["kind"] for c in LLM_CALLS}),
    )
    # Each use of the text model brings its own settings: extraction reads a whole
    # chunk of transcript, so it carries a grade and a patience the reply path would
    # never grant, and neither is a call-site exception.
    _txtcfg = config().default.capabilities.text
    check(
        "extraction and replies each run on their own configured settings",
        all(
            c["grade"] == _txtcfg.extract.reasoning_effort
            and c["timeout"] == _txtcfg.extract.timeout_sec
            for c in LLM_CALLS
            if c["kind"] == "extract"
        )
        and all(
            c["grade"] == _txtcfg.reasoning_effort and c["timeout"] == _txtcfg.timeout_sec
            for c in LLM_CALLS
            if c["kind"] == "reply"
        ),
    )

    await GATEWAY.shutdown()
    await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
