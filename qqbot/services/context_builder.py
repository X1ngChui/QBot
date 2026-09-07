"""Rendering long-term memory as prompt text. No queries here - input is what the
repositories produced, output is the exact wording the model (and the owner, through
/who) reads.

The roster and the facts are rendered elsewhere (core.retrieval builds the whole-roster
system block); what remains here is the piece every layer shares: how one fact reads as
a phrase.
"""

from __future__ import annotations

#: The predicate a hand-written note is filed under. It renders as itself, with no verb
#: in front: an owner who types a note has already written the sentence they want.
NOTE = "note"

#: How each predicate reads in Chinese. Prompt-facing text, so it is written the way the
#: group talks rather than the way the schema does.
VERB: dict[str, str] = {
    # who they are, where they are, what they do
    "lives_in": "住在",
    "from_place": "来自",
    "works_as": "做",
    "works_at": "就职于",
    "studies_at": "就读于",
    "majors_in": "主修",
    "birthday": "生日",
    # what they think of things
    "likes": "喜欢",
    "dislikes": "不喜欢",
    "avoids": "回避",
    "wants": "想要",
    "fears": "害怕",
    # what they do and have
    "plays": "在玩",
    "watches": "在追",
    "listens_to": "在听",
    "reads": "在读",
    "uses": "在用",
    "owns": "有",
    "collects": "收集",
    "has_pet": "养着",
    "visited": "去过",
    # what they can do, who they are with, and what their body says no to.
    # A value containing {} is a template the object drops into; everything else is a
    # verb the object follows - an allergy reads object-first in Chinese.
    "good_at": "擅长",
    "speaks": "会说",
    "member_of": "属于",
    "allergic_to": "对{}过敏",
}


def render_fact(predicate: str, object_value, object_key: str | None = None) -> str:
    """One fact as a phrase, or "" if there is nothing to show.

    Shared by the prompt and by the ops commands on purpose: /who must show an owner the
    same wording the model reads, and with one renderer no difference can open up between
    them.
    """
    obj = "" if object_value is None else str(object_value).strip()
    if not obj:
        return ""
    if predicate == NOTE:
        return obj
    # A group's terms are keyed by the word they define, so the word is half the sentence
    # and there is no verb to put in front of it.
    if predicate == "term":
        return f"{object_key}：{obj}"
    if predicate == "topic":
        return obj
    verb = VERB.get(predicate, predicate)
    return verb.format(obj) if "{}" in verb else f"{verb}{obj}"


