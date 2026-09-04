"""The domain layer: no database, no model, pure logic.

That this layer can be tested like this is one of the points of the architecture:
every judgement here is checkable on its own, without running the whole chain.
"""
import pathlib
import sys
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from qqbot.domain.identity import (  # noqa: E402
    Alias, AliasEvidence, AliasStatus, AliasType, Entity, EvidenceType,
    IdentityAccount, normalize,
)
from qqbot.domain.memory import (  # noqa: E402
    Candidate, CandidateType, Episode, EpisodeType, Fact, Participant,
    RejectReason,
)

fails = []


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


# ---- people and accounts --------------------------------------------------
# Merging, superseding and group isolation are enforced by SQL in one transaction, and
# they are checked there - see test_repositories.py. They were also restated as domain
# methods, tested here, and never called: two versions of one rule where only one runs.
wang = Entity.new(name="老王")
alt = Entity.new(name="老王的小号")
check("一个新实体默认是人", wang.entity_type == "person" and wang.merged_into is None)

acc = IdentityAccount(entity_id=wang.id, platform="qq", platform_user_id="123456")
check("账号的唯一键与表上的 UNIQUE 一致", acc.key == ("qq", "123456"))

# ---- name normalization ---------------------------------------------------
# Padding a group card with full-width and zero-width characters is a common enough
# joke, and it is still the same person underneath.
check("全角与半角归一", normalize("Ｓｋｙ") == normalize("sky"))
check("空白不影响匹配", normalize("s k y") == "sky")
check("零宽字符被去掉", normalize("Sk​y") == "sky")
check("大小写不影响匹配", normalize("SKY") == normalize("sky"))

# ---- evidence decides status ----------------------------------------------
# When the model reports that everyone calls somebody by a name, that sentence
# is a clue, not a conclusion.
a = Alias(alias_text="老周", target_entity_id=wang.id, group_id=111,
          alias_type=AliasType.NICKNAME)
check("新称呼不能直接拿来指认", not a.is_usable and a.status is AliasStatus.CANDIDATE)

only_llm = a.scored([AliasEvidence(EvidenceType.LLM_INFERENCE)])
check("单凭模型推断升不上确认",
      only_llm.status is AliasStatus.CANDIDATE and not only_llm.is_usable,
      f"{only_llm.confidence:.2f}")

with_at = a.scored([AliasEvidence(EvidenceType.LLM_INFERENCE),
                    AliasEvidence(EvidenceType.EXPLICIT_AT)])
check("一次显式 @ 足以确认",
      with_at.status is AliasStatus.CONFIRMED and with_at.is_usable,
      f"{with_at.confidence:.2f}")

# Within one channel repetition buys nothing - ten sightings of the same kind are one
# fact about that channel, so they can never sum their way to confirmation.
many_weak = a.scored([AliasEvidence(EvidenceType.LLM_INFERENCE) for _ in range(10)])
check("弱证据再多也不叠加成确认", many_weak.status is AliasStatus.CANDIDATE,
      f"{many_weak.confidence:.2f}")
check("同渠道重复十次等于一次",
      abs(many_weak.confidence
          - a.scored([AliasEvidence(EvidenceType.LLM_INFERENCE)]).confidence) < 1e-9)

# Across channels, independent kinds of support genuinely reinforce: the old max threw
# this away, and a name with a card AND an @ AND usage scored no higher than the @ alone.
from qqbot.domain.identity import fused_confidence
two = fused_confidence([AliasEvidence(EvidenceType.GROUP_CARD),
                        AliasEvidence(EvidenceType.SELF_CLAIM)])
check("跨渠道的独立证据互相加强", two > 0.80 - 1e-9,
      f"{two:.3f} > max alone 0.80")
# The joke guard: a first-day card (stability-scored 0.5) plus a joking self-claim must
# still fall short of the line - both showed up in the same evening of rename games.
joke = fused_confidence([AliasEvidence(EvidenceType.GROUP_CARD, score=0.5),
                         AliasEvidence(EvidenceType.SELF_CLAIM)])
check("首日名片加自认仍到不了确认线", joke < 0.75, f"{joke:.3f}")

back = with_at.scored([AliasEvidence(EvidenceType.LLM_INFERENCE)])
check("确认过的不因一条弱证据退回", back.status is AliasStatus.CONFIRMED)

# ---- earned confidence for facts ------------------------------------------
from qqbot.domain.memory import earned_confidence
check("一次观察不是半数确信", 0.1 < earned_confidence(1) < 0.3,
      f"{earned_confidence(1):.2f}")
check("重复确认单调抬升", earned_confidence(1) < earned_confidence(3)
      < earned_confidence(8) < earned_confidence(20))
check("满口径但样本少，仍然保守",
      earned_confidence(1) < earned_confidence(45, 5),
      f"1/1 {earned_confidence(1):.2f} vs 45/50 {earned_confidence(45, 5):.2f}")
check("矛盾把下界往下拽", earned_confidence(3, 1) < earned_confidence(3),
      f"{earned_confidence(3, 1):.2f} < {earned_confidence(3):.2f}")
check("无证据即为零", earned_confidence(0) == 0.0)

check("群内称呼不是全局的", not a.is_global)
check("只有不带群号的才是全局", Alias(alias_text="小X", target_entity_id=wang.id).is_global)

# ---- facts ----------------------------------------------------------------
# The one rule this class enforces rather than describes: a fact with no object is not a
# fact, and saying so at construction is what stops it reaching a table.
try:
    Fact(subject_entity_id=wang.id, predicate="likes")
    check("没有宾语的事实被拦下", False, "没有报错")
except ValueError:
    check("没有宾语的事实被拦下", True)

# ---- candidates -----------------------------------------------------------
cand = Candidate(candidate_type=CandidateType.ALIAS, payload={"alias": "老周"},
                 group_id=1)
check("候选默认待处理", cand.status == "pending")
bad = cand.rejected(RejectReason.AMBIGUOUS_ALIAS)
check("否掉时记下理由", bad.status == "rejected" and bad.reject_reason == "ambiguous_alias")
check("否掉不改原对象", cand.status == "pending")

# ---- episodes -------------------------------------------------------------
ep = Episode(group_id=111, summary="讨论了买哪把键盘",
             episode_type=EpisodeType.DISCUSSION,
             participants=(Participant(entity_id=wang.id, role="推荐者"),),
             event_ids=(uuid.uuid4(),))
check("事件挂着原始消息作为凭据", len(ep.event_ids) == 1)

print()
print("FAILED:", fails if fails else "none")
sys.exit(1 if fails else 0)
