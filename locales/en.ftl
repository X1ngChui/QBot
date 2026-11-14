# English catalog (Fluent): the source of truth. Every message id the code uses must appear here,
# and may use only the variables the code supplies for it. See https://projectfluent.org/
# The language the bot writes its memory and picture descriptions in, as a model reads it.
writing-language = English
command-owner_only = Only a bot owner can do this.
command-delegated_only = Only a bot owner or a delegated admin of this group can do this.
command-delegated_target = Delegated admins can act only on ordinary members of this group, not on bot owners or other delegated admins.
command-authority_unknown = Your permissions could not be checked, so nothing was changed. Please try again.
command-outcome_unknown = The result could not be confirmed. Check whether the change took effect before trying again.
command-unknown_account = There is no record of that account yet.
command-mentions_exact =
    { $count ->
        [one] Mention exactly one account.
       *[other] Mention exactly { $count } accounts.
    }
command-mentions_max =
    { $max ->
        [one] Mention at most one account.
       *[other] Mention at most { $max } accounts.
    }
command-bad_number = Numbers must be whole numbers from 1 to 10000. See /help.
command-linked_once = Use --linked only once.
command-unknown_option = Unknown option. See /help for the syntax.
command-own_accounts_only = You can act only on your own account and the accounts linked to it.
command-duration_invalid = Invalid duration. Use a value such as 30m, 12h or 3d.
command-truncated = (truncated)
command-list_separator = ,{ " " }
command-display_fallback = account { $account }
command-display_member = member { $number }
command-scope_exact = single account
command-scope_linked = linked accounts
usage-who = Usage: /who [--linked] [@account]
usage-name_list = Usage: /name [--linked] [@account]
usage-name_edit = Usage: /name { $action } [--linked] [@account] name
usage-note =
    Usage: /note [--linked] [@account]
    /note add [@account] text
    /note edit [--linked] [@account] N text
    /note remove [--linked] [@account] N
    /note clear [--linked] [@account]
usage-group = Usage: /group
usage-runs = Usage: /runs [run number]
usage-logs = Usage: /logs [count]
usage-link_confirm = Usage: /link confirm
usage-link_cancel = Usage: /link cancel
usage-link_issue = Usage: /link @account, /link confirm or /link cancel
usage-unlink = Usage: /unlink
usage-stats = Usage: /stats [global]
usage-top = Usage: /top [--linked] [count]
usage-tasks_list = Usage: /tasks list [page]
usage-tasks_one = Usage: /tasks { $action } ID
usage-tasks_add = Usage: /tasks add (--at TIME | --in DURATION | --every RULE [--at TIME | --in DURATION]) -- content
usage-tasks_edit = Usage: /tasks edit ID [--at TIME | --in DURATION] [--every RULE] [-- content]
usage-members = Usage: /members
usage-block = Usage: /block add|remove [--linked] @account [duration]
usage-block_add = Usage: /block add [--linked] @account [30m|12h|3d]
usage-block_remove = Usage: /block remove [--linked] @account
usage-mute = Usage: /mute [status|on|off]
usage-help = Usage: /help [command]
help-header = Available commands:
help-category_profile = Profiles
help-category_identity = Identity
help-category_group = This group
help-category_admin = Administration
help-category_help = Help
help-entry = { $command }  { $what }
help-owner_tag = (owner only)
help-delegated_tag = (owner or delegated admin)
help-footer = For details: /help <command>
help-unknown = Unknown command "{ $name }". Send /help for the list.
help-access_member = Access: all members
help-access_owner = Access: bot owners only
help-access_delegated = Access: bot owners and this group's delegated admins
help-detail =
    { $command }  { $what }
    { $access }

    { $detail }
what-who = Show a member's record
what-name = Manage the names a member goes by
what-note = Manage notes about a member
detail-note =
    /note [--linked] [@account]
    /note add [@account] text
    /note edit [--linked] [@account] N text
    /note remove [--linked] [@account] N
    /note clear [--linked] [@account]

    Notes are written by people and kept exactly as written. The bot reads them but never changes them. They are separate from what the bot learns from chat (see /forget).
    N is the note's number in /note or /who for the same scope.

    Who can manage notes:
    - Members: notes about their own accounts.
    - Delegated admins: also notes about ordinary members of this group, except notes a bot owner wrote.
    - Bot owners: all notes.
what-admin = Manage this group's delegated admins
detail-admin =
    /admin
    /admin add @account
    /admin remove @account

    A delegated admin has extra permissions in this group only:
    - Use /mute, /block and /members.
    - Manage the names, notes and memories of ordinary members (/who, /name, /note, /forget).

    Limits:
    - No action on a bot owner or another delegated admin, or on a bot owner's notes and blocks.
    - Only a bot owner can add or remove delegated admins.
    - The permission belongs to that account alone, not to accounts linked to it.
usage-admin = Usage: /admin, /admin add @account or /admin remove @account
admin-list_empty = This group has no delegated admins.
admin-list_header = Delegated admins of this group:
admin-list_row = - { $label }
admin-added = { $label } is now a delegated admin of this group. See /help admin for the permissions.
admin-already = { $label } is already a delegated admin of this group.
admin-removed = { $label } is no longer a delegated admin of this group.
admin-remove_none = { $label } is not a delegated admin of this group.
admin-bot = The bot cannot be a delegated admin.
note-owner_authored = This note was written by a bot owner. Only a bot owner or the member it is about can change or remove it.
admin-bot_owner = Bot owners already have all permissions.
what-link = Link your accounts
detail-link =
    /link @account
    /link confirm
    /link cancel

    /link @account invites another account to link with yours. The invited account confirms with /link confirm in this group; either side can cancel with /link cancel.
    - An account can have one pending invitation per group.
    - A link applies only in the group where it was made.

    Bot owners: /link @accountA @accountB links two accounts directly, without an invitation.
what-unlink = Unlink your account
detail-unlink =
    /unlink

    Unlinks the account that sends the command, in this group only. The other accounts stay linked.

    Bot owners: /unlink @account unlinks that account.
what-stats = Show today's usage
detail-stats =
    /stats
    /stats global (owner only)

    /stats shows today's usage in this group; /stats global shows it for all groups.
what-top = Show this month's most active members
detail-top =
    /top [--linked] [count]

    Ranks members by the number of replies they started this month. --linked counts linked accounts as one person.
what-tasks = Manage this group's scheduled tasks
detail-tasks =
    /tasks [list [page]]
    /tasks show ID
    /tasks add (--at TIME | --in 30m|12h|3d) -- content
    /tasks add --every RULE [--at TIME | --in DURATION] -- content
    /tasks edit ID [--at TIME | --in DURATION] [--every RULE] [-- content]
    /tasks cancel ID

    Lists this group's pending and running tasks. A task may run later than its scheduled time.

    RULE makes a task repeat until it is cancelled, in the bot's time zone:
    - an interval: 2h, 3d
    - every day: daily@08:00
    - chosen days: mon,thu@20:30
    With a rule, --at or --in sets the earliest first run. Runs missed while the bot was offline are made up by a single run.
what-members = List this group's members
detail-members =
    /members

    Lists every member on record in this group, with message counts and names.
what-block = Manage reply blocks
detail-block =
    /block
    /block add [--linked] @account [30m|12h|3d]
    /block remove [--linked] @account

    /block lists the current blocks. Messages from a blocked member never start a reply, but the bot still reads them as context. Without a duration, a block lasts until it is removed. --linked covers every account currently linked to that person.
what-mute = Mute or unmute the bot in this group
detail-mute =
    /mute [status|on|off]

    While the group is muted the bot does not reply here. /mute alone shows the current state.
what-group = Show what the bot remembers about this group
what-runs = Show recent bot runs
detail-runs =
    /runs
    /runs N

    /runs lists this group's latest runs. /runs N shows the steps of run N: tool calls, their results and the messages sent.
what-logs = Show recent warnings and errors
detail-logs =
    /logs [count]

    Shows the bot's latest warnings and errors since it started, newest last. Entries about this group are shown in full; entries not tied to a group are shown without details.
what-help = List commands or explain one
detail-help =
    /help [command]

    /help lists the commands you can use; /help <command> explains one.
who-no_record = There is no record of this account in this group.
who-title = Account record | { $display } | { $scope }
who-messages = Messages in this group: { $count }
who-linked = Linked accounts: { $count }
name-none = No names are on record for { $display }.
name-removed = Removed the name "{ $name }" from { $display }.
name-remove_none = { $display } has no name "{ $name }" in this scope.
name-taken = "{ $name }" is already used for someone else in this group.
name-empty_name = The name cannot be empty.
name-too_long = A name can have at most { $max } characters.
name-added = Recorded: { $display } is also called "{ $name }".
link-confirmed = Link confirmed. The two accounts are now linked.
link-cancelled = The pending link invitation in this group was cancelled.
link-nothing_pending = This account has no pending link invitation in this group.
link-invited = Invited { $target } to link. The invited account must send /link confirm in this group within { $seconds } seconds.
link-self = An account cannot be linked to itself.
link-already = These two accounts are already linked in this group.
link-busy = One of these accounts already has a pending invitation in this group. Confirm or cancel it first.
link-no_invitation = This account has no pending invitation to confirm in this group.
link-expired = The invitation has expired. Send a new one.
link-not_target = Only the invited account can confirm the invitation.
link-stale = The links of an account involved have changed since the invitation was sent. Send a new one.
link-bot = The bot's own account cannot be linked.
unlink-not_linked = This account is not linked to any other account in this group.
unlink-done = This account was unlinked. The other accounts stay linked.
link-unknown_account = There is no record of account { $account }, so it cannot be linked.
link-forced = The two accounts are now linked.
link-owner_only = Only a bot owner can link two other accounts directly. Use /link @account to send an invitation.
unlink-unknown_account = There is no record of account { $account }, so it cannot be unlinked.
unlink-forced = The selected account was unlinked. The other accounts stay linked.
unlink-owner_only = Only a bot owner can unlink another account. /unlink unlinks your own.
mute-status_on = This group is muted.
mute-status_off = This group is not muted.
mute-set_on = This group is now muted.
mute-set_off = Replies are enabled again in this group.
mute-already_on = This group is already muted.
mute-already_off = Replies are already enabled in this group.
block-cannot_block = The bot and bot owners cannot be blocked.
block-delegated_target = Delegated admins can block only ordinary members, not other delegated admins.
block-owner_imposed = The block on { $label } was set by a bot owner. Only a bot owner can change or remove it.
block-list_empty = This group has no reply blocks.
block-list_header = Reply blocks in this group:
block-list_row = - { $label }{ $until }
block-until_suffix = { "  " }(until { $time })
block-lapse_until = until { $time }
block-lapse_forever = until removed
block-scope_exact = single account
block-scope_linked = all linked accounts
block-removed = Removed the reply block: { $label } | { $scope }.
block-remove_none = There is no reply block on { $label } ({ $scope }).
block-added = Blocked replies to { $label } | { $scope } | { $lapse }.
members-empty = This group has no member records yet.
members-header = { $total } members on record (messages | names):
members-row =
    - { $display } ({ $messages ->
        [one] 1 message
       *[other] { $messages } messages
    })
members-row_names =
    - { $display } ({ $messages ->
        [one] 1 message
       *[other] { $messages } messages
    }) | { $names }
stats-group_title = Usage today | group { $group }
stats-global_title = Usage today | all groups
stats-runs = Replies: { $runs } ({ $tool_calls } { $tool_calls ->
        [one] tool call
       *[other] tool calls
    })
stats-replies_muted = Group replies: muted
stats-replies_enabled = Group replies: enabled
stats-blocks = Reply blocks: { $count }
top-header = Most active members this month | { $scope }
top-empty = No replies were started by identifiable accounts in this group this month.
top-row =
    { $rank }. { $name }{ $tag }  { $runs ->
        [one] 1 reply
       *[other] { $runs } replies
    }, { $tokens } tokens
top-linked_tag = { " " }({ $count } accounts)
tasks-no_mentions = /tasks manages this group's tasks and does not take mentions.
tasks-bad_id = Invalid task ID or arguments. See /help tasks.
tasks-unknown_action = Unknown task action. See /help tasks.
tasks-option_value = Each time option needs exactly one value.
tasks-option_dupe = Use each of --at, --in and --every at most once. No other options are accepted.
tasks-bad_every = Invalid rule. --every takes an interval (2h, 3d), daily@HH:MM, or days and a time such as mon,thu@20:30.
tasks-interval_too_short = A repeating task's interval must be at least { $min }.
tasks-at_in_conflict = Use either --at or --in, not both.
tasks-bad_time = --at needs an RFC 3339 time with a UTC offset, for example 2030-01-02T09:00:00+08:00.
tasks-empty_content = The task content cannot be empty.
tasks-list_empty = This group has no pending or running tasks.
tasks-list_page_empty = This page has no tasks. Try an earlier page.
tasks-list_header = Active tasks of this group | page { $page } (pending and running)
tasks-list_next = Next page: /tasks list { $page }
tasks-list_footer = Full content and results: /tasks show ID
tasks-not_found_cancel = No such task in this group, or it is no longer pending and cannot be cancelled.
tasks-not_found_edit = No such task in this group, or it is no longer pending and cannot be changed.
tasks-not_found = No such task in this group.
tasks-shown =
    Task details:
    { $task }
tasks-cancelled =
    Task cancelled:
    { $task }
tasks-cancelled_repeating =
    Repeating task cancelled. No further runs are scheduled; a run already in progress will finish.
    { $task }
tasks-created =
    Task created:
    { $task }
tasks-edited =
    Task updated:
    { $task }
tasks-detail =
    { $id }
    State: { $state } | Due: { $due }
    { $intent }
tasks-detail_outcome = Result: { $outcome }
tasks-detail_repeat = Repeats { $rule }. Due is the next run.
tasks-detail_last_run = Last run (due { $due }): { $outcome }
tasks-repeat_interval = every { $interval }
tasks-repeat_daily = every day at { $time } ({ $zone })
tasks-repeat_weekly = every { $days } at { $time } ({ $zone })
tasks-weekday = { $day ->
        [mon] Monday
        [tue] Tuesday
        [wed] Wednesday
        [thu] Thursday
        [fri] Friday
        [sat] Saturday
       *[sun] Sunday
    }
tasks-state_pending = pending
tasks-state_running = running
tasks-state_done = finished
tasks-state_failed = failed
tasks-state_cancelled = cancelled
tasks-state_interrupted = interrupted
tasks-skipped_muted = skipped (the group was muted)
tasks-too_soon = That time is too soon. The earliest allowed is { $earliest }.
tasks-too_many = This group already has the maximum of { $limit } pending tasks.
tasks-chain_deep = The follow-up chain has reached its limit of { $limit } tasks.
tasks-nothing_to_change = Nothing to change. Give new content, a new time or a new rule.
tasks-empty_intent = The task content cannot be empty.
who-notes = Notes written by people ({ $count }):
who-no_notes = Notes written by people: none
who-note_line = { $index }. { $text }
note-none = There are no notes about { $display } in this scope.
note-header = Notes about { $display } | { $scope }:
note-line = { $index }. { $text } (by { $author }, { $time })
note-added = Added note { $index } about { $display }.
note-edited = Updated note { $index }.
note-removed = Removed note { $index }: { $text }
note-cleared =
    { $count ->
        [one] Removed 1 note about { $display }.
       *[other] Removed { $count } notes about { $display }.
    }
note-no_such = There is no note { $index } in this scope. Send /note to see the list.
note-empty = The note text cannot be empty.
note-too_many = An account can have at most { $max } notes. Remove one first.
runs-empty = This group has no runs yet.
runs-header = Latest runs of this group ({ $count }):
runs-row = #{ $id } { $time } { $trigger } | { $end } | { $turns } turns, { $tools } tool calls, { $sends } sent | { $tokens } tokens
runs-footer = Steps of a run: /runs N
runs-no_such = This group has no run #{ $id }.
runs-detail = Run #{ $id } | { $time } | { $trigger } | { $end } | { $tokens } tokens
runs-step_chat =
    { $count ->
        [one] chat: 1 line
       *[other] chat: { $count } lines
    }
runs-step_summary = summary: { $text }
runs-step_text = model text: { $text }
runs-step_call = call { $name }: { $args }
runs-step_result = result ({ $outcome }): { $text }
runs-trigger_addressed = addressed
runs-trigger_wake = scheduled
runs-trigger_spontaneous = spontaneous
runs-open = still running
logs-empty = No warnings or errors since the bot started.
logs-header = Latest warnings and errors ({ $count }):
logs-line = { $time } { $level } { $text }
report-title = Daily report | { $date }
report-runs = Replies: { $runs } (addressed { $addressed }, scheduled { $wake }, spontaneous { $spontaneous })
report-ends = Run outcomes: { $ends }
report-errors = Model errors: { $errors }
report-count_entry = { $label } { $count }
calls-none = Provider calls: none
calls-line =
    - { $purpose }, { $kind }: { $calls } { $calls ->
        [one] call
       *[other] calls
    } ({ $requests } { $requests ->
        [one] request
       *[other] requests
    }, { $failed } failed); { $amounts }
calls-total =
    Provider calls in total: { $calls } ({ $requests } { $requests ->
        [one] request
       *[other] requests
    }, { $failed } failed); { $input } { $input ->
        [one] token
       *[other] tokens
    } in, { $output } out; { $credits } { $credits ->
        [one] credit
       *[other] credits
    }
calls-tokens = { $input } { $input ->
        [one] token
       *[other] tokens
    } in ({ $cache }), { $output } out
calls-cached = { $cached } cached, { $hit }%
calls-cache_unreported = cache not reported
calls-input_tokens = { $input } { $input ->
        [one] token
       *[other] tokens
    }
calls-credits = { $credits } { $credits ->
        [one] credit
       *[other] credits
    }
calls-credits_unreported = credits not reported
purpose-reply = Replies
purpose-extraction = Memory
purpose-picture = Pictures
purpose-unattributed = Unattributed
call_kind-model = model
call_kind-embedding = embeddings
call_kind-search = web search
call_kind-extract = page reading
report-tools = Tool calls: { $calls } ({ $failed } unsuccessful)
report-chat =
    Chat: { $messages } { $messages ->
        [one] message
       *[other] messages
    } archived; replies sent in { $groups } { $groups ->
        [one] group
       *[other] groups
    }
report-groups = Groups: { $new } new, { $muted } muted
report-memory = Memory: { $episodes } new { $episodes ->
        [one] episode
       *[other] episodes
    }
report-jobs =
    Background jobs: { $pending } pending, { $failed } failed; { $interrupted } scheduled { $interrupted ->
        [one] task
       *[other] tasks
    } interrupted
report-backup_age = Last backup: { $hours } { $hours ->
        [one] hour
       *[other] hours
    } ago
report-backup_none = Last backup: none found

who-names = Names ({ $count }):
who-shown = Shown as: { $names }
name-header = Names of { $display }:
name-line = - { $text } ({ $how })
name-how_set = set with /name
name-how_own =
    used for themselves in { $count } { $count ->
        [one] conversation
       *[other] conversations
    }
name-how_others =
    used by others in { $count } { $count ->
        [one] conversation
       *[other] conversations
    }, last on { $date }
name-how_shared = also used for { $members }
who-memories = Remembered from chat ({ $count }):
who-no_memories = Remembered from chat: nothing
who-history = No longer current, still kept ({ $count }):
memory-replaced_line = { $id } { $text } (replaced on { $date })
memory-ended_line = { $id } { $text } (ended as of { $date })
memory-line =
    { $id } { $text } ({ $source }; { $count } { $count ->
        [one] conversation
       *[other] conversations
    }, last on { $date })
memory-source_own = said by them
memory-source_others = said by others
memory-source_both = said by them and by others
memory-source_members = said by members
who-hint = To remove: a note with /note remove N; one memory with /forget ID (for example /forget m12); every memory about this member with /forget all.
group-empty = Nothing is remembered about this group yet.
group-header = Remembered about this group:
group-hint = Bot owners can remove an entry with /forget group ID.
forget-done = Forgot { $id }: { $text }
forget-no_such = There is no memory { $id } in this scope. Send /who to see the list.
forget-group_done = Forgot { $id }: { $text }
forget-group_no_such = There is no group memory { $id }. Send /group to see the list.
forget-all_done =
    { $count ->
        [one] Forgot the 1 memory about { $display }.
       *[other] Forgot all { $count } memories about { $display }.
    }
usage-forget = Usage: /forget [--linked] [@account] ID, /forget all [--linked] [@account], or /forget group ID
what-forget = Delete what the bot remembers from chat
detail-forget =
    /forget [--linked] [@account] ID
    /forget all [--linked] [@account]
    /forget group ID (owner only)

    Deletes what the bot learned from chat on its own, together with its sources:
    - /forget ID: the memory with that ID in /who (same scope).
    - /forget all: every memory about the member.
    - /forget group ID: the group memory with that ID in /group.
    Notes and names are not affected; use /note and /name remove for those.

    Who can forget:
    - Members: memories about themselves.
    - Delegated admins: also memories about ordinary members of this group.
    - Bot owners: all memories.
detail-who =
    /who [--linked] [@account]

    Shows what is on record about a member:
    - the names people use for them;
    - notes people wrote;
    - what the bot remembers from chat, with who said it and when. Memories that a later statement replaced or ended are listed too, until they are forgotten.
    By default only the exact account is shown; --linked includes its linked accounts.

    Who can view:
    - Members: their own record.
    - Delegated admins: also ordinary members of this group.
    - Bot owners: anyone.
detail-name =
    /name [--linked] [@account]
    /name add [--linked] [@account] name
    /name remove [--linked] [@account] name

    Lists the names people use for a member and how each is used: set with /name, used by the member, or used by others in conversation. A name removed with /name is not learned again unless someone adds it back. --linked applies to the person behind all linked accounts.

    Who can manage names:
    - Members: their own.
    - Delegated admins: also ordinary members of this group.
    - Bot owners: anyone.
detail-group =
    /group

    Shows what the bot remembers about this group: the terms it uses and what the group is about, with how often and when each was last said. Bot owners can remove an entry with /forget group ID.
