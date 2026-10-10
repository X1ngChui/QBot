# English catalog (Fluent): the source of truth. Every message id the code uses must appear here,
# and may use only the variables the code supplies for it. See https://projectfluent.org/
# The language the bot writes its memory and picture descriptions in, as a model reads it.
writing-language = English
command-owner_only = This action needs bot-owner permission.
command-outcome_unknown = Could not confirm the result of that action. It may or may not have happened; check before repeating it.
command-unknown_account = That account has no record yet, so this cannot be done.
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
command-bad_number = Numbers and page numbers must be whole numbers from 1 to 10000. See /help.
command-linked_once = --linked may be given only once.
command-unknown_option = Unknown option. Send /help for the current syntax.
command-own_accounts_only = You can only act on your own account or on accounts you have confirmed as linked.
command-duration_invalid = Invalid duration; use 30m, 12h or 3d.
command-truncated = (truncated)
command-list_separator = ,{ " " }
command-name_entry = { $text } (confidence { $confidence })
command-display_fallback = account { $account }
command-display_member = member { $number }
command-scope_exact = exact account
command-scope_linked = linked identity
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
usage-tasks_add = Usage: /tasks add (--at TIME | --in DURATION) -- content
usage-tasks_edit = Usage: /tasks edit ID [--at TIME | --in DURATION] [-- content]
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
help-footer = Details: /help <command>
help-unknown = There is no command called "{ $name }".
help-access_member = Available to all members
help-access_owner = Bot owner only
help-detail =
    { $command }  { $what }
    { $access }

    { $detail }
what-who = Show what is on record about a member
detail-who =
    /who [--linked] [@account]
    Shows names, notes people wrote, and what the bot learned from chat, each numbered separately. Shows the exact account by default; --linked includes its linked accounts.
what-name = Manage the names a member is called by
detail-name =
    /name [--linked] [@account]
    /name add [--linked] [@account] name
    /name remove [--linked] [@account] name
    Names are backed by evidence; a name added by hand counts as confirmed. --linked applies to the person behind all linked accounts.
what-note = Manage notes people wrote about a member
detail-note =
    /note [--linked] [@account]
    /note add [@account] text
    /note edit [--linked] [@account] N text
    /note remove [--linked] [@account] N
    /note clear [--linked] [@account]
    Notes are written by hand and kept exactly as written; the bot can read them but never changes them, and they are separate from what it learns from chat (see /forget). Members manage notes about their own accounts; owners about anyone. Numbers come from /note or /who in the same scope.
what-link = Confirm your linked accounts
detail-link =
    /link @account
    /link confirm
    /link cancel
    Sends a link invitation to another account. The invited account confirms in this group; either side may cancel here. An account can be in only one pending invitation per group.
    Owner: /link @accountA @accountB links two accounts at once, without an invitation.
what-unlink = Unlink this account
detail-unlink =
    /unlink
    Detaches only the account that sends the command; the other accounts stay linked.
    Owner: /unlink @account detaches that account.
what-stats = Show usage
detail-stats =
    /stats
    /stats global (owner only)
what-top = Show who uses the bot most here
detail-top =
    /top [--linked] [count]
    Ranks members by how many replies they started this month. --linked combines linked accounts.
what-tasks = Manage this group's scheduled tasks
detail-tasks =
    /tasks [list [page]]
    /tasks show ID
    /tasks add (--at TIME | --in 30m|12h|3d) -- content
    /tasks edit ID [--at TIME | --in DURATION] [-- content]
    /tasks cancel ID
    Lists pending and running tasks of this group. A scheduled time is not a promise of on-time delivery.
what-members = List the group's members
detail-members = /members
what-block = Manage reply blocks
detail-block =
    /block
    /block add [--linked] @account [30m|12h|3d]
    /block remove [--linked] @account
    A blocked member's messages never start a reply, but stay visible to the bot as context. --linked covers the accounts linked to that person right now.
what-mute = Mute or unmute the bot here
detail-mute = /mute [status|on|off]
what-group = Show what the bot learned about this group
detail-group =
    /group
    The group's topic and the terms it uses, as learned from chat. Owners remove an entry with /forget group N.
what-runs = Show recent bot runs
detail-runs =
    /runs
    /runs N
    Lists this group's latest runs, or shows the steps of run N: tool calls, their results and what was sent.
what-logs = Show recent warnings and errors
detail-logs =
    /logs [count]
    The bot's latest warnings and errors since it started, newest last.
what-help = Show the command list
detail-help = /help [command]
who-no_record = This account has no record in this group.
who-title = Account record | { $display } | { $scope }
who-messages = Messages in this group: { $count }
who-linked = Linked accounts: { $count }
who-names = Confirmed names: { $names }
who-leads = Unconfirmed leads (not to be used to identify anyone): { $names }
name-none = { $display } has no names on record.
name-header = Names of { $display }:
name-confirmed_line = - { $text } (confirmed, confidence { $confidence })
name-candidate_line = - { $text } (unconfirmed, confidence { $confidence })
name-removed = Removed the name "{ $name }" from { $display }.
name-remove_none = { $display } has no active name "{ $name }" in this scope.
name-taken = "{ $name }" already refers to someone else in this group.
name-empty_name = A name must not be empty.
name-too_long = A name can be at most { $max } characters.
name-added = Recorded: { $display } is also called "{ $name }".
link-confirmed = Link confirmed. Both accounts are now linked.
link-cancelled = Cancelled the pending link invitation in this group.
link-nothing_pending = There is no pending invitation involving this account in this group.
link-invited = Invited { $target } to link. The invited account should send /link confirm in this group within { $seconds } seconds.
link-self = An account cannot be linked with itself.
link-already = These two accounts are already linked.
link-busy = An account involved already has a pending invitation in this group; deal with that one first.
link-no_invitation = This account has no pending invitation to confirm in this group.
link-expired = The invitation has expired; send a new one.
link-not_target = Only the invited account can confirm.
link-out_of_order = The confirmation does not come after the invitation.
link-stale = An account involved changed since the invitation; send a new one.
link-bot = The bot's own account cannot be linked.
unlink-not_linked = This account is not linked to any other account, so there is nothing to detach.
unlink-done = Unlinked this account; the others stay linked.
link-unknown_account = Account { $account } has no record and cannot be linked.
link-forced = The two accounts are now linked.
link-owner_only = Linking two other accounts directly needs bot-owner permission; use /link @account to invite.
unlink-unknown_account = Account { $account } has no record and cannot be detached.
unlink-forced = Detached the selected account; the others stay linked.
unlink-owner_only = Detaching another account needs bot-owner permission; /unlink detaches only your own.
mute-status_on = This group is muted.
mute-status_off = This group is not muted.
mute-set_on = Muted this group.
mute-set_off = Replies are enabled again in this group.
mute-already_on = This group is already muted.
mute-already_off = Replies are already enabled in this group.
block-cannot_block = The bot and bot owners cannot be blocked.
block-list_empty = This group has no reply blocks.
block-list_header = Reply blocks in this group:
block-list_row = - { $label }{ $until }
block-until_suffix = { "  " }(until { $time })
block-lapse_until = until { $time }
block-lapse_forever = until removed
block-scope_exact = exact account
block-scope_linked = all linked accounts
block-removed = Removed the reply block: { $label } | { $scope }.
block-remove_none = There is no reply block on { $label } in scope: { $scope }.
block-added = Blocked replies to { $label } | { $scope } | { $lapse }.
members-empty = This group has no member records yet.
members-header = { $total } members on record (messages, names):
members-row =
    - { $display } ({ $messages ->
        [one] one message
       *[other] { $messages } messages
    })
members-row_names =
    - { $display } ({ $messages ->
        [one] one message
       *[other] { $messages } messages
    }) | { $names }
stats-group_title = Usage today | group { $group }
stats-global_title = Usage today | all groups
stats-runs = Runs: { $runs } ({ $model_calls } model calls, { $tool_calls } tool calls)
stats-tokens = Tokens: { $input } in ({ $cached } cached), { $output } out
stats-replies_muted = Group replies: muted
stats-replies_enabled = Group replies: enabled
stats-blocks = Block rules: { $count }
top-header = Most active requesters this month | { $scope }
top-empty = No replies were started by identifiable accounts in this group this month.
top-row =
    { $rank }. { $name }{ $tag }  { $runs ->
        [one] one reply
       *[other] { $runs } replies
    }, { $tokens } tokens
top-linked_tag = { " " }({ $count } accounts)
tasks-no_mentions = /tasks manages this group's tasks only and takes no member targets.
tasks-bad_id = Invalid task ID or arguments. See /help tasks.
tasks-unknown_action = Unknown task action. See /help tasks.
tasks-option_value = Each time option needs exactly one value.
tasks-option_dupe = Only one --at or --in is accepted, and no other options.
tasks-at_in_conflict = Use either --at or --in, not both.
tasks-bad_time = --at needs an RFC 3339 time with a UTC offset, for example 2030-01-02T09:00:00+08:00.
tasks-empty_content = The task content must not be empty.
tasks-list_empty = This group has no pending or running tasks.
tasks-list_page_empty = There are no tasks on this page; look at an earlier page.
tasks-list_header = Active tasks of this group | page { $page } (pending, running)
tasks-list_next = Next page: /tasks list { $page }
tasks-list_footer = Full content and results: /tasks show ID
tasks-not_found_cancel = This group has no such task, or it is no longer pending and so cannot be cancelled.
tasks-not_found_edit = This group has no such task, or it is no longer pending and so cannot be changed.
tasks-not_found = This group has no such task.
tasks-shown =
    Task details:
    { $task }
tasks-cancelled =
    Cancelled the task:
    { $task }
tasks-created =
    Created the task:
    { $task }
tasks-edited =
    Changed the task:
    { $task }
tasks-detail =
    { $id }
    State: { $state } | Due: { $due }
    { $intent }
tasks-detail_outcome = Result: { $outcome }
tasks-state_pending = pending
tasks-state_running = running
tasks-state_done = finished
tasks-state_failed = failed
tasks-state_cancelled = cancelled
tasks-state_interrupted = interrupted
tasks-skipped_muted = skipped (the group was muted)
tasks-too_soon = The time is too soon; the earliest allowed is { $earliest }.
tasks-too_many = This group already has { $limit } pending tasks.
tasks-chain_deep = The follow-up chain is already { $limit } tasks deep.
tasks-nothing_to_change = Editing a task needs new content or a new time.
tasks-empty_intent = The task content must not be empty.
who-facts = Learned from chat ({ $count }):
who-no_facts = Learned from chat: nothing yet
who-fact_line = { $index }. { $predicate }: { $object } (confidence { $confidence })
who-notes = Notes written by people ({ $count }):
who-no_notes = Notes written by people: none
who-note_line = { $index }. { $text }
who-hint = Remove a note with /note remove N, a learned fact with /forget N (same scope).
note-none = There are no notes about { $display } in this scope.
note-header = Notes about { $display } | { $scope }:
note-line = { $index }. { $text } (by { $author }, { $time })
note-added = Added note { $index } about { $display }.
note-edited = Changed note { $index }.
note-removed = Removed note { $index }: { $text }
note-cleared =
    { $count ->
        [one] Removed one note about { $display }.
       *[other] Removed { $count } notes about { $display }.
    }
note-no_such = There is no note { $index } in that scope. Send /note to see the list.
note-empty = A note needs some text.
note-too_many = An account can have at most { $max } notes; remove one first.
group-empty = Nothing has been learned about this group yet.
group-header = Learned about this group:
group-topic_line = { $index }. What the group is about: { $text }
group-term_line = { $index }. "{ $term }" means: { $text }
group-hint = Owners remove an entry with /forget group N.
forget-group_done = Forgot group knowledge { $index }: { $text }
forget-group_no_such = There is no group knowledge { $index }. Send /group to see the list.
runs-empty = This group has no runs yet.
runs-header = Latest runs of this group ({ $count }):
runs-row = #{ $id } { $time } { $trigger } | { $end } | { $turns } turns, { $tools } tool calls, { $sends } sent | { $tokens } tokens
runs-footer = Steps of one run: /runs N
runs-no_such = This group has no run { $id }.
runs-detail = Run #{ $id } | { $time } | { $trigger } | { $end } | { $tokens } tokens
runs-step_chat =
    { $count ->
        [one] chat: one line
       *[other] chat: { $count } lines
    }
runs-step_summary = summary: { $text }
runs-step_text = model text: { $text }
runs-step_call = call { $name }: { $args }
runs-step_result = result ({ $outcome }): { $text }
runs-trigger_addressed = addressed
runs-trigger_wake = scheduled
runs-trigger_spontaneous = on its own
runs-open = still running
logs-empty = No warnings or errors since the bot started.
logs-header = Latest warnings and errors ({ $count }):
logs-line = { $time } { $level } { $text }
usage-forget = Usage: /forget [--linked] [@account] N, or /forget group N
forget-done = Forgot { $index }. { $predicate }: { $object }.
forget-no_such = There is no learned fact { $index } in that scope. Send /who to see the list.
what-forget = Remove something the bot learned from chat
detail-forget =
    /forget [--linked] [@account] N
    /forget group N (owner only)
    Removes what the bot learned on its own: the learned fact numbered N in /who (same scope), or the group knowledge numbered N in /group. Notes people wrote are not affected; use /note for those.
report-title = Daily report | { $date }
report-runs = Replies: { $runs } ({ $addressed } addressed, { $wake } scheduled)
report-ends = How runs ended: { $ends }
report-errors = Model errors: { $errors }
report-count_entry = { $label } { $count }
report-model = Model calls: { $calls } ({ $failed } failed)
report-tokens = Tokens: { $input } in ({ $hit }% from cache, { $cached } tokens), { $output } out
report-tools = Tool calls: { $calls } ({ $failed } not ok)
report-chat = Chat: { $messages } messages archived; replied in { $groups } groups
report-groups = Groups: { $new } new, { $muted } muted
report-memory = Memory: { $episodes } new episodes
report-jobs = Jobs: { $pending } pending, { $failed } failed; { $interrupted } scheduled tasks interrupted
report-backup_age = Last backup: { $hours } hours ago
report-backup_none = Last backup: none found
