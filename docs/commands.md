# Commands

Commands are sent in a group and start with `/`. The command name must end or be
followed by a space; `/who@someone` is not a command. `/help` shows the same catalogue
to everyone, with authorization labels, and `/help <name>` shows the exact syntax.

Authorization never changes an action's meaning. The same command and arguments select
the same target and scope for an owner and a member; authorization only allows or rejects
the action. The QQ group's administrator role is not used. "Owner" means an account in
the bot's global `owners` setting.

## Account scope

Person commands distinguish an exact platform account from the account holder's current
linked set:

- Exact account is the default.
- `--all` explicitly selects the linked account set and its holder-scoped records.
- A member may target only their own exact account or linked set. An owner may
  target another account with a structured `@` mention.
- Bare `/who` therefore means the caller's exact account for everyone. The owner-only
  whole-group directory is the separate `/members` command.

Unknown flags, duplicate flags, surplus text and surplus mentions are rejected. Retired
implicit forms such as `-` for clearing a value are not aliases for the current syntax.

## Permission levels

| Level | Who | Commands |
| --- | --- | --- |
| Member | Anyone in the group | `/help`, `/who`, `/note`, `/alias`, `/forget`, `/link`, `/unlink`, `/card`, `/stats`, `/top` |
| Owner | An account in `owners` | `/members`, `/block`, `/mute`, `/merge`, `/split`, `/debug`, `/log`, plus owner-only sub-actions noted below |

## My account data

| Command | Meaning |
| --- | --- |
| `/who [--all] [@account]` | Show the exact account by default, or the linked aggregate with `--all`. Fact indexes belong to this view. |
| `/note [--all] [@account]` | Show the note at the selected scope. |
| `/note set [--all] [@account] TEXT` | Replace the note at the selected scope. Notes are explicit confirmed context and are never rewritten automatically. |
| `/note clear [--all] [@account]` | Clear the selected note. |
| `/alias [--all] [@account]` | List aliases at the selected scope, marked confirmed or unconfirmed. |
| `/alias add [--all] [@account] NAME` | Add a confirmed alias. |
| `/alias remove [--all] [@account] NAME` | Retire an alias. Historical messages remain unchanged. |
| `/alias confidence [--all] [@account] SCORE NAME` | Set manual confidence from 0 through 1. At least 0.75 confirms the alias; below that it remains visible only as an unconfirmed hint, not an identity key. |
| `/forget [--all] [@account] N` | Retract fact N from the matching `/who` view. |

## Linking accounts

Member self-service and owner repair use different commands.

| Command | Meaning |
| --- | --- |
| `/link @other-account` | Create a short-lived link request. This command confirms the initiating account. |
| `/link confirm CODE` | The invited account confirms in the same group. Both current linked sets are then merged. |
| `/link cancel CODE` | Either endpoint cancels a pending request. |
| `/unlink` | Immediately detach only the account sending the command. It accepts no target; every other account in the set stays linked. |
| `/merge @account-A @account-B` | Owner-only repair: symmetrically merge the two current linked sets. |
| `/split @account` | Owner-only repair: detach only the mentioned exact account. |

Link codes prevent mix-ups and replay; ownership is proven by the invited platform account
sending the confirmation. A request expires or becomes invalid if either linked set
changes before confirmation.

## Group data and usage

| Command | Meaning |
| --- | --- |
| `/card` | Show structured facts about this group, such as its topic and local terminology. |
| `/card forget N` | Owner-only: retract group fact N. |
| `/stats` | Show this group's usage and state. |
| `/stats global` | Owner-only: show global usage and diagnostics. |
| `/top [N]` | Show up to N spending rows for exact accounts. |
| `/top --all [N]` | Aggregate spending by current linked account sets. |
| `/members` | Owner-only: show the whole-group account directory. This is never an alternate meaning of `/who`. |

## Blocking and muting

| Command | Meaning |
| --- | --- |
| `/block` | List the group's current exact-account and linked-set rules. |
| `/block add @account [30m\|12h\|3d]` | Add or replace an exact-account rule, optionally with an expiry. |
| `/block add --all @account [30m\|12h\|3d]` | Add or replace a rule for the account's linked set. The rule follows later links and splits dynamically. |
| `/block remove [--all] @account` | Remove the matching exact-account or linked-set rule. |
| `/mute [status]` | Show this group's mute state. |
| `/mute on` / `/mute off` | Enable or disable group mute. |

Blocked and muted messages are still admitted to the canonical archive. Owners cannot be
blocked.

## Maintenance

| Command | Meaning |
| --- | --- |
| `/debug [status]` | Show debug capture state. |
| `/debug start N` | Capture the next N model rounds, bounded by `diagnostics.debug_max_rounds`. |
| `/debug stop` | Stop capture. |
| `/log [N]` | Show the configured bounded tail of the application log. |

Debug captures contain provider-neutral prompt and completed-turn data. Credentials,
provider wire state and model reasoning are not written.

Command messages and bot command responses enter the same canonical archive as ordinary
messages.
