//! The closed set of member-facing messages. Adding one means adding its variant here and its
//! text to every shipped catalog in `locales/`; a test keeps the catalogs complete and their
//! variables consistent. Catalogs are Fluent (`.ftl`), so wording that depends on a number (plural
//! forms) is written in the catalog, not in code.

use fluent_bundle::FluentValue;
use fluent_bundle::types::FluentNumber;

/// What kind of value a message argument carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    Text,
    Number,
}

/// A type usable as a message argument.
pub trait Arg {
    const KIND: ArgKind;
    fn value(&self) -> FluentValue<'static>;
}

impl Arg for String {
    const KIND: ArgKind = ArgKind::Text;
    fn value(&self) -> FluentValue<'static> {
        FluentValue::String(self.clone().into())
    }
}

macro_rules! number_arg {
    ($($t:ty),*) => {$(
        impl Arg for $t {
            const KIND: ArgKind = ArgKind::Number;
            fn value(&self) -> FluentValue<'static> {
                FluentValue::Number(FluentNumber::from(*self))
            }
        }
    )*};
}
number_arg!(u32, u64);

macro_rules! messages {
    ($( $key:literal => $name:ident $( { $( $field:ident : $ty:ty ),* $(,)? } )? ),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq)]
        pub enum Msg {
            $( $name $( { $( $field: $ty ),* } )? ),*
        }

        impl Msg {
            /// The Fluent message id.
            pub fn id(&self) -> &'static str {
                match self {
                    $( Msg::$name $( { $( $field: _ ),* } )? => $key ),*
                }
            }

            /// The named arguments.
            pub fn args(&self) -> Vec<(&'static str, FluentValue<'static>)> {
                match self {
                    $( Msg::$name $( { $( $field ),* } )? => vec![ $( $( (stringify!($field), Arg::value($field)) ),* )? ] ),*
                }
            }
        }

        /// Every message id with the arguments the code supplies for it.
        pub const SCHEMA: &[(&str, &[(&str, ArgKind)])] = &[
            $( ($key, &[ $( $( (stringify!($field), <$ty as Arg>::KIND) ),* )? ]) ),*
        ];
    };
}

messages! {
    // The language the bot writes its memory and picture descriptions in, named for a model.
    "writing-language" => WritingLanguage {},
    "command-owner_only" => OwnerOnly {},
    "command-outcome_unknown" => OutcomeUnknown {},
    "command-unknown_account" => UnknownAccount {},
    "command-mentions_exact" => MentionsExact { count: u32 },
    "command-mentions_max" => MentionsMax { max: u32 },
    "command-bad_number" => BadNumber {},
    "command-linked_once" => LinkedOnce {},
    "command-unknown_option" => UnknownOption {},
    "command-own_accounts_only" => OwnAccountsOnly {},
    "command-duration_invalid" => DurationInvalid {},
    "command-truncated" => Truncated {},
    "command-list_separator" => ListSeparator {},
    "command-name_entry" => NameEntry { text: String, confidence: String },
    "command-display_fallback" => DisplayFallback { account: String },
    "command-display_member" => DisplayMember { number: u32 },
    "command-scope_exact" => ScopeExact {},
    "command-scope_linked" => ScopeLinked {},
    "usage-who" => UsageWho {},
    "usage-name_list" => UsageNameList {},
    "usage-name_edit" => UsageNameEdit { action: String },
    "usage-note" => UsageNote {},
    "usage-group" => UsageGroup {},
    "usage-runs" => UsageRuns {},
    "usage-logs" => UsageLogs {},
    "usage-link_confirm" => UsageLinkConfirm {},
    "usage-link_cancel" => UsageLinkCancel {},
    "usage-link_issue" => UsageLinkIssue {},
    "usage-unlink" => UsageUnlink {},
    "usage-stats" => UsageStats {},
    "usage-top" => UsageTop {},
    "usage-tasks_list" => UsageTasksList {},
    "usage-tasks_one" => UsageTasksOne { action: String },
    "usage-tasks_add" => UsageTasksAdd {},
    "usage-tasks_edit" => UsageTasksEdit {},
    "usage-members" => UsageMembers {},
    "usage-block" => UsageBlock {},
    "usage-block_add" => UsageBlockAdd {},
    "usage-block_remove" => UsageBlockRemove {},
    "usage-mute" => UsageMute {},
    "usage-help" => UsageHelp {},
    "help-header" => HelpHeader {},
    "help-category_profile" => HelpCategoryProfile {},
    "help-category_identity" => HelpCategoryIdentity {},
    "help-category_group" => HelpCategoryGroup {},
    "help-category_admin" => HelpCategoryAdmin {},
    "help-category_help" => HelpCategoryHelp {},
    "help-entry" => HelpEntry { command: String, what: String },
    "help-owner_tag" => HelpOwnerTag {},
    "help-footer" => HelpFooter {},
    "help-unknown" => HelpUnknown { name: String },
    "help-access_member" => HelpAccessMember {},
    "help-access_owner" => HelpAccessOwner {},
    "help-detail" => HelpDetail { command: String, what: String, access: String, detail: String },
    "what-who" => WhatWho {},
    "detail-who" => DetailWho {},
    "what-link" => WhatLink {},
    "detail-link" => DetailLink {},
    "what-unlink" => WhatUnlink {},
    "detail-unlink" => DetailUnlink {},
    "what-stats" => WhatStats {},
    "detail-stats" => DetailStats {},
    "what-top" => WhatTop {},
    "detail-top" => DetailTop {},
    "what-tasks" => WhatTasks {},
    "detail-tasks" => DetailTasks {},
    "what-members" => WhatMembers {},
    "detail-members" => DetailMembers {},
    "what-block" => WhatBlock {},
    "detail-block" => DetailBlock {},
    "what-mute" => WhatMute {},
    "detail-mute" => DetailMute {},
    "what-name" => WhatName {},
    "detail-name" => DetailName {},
    "what-note" => WhatNote {},
    "detail-note" => DetailNote {},
    "what-group" => WhatGroup {},
    "detail-group" => DetailGroup {},
    "what-runs" => WhatRuns {},
    "detail-runs" => DetailRuns {},
    "what-logs" => WhatLogs {},
    "detail-logs" => DetailLogs {},
    "what-help" => WhatHelp {},
    "detail-help" => DetailHelp {},
    "who-no_record" => WhoNoRecord {},
    "who-title" => WhoTitle { display: String, scope: String },
    "who-messages" => WhoMessages { count: String },
    "who-linked" => WhoLinked { count: String },
    "who-names" => WhoNames { names: String },
    "who-leads" => WhoLeads { names: String },
    "name-none" => NameNone { display: String },
    "name-header" => NameHeader { display: String },
    "name-confirmed_line" => NameConfirmedLine { text: String, confidence: String },
    "name-candidate_line" => NameCandidateLine { text: String, confidence: String },
    "name-removed" => NameRemoved { display: String, name: String },
    "name-remove_none" => NameRemoveNone { display: String, name: String },
    "name-taken" => NameTaken { name: String },
    "name-empty_name" => NameEmptyName {},
    "name-too_long" => NameTooLong { max: u32 },
    "name-added" => NameAdded { display: String, name: String },
    "link-confirmed" => LinkConfirmed {},
    "link-cancelled" => LinkCancelled {},
    "link-nothing_pending" => LinkNothingPending {},
    "link-invited" => LinkInvited { target: String, seconds: u64 },
    "link-self" => LinkSelf {},
    "link-already" => LinkAlready {},
    "link-busy" => LinkBusy {},
    "link-no_invitation" => LinkNoInvitation {},
    "link-expired" => LinkExpired {},
    "link-not_target" => LinkNotTarget {},
    "link-out_of_order" => LinkOutOfOrder {},
    "link-stale" => LinkStale {},
    "link-bot" => LinkBot {},
    "unlink-not_linked" => NotLinked {},
    "unlink-done" => UnlinkDone {},
    "link-unknown_account" => LinkUnknown { account: String },
    "link-forced" => LinkForced {},
    "link-owner_only" => LinkOwnerOnly {},
    "unlink-unknown_account" => UnlinkUnknown { account: String },
    "unlink-forced" => UnlinkForced {},
    "unlink-owner_only" => UnlinkOwnerOnly {},
    "mute-status_on" => MuteStatusOn {},
    "mute-status_off" => MuteStatusOff {},
    "mute-set_on" => MuteSetOn {},
    "mute-set_off" => MuteSetOff {},
    "mute-already_on" => MuteAlreadyOn {},
    "mute-already_off" => MuteAlreadyOff {},
    "block-cannot_block" => BlockCannot {},
    "block-list_empty" => BlockListEmpty {},
    "block-list_header" => BlockListHeader {},
    "block-list_row" => BlockListRow { label: String, until: String },
    "block-until_suffix" => BlockUntilSuffix { time: String },
    "block-lapse_until" => BlockLapseUntil { time: String },
    "block-lapse_forever" => BlockLapseForever {},
    "block-scope_exact" => BlockScopeExact {},
    "block-scope_linked" => BlockScopeLinked {},
    "block-removed" => BlockRemoved { label: String, scope: String },
    "block-remove_none" => BlockRemoveNone { label: String, scope: String },
    "block-added" => BlockAdded { label: String, scope: String, lapse: String },
    "members-empty" => MembersEmpty {},
    "members-header" => MembersHeader { total: String },
    "members-row" => MembersRow { display: String, messages: u64 },
    "members-row_names" => MembersRowNames { display: String, messages: u64, names: String },
    "stats-group_title" => StatsGroupTitle { group: String },
    "stats-global_title" => StatsGlobalTitle {},
    "stats-runs" => StatsRuns { runs: u64, model_calls: u64, tool_calls: u64 },
    "stats-tokens" => StatsTokens { input: u64, cached: u64, output: u64 },
    "stats-replies_muted" => StatsMuted {},
    "stats-replies_enabled" => StatsEnabled {},
    "stats-blocks" => StatsBlocks { count: u64 },
    "top-header" => TopHeader { scope: String },
    "top-empty" => TopEmpty {},
    "top-row" => TopRow { rank: u64, name: String, tag: String, runs: u64, tokens: u64 },
    "top-linked_tag" => TopLinkedTag { count: u64 },
    "tasks-no_mentions" => TasksNoMentions {},
    "tasks-bad_id" => TasksBadId {},
    "tasks-unknown_action" => TasksUnknownAction {},
    "tasks-option_value" => TasksOptionValue {},
    "tasks-option_dupe" => TasksOptionDupe {},
    "tasks-at_in_conflict" => TasksAtInConflict {},
    "tasks-bad_time" => TasksBadTime {},
    "tasks-empty_content" => TasksEmptyContent {},
    "tasks-list_empty" => TasksListEmpty {},
    "tasks-list_page_empty" => TasksListPageEmpty {},
    "tasks-list_header" => TasksListHeader { page: String },
    "tasks-list_next" => TasksListNext { page: String },
    "tasks-list_footer" => TasksListFooter {},
    "tasks-not_found_cancel" => TasksNotFoundCancel {},
    "tasks-not_found_edit" => TasksNotFoundEdit {},
    "tasks-not_found" => TasksNotFound {},
    "tasks-shown" => TasksShown { task: String },
    "tasks-cancelled" => TasksCancelled { task: String },
    "tasks-created" => TasksCreated { task: String },
    "tasks-edited" => TasksEdited { task: String },
    "tasks-detail" => TasksDetail { id: String, state: String, due: String, intent: String },
    "tasks-detail_outcome" => TasksDetailOutcome { outcome: String },
    "tasks-state_pending" => TasksStatePending {},
    "tasks-state_running" => TasksStateRunning {},
    "tasks-state_done" => TasksStateDone {},
    "tasks-state_failed" => TasksStateFailed {},
    "tasks-state_cancelled" => TasksStateCancelledState {},
    "tasks-state_interrupted" => TasksStateInterrupted {},
    "tasks-skipped_muted" => TasksSkippedMuted {},
    "tasks-too_soon" => TasksTooSoon { earliest: String },
    "tasks-too_many" => TasksTooMany { limit: u64 },
    "tasks-chain_deep" => TasksChainDeep { limit: u32 },
    "tasks-nothing_to_change" => TasksNothingToChange {},
    "tasks-empty_intent" => TasksEmptyIntent {},
    "who-facts" => WhoFacts { count: u64 },
    "who-no_facts" => WhoNoFacts {},
    "who-fact_line" => WhoFactLine { index: u64, predicate: String, object: String, confidence: String },
    "who-notes" => WhoNotes { count: u64 },
    "who-no_notes" => WhoNoNotes {},
    "who-note_line" => WhoNoteLine { index: u64, text: String },
    "who-hint" => WhoHint {},
    "note-none" => NoteNone { display: String },
    "note-header" => NoteHeader { display: String, scope: String },
    "note-line" => NoteLine { index: u64, text: String, author: String, time: String },
    "note-added" => NoteAdded { display: String, index: u64 },
    "note-edited" => NoteEdited { index: u64 },
    "note-removed" => NoteRemoved { index: u64, text: String },
    "note-cleared" => NoteCleared { display: String, count: u64 },
    "note-no_such" => NoteNoSuch { index: u64 },
    "note-empty" => NoteEmpty {},
    "note-too_many" => NoteTooMany { max: u64 },
    "group-empty" => GroupEmpty {},
    "group-header" => GroupHeader {},
    "group-topic_line" => GroupTopicLine { index: u64, text: String },
    "group-term_line" => GroupTermLine { index: u64, term: String, text: String },
    "group-hint" => GroupHint {},
    "forget-group_done" => ForgetGroupDone { index: u64, text: String },
    "forget-group_no_such" => ForgetGroupNoSuch { index: u64 },
    "runs-empty" => RunsEmpty {},
    "runs-header" => RunsHeader { count: u64 },
    "runs-row" => RunsRow { id: u64, time: String, trigger: String, end: String, turns: u64, tools: u64, sends: u64, tokens: u64 },
    "runs-footer" => RunsFooter {},
    "runs-no_such" => RunsNoSuch { id: String },
    "runs-detail" => RunsDetail { id: u64, time: String, trigger: String, end: String, tokens: u64 },
    "runs-step_chat" => RunsStepChat { count: u64 },
    "runs-step_summary" => RunsStepSummary { text: String },
    "runs-step_text" => RunsStepText { text: String },
    "runs-step_call" => RunsStepCall { name: String, args: String },
    "runs-step_result" => RunsStepResult { outcome: String, text: String },
    "runs-trigger_addressed" => RunsTriggerAddressed {},
    "runs-trigger_wake" => RunsTriggerWake {},
    "runs-open" => RunsOpen {},
    "logs-empty" => LogsEmpty {},
    "logs-header" => LogsHeader { count: u64 },
    "logs-line" => LogsLine { time: String, level: String, text: String },
    "usage-forget" => UsageForget {},
    "forget-done" => ForgetDone { index: u64, predicate: String, object: String },
    "forget-no_such" => ForgetNoSuch { index: u64 },
    "what-forget" => WhatForget {},
    "detail-forget" => DetailForget {},
    "report-title" => ReportTitle { date: String },
    "report-runs" => ReportRuns { runs: u64, addressed: u64, wake: u64 },
    "report-ends" => ReportEnds { ends: String },
    "report-errors" => ReportErrors { errors: String },
    "report-count_entry" => ReportCountEntry { label: String, count: u64 },
    "report-model" => ReportModel { calls: u64, failed: u64 },
    "report-tokens" => ReportTokens { input: u64, cached: u64, output: u64, hit: u64 },
    "report-tools" => ReportTools { calls: u64, failed: u64 },
    "report-chat" => ReportChat { messages: u64, groups: u64 },
    "report-groups" => ReportGroups { new: u64, muted: u64 },
    "report-memory" => ReportMemory { episodes: u64 },
    "report-jobs" => ReportJobs { pending: u64, failed: u64, interrupted: u64 },
    "report-backup_age" => ReportBackupAge { hours: u64 },
    "report-backup_none" => ReportBackupNone {},
}
