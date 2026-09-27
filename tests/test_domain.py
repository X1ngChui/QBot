"""Pure domain rules without database or provider dependencies."""

from datetime import UTC, datetime, timedelta
from uuid import uuid4

import pytest

from qqbot.domain.evidence import EvidenceItem, EvidenceMemo, EvidenceOutcome, EvidenceSource
from qqbot.domain.identity import (
    Alias,
    AliasEvidence,
    AliasStatus,
    AliasType,
    Entity,
    EvidenceType,
    IdentityAccount,
    fused_confidence,
    normalize,
)
from qqbot.domain.memory import (
    Candidate,
    CandidateType,
    Episode,
    EpisodeType,
    Fact,
    RejectReason,
    earned_confidence,
)


def test_entity_and_account_defaults():
    person = Entity(id=uuid4(), canonical_name="老王")
    assert person.entity_type == "person"
    assert person.merged_into is None
    account = IdentityAccount(entity_id=person.id, platform="qq", platform_user_id="123456")
    assert (account.platform, account.platform_user_id) == ("qq", "123456")


@pytest.mark.parametrize(
    ("name", "expected"),
    [("Ｓｋｙ", "sky"), ("s k y", "sky"), ("Sk​y", "sky"), ("SKY", "sky")],
)
def test_normalize_name(name, expected):
    assert normalize(name) == normalize(expected)


def test_alias_evidence_status_and_independent_channels():
    alias = Alias(
        alias_text="老周", target_entity_id=uuid4(), group_id=111, alias_type=AliasType.NICKNAME
    )
    weak = AliasEvidence(EvidenceType.LLM_INFERENCE)
    assert alias.status is AliasStatus.CANDIDATE
    assert not alias.is_usable
    only_llm = alias.scored([weak])
    assert only_llm.status is AliasStatus.CANDIDATE
    assert not only_llm.is_usable

    with_at = alias.scored([weak, AliasEvidence(EvidenceType.EXPLICIT_AT)])
    assert with_at.status is AliasStatus.CONFIRMED
    assert with_at.is_usable
    many_weak = alias.scored([weak] * 10)
    assert many_weak.status is AliasStatus.CANDIDATE
    assert many_weak.confidence == pytest.approx(only_llm.confidence)
    assert with_at.scored([weak]).status is AliasStatus.CONFIRMED

    two = fused_confidence(
        [AliasEvidence(EvidenceType.GROUP_CARD), AliasEvidence(EvidenceType.SELF_CLAIM)]
    )
    assert two > 0.80 - 1e-9
    joke = fused_confidence(
        [AliasEvidence(EvidenceType.GROUP_CARD, score=0.5), AliasEvidence(EvidenceType.SELF_CLAIM)]
    )
    assert joke < 0.75
    assert not alias.is_global
    assert Alias(alias_text="小X", target_entity_id=alias.target_entity_id).is_global


def test_fact_confidence_tracks_support_and_conflict():
    assert 0.1 < earned_confidence(1) < 0.3
    assert (
        earned_confidence(1) < earned_confidence(3) < earned_confidence(8) < earned_confidence(20)
    )
    assert earned_confidence(1) < earned_confidence(45, 5)
    assert earned_confidence(3, 1) < earned_confidence(3)
    assert earned_confidence(0) == 0.0


def test_fact_requires_object():
    with pytest.raises(ValueError):
        Fact(subject_entity_id=uuid4(), predicate="likes")


def test_candidate_rejection_preserves_original():
    candidate = Candidate(candidate_type=CandidateType.ALIAS, payload={"alias": "老周"}, group_id=1)
    assert candidate.status == "pending"
    rejected = candidate.rejected(RejectReason.AMBIGUOUS_ALIAS)
    assert (rejected.status, rejected.reject_reason) == ("rejected", "ambiguous_alias")
    assert candidate.status == "pending"


def test_episode_retains_provenance():
    extraction_id, event_id = uuid4(), uuid4()
    episode = Episode(
        group_id=111,
        summary="讨论了买哪把键盘",
        episode_type=EpisodeType.DISCUSSION,
        extraction_id=extraction_id,
        event_ids=(event_id,),
    )
    assert episode.event_ids == (event_id,)
    assert episode.extraction_id == extraction_id


def test_evidence_memo_roundtrip_and_projection():
    now = datetime.now(UTC)
    memo = EvidenceMemo(
        items=(
            EvidenceItem(
                source=EvidenceSource.HISTORY,
                request="虚构查询",
                outcome=EvidenceOutcome.VERIFIED,
                digest="虚构结果",
            ),
        ),
        created_at=now,
        expires_at=now + timedelta(days=30),
    )
    assert EvidenceMemo.from_dict(memo.to_dict()) == memo
    assert memo.render() == "⟦检索记录⟧\n查档“虚构查询”：虚构结果"
    with pytest.raises(ValueError):
        EvidenceMemo(items=(), created_at=now, expires_at=now)
