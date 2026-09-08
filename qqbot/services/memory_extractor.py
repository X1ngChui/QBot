"""Pulling candidates out of a stretch of group chat: names and facts.

By function call rather than by asking the model for JSON (design goal 7). The
difference is not the format - it is that the shape of each argument is defined here and
enforced by the backend: the predicate is an enum, the subject can only be a code from
the roster it was given, and every record has to carry a verbatim quote. A field the
model cannot answer is one it cannot invent; all it can do is not call the tool.

Accounts are always named by code, never by nickname. Two people in one group sharing a
name is ordinary, and a record filed under the wrong person stays in long-term memory.

The prompt is in two parts: fixed rules first, this batch's transcript second. The first
part never varies, so it sits inside the prefix cache.
"""

from __future__ import annotations

import json
import logging
import uuid
from dataclasses import dataclass

from ..domain.identity import AliasType
from ..domain.memory import Candidate, CandidateType
from ..providers import Kind, providers
from ..settings import Settings, ptext

log = logging.getLogger("qqbot.extract")

#: Predicates a person can only have one of at a time. A new answer overturns the old
#: one, which is what makes "he moved" expressible: the previous city gets a valid_to
#: rather than sitting alongside the new one.
SINGLE_VALUED = (
    "lives_in", "from_place", "works_as", "works_at", "studies_at",
    "majors_in", "birthday",
)

#: Predicates a person can have many of at once. Each object is stored as its own row -
#: the object doubles as the row's object_key under the one-current-fact index - so a
#: second thing somebody likes sits beside the first, and each ages on its own evidence:
#: a game mentioned once falls away while the one they talk about weekly stays.
MULTI_VALUED = (
    "likes", "dislikes", "avoids", "wants",
    "plays", "watches", "listens_to", "reads", "uses", "owns", "collects", "has_pet",
    "good_at", "speaks", "member_of",
    "visited", "fears", "allergic_to",
)

#: The closed set the tool schema offers. An open string would let one fact be written as
#: likes / like / liked on three rows that nothing could reconcile, so adding one means
#: editing a tuple above - a deliberate decision rather than something the model can do.
#: Predicates that overlap an existing one - known_for against good_at, plans_to against
#: wants, and their kin - are deliberately excluded (orthogonality rule): where two
#: predicates could hold the same fact, the vaguer one collects everything.
PREDICATES = SINGLE_VALUED + MULTI_VALUED


#: Recording one retracts the other about the same object: somebody who says they have
#: gone off something is not simultaneously a person who likes it. Without this the two
#: sit side by side and both go into the prompt.
OPPOSITES = {"likes": "dislikes", "dislikes": "likes"}

#: Deliberately without `note`: that predicate belongs to what an owner typed, and the
#: model must have no way to write over it.
ALIAS_KINDS = tuple(t.value for t in (
    AliasType.NICKNAME, AliasType.SHORT_NAME, AliasType.JOKE_NAME,
    AliasType.TITLE, AliasType.RELATIONSHIP_NAME,
))

#: What may be recorded about the group itself, as opposed to about anyone in it. Two
#: kinds, and the omissions are the point.
#:
#: There is no kind for an in-joke, because it is the one category that cannot work:
#: nothing can check it, the model will always find one, and once written down it gets
#: used in a reply, archived, and read back by the next pass as evidence that the group
#: still says it. The joke outlives the group's interest in it, with the bot keeping it
#: alive.
#:
#: What is left is what the model cannot work out from the transcript in front of it: what
#: this group is for, and what its words mean.
GROUP_TOPIC = "topic"
#: A term row's object_key is the word being defined, so redefining supersedes rather
#: than accumulating, and each word is one row that ages on its own evidence.
GROUP_TERM = "term"

#: How fast each kind of fact goes stale, as a base half-life in days. The class, not the
#: repetition count, is the primary axis of forgetting: where somebody lives changes on
#: the scale of years, what they are currently playing on the scale of weeks, and a decay
#: clock that ignores the difference discounts both wrongly. Defined here because it is
#: predicate metadata, exactly like cardinality above; the repository receives the lists
#: and knows nothing about what predicates mean.
STABLE_PREDICATES = ("lives_in", "from_place", "works_as", "works_at", "studies_at",
                     "majors_in", "birthday", "member_of", "speaks", "has_pet", "owns",
                     "visited", "fears", "allergic_to", GROUP_TOPIC)
FAST_PREDICATES = ("plays", "watches", "uses", "wants")
#: Everything else - preferences, skills, group terms - sits in the middle.
STABLE_DAYS, DEFAULT_DAYS, FAST_DAYS = 90.0, 30.0, 14.0

TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "record_alias",
            "description": "记录一个称呼：群里用某个名字指代某个账号。",
            "parameters": {
                "type": "object",
                "properties": {
                    "alias": {"type": "string", "description": "被使用的称呼原文"},
                    "account": {
                        "type": "integer",
                        "description": "被指代账号的编号，取自「本群账号」列表",
                    },
                    "kind": {
                        "type": "string",
                        "enum": list(ALIAS_KINDS),
                        "description": "nickname 常用称呼；short_name 由昵称简化而来；"
                                       "joke_name 玩笑性质的称呼；title 头衔或职务；"
                                       "relationship_name 按关系叫的（如「师兄」）",
                    },
                    "quote": {"type": "string", "description": "记录中逐字存在的一句，作为依据"},
                },
                "required": ["alias", "account", "kind", "quote"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "record_fact",
            "description": "记录一条关于某个账号的稳定事实。",
            "parameters": {
                "type": "object",
                "properties": {
                    "account": {
                        "type": "integer",
                        "description": "主语账号的编号，取自「本群账号」列表",
                    },
                    "predicate": {"type": "string", "enum": list(PREDICATES)},
                    "object": {
                        "type": "string",
                        "description": "宾语，只写值本身。举例：lives_in 写「杭州」，"
                                       "works_as 写「实习生」，works_at 写「某某券商」。"
                                       "不加括号注解、补充说明或时间限定，"
                                       "也不要把职位和单位写进同一个值",
                    },
                    "quote": {"type": "string", "description": "记录中逐字存在的一句，作为依据"},
                },
                "required": ["account", "predicate", "object", "quote"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "record_group_term",
            "description": "记录本群一个术语、缩写或行话的含义。",
            "parameters": {
                "type": "object",
                "properties": {
                    "term": {"type": "string", "description": "这个词本身，原文照抄"},
                    "meaning": {
                        "type": "string",
                        "description": "它在本群指什么，一句话说清",
                    },
                    "quote": {"type": "string", "description": "记录中逐字存在的一句，作为依据"},
                },
                "required": ["term", "meaning", "quote"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "record_group_topic",
            "description": "记录本群是干什么的。一个群只有一条。",
            "parameters": {
                "type": "object",
                "properties": {
                    "topic": {"type": "string", "description": "本群的性质与主题，一句话"},
                    "quote": {"type": "string", "description": "记录中逐字存在的一句，作为依据"},
                },
                "required": ["topic", "quote"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "record_episode",
            "description": "记录一件本群发生过的、以后可能被提起的事。",
            "parameters": {
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "这件事是什么，一到两句话，写清谁做了什么；"
                                       "只写记录里有的，不要补充没提到的细节",
                    },
                    "participants": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "参与者的账号编号，取自「本群账号」列表；"
                                       "只填能确认的人，指不准的宁可不填；"
                                       "一个都指不出就不要调用",
                    },
                    "quote": {"type": "string", "description": "记录中逐字存在的一句，作为依据"},
                },
                "required": ["summary", "participants", "quote"],
            },
        },
    },
]

#: How long one extraction may take. Deliberately far above the reply path's 30s: that
#: ceiling exists because somebody is waiting in a chat window, and here nobody is. One
#: call reads up to a window of messages (settings.EXTRACT_WINDOW, 120) and answers
#: with a dozen tool calls, which is simply slower
#: than answering one line - and a timeout costs the whole batch, then costs it again on
#: the retry.
TIMEOUT_SEC = 120.0



@dataclass(frozen=True, slots=True)
class SourceLine:
    """One line of the transcript, and the event it came from."""

    event_id: uuid.UUID
    text: str
    #: The bot's own line. Rendered into the transcript so the model reads both
    #: halves of a conversation, and excluded from evidence: source_of skips it,
    #: so any candidate quoting it fails validation mechanically. Comprehension
    #: without the self-loop - the bot's words never come back as evidence.
    own: bool = False


@dataclass(frozen=True, slots=True)
class ExtractionInput:
    """One batch to read. `account_codes` maps a code to a person, built by the caller
    from the roster it rendered."""

    group_id: int
    transcript: str
    roster: str
    account_codes: dict[int, uuid.UUID]
    #: The lines the transcript was built from, so each record can be attributed to the
    #: message it actually came from rather than to the batch as a whole.
    lines: tuple[SourceLine, ...] = ()
    #: The last message of this batch. Stored on every candidate so that validation, which
    #: runs later and separately, can reproduce this exact batch instead of re-fetching
    #: whatever the recent messages happen to be by then.
    source_event_id: uuid.UUID | None = None
    #: How many rows the batch held - the other half of exact reproduction, since
    #: gap-cut batches vary in length.
    batch_size: int = 0
    #: What is already on record for this group, rendered. Given to the model so it
    #: proposes what is new rather than re-deriving what is known - see the extract prompt.
    known: str = ""
    #: The bot's own trigger names, joined for display. Without them the extractor
    #: cannot recognise its own name in other people's mouths and files it as an
    #: alias of whichever member happens to sit nearby - measured in production,
    #: where the bot's name ended up a confirmed alias of another bot's account.
    self_names: str = ""

    def source_of(self, quote: str) -> uuid.UUID | None:
        """Which message a quote came from, or None if no single message contains it.

        Deliberately no fallback to the batch: a quote that matches no line must not
        come back with a plausible-looking event id, or a record would be filed as
        evidence from a message that does not contain it. Evidence attribution is not
        decorative here - how many *different* people were seen using a name is the one
        route by which a name the model merely observed becomes usable, and that count
        is taken over exactly these rows.
        """
        quote = (quote or "").strip()
        if not quote:
            return None
        for line in self.lines:
            if line.own:
                # The bot's own lines are context, never evidence: a quote found
                # only there validates nowhere, and the candidate dies for it.
                continue
            if quote in line.text:
                return line.event_id
        return None


class MemoryExtractor:
    def __init__(self, cfg: Settings, *, legend: str = "") -> None:
        """`legend` explains the transcript markers this system writes into the text.

        Passed in rather than imported: what a marker looks like belongs to the layer that
        renders messages, and a service reaching up into that layer would invert the
        dependency this package is arranged around. The worker composes it, because a
        worker is allowed to know about both sides.

        Without it the model reads a picture description as something a person typed, and
        records that the group is able to send pictures.
        """
        self._cfg = cfg
        # Composed once, at construction: the fixed half lives in the prefix cache for
        # the worker's lifetime, so a prompt override applies from the next restart
        # rather than mid-batch. tone_rules is the discernment core shared with the
        # reply path - what counts as said-in-earnest is one judgment, stated once -
        # followed by this path's consequence note (what not to record).
        base = (ptext("extract") + "\n\n【群聊语用】\n"
                + ptext("tone_rules") + "\n\n" + ptext("tone_extract_note"))
        self._prompt = (base + "\n\n" + legend.strip()) if legend.strip() else base

    @property
    def prompt(self) -> str:
        """The fixed half, as it will be sent. Fixed for the life of the process, which
        is what puts it inside the prefix cache."""
        return self._prompt

    async def extract(self, inp: ExtractionInput) -> list[Candidate]:
        """One call reads the whole batch. Returns candidates; writes and validates
        nothing."""
        res = await providers().text.chat(
            [
                {"role": "system", "content": self._prompt},
                # Ordered by how often each part changes, as everywhere else: the accounts
                # and what is already known move once a day, the transcript every batch.
                {"role": "user", "content": "\n\n".join(p for p in (
                    f"你的名字：{inp.self_names}" if inp.self_names else "",
                    f"本群账号：\n{inp.roster}",
                    f"【已经记过的】\n{inp.known}" if inp.known else "",
                    f"群聊记录：\n{inp.transcript}",
                ) if p)},
            ],
            cfg=self._cfg.llm.text,
            tools=TOOLS,
            timeout=TIMEOUT_SEC,
            # Extraction's own grade, not the reply path's: the schema and the
            # Validator carry most of the thinking, so anything above "low" measured
            # as waste here (~6k thought tokens a pass at the vendor default). If the
            # rejection rate climbs at "off", the grade is the lever.
            effort=self._cfg.memory.reasoning_effort,
            kind=Kind.EXTRACT,
            group_id=str(inp.group_id),
        )
        return [c for tc in res.tool_calls
                if (c := self._to_candidate(tc, inp)) is not None]

    @staticmethod
    def _to_candidate(call: dict, inp: ExtractionInput) -> Candidate | None:
        fn = (call.get("function") or {})
        name = fn.get("name")
        try:
            args = json.loads(fn.get("arguments") or "{}")
        except json.JSONDecodeError:
            log.warning("group %s: tool arguments were not JSON, dropped", inp.group_id)
            return None

        kind = {"record_alias": CandidateType.ALIAS,
                "record_fact": CandidateType.FACT,
                "record_group_term": CandidateType.GROUP_FACT,
                "record_group_topic": CandidateType.GROUP_FACT,
                "record_episode": CandidateType.EPISODE}.get(name)
        if kind is CandidateType.GROUP_FACT:
            # Which tool it was is what tells the two group predicates apart; the payload
            # alone cannot, and the consolidator needs to know.
            args = args | {"kind": GROUP_TERM if name == "record_group_term"
                           else GROUP_TOPIC}
        if kind is None:
            log.warning("group %s: unknown tool %r, dropped", inp.group_id, name)
            return None

        return Candidate(
            candidate_type=kind,
            payload=args,
            group_id=inp.group_id,
            source_event_id=inp.source_of(args.get("quote", "")),
            batch_event_id=inp.source_event_id,
            batch_size=inp.batch_size or None,
        )
