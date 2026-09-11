"""Rendering long-term memory as prompt text. No queries here - input is what the
repositories produced, output is the exact wording the model (and the owner, through
/who) reads.

The roster and the facts are rendered elsewhere (core.retrieval builds the whole-roster
system block); what remains here is the piece every layer shares: how one fact reads as
a phrase.
"""

from __future__ import annotations

from ..settings import config
from .memory_extractor import GROUP_TERM, GROUP_TOPIC

#: The predicate a hand-written note is filed under. It renders as itself, with no verb
#: in front: an owner who types a note has already written the sentence they want.
NOTE = "note"


def verb_of(predicate: str) -> str:
    """How this predicate reads in Chinese, or "" if it is not a configured one.

    Prompt-facing text, so it is written the way the group talks rather than the way
    the schema does, and it lives with the rest of the predicate's definition in
    predicates.yaml. Empty for anything the table does not name: a predicate that was
    removed leaves rows behind, and the bare English name is not something to put in
    front of the model.
    """
    entry = config().predicates.person.get(predicate)
    return entry.verb if entry else ""


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
    if predicate == GROUP_TERM:
        return f"{object_key}：{obj}"
    if predicate == GROUP_TOPIC:
        return obj
    verb = verb_of(predicate)
    if not verb:
        return ""
    return verb.format(obj) if "{}" in verb else f"{verb}{obj}"


