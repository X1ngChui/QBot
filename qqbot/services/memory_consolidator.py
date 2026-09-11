"""Validate candidates, then write them down.

Where the line is enforced: the model may only produce candidates, and
this is what writes long-term memory.

Validation is a pure function (`Validator.check`) that touches no database, so every rule
can be tested on its own.
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass
from datetime import datetime

from ..domain.identity import Alias, AliasEvidence, AliasType, EvidenceType, normalize
from ..domain.memory import (
    earned_confidence,
    Candidate, CandidateType, Episode, Fact, FactEvidence, MemoryType, Participant,
    RejectReason,
)
from ..repositories import EpisodeRepository, IdentityRepository, MemoryRepository
from ..settings import config
from .memory_extractor import (
    ALIAS_KINDS, GROUP_TERM, GROUP_TOPIC, line_body, multi_valued, opposites,
    predicate_names, quoted_in,
)

log = logging.getLogger("qqbot.consolidate")

#: The alias column's width. A longer "name" is not a name, and the store would
#: refuse the row after the validator had passed it.
ALIAS_MAX_CHARS = 256


def _text(p: dict, key: str) -> str:
    """A string field of a payload, stripped; "" for a missing one or one of another
    type. The model can send a number or a list where the schema said string, and
    the validator's job is to reject that shape, not to crash on it."""
    v = p.get(key)
    return v.strip() if isinstance(v, str) else ""


def _fact_kind(pred: str) -> MemoryType:
    """How a fact is classified once stored, from the predicate table."""
    entry = config().predicates.person.get(pred)
    return MemoryType(entry.kind) if entry else MemoryType.ATTRIBUTE


@dataclass(frozen=True, slots=True)
class Verdict:
    """The verdict. When ok is false, reason is always set."""

    ok: bool
    reason: RejectReason | None = None

    @classmethod
    def no(cls, reason: RejectReason) -> Verdict:
        return cls(False, reason)


PASS = Verdict(True)


class Validator:
    """Pure logic: whether one candidate may enter long-term memory.

    `codes` is the set of account codes this batch allows, and `lines` is the batch itself,
    one entry per message. Both are supplied by the caller; the validator looks nothing up
    itself, which is what makes it testable on its own.

    The batch arrives as separate lines rather than as one blob because the two checks
    below want different things from it. A quote has to be inside a single message - it is
    a thing somebody said, and text that only matches once the lines are joined spans a
    line break, which nobody typed. A name or a term only has to appear somewhere in the
    conversation, by anyone.
    """

    def __init__(self, codes: dict[int, uuid.UUID], lines: tuple[str, ...]) -> None:
        self._codes = codes
        self._bodies = tuple(line_body(line) for line in lines)
        self._transcript = "\n".join(lines)

    def check(self, c: Candidate) -> Verdict:
        p = c.payload
        if not isinstance(p, dict) or not p:
            return Verdict.no(RejectReason.EMPTY)

        quote = _text(p, "quote")
        if not quote or quoted_in(quote, self._bodies) is None:
            # The quote has to be present word for word, inside exactly one message,
            # and inside what the member typed rather than the speaker prefix. This
            # is the only check that stops something that sounds plausible but was
            # never said - and a quote found in two messages backs neither.
            return Verdict.no(RejectReason.MALFORMED)
        if c.source_event_id is None:
            # The extractor could not say which message the quote came from, so nothing
            # here can be filed as evidence of anything.
            return Verdict.no(RejectReason.MALFORMED)

        # A fact about the group names nobody, so the account checks below do not apply
        # to it. Branching on the type first, rather than treating a missing account as a
        # failure, is what lets one validator cover both.
        if c.candidate_type is CandidateType.GROUP_FACT:
            return self._check_group(p)
        if c.candidate_type is CandidateType.EPISODE:
            return self._check_episode(p)

        code = p.get("account")
        if not isinstance(code, int) or code not in self._codes:
            # A model that cannot produce a code will invent one. Anything not on the
            # list is discarded. This is also what keeps the bot out of its own memory:
            # nothing gives the bot an entity, so it never takes a code in the roster the
            # worker renders, and a candidate naming it could only carry an invented one.
            return Verdict.no(RejectReason.UNKNOWN_ENTITY)

        if c.candidate_type is CandidateType.ALIAS:
            return self._check_alias(p)
        if c.candidate_type is CandidateType.FACT:
            return self._check_fact(p)
        return Verdict.no(RejectReason.MALFORMED)

    def _check_alias(self, p: dict) -> Verdict:
        text = _text(p, "alias")
        if not text:
            return Verdict.no(RejectReason.EMPTY)
        if p.get("kind") not in ALIAS_KINDS or len(text) > ALIAS_MAX_CHARS:
            return Verdict.no(RejectReason.MALFORMED)
        if text not in self._transcript:
            return Verdict.no(RejectReason.MALFORMED)
        return PASS

    def _check_fact(self, p: dict) -> Verdict:
        if p.get("predicate") not in predicate_names():
            return Verdict.no(RejectReason.MALFORMED)
        if not _text(p, "object"):
            return Verdict.no(RejectReason.EMPTY)
        return PASS

    def _check_group(self, p: dict) -> Verdict:
        kind = p.get("kind")
        if kind == GROUP_TERM:
            term = _text(p, "term")
            if not term or not _text(p, "meaning"):
                return Verdict.no(RejectReason.EMPTY)
            # The word has to be one the group actually used. A definition of a word that
            # appears nowhere in the transcript is the model explaining its own vocabulary.
            if term not in self._transcript:
                return Verdict.no(RejectReason.MALFORMED)
            return PASS
        if kind == GROUP_TOPIC:
            if not _text(p, "topic"):
                return Verdict.no(RejectReason.EMPTY)
            return PASS
        return Verdict.no(RejectReason.MALFORMED)

    def _check_episode(self, p: dict) -> Verdict:
        if not _text(p, "summary"):
            return Verdict.no(RejectReason.EMPTY)
        codes = p.get("participants")
        if not isinstance(codes, list) or not codes:
            return Verdict.no(RejectReason.MALFORMED)
        # An episode is retrieved by who took part in it, so a participant the roster does
        # not contain makes it unreachable - and one invented code would file the whole
        # thing under a person who was not there.
        if any(not isinstance(x, int) or x not in self._codes for x in codes):
            return Verdict.no(RejectReason.UNKNOWN_ENTITY)
        return PASS

    def ambiguous_aliases(self, cands: list[Candidate]) -> set[str]:
        """Names in this batch that point at more than one account. A name belongs to one
        person, so writing both means both are wrong - and neither is written."""
        seen: dict[str, set[int]] = {}
        for c in cands:
            if c.candidate_type is not CandidateType.ALIAS or not isinstance(c.payload, dict):
                continue
            text = normalize(_text(c.payload, "alias"))
            code = c.payload.get("account")
            if text and isinstance(code, int):
                seen.setdefault(text, set()).add(code)
        return {t for t, codes in seen.items() if len(codes) > 1}


class MemoryConsolidator:
    """Writes the candidates that pass into L1/L3, and records the verdict against each
    candidate."""

    def __init__(self, ids: IdentityRepository, mem: MemoryRepository,
                 eps: EpisodeRepository) -> None:
        self._ids = ids
        self._mem = mem
        self._eps = eps

    async def consolidate(
        self, cands: list[Candidate], *, group_id: int, codes: dict[int, uuid.UUID],
        lines: tuple[str, ...], when: datetime,
        occurred: dict[uuid.UUID, datetime] | None = None,
    ) -> tuple[int, int]:
        """Validate and write one batch. Returns (written, rejected).

        `lines` must be the batch the model read, not whatever is recent now - see
        MemoryWorker.consolidate, which reproduces it from the anchor stored on each
        candidate.

        `occurred` maps each of the batch's event ids to when that message was sent.
        A record is dated by the conversation it came from, not by this write: the
        two are hours apart on the nightly drain and days apart after a retry, and a
        fact that started aging from the write would outlive its evidence. `when` is
        the write time and stamps last_confirmed_at only. A candidate whose source
        is not in the map falls back to `when`.
        """
        v = Validator(codes, lines)
        ambiguous = v.ambiguous_aliases(cands)
        occurred = occurred or {}
        written = rejected = 0

        for c in cands:
            verdict = v.check(c)
            if verdict.ok and c.candidate_type is CandidateType.ALIAS \
                    and normalize(_text(c.payload, "alias")) in ambiguous:
                verdict = Verdict.no(RejectReason.AMBIGUOUS_ALIAS)

            if not verdict.ok:
                await self._mem.settle(c.rejected(verdict.reason.value))
                rejected += 1
                log.info("group %s: candidate rejected (%s): %s",
                         group_id, verdict.reason.value, c.payload)
                continue

            at = occurred.get(c.source_event_id, when)
            try:
                if c.candidate_type is CandidateType.EPISODE:
                    await self._write_episode(c, group_id, codes, at)
                elif c.candidate_type is CandidateType.GROUP_FACT:
                    await self._write_group_fact(c, group_id, when, at)
                elif c.candidate_type is CandidateType.ALIAS:
                    await self._write_alias(c, group_id, codes[c.payload["account"]])
                else:
                    await self._write_fact(c, group_id, codes[c.payload["account"]],
                                           when, at)
            except Exception:
                # One candidate the store will not take (a shape the validator did
                # not anticipate) is settled as rejected and the batch goes on.
                # Left pending it would head every later page - pending() reads
                # oldest first - and fail every consolidation of this group for good.
                log.exception("group %s: candidate could not be written, rejected: %s",
                              group_id, c.payload)
                await self._mem.settle(c.rejected(RejectReason.MALFORMED.value))
                rejected += 1
                continue
            await self._mem.settle(c.accepted())
            written += 1

        return written, rejected

    async def _write_alias(self, c: Candidate, group_id: int,
                           entity_id: uuid.UUID) -> None:
        # The model's judgement is worth one piece of LLM_INFERENCE evidence (0.25),
        # which does not reach confirmed. Confirming takes an @ or somebody typing it in -
        # which is the whole point of there being a candidate layer.
        await self._ids.upsert_alias(
            Alias(
                alias_text=c.payload["alias"].strip(),
                target_entity_id=entity_id,
                group_id=group_id,
                alias_type=AliasType(c.payload["kind"]),
            ),
            [AliasEvidence(EvidenceType.LLM_INFERENCE, c.source_event_id)],
        )

    async def _write_episode(self, c: Candidate, group_id: int,
                             codes: dict[int, uuid.UUID], at: datetime) -> None:
        """One thing that happened, with the people it happened to.

        Retrieval finds an episode by participant before it considers what the text looks
        like, which is why the participants are the part that has to be right - a summary
        that resembles the question is worth nothing if it is filed under the wrong
        people. The raw event it came from rides along as the evidence.

        The episode takes the candidate's id. One candidate is one episode, and a
        job retried after writing the row but before settling the candidate must
        find the same row rather than file the event twice - the repository's insert
        tolerates the duplicate on that id.
        """
        p = c.payload
        await self._eps.add(Episode(
            id=c.id,
            group_id=group_id,
            summary=p["summary"].strip(),
            started_at=at,
            ended_at=at,
            # The extractor does not score importance yet, so this is a constant in
            # practice and episode ranking degrades to recency - acceptable. Written so
            # an explicit 0.0 is not coerced the day a real score arrives.
            importance=c.confidence if c.confidence is not None else 0.5,
            confidence=c.confidence if c.confidence is not None else 0.5,
            participants=tuple(
                Participant(entity_id=codes[x]) for x in dict.fromkeys(p["participants"])
            ),
            event_ids=(c.source_event_id,) if c.source_event_id else (),
        ))

    async def _write_group_fact(self, c: Candidate, group_id: int,
                                when: datetime, at: datetime) -> None:
        """A fact whose subject is the group itself.

        A term's object_key is the word being defined, so redefining a word supersedes
        the old meaning while a second word is a second row. The topic has no key: a
        group has one, and a new one supersedes it.
        """
        p = c.payload
        subject = await self._ids.group_entity(group_id)
        if p["kind"] == GROUP_TERM:
            predicate, key, value = GROUP_TERM, p["term"].strip(), p["meaning"].strip()
            # One word, one row, whatever the capitalisation or width: supersession
            # matches object_key by equality, so "YYDS" and "yyds" would otherwise
            # be two definitions aging separately. Fold to the spelling already on
            # file - the display keeps the form the group established first.
            for f in await self._mem.current_facts(group_id, [subject]):
                if (f.predicate == GROUP_TERM and f.object_key
                        and normalize(f.object_key) == normalize(key)):
                    key = f.object_key
                    break
        else:
            predicate, key, value = GROUP_TOPIC, None, p["topic"].strip()
        await self._mem.supersede(
            Fact(
                subject_entity_id=subject,
                predicate=predicate,
                object_key=key,
                object_value=value,
                group_id=group_id,
                memory_type=MemoryType.GROUP,
                # Earned, not guessed: what one supporting event is worth. Rises as
                # later batches confirm it - see MemoryRepository.supersede.
                confidence=earned_confidence(1),
            ),
            [FactEvidence(c.source_event_id)] if c.source_event_id else [],
            when=when, observed_at=at,
        )

    async def _write_fact(self, c: Candidate, group_id: int,
                          entity_id: uuid.UUID, when: datetime, at: datetime) -> None:
        pred = c.payload["predicate"]
        obj = c.payload["object"].strip()

        # Saying somebody has gone off a thing retracts their liking it. The two are
        # separate rows about the same object, so nothing else would ever reconcile them
        # and both would end up in the prompt. Compared in folded form, the way the
        # keys themselves are matched: "Yyds" and "yyds" are one object.
        if (opposite := opposites().get(pred)) is not None:
            for f in await self._mem.current_facts(group_id, [entity_id]):
                if (f.predicate == opposite and f.object_key
                        and normalize(f.object_key) == normalize(obj)):
                    await self._mem.retract(f.id)

        await self._mem.supersede(
            Fact(
                subject_entity_id=entity_id,
                predicate=pred,
                # The key is what makes a multi-valued predicate multi-valued: each
                # object is its own row under the one-current-fact index. Single-valued
                # predicates leave it empty, so a new value supersedes the old.
                object_key=obj if pred in multi_valued() else None,
                object_value=obj,
                group_id=group_id,
                memory_type=_fact_kind(pred),
                confidence=earned_confidence(1),
            ),
            [FactEvidence(c.source_event_id)] if c.source_event_id else [],
            when=when, observed_at=at,
        )
