# Commands

Commands are typed in the group and start with `/`. A command name must be followed by
a space or the end of the message; `/who@someone` without the space is not a command.
Command lines are archived and shown to the model like any other message, and the
bot's answers to them are on the record too.

`/help` lists exactly the commands the reader may run, and `/help <name>` shows one in
detail. Anything the reader may not run is ignored without a reply.

## Permission levels

| Level | Who | What |
| --- | --- | --- |
| Owner | Accounts in `owners` (the group's persona may override the list) | The whole console |
| Global owner | Accounts in the top-level `owners` list only | Additionally the commands whose effect spans every group |
| Member (self) | Anyone who has accepted the agreement | `/who`, `/note`, `/alias`, `/forget` against their own record |
| Member | Anyone who has accepted the agreement | The read-only `/card`, `/stats`, `/top`, `/groupstats` |
| Anyone | Before accepting the agreement | `/agree`, `/terms` |

"Self" means the person, not the account: a merged alt operates its main's record. The
QQ group's own admin role is never consulted.

## Reference

### Agreement

| Command | Usage |
| --- | --- |
| `/terms` | Shows the user agreement. |
| `/agree` | Records acceptance for this group and the current agreement version. Until then the bot does not reply to the member and only points at `/terms`; their messages are still read and archived. |

### Member records

| Command | Usage |
| --- | --- |
| `/who` | Lists the group's members. |
| `/who @member` | Shows what is known about the member: names with confidence, facts with their index and confidence, the note. Members may only look at themselves. |
| `/note @member` | Shows the note. |
| `/note @member text` | Writes the note, replacing the previous one. Notes are never rewritten automatically and are treated as confirmed information by extraction. |
| `/note @member -` | Clears the note. |
| `/alias @member` | Lists the member's names with confidence. |
| `/alias @member name` | Registers a name at confidence 1.0. |
| `/alias @member name=0.6` | Sets a name's confidence. Manual values are final; automatic observation does not override them. |
| `/alias @member -name` | Retires a name. Old messages using it are still resolved. |
| `/forget @member N` | Deletes fact N from the member's record. Indexes come from `/who @member`. |

### Group record

| Command | Usage |
| --- | --- |
| `/card` | What is known about the group itself: its topic and the meaning of its jargon. Same source and same decay as member records. |
| `/forget N` | Deletes fact N from the group record. Owners only. |
| `/relearn` | Re-reads the most recent extraction window of this group's chat now instead of waiting for the nightly run. Costs model calls; results land later. |

### Identity

Global owners only. These rewrite the identity graph, which spans every group.

| Command | Usage |
| --- | --- |
| `/merge @alt @main` | Declares the two accounts one person. Records are combined, not rewritten. A block on either account then covers both. |
| `/split @account` | Gives the account its own person again. Names it produced itself move with it. |

### Blocking and muting

| Command | Usage |
| --- | --- |
| `/block` | Lists the group's block list. |
| `/block @member` | Stops answering the member until unblocked. Their messages are still read, archived and used for memory. Covers every account of the person. Owners cannot be blocked. |
| `/block @member 3d` | Same, expiring after the duration (`m`, `h`, `d`). A second `/block` replaces the duration. |
| `/unblock @member` | Lifts the block. |
| `/mute` | The bot stops speaking in this group, even when addressed. Survives restarts. |
| `/unmute` | Resumes. |

### Usage

| Command | Usage |
| --- | --- |
| `/stats` | Totals over every group: today's spend against the cap, calls by kind, search allowance for the month, extraction backlog, cache hit rates, recent errors. |
| `/groupstats` | This group today: persona, spend, replies, backlog, mute state, blocked members. |
| `/top` | The group's top spenders this month, by person. A reply's whole cost is booked to whoever addressed the bot; a picture's description to whoever posted it. |
| `/top N` | Top N, at most 20. |

### Maintenance

Global owners only.

| Command | Usage |
| --- | --- |
| `/reload` | Re-reads settings, personas, prompts, predicates and the agreement. An invalid configuration is rejected and the old one stays. Cron changes need a restart; code changes need a rebuild. |
| `/debug N` | Captures the next N model rounds (at most 50) from every group into `logs/debug/`, one JSON file per round with the full request and raw response. Turns itself off when done or on restart. `/debug` shows the state, `/debug off` stops it. |
| `/log` | The last 15 lines of the application log. |
| `/log N` | The last N lines, at most 60. |
