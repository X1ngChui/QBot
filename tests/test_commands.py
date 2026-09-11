"""The command layer: the catalogue, who may run what, and what the commands do.

Split into two halves that fail for different reasons.

The first half is pure. It checks the catalogue and the permission rules, which is where
the decisions live precisely because plugins/commands.py cannot be imported by a test -
on_command() runs at import time and needs a NoneBot runtime. A permission bug in there
is invisible until somebody exploits it, and one was: an ordinary member wrote onto
another member's record because the check read a QQ group role.

The second half runs against a real database, through services.Directory - the object the
handlers call. Between them, what a command does is covered; the handlers' own argument
parsing and the wiring of each check into a handler are not, because the handler module
cannot be imported here.
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
import inspect
import itertools

from qqbot.core import perms
from qqbot.core.command_catalog import CATALOG, PREFIXES, detail_text, find, help_text
from qqbot.db import close_pool, init_pool, pool
from qqbot.domain.memory import Fact, MemoryType
from qqbot.gateway.ingest import ingestor
from qqbot.gateway.onebot import GroupMessage, Sender
from qqbot.repositories import (
    EventRepository, IdentityRepository, JobQueue, MemoryRepository,
)
from qqbot.repositories.job import JobType
from qqbot.services import Directory, IdentityResolver, NameTaken, UnknownAccount
from qqbot.util import now_local
from _db import reset

fails = []
GROUP = 8001
OTHER = 8002
_SEQ = itertools.count(1)


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


def wiring() -> None:
    """The handlers reach NoneBot only by plugin.py importing the module they live in.

    Checked in the source rather than by importing it, because importing is the one
    thing a test cannot do here. Nothing reads the name afterwards, so it looks
    removable to a person and to a linter alike - and removing it is silent: every
    command falls through to the reply path, with no error anywhere.
    """
    import ast
    src = (ROOT / "qqbot" / "plugin.py").read_text(encoding="utf-8")
    imported = {
        alias.name
        for node in ast.walk(ast.parse(src)) if isinstance(node, ast.ImportFrom)
        if (node.module or "").endswith("plugins") for alias in node.names
    }
    for mod in ("commands", "tasks"):
        check(f"plugin.py imports plugins.{mod}, which is what registers it",
              mod in imported, str(sorted(imported)))


def _commands_source():
    """plugins/commands.py as a syntax tree - the only way a test can look at it."""
    import ast
    return ast.parse((ROOT / "qqbot" / "plugins" / "commands.py").read_text(encoding="utf-8"))


def registrations() -> None:
    """Every catalogued command is registered, and registered so that only its own
    name reaches it.

    NoneBot resolves a message against the longest registered prefix, so without a
    required break after the name an unregistered longer name - /topology, /cards,
    /whoami - would run the shorter command it starts with, carrying the rest as
    its argument. Read from the source, like wiring(): the registrations run at
    import time and cannot be observed any other way.
    """
    import ast
    tree = _commands_source()
    shared = next((ast.literal_eval(node.value) for node in ast.walk(tree)
                   if isinstance(node, ast.Assign)
                   and any(isinstance(t, ast.Name) and t.id == "_CMD" for t in node.targets)),
                  {})
    calls = [node for node in ast.walk(tree)
             if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
             and node.func.id == "on_command"]

    def demands_break(call) -> bool:
        for kw in call.keywords:
            if kw.arg == "force_whitespace":
                return isinstance(kw.value, ast.Constant) and kw.value.value is True
            if kw.arg is None and isinstance(kw.value, ast.Name) and kw.value.id == "_CMD":
                return shared.get("force_whitespace") is True
        return False

    names = {"/" + c.args[0].value for c in calls
             if c.args and isinstance(c.args[0], ast.Constant)}
    check("every catalogued command is registered, and nothing else is",
          names == set(PREFIXES), str(names ^ set(PREFIXES)))
    check("every registration demands a break after the name",
          bool(calls) and all(demands_break(c) for c in calls),
          str([c.args[0].value for c in calls if not demands_break(c)]))


def reload_refusal() -> None:
    """What /reload posts when the config fails validation.

    The formatter is compiled out of the source on its own, because the module it
    lives in cannot be imported here. It is written to need nothing but the
    exception, which is what makes that possible.
    """
    import ast
    from pydantic import BaseModel, ValidationError

    tree = _commands_source()
    fn = next(node for node in ast.walk(tree)
              if isinstance(node, ast.FunctionDef) and node.name == "_validation_summary")
    ns = {"ValidationError": ValidationError}
    exec(compile(ast.Module(body=[fn], type_ignores=[]), "commands.py", "exec"), ns)
    summary = ns["_validation_summary"]

    class Proxy(BaseModel):
        url: str
        timeout: int

    class Settings(BaseModel):
        proxy: Proxy
        owners: list[str]

    try:
        Settings.model_validate({"proxy": {"url": "http://user:hunter2@proxy.test"},
                                 "owners": "10001"})
        check("the fixture fails validation", False)
        return
    except ValidationError as e:
        err = e
    out = summary(err)
    # The guard is only meaningful if the naive rendering would have leaked.
    check("pydantic's own rendering echoes the block that failed", "hunter2" in str(err))
    check("the summary names each failing key by its path",
          "proxy.timeout: " in out and "owners: " in out, out)
    check("one line per error", len(out.splitlines()) == err.error_count(), out)
    check("and never the value that failed", "hunter2" not in out and "10001" not in out,
          out)


def catalogue() -> None:
    """The listing, the routing table and the permission rules. No I/O."""
    # Every documented command must be routed away from the reply pipeline, otherwise
    # typing one would draw a chat reply on top of the command output.
    from qqbot.core.pipeline import COMMANDS
    check("the gateway routes exactly the documented commands",
          set(COMMANDS) == set(PREFIXES), str(set(COMMANDS) ^ set(PREFIXES)))
    check("/help documents itself", "/help" in PREFIXES)

    listing = help_text()
    check("every command is listed", all(c.name in listing for c in CATALOG), listing)
    # The permission levels, pinned at the data: self-serve is the record
    # commands a member runs against themselves, `member` the read-only
    # surfaces they see whole, and a member's listing shows those and nothing
    # else - an owner command in it would advertise what silence hides.
    check("self-serve is exactly the seven subject commands",
          {c.name for c in CATALOG if c.self_serve}
          == {"/help", "/agree", "/terms", "/who", "/note", "/alias", "/forget"},
          str({c.name for c in CATALOG if c.self_serve}))
    check("member-whole is exactly the four read-only surfaces",
          {c.name for c in CATALOG if c.member}
          == {"/card", "/stats", "/top", "/groupstats"},
          str({c.name for c in CATALOG if c.member}))
    check("no command is both self-serve and member-whole",
          not any(c.self_serve and c.member for c in CATALOG))
    member = help_text(owner=False)
    check("a member's listing shows exactly what a member can run",
          all((c.name in member) == (c.self_serve or c.member) for c in CATALOG),
          member)
    check("every open command's detail says so",
          all("普通成员" in c.detail for c in CATALOG
              if (c.self_serve or c.member) and c.name != "/help"),
          str([c.name for c in CATALOG
               if (c.self_serve or c.member) and "普通成员" not in c.detail]))
    check("every command carries a description", all(c.what for c in CATALOG))
    # The listing is one line each; anything longer belongs in the detail text, which is
    # only read by someone who asked for it.
    check("the listing stays one line per command",
          all(len(c.what.splitlines()) == 1 for c in CATALOG),
          str([c.name for c in CATALOG if len(c.what.splitlines()) != 1]))
    check("and says how to get more", "详细用法：/help" in listing)
    check("every command is reachable by name, with or without the slash",
          all(find(c.name) is c and find(c.name.lstrip("/")) is c for c in CATALOG))
    check("an unknown name resolves to nothing", find("nope") is None and find("") is None)
    check("the detail names the command and repeats its summary",
          all(c.name in detail_text(c) and c.what in detail_text(c) for c in CATALOG))
    # Every command that takes arguments has to say what they are somewhere, and the
    # listing is not the place.
    check("commands with arguments document them",
          all(find(n).detail.strip()
              for n in ("/who", "/note", "/alias", "/forget", "/merge", "/split", "/log")))
    # Shared numbers and per-group numbers are separate commands: side by side, a global
    # call count reads as this group's the moment it sits next to a per-group figure.
    check("global and per-group stats are separate commands",
          {"/stats", "/groupstats"} <= set(PREFIXES))
    # The memory system writes into a prompt nobody sees, and a wrong record looks exactly
    # like a right one until the bot says something odd - so every store it keeps has a
    # way to read it, to correct it, and to force a rebuild.
    check("every store the bot writes to can be read back",
          {"/card", "/who"} <= set(PREFIXES))
    check("and each has a way to correct it by hand",
          {"/note", "/alias", "/forget"} <= set(PREFIXES))
    check("and one pass relearns all of it",
          "/relearn" in set(PREFIXES) and "/recard" not in set(PREFIXES))
    # A wrong merge is the worst thing this system can do to itself, so the undo has to
    # exist as a command rather than as a database session.
    check("a merge can be undone from inside the group",
          {"/merge", "/split"} <= set(PREFIXES))
    check("with the log reachable from inside the group", "/log" in PREFIXES)
    # The member-facing half is gone: reading your own record and correcting it are things
    # the bot does in conversation, and a command that duplicates them is a second
    # interface to maintain.
    check("nothing is left that duplicates talking to the bot",
          "/whoami" not in PREFIXES)

    # NoneBot resolves a message against the longest *registered* prefix, so a command
    # containing another is safe only while both are registered - the danger is a name
    # nobody registered, which arrives as the shorter command carrying the rest as its
    # argument. This is that rule, not "no command may contain another".
    def _resolves(text):
        return max((p for p in PREFIXES if text.startswith(p)), key=len, default=None)

    check("every command resolves to itself, with or without arguments",
          all(_resolves(c.name) == c.name and _resolves(c.name + " x") == c.name
              for c in CATALOG),
          str([c.name for c in CATALOG if _resolves(c.name + " x") != c.name]))
    check("/relearn is not swallowed by /reload", _resolves("/relearn") == "/relearn")
    check("/groupstats is not swallowed by the /stats prefix",
          not "/groupstats".startswith("/stats"))

    # -- who may run what ---------------------------------------------------
    # One question, one answer. Invented ids, like every account in this file: a test that
    # pastes in the real owner list turns the config into something anyone reading the
    # repo can look up.
    OWNERS = ["o1", " o2 ", ""]
    check("a configured owner is an owner", perms.is_owner("o1", OWNERS))
    check("owner ids are matched after trimming", perms.is_owner("o2", OWNERS))
    check("anyone else is not", not perms.is_owner("111", OWNERS))
    check("an empty owner list leaves nobody an owner", not perms.is_owner("111", []))
    # The incident this replaced: running the QQ group was read as running the bot, so a
    # group admin who was not an owner wrote a record onto the group owner's account. No
    # platform role can reach the decision now, because there is no argument to pass one
    # through.
    check("a speaker's group role cannot reach the decision at all",
          list(inspect.signature(perms.is_owner).parameters) == ["user_id", "owners"],
          str(inspect.signature(perms.is_owner)))
    # Naming a person is an @ and nothing else. Shown rather than stated: the example is
    # what a reader copies, and a sentence saying so as well is a sentence nobody needs.
    check("every command that names a person does it by @",
          all("@某人" in find(n).detail
              for n in ("/who", "/note", "/alias", "/forget", "/split")),
          str([n for n in ("/who", "/note", "/alias", "/forget", "/split")
               if "@某人" not in find(n).detail]))
    # A detail page is read by someone who is about to type the thing.
    check("every command shows a worked example",
          all("示例：" in c.detail for c in CATALOG),
          str([c.name for c in CATALOG if "示例：" not in c.detail]))


async def say(group, uid, name, text="随便说说", *, at=None):
    """One message through the real inbound path, so speaking creates a person."""
    await ingestor().ingest(
        GroupMessage(
            message_id=f"c{next(_SEQ)}", group_id=group,
            sender=Sender(user_id=uid, card=name), segments=[],
            self_id="999", occurred_at=at or now_local(), plain_text=text,
        ),
    )


async def directory_service() -> None:
    """What the handlers actually call."""
    ids = IdentityRepository()
    mem = MemoryRepository()
    d = Directory(
        identity=IdentityResolver(ids), ids=ids, memory=mem,
        events=EventRepository(), jobs=JobQueue("test"),
    )

    # -- reading ------------------------------------------------------------
    # An account nobody has seen is a different answer from an account with nothing
    # recorded, and the handler says so differently.
    try:
        await d.person(GROUP, "ghost")
        check("an unseen account raises rather than inventing a person", False)
    except UnknownAccount as e:
        check("an unseen account raises rather than inventing a person",
              e.user_id == "ghost")

    await say(GROUP, "m1", "小明", "早")
    await say(GROUP, "m1", "小明", "在的")
    await say(GROUP, "m2", "阿花", "早啊")
    card = await d.person(GROUP, "m1")
    check("speaking creates a person with their group card as a name",
          card.display == "小明" and card.messages == 2, f"{card.display} {card.messages}")
    check("and nothing is recorded about them yet", not card.facts, str(card.facts))

    # -- notes --------------------------------------------------------------
    await d.note(GROUP, "m1", "在读研，别问工作")
    card = await d.person(GROUP, "m1")
    check("a note is readable straight back", card.note == "在读研，别问工作", card.note)
    check("and is marked as hand-written",
          [f.manual for f in card.facts] == [True], str(card.facts))
    # It replaces rather than appends: /note shows the current text and takes a whole new
    # one, so a second call is an edit.
    await d.note(GROUP, "m1", "改过一次")
    card = await d.person(GROUP, "m1")
    check("writing again replaces it", card.note == "改过一次" and len(card.facts) == 1,
          str(card.facts))
    await d.note(GROUP, "m1", "")
    check("and an empty note clears it", not (await d.person(GROUP, "m1")).note)
    await d.note(GROUP, "m1", "在读研，别问工作")

    # A note is a fact under its own predicate, which is what keeps extraction from
    # writing over it - and what puts it into the prompt through the ordinary path.
    eid = (await d.person(GROUP, "m1")).entity_id
    await mem.supersede(
        Fact(subject_entity_id=eid, predicate="likes", object_value="辣的",
             memory_type=MemoryType.PREFERENCE, group_id=GROUP, confidence=0.7),
        [], when=now_local())
    card = await d.person(GROUP, "m1")
    check("an extracted fact does not disturb the note",
          card.note == "在读研，别问工作" and card.summary == "喜欢辣的",
          f"{card.note!r} {card.summary!r}")
    check("the two are told apart by where they came from",
          len(card.learned) == 1 and len(card.facts) == 2, str(card.facts))

    # -- numbering ----------------------------------------------------------
    # /forget takes the number /who showed, so the numbering has to survive between the
    # two calls. Ordered by predicate rather than by confidence for exactly that reason.
    first = [(f.index, f.predicate) for f in (await d.person(GROUP, "m1")).facts]
    second = [(f.index, f.predicate) for f in (await d.person(GROUP, "m1")).facts]
    check("the numbering is stable between reads", first == second, str(first))
    check("and it starts at 1", first[0][0] == 1, str(first))

    check("forgetting a number nobody showed changes nothing",
          await d.forget(GROUP, "m1", 99) is None)
    target = next(f for f in (await d.person(GROUP, "m1")).facts if not f.manual)
    dropped = await d.forget(GROUP, "m1", target.index)
    check("forgetting reports what it removed",
          dropped is not None and dropped.text == "喜欢辣的", str(dropped))
    check("and the fact is gone from the record",
          not (await d.person(GROUP, "m1")).learned)
    check("while the hand-written note survives it",
          (await d.person(GROUP, "m1")).note == "在读研，别问工作")

    # -- names --------------------------------------------------------------
    await d.name(GROUP, "m1", "阿明")
    card = await d.person(GROUP, "m1")
    check("a hand-bound name is confirmed at once",
          "阿明" in card.other_names, str(card.other_names))
    check("and outranks what the platform reported",
          max(n.confidence for n in card.names if n.text == "阿明") >= 1.0,
          str([(n.text, n.confidence) for n in card.names]))
    # It has to be resolvable, or binding it accomplished nothing.
    hits = await ids.lookup(GROUP, "阿明")
    check("a bound name resolves to that person",
          [h.target_entity_id for h in hits] == [card.entity_id], str(hits))
    # Scoped to this group unless somebody asks otherwise: a name that resolves
    # everywhere carries what one group knows into another.
    check("and only in the group it was bound in", not await ids.lookup(OTHER, "阿明"))

    # A name belongs to one person. The validator refuses a batch where the model points
    # one name at two accounts - writing both means both are wrong - and the hand-typed
    # path had no such check at all, so the input carrying the most authority was the one
    # that could break the rule silently.
    try:
        await d.name(GROUP, "m2", "阿明")
        taken = None
    except NameTaken as e:
        taken = e
    check("a name already taken here is refused rather than pointed at two people",
          taken is not None and taken.text == "阿明", str(taken))
    check("and the refusal says who holds it",
          taken is not None and taken.holder == card.display, str(taken and taken.holder))
    check("binding the same name to the same person again is fine",
          (await d.name(GROUP, "m1", "阿明")).text == "阿明")
    check("and another group is another scope", (await d.name(OTHER, "m2", "阿明")).text
          == "阿明")

    # -- trust as a lever ---------------------------------------------------
    # The lever between "certain" and "struck out". Most of what needs correcting is a
    # name that is real but over-trusted, which neither full trust nor the axe can fix.
    low = await d.set_confidence(GROUP, "m1", "阿明", 0.4)
    check("a hand-lowered name stops being used without being struck out",
          low.confidence == 0.4
          and "阿明" not in (await d.person(GROUP, "m1")).other_names
          and "阿明" in [n.text for n in (await d.person(GROUP, "m1")).candidates],
          str(low))
    # The owner's number is a verdict, not a vote: the platform re-reporting the name
    # must not raise it back.
    await say(GROUP, "m1", "阿明", "又冒了个泡")
    still = [n for n in (await d.person(GROUP, "m1")).candidates if n.text == "阿明"]
    check("and automatic sightings cannot outvote it",
          still and abs(still[0].confidence - 0.4) < 1e-6,
          str([(n.text, n.confidence) for n in (await d.person(GROUP, "m1")).candidates]))
    up = await d.set_confidence(GROUP, "m1", "阿明", 0.9)
    check("raised back above the line, it is usable on the spot",
          up.confidence == 0.9 and "阿明" in (await d.person(GROUP, "m1")).other_names)
    check("setting a name they do not carry yet coins it at that confidence",
          (await d.set_confidence(GROUP, "m1", "新外号", 0.8)).confidence == 0.8
          and "新外号" in (await d.person(GROUP, "m1")).other_names)
    try:
        await d.set_confidence(GROUP, "m2", "阿明", 0.9)
        check("adjusting somebody else's name is refused", False)
    except NameTaken:
        check("adjusting somebody else's name is refused", True)

    # -- retirement sticks --------------------------------------------------
    # It must survive the very next message: the platform re-reports the card on
    # every message, and a rescore allowed to confirm it would bring a name struck
    # out at noon back by one past.
    await say(GROUP, "m9", "要撤的名片", "我说一句")
    await pool().execute(
        """UPDATE alias_evidence SET created_at = created_at - INTERVAL '1 day'
            WHERE alias_id IN (SELECT id FROM alias WHERE group_id=$1
                                AND alias_text='要撤的名片')""", GROUP)
    await say(GROUP, "m9", "要撤的名片", "再说一句")
    check("a card worn into a second day is a confirmed name",
          (await d.person(GROUP, "m9")).display == "要撤的名片"
          and any(n.text == "要撤的名片" for n in (await d.person(GROUP, "m9")).names))
    check("retiring it reports success", await d.unname(GROUP, "m9", "要撤的名片"))
    await say(GROUP, "m9", "要撤的名片", "又说一句")
    after_retire = await d.person(GROUP, "m9")
    check("and the next message does not resurrect it",
          all(n.text != "要撤的名片" for n in after_retire.names + after_retire.candidates),
          str([n.text for n in after_retire.names + after_retire.candidates]))

    check("retiring a name it does not answer to reports so",
          not await d.unname(GROUP, "m1", "没这个名字"))
    check("retiring one it does answer to succeeds", await d.unname(GROUP, "m1", "阿明"))
    check("the name stops resolving", not await ids.lookup(GROUP, "阿明"))
    # Retired, not deleted: messages already archived still need it to be readable, and
    # a deleted row is one nothing could reconstruct. aliases_for is the "answers to this
    # now" view and correctly omits it, so this reads the table.
    kept = await pool().fetchval(
        "SELECT status FROM alias WHERE alias_text='阿明' AND target_entity_id=$1",
        card.entity_id)
    check("but the row is kept for reading old messages", kept == "inactive", str(kept))

    # -- merge and split ----------------------------------------------------
    await say(GROUP, "alt", "小明的小号", "我是小号")
    check("two accounts start as two people",
          (await d.person(GROUP, "alt")).entity_id != card.entity_id)
    check("merging reports that something changed", await d.merge("alt", "m1"))
    merged = await d.person(GROUP, "alt")
    check("after a merge both accounts are one person",
          merged.entity_id == card.entity_id and merged.merged, str(merged.accounts))
    check("and their message counts add up", merged.messages == 4, str(merged.messages))
    check("the roster shows them once",
          len([c for c in await d.roster(GROUP) if c.entity_id == card.entity_id]) == 1)
    check("merging again reports that nothing changed",
          not await d.merge("alt", "m1"))
    try:
        await d.merge("alt", "ghost")
        check("merging an unseen account refuses", False)
    except UnknownAccount:
        check("merging an unseen account refuses", True)

    # Blocking is a decision about a person, so it has to cover every account they
    # hold: blocking the one that was @-ed while the alt keeps talking blocks
    # nobody. The command layer expands through this, and the DB write takes the
    # whole list.
    from qqbot.db import repo as _dbrepo
    accounts = await d.accounts_of_person("alt")
    check("a merged person answers with all their accounts",
          set(accounts) == {"alt", "m1"}, str(accounts))
    check("an account nobody has seen is a person of one",
          await d.accounts_of_person("ghost") == ["ghost"])
    await _dbrepo.block(GROUP, accounts)
    _muted, blocked = await _dbrepo.group_switches(GROUP)
    check("blocking a person blocks every account", {"alt", "m1"} <= set(blocked),
          str(blocked))
    check("a plain block has no expiry",
          all(blocked[a] is None for a in ("alt", "m1")), str(blocked))
    check("unblocking lifts all of them at once",
          await _dbrepo.unblock(GROUP, accounts))
    _muted, blocked = await _dbrepo.group_switches(GROUP)
    check("so nothing of that person is left blocked",
          not ({"alt", "m1"} & set(blocked)), str(blocked))
    check("and lifting a block nobody had reports nothing",
          not await _dbrepo.unblock(GROUP, accounts))

    # Timed blocks: the expiry is a column, loaded with the entry, and a lapsed
    # row is invisible from the moment it lapses - deleted whenever noticed.
    from datetime import timedelta as _btd
    from qqbot.util import now_local as _bnl
    _t_until = _bnl() + _btd(hours=2)
    await _dbrepo.block(GROUP, ["alt"], until=_t_until)
    _muted, blocked = await _dbrepo.group_switches(GROUP)
    check("a timed block loads with its expiry",
          blocked.get("alt") is not None
          and abs((blocked["alt"] - _t_until).total_seconds()) < 1, str(blocked))
    await _dbrepo.block(GROUP, ["alt"])
    _muted, blocked = await _dbrepo.group_switches(GROUP)
    check("re-blocking without a duration makes it permanent",
          "alt" in blocked and blocked["alt"] is None, str(blocked))
    await _dbrepo.block(GROUP, ["alt"], until=_bnl() - _btd(minutes=1))
    _muted, blocked = await _dbrepo.group_switches(GROUP)
    check("an already-lapsed block does not load", "alt" not in blocked,
          str(blocked))
    await _dbrepo.unblock_expired(GROUP)
    check("and the sweep removes its row",
          await pool().fetchval(
              "SELECT count(*) FROM group_blocklist WHERE group_id=$1", GROUP) == 0)

    # A block placed before the merge must follow it: the accounts became one
    # person afterwards, and half a block is not one. A timed one spreads with
    # its own clock - the merge widens who, never how long.
    await _dbrepo.block(GROUP, ["m1"], until=_t_until)
    spread = await d.blocks_after_merge("alt")
    _muted, blocked = await _dbrepo.group_switches(GROUP)
    check("a merge extends an existing block, expiry and all",
          spread == [(GROUP, _t_until)] and {"alt", "m1"} <= set(blocked)
          and blocked["alt"] == _t_until, f"{spread} {blocked}")
    # The caller may veto groups: /block refuses the owner and the bot itself,
    # and a merge must not smuggle either onto a blocklist through the back door.
    check("a shielded group keeps its blocklist untouched by the merge",
          await d.blocks_after_merge("alt", shielded=lambda gid: True) == [])
    await _dbrepo.unblock(GROUP, accounts)
    check("a person nobody blocked needs no propagation",
          await d.blocks_after_merge("alt") == [])

    # -- the spend leaderboard: person-level by query-time aggregation --------
    # The ledger stores the causing account (attribution rides a contextvar so no
    # provider learns about people); /top groups accounts through identity_account
    # at read time, which is what makes a merge issued AFTER the spending still
    # pull the history together - append-only rows, person-level answers.
    from qqbot.core.budget import BUDGET as _B
    with _B.attribute("alt"):
        await _B.record(kind="reply", model="t", cny=0.30, group_id=str(GROUP))
    with _B.attribute("m1"):
        await _B.record(kind="vision", model="t", cny=0.20, group_id=str(GROUP))
    with _B.attribute("stranger"):
        await _B.record(kind="reply", model="t", cny=0.10, group_id=str(GROUP))
    await _B.record(kind="extract", model="t", cny=9.0, group_id=str(GROUP))
    top = await _dbrepo.top_spenders(GROUP, k=5)
    check("merged accounts rank as one person",
          len(top) == 2 and set(top[0]["accounts"]) == {"alt", "m1"}
          and abs(float(top[0]["cny"]) - 0.50) < 1e-9,
          str([(r["accounts"], float(r["cny"])) for r in top]))
    check("an unmerged account ranks alone",
          top[1]["accounts"] == ["stranger"]
          and abs(float(top[1]["cny"]) - 0.10) < 1e-9, str(top[1]))
    check("shared spend nobody caused stays off the board",
          all(float(r["cny"]) < 1.0 for r in top))
    check("outside the attribution context nothing is attributed",
          await pool().fetchval(
              "SELECT user_id FROM cost_ledger WHERE kind='extract' AND model='t'")
          == "")

    await d.split("alt")
    after = await d.person(GROUP, "alt")
    check("splitting restores a person of their own",
          after.entity_id != card.entity_id and not after.merged, str(after.accounts))
    check("names the split account produced itself go with it",
          "小明的小号" in (after.display, *after.other_names), str(after.other_names))
    check("and names belonging to the other account stay behind",
          "小明" not in (after.display, *after.other_names), str(after.other_names))
    check("the note stayed with the original person",
          (await d.person(GROUP, "m1")).note == "在读研，别问工作"
          and not after.note, str(after.note))

    # -- isolation ----------------------------------------------------------
    # The property the whole design turns on. Every method takes group_id, and the reads
    # are filtered by it before anything else happens.
    await say(OTHER, "m1", "小明", "在别的群")
    await d.note(OTHER, "m1", "只在这个群成立")
    check("a note written in one group is not visible in another",
          (await d.person(OTHER, "m1")).note == "只在这个群成立"
          and (await d.person(GROUP, "m1")).note == "在读研，别问工作")
    check("and a roster is built only from what happened in that group",
          {c.user_id for c in await d.roster(OTHER)} == {"m1"},
          str([c.user_id for c in await d.roster(OTHER)]))
    check("group_id is required on every read, so it cannot be forgotten",
          all("group_id" in inspect.signature(getattr(Directory, n)).parameters
              for n in ("roster", "person", "note", "name", "unname", "forget")))

    # -- ordering -----------------------------------------------------------
    # The roster goes into the cached prefix, so its order must not depend on who spoke
    # last. Ranked by volume, tie-broken by account id.
    for i, uid in enumerate(["z1", "z2", "z3"]):
        await say(GROUP, uid, uid, "hi")
        for _ in range(i):
            await say(GROUP, uid, uid, "again")
    order = [c.user_id for c in await d.roster(GROUP)]
    check("the roster is ordered by volume, not recency",
          order.index("z3") < order.index("z2") < order.index("z1"), str(order))
    check("and asking twice gives the same order",
          [c.user_id for c in await d.roster(GROUP)] == order)
    check("the bot's own account can be left out",
          "m1" not in [c.user_id for c in await d.roster(GROUP, exclude={"m1"})])

    # -- relearn ------------------------------------------------------------
    # Queued rather than run: it is a paid model call, and a handler waiting on one stays
    # open long enough for the platform to time the reply out.
    job_id = await d.relearn(GROUP)
    claimed = await JobQueue("test-worker").claim()
    check("relearn queues an extraction job for this group",
          claimed is not None and claimed.id == job_id
          and claimed.job_type is JobType.EXTRACT_MEMORY
          and claimed.payload["group_id"] == GROUP,
          str(claimed))


async def main():
    wiring()
    registrations()
    reload_refusal()
    catalogue()
    await init_pool()
    await reset()
    try:
        await directory_service()
    finally:
        await close_pool()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(asyncio.run(main()))
