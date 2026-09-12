"""Telling namesakes apart: which accounts sharing a display name wear the tag, and
which serial.

One rule for every renderer - the live member table, the window relabel, the
roster, search results - so a member is spelled one way everywhere. Accounts
whose bare names collide are tagged, unless they all belong to one person: a
merged main and alt share a name legitimately, and a tag on each would tell the
model they are two people. Where two or more persons collide, every account of
each person wears that person's serial - the smallest of its accounts' permanent
serials - so either account resolves back to the same person, and a serial in an
old transcript keeps meaning who it meant.
"""

from __future__ import annotations

import re

from ..db import repo
from ..util import SYS_L, SYS_R, namesake_tag

#: The tag as it rides behind a name, for stripping a rendered name back to the bare one.
TAG = re.compile(rf"{re.escape(SYS_L)}同名\d{{1,9}}{re.escape(SYS_R)}$")


def bare(name: str) -> str:
    """The display name without its namesake tag."""
    return TAG.sub("", name)


async def tags(group_id: int, names: dict[str, str]) -> dict[str, str]:
    """Account -> tag for every account that needs one; absent means bare.

    `names` maps accounts to bare display names. Serials are assigned on first
    need and never change (repo.member_seqs); persons come from the identity
    layer, and an account it has never seen counts as a person of its own.
    """
    by_name: dict[str, list[str]] = {}
    for uid, name in names.items():
        if uid and name:
            by_name.setdefault(bare(name), []).append(uid)
    clashing = [uids for uids in by_name.values() if len(uids) > 1]
    if not clashing:
        return {}
    persons = await repo.person_of_accounts([u for uids in clashing for u in uids])
    out: dict[str, str] = {}
    for uids in clashing:
        parts: dict[object, list[str]] = {}
        for u in uids:
            parts.setdefault(persons.get(u) or u, []).append(u)
        if len(parts) < 2:
            continue  # one person under several accounts: nobody to tell apart
        seqs = await repo.member_seqs(group_id, uids)
        for members in parts.values():
            have = [seqs[u] for u in members if u in seqs]
            if have:
                tag = namesake_tag(min(have))
                for u in members:
                    out[u] = tag
    return out
