# Commands

Send commands in a group, starting with `/`. The name must end or be followed by
whitespace: `/who@someone` is not a command. `/help` shows the same catalogue and
permission labels to everyone; `/help NAME` shows the current syntax.

**Owner means the bot owner configured in `bot.owners`, not a QQ group administrator.**
Permissions allow or reject the same operation; they do not reinterpret its target.
Commands run directly, without a model call. Their requests and replies are archived.

## Scope and permissions

- Personal commands default to one **exact account**.
- `/who --all` aggregates the current linked identity. `/alias --all` manages linked
  aliases; `/note --all` manages **shared identity notes only**, not each account's notes.
- A member can target their own account or a verified linked account. An owner can target
  another account through a structured `@` mention. No target means the sending account.
- Group commands operate in the **current group**. `--all` never means all groups.
- Unknown or duplicate options, surplus arguments, and surplus mentions are rejected.


| Access | Commands |
| --- | --- |
| Member | `/help`, `/who`, `/note`, `/alias`, `/forget`, `/link`, `/unlink`, `/card`, `/stats`, `/top` |
| Bot owner | `/tasks`, `/members`, `/block`, `/mute`, `/merge`, `/split`, `/debug`, `/log`, plus the owner-only sub-actions below |

## Personal records

### Viewing and forgetting

| Command | Result |
| --- | --- |
| `/who [--all] [@account]` | Exact-account or linked-identity records, with separate manual notes, learned facts, confirmed names, and unconfirmed hints. Long views are explicitly previews. |
| `/forget [--all] [@account] N` | Retract learned fact N in the matching `/who` automatic-facts section. Never retracts a manual note or alias. |
| `/members` | Owner-only whole-group directory, not an alternate meaning of `/who`. |

Numbers are positions in the **current matching list**, not persistent record IDs.
Refresh the same view after records change. Manual notes are not part of `/forget`
numbering; a note-management number is not an automatic-fact number.

### Multiple manual notes

```text
/note [--all] [@account]
/note list [--all] [@account] [PAGE]
/note add [--all] [@account] -- TEXT
/note edit [--all] [@account] N -- TEXT
/note remove [--all] [@account] N
/note clear [--all] [@account]
```

Bare `/note` lists page 1. Each page contains up to five notes; numbers continue across
pages. `--` ends the option header and preserves the text that follows it, including
internal whitespace. Add creates one independent note, even if another has identical
text. Edit changes one note and preserves its logical key and history. Remove retracts
one note; clear retracts all current notes in the selected management scope and reports
the actual count. Neither operation physically deletes history.

The default scope is exact-account notes. With `--all`, **every** note action targets only
shared holder-scoped notes, including inherited shared notes from merged identities.
Clearing shared notes preserves all linked accounts' individual notes. `/who --all`
may display both scopes, with labels; use the appropriate `/note` view to manage them.

New additions allow 20 current notes per management scope and 500 characters per note.
Existing inherited records above the count limit are preserved and remain manageable;
further additions are rejected until below the limit. Automatic extraction and decay do
not overwrite or age manual notes. Use `/alias`, not notes or forgetting, for names.

### Names

| Command | Result |
| --- | --- |
| `/alias [--all] [@account]` | Confirmed aliases and unconfirmed hints, with labelled confidence. |
| `/alias add [--all] [@account] NAME` | Add a manually confirmed alias. |
| `/alias remove [--all] [@account] NAME` | Retire an alias; historical messages stay unchanged. |
| `/alias confidence [--all] [@account] SCORE NAME` | Set confidence from 0 to 1. At least 0.75 confirms it; lower values remain hints and cannot identify a person. |

## Group scheduled tasks

Tasks belong to the group, **not a creator or the current conversation participant**.
The reply model can autonomously create, inspect, edit, or cancel appropriate group tasks
in an ordinary reply or scheduled wakeup. Direct `/tasks` commands remain owner-only.
Member block rules still apply to ordinary addressed replies, while group wakeups use
group-level eligibility. Group mute, budgets, deadlines, and execution limits still apply.

```text
/tasks
/tasks list [PAGE]
/tasks show UUID
/tasks add (--at OFFSET_ISO8601 | --in DURATION) -- INTENT
/tasks edit UUID [--at OFFSET_ISO8601 | --in DURATION] [-- INTENT]
/tasks cancel UUID
```

- Bare `/tasks` lists active tasks on page 1: **pending and running**, up to five per page.
  Follow the next-page instruction rather than assuming a preview is the full list.
- Show queries a complete UUID in the current group, including retained terminal records.
- Add requires one time and nonempty intent. Edit requires at least one changed field;
  omitted fields stay unchanged. Both time options together are invalid.
- `--in` uses `30m`, `12h`, or `3d`, not bare seconds or natural-language times. `--at`
  requires an offset-aware ISO timestamp, such as `2030-01-02T09:00:00+08:00`.
- The minimum is 300 seconds; the horizon and group/chain/day limits come from `tasks`.
  Rejection never silently postpones a task. An intent-only edit does not revalidate the
  unchanged old time against a new minimum delay.
- Only pending tasks can be edited or cancelled. Editing preserves UUID, creation time,
  and chain; cancellation is a terminal transition, not physical deletion. Neither
  operation resurrects running or terminal records.
- All operations bind the current group, even for a global owner. Do not use list positions
  or guessed UUID prefixes. A scheduled time is not a punctual-delivery guarantee.
- Repetition creates only the next occurrence and remains bounded. Multi-task merge or
  split workflows are not atomic; confirm each result and query again afterward.

If an operation's result is unknown, query the current state before deciding what to do.
Do not treat a timeout as successful deletion or blindly repeat a create request.

## Linking accounts

| Command | Result |
| --- | --- |
| `/link @other-account` | Issue a short-lived request; the sending account confirms its side. |
| `/link confirm` | Confirm the invitation addressed to the sending account in this group, merging the two current identities. |
| `/link cancel` | Cancel this group's pending invitation involving the sending account. |
| `/unlink` | Detach only the sending account; the remaining accounts stay linked. |
| `/merge @account-A @account-B` | Owner repair: merge the two current identities. |
| `/split @account` | Owner repair: detach only that exact account. |

Each account can participate in only one pending invitation in each group, as either endpoint.
Invitations in different groups are independent.
Only the invited exact account can confirm, and either endpoint can cancel in the same
group. Invitations expire after ten minutes; changes to either linked identity invalidate
confirmation. An admitted confirmation is bound to its authenticated account and cannot
select an invitation issued after that message arrived. Replaying an applied confirmation
event is idempotent.

Exact-account records follow a detached account; shared records remain with the original
linked identity because their exact-account provenance is unknown.

## Group records and usage

| Command | Result |
| --- | --- |
| `/card` | Fixed group context and separately numbered learned group facts. |
| `/card forget N` | Owner-only retraction of a learned group fact. |
| `/stats` | Today's group costs, calls, extraction backlog, and mute state. |
| `/stats global` | Owner-only totals across all groups and budget/ledger health. |
| `/top [N]` | This month's attributable account costs. |
| `/top --all [N]` | The same costs aggregated by current linked identity. |

Group-only work can have no single causing account; its costs still appear in the group
and global ledger, but are not invented as somebody's personal spending. Monetary limits
are stop-loss on accounted spend, not a strict concurrency-safe prepaid cap. An uncertain
ledger is a protected state, distinct from an ordinary exhausted daily budget.

## Blocking, muting, and diagnostics

| Command | Result |
| --- | --- |
| `/block` | Current-group exact-account and linked-identity block rules. |
| `/block add @account [30m\|12h\|3d]` | Set an exact-account rule, optionally expiring. |
| `/block add --all @account [30m\|12h\|3d]` | Set a linked-identity rule; later links/splits dynamically affect membership. |
| `/block remove [--all] @account` | Remove the matching rule, without claiming unrelated rules disappeared. |
| `/mute [status\|on\|off]` | Inspect or change current-group mute. |
| `/debug [status\|start N\|stop]` | Owner-only bounded model-round capture. |
| `/log [N]` | Owner-only bounded application-log tail. |

Blocked messages cannot independently trigger ordinary response tasks. They remain in archived
and generated conversation context; another triggered session may freely refer or respond to
them. Blocking is not a structural-send filter or a promise of semantic silence. Muted messages
are also archived. Owners cannot be blocked. Diagnostic
limits are code-owned; debug captures contain provider-neutral prompts and completed turns,
not credentials, provider wire state, or model reasoning.
