//! Short model-facing wording: tool and parameter descriptions, tool results and error
//! explanations, and the small texts the prompt layer and memory extraction write. The texts are
//! in `prompts/wording.toml`; this crate is the closed set of them with their slots, so code names
//! a text by a typed variant and a test keeps the file and the code in step. Long prompts are
//! templates in `qbot-prompt`; markers the code writes and parses stay in code.

use std::collections::HashMap;
use std::sync::LazyLock;

/// The wording file, compiled in.
pub const SOURCE: &str = include_str!("../../../prompts/wording.toml");

static TEXTS: LazyLock<HashMap<String, String>> = LazyLock::new(|| flatten(SOURCE));

/// Every string in the file under its dotted key. A file that does not parse yields nothing; the
/// tests make that impossible to ship.
fn flatten(source: &str) -> HashMap<String, String> {
    fn walk(prefix: &str, table: &toml::Table, out: &mut HashMap<String, String>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) => walk(&path, inner, out),
                toml::Value::String(text) => {
                    out.insert(path, text.clone());
                }
                _ => {}
            }
        }
    }
    let mut out = HashMap::new();
    if let Ok(table) = source.parse::<toml::Table>() {
        walk("", &table, &mut out);
    }
    out
}

macro_rules! wording {
    ($( $key:literal => $name:ident { $( $field:ident ),* $(,)? } ),* $(,)?) => {
        /// One model-facing text, with the values for its slots.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Text {
            $( $name { $( $field: String ),* } ),*
        }

        impl Text {
            /// The text's key in the wording file.
            pub fn key(&self) -> &'static str {
                match self {
                    $( Text::$name { .. } => $key ),*
                }
            }

            fn slots(&self) -> Vec<(&'static str, &str)> {
                match self {
                    $( Text::$name { $( $field ),* } => vec![ $( (stringify!($field), $field.as_str()) ),* ] ),*
                }
            }

            /// Every key with the slots its text must use, for the consistency tests.
            pub fn schema() -> Vec<(&'static str, Vec<&'static str>)> {
                vec![ $( ($key, vec![ $( stringify!($field) ),* ]) ),* ]
            }
        }
    };
}

wording! {
    "tools.send_message.description" => SendMessageDescription {},
    "tools.send_message.param_text" => SendMessageParamText {},
    "tools.send_message.param_end_turn" => SendMessageParamEndTurn {},
    "tools.send_message.sent" => SendMessageSent { id, text },
    "tools.send_message.limit" => SendMessageLimit { max },
    "tools.send_message.rejected" => SendMessageRejected { reason },
    "tools.send_message.unconfirmed" => SendMessageUnconfirmed {},
    "tools.send_message.empty" => SendMessageEmpty {},
    "tools.send_message.alone" => SendMessageAlone {},
    "tools.send_message.chosen_result" => SendMessageChosenResult { name },
    "tools.send_message.reply_first" => SendMessageReplyFirst {},
    "tools.send_message.no_member" => SendMessageNoMember { member },
    "tools.send_message.no_message" => SendMessageNoMessage { id },
    "tools.send_message.not_message_id" => SendMessageNotMessageId { id },
    "tools.send_message.needs_member" => SendMessageNeedsMember { name },
    "tools.send_message.needs_face" => SendMessageNeedsFace { name },
    "tools.send_message.needs_message" => SendMessageNeedsMessage { name },
    "tools.send_message.too_large" => SendMessageTooLarge { name },
    "tools.send_message.at_all" => SendMessageAtAll {},
    "tools.stay_silent.description" => StaySilentDescription {},
    "tools.stay_silent.result" => StaySilentResult {},
    "tools.search_history.description" => SearchHistoryDescription {},
    "tools.search_history.param_query" => SearchHistoryParamQuery {},
    "tools.search_history.param_speaker" => SearchHistoryParamSpeaker {},
    "tools.search_history.param_limit" => SearchHistoryParamLimit {},
    "tools.search_history.no_matches" => SearchHistoryNoMatches {},
    "tools.search_history.limit_range" => SearchHistoryLimitRange { max },
    "tools.search_history.query_unreadable" => SearchHistoryQueryUnreadable { at },
    "tools.search_history.query_expected" => SearchHistoryQueryExpected { at, expected },
    "tools.search_history.query_only_excludes" => SearchHistoryQueryOnlyExcludes {},
    "tools.search_history.expect_operand" => SearchHistoryExpectOperand {},
    "tools.search_history.expect_after_or" => SearchHistoryExpectAfterOr {},
    "tools.search_history.expect_after_minus" => SearchHistoryExpectAfterMinus {},
    "tools.search_history.expect_after_not" => SearchHistoryExpectAfterNot {},
    "tools.search_history.expect_phrase_text" => SearchHistoryExpectPhraseText {},
    "tools.search_history.expect_closing_quote" => SearchHistoryExpectClosingQuote {},
    "tools.search_history.expect_closing_paren" => SearchHistoryExpectClosingParen {},
    "tools.search_history.expect_end" => SearchHistoryExpectEnd {},
    "tools.recall_episodes.description" => RecallEpisodesDescription {},
    "tools.recall_episodes.param_question" => RecallEpisodesParamQuestion {},
    "tools.recall_episodes.empty_question" => RecallEpisodesEmptyQuestion {},
    "tools.recall_episodes.no_matches" => RecallEpisodesNoMatches {},
    "tools.recall_episodes.episode" => RecallEpisodesEpisode { id, start, end, count, title, summary },
    "tools.read_episode.description" => ReadEpisodeDescription {},
    "tools.read_episode.param_id" => ReadEpisodeParamId {},
    "tools.read_episode.no_such" => ReadEpisodeNoSuch { id },
    "tools.lookup_member.description" => LookupMemberDescription {},
    "tools.lookup_member.param_member" => LookupMemberParamMember {},
    "tools.lookup_member.no_member" => LookupMemberNoMember { member },
    "tools.lookup_member.shown_as" => LookupMemberShownAs { name },
    "tools.lookup_member.shown_unavailable" => LookupMemberShownUnavailable {},
    "tools.lookup_member.other_names" => LookupMemberOtherNames { names },
    "tools.lookup_member.no_other_names" => LookupMemberNoOtherNames {},
    "tools.lookup_member.name_confirmed" => LookupMemberNameConfirmed { name },
    "tools.lookup_member.name_lead" => LookupMemberNameLead { name, confidence },
    "tools.lookup_member.notes" => LookupMemberNotes {},
    "tools.lookup_member.no_notes" => LookupMemberNoNotes {},
    "tools.lookup_member.note" => LookupMemberNote { text, age },
    "tools.lookup_member.learned" => LookupMemberLearned {},
    "tools.lookup_member.no_learned" => LookupMemberNoLearned {},
    "tools.lookup_member.fact" => LookupMemberFact { predicate, object, confidence, age },
    "tools.lookup_member.today" => LookupMemberToday {},
    "tools.lookup_member.one_day" => LookupMemberOneDay {},
    "tools.lookup_member.days" => LookupMemberDays { days },
    "tools.schedule_task.description" => ScheduleTaskDescription {},
    "tools.schedule_task.param_intent" => ScheduleTaskParamIntent {},
    "tools.schedule_task.param_run_at" => ScheduleTaskParamRunAt {},
    "tools.schedule_task.param_delay_seconds" => ScheduleTaskParamDelaySeconds {},
    "tools.schedule_task.scheduled" => ScheduleTaskScheduled { task },
    "tools.list_tasks.description" => ListTasksDescription {},
    "tools.list_tasks.param_page" => ListTasksParamPage {},
    "tools.list_tasks.none" => ListTasksNone {},
    "tools.list_tasks.empty_page" => ListTasksEmptyPage {},
    "tools.list_tasks.more" => ListTasksMore { page },
    "tools.get_task.description" => GetTaskDescription {},
    "tools.get_task.param_id" => GetTaskParamId {},
    "tools.update_task.description" => UpdateTaskDescription {},
    "tools.update_task.param_id" => UpdateTaskParamId {},
    "tools.update_task.param_intent" => UpdateTaskParamIntent {},
    "tools.update_task.param_run_at" => UpdateTaskParamRunAt {},
    "tools.update_task.param_delay_seconds" => UpdateTaskParamDelaySeconds {},
    "tools.update_task.updated" => UpdateTaskUpdated { task },
    "tools.cancel_task.description" => CancelTaskDescription {},
    "tools.cancel_task.param_id" => CancelTaskParamId {},
    "tools.cancel_task.cancelled" => CancelTaskCancelled { task },
    "tools.web_search.description" => WebSearchDescription {},
    "tools.web_search.param_query" => WebSearchParamQuery {},
    "tools.web_search.empty_query" => WebSearchEmptyQuery {},
    "tools.web_search.allowance" => WebSearchAllowance {},
    "tools.web_search.no_results" => WebSearchNoResults { query },
    "tools.web_search.header" => WebSearchHeader { query },
    "tools.web_search.published" => WebSearchPublished { date },
    "tools.read_url.description" => ReadUrlDescription {},
    "tools.read_url.param_url" => ReadUrlParamUrl {},
    "tools.read_url.param_question" => ReadUrlParamQuestion {},
    "tools.read_url.invalid_url" => ReadUrlInvalidUrl { error },
    "tools.read_url.scheme" => ReadUrlScheme {},
    "tools.read_url.no_host" => ReadUrlNoHost {},
    "tools.read_url.credentials" => ReadUrlCredentials {},
    "tools.read_url.allowance" => ReadUrlAllowance {},
    "tools.read_url.failed" => ReadUrlFailed { url, reason },
    "tools.read_url.no_text" => ReadUrlNoText { url },
    "tools.read_url.about_question" => ReadUrlAboutQuestion { url, question },
    "tools.read_url.about_page" => ReadUrlAboutPage { url },
    "tools.read_url.header" => ReadUrlHeader { what },
    "tools.read_url.cut" => ReadUrlCut { total, shown },
    "tools.open_images.description" => OpenImagesDescription {},
    "tools.open_images.param_pictures" => OpenImagesParamPictures {},
    "tools.open_images.param_message" => OpenImagesParamMessage {},
    "tools.open_images.param_position" => OpenImagesParamPosition {},
    "tools.open_images.param_sticker" => OpenImagesParamSticker {},
    "tools.open_images.none" => OpenImagesNone {},
    "tools.open_images.not_message_id" => OpenImagesNotMessageId { id },
    "tools.open_images.not_in_chat" => OpenImagesNotInChat { id },
    "tools.open_images.position" => OpenImagesPosition {},
    "tools.open_images.no_such" => OpenImagesNoSuch {},
    "tools.open_images.too_large" => OpenImagesTooLarge {},
    "tools.open_images.gone" => OpenImagesGone {},
    "tasks.line" => TasksLine { id, state, due, intent },
    "tasks.pending" => TasksPending {},
    "tasks.running" => TasksRunning {},
    "tasks.done" => TasksDone {},
    "tasks.cancelled" => TasksCancelled {},
    "tasks.interrupted" => TasksInterrupted {},
    "tasks.run_at_format" => TasksRunAtFormat { text },
    "tasks.both_times" => TasksBothTimes {},
    "tasks.no_time" => TasksNoTime {},
    "tasks.empty_intent" => TasksEmptyIntent {},
    "tasks.too_soon" => TasksTooSoon { earliest },
    "tasks.too_many" => TasksTooMany { limit },
    "tasks.chain_deep" => TasksChainDeep { limit },
    "tasks.not_found" => TasksNotFound {},
    "tasks.not_pending" => TasksNotPending {},
    "tasks.nothing_to_change" => TasksNothingToChange {},
    "tasks.storage" => TasksStorage {},
    "outcome.failed" => OutcomeFailed { why },
    "outcome.invalid_arguments" => OutcomeInvalidArguments {},
    "outcome.execution" => OutcomeExecution {},
    "outcome.unavailable" => OutcomeUnavailable {},
    "outcome.timeout" => OutcomeTimeout {},
    "outcome.refused" => OutcomeRefused { why },
    "outcome.limit_reached" => OutcomeLimitReached {},
    "outcome.not_allowed" => OutcomeNotAllowed {},
    "outcome.conflict" => OutcomeConflict {},
    "outcome.interrupted" => OutcomeInterrupted {},
    "outcome.unknown_tool" => OutcomeUnknownTool { name },
    "prompt.group_term" => PromptGroupTerm { term, text },
    "prompt.group_topic" => PromptGroupTopic { text },
    "prompt.member_name" => PromptMemberName { member, name },
    "prompt.member_name_unavailable" => PromptMemberNameUnavailable { member },
    "prompt.blocked" => PromptBlocked { members },
    "prompt.same_person" => PromptSamePerson { members },
    "prompt.recap" => PromptRecap { start, end, title, summary },
    "extract.tool_description" => ExtractToolDescription {},
    "extract.param_title" => ExtractParamTitle {},
    "extract.param_summary" => ExtractParamSummary {},
    "extract.param_evidence" => ExtractParamEvidence {},
    "extract.param_evidence_line" => ExtractParamEvidenceLine {},
    "extract.param_evidence_quote" => ExtractParamEvidenceQuote {},
    "extract.param_names" => ExtractParamNames {},
    "extract.param_name_member" => ExtractParamNameMember {},
    "extract.param_name_name" => ExtractParamNameName {},
    "extract.param_facts" => ExtractParamFacts {},
    "extract.param_fact_member" => ExtractParamFactMember {},
    "extract.param_fact_predicate" => ExtractParamFactPredicate {},
    "extract.param_fact_object" => ExtractParamFactObject {},
    "extract.param_knowledge" => ExtractParamKnowledge {},
    "extract.param_knowledge_kind" => ExtractParamKnowledgeKind {},
    "extract.param_knowledge_term" => ExtractParamKnowledgeTerm {},
    "extract.param_knowledge_text" => ExtractParamKnowledgeText {},
    "extract.param_line" => ExtractParamLine {},
    "extract.param_quote" => ExtractParamQuote {},
    "extract.context_before" => ExtractContextBefore {},
    "extract.target" => ExtractTarget {},
    "extract.context_after" => ExtractContextAfter {},
    "extract.predicate" => ExtractPredicate { name, cardinality, rule },
    "extract.one_value" => ExtractOneValue {},
    "extract.several_values" => ExtractSeveralValues {},
    "extract.no_call" => ExtractNoCall { tool },
    "extract.bad_arguments" => ExtractBadArguments { error },
    "extract.call_once" => ExtractCallOnce { tool },
    "extract.rejected" => ExtractRejected { problems, tool },
    "extract.problem" => ExtractProblem { problem },
    "extract.empty_title" => ExtractEmptyTitle {},
    "extract.empty_summary" => ExtractEmptySummary {},
    "extract.evidence_count" => ExtractEvidenceCount { count },
    "extract.evidence_line" => ExtractEvidenceLine { line },
    "extract.evidence_quote" => ExtractEvidenceQuote { line },
    "extract.findings_rejected" => ExtractFindingsRejected { problems, tool },
    "extract.finding" => ExtractFinding { list, item, reason },
    "extract.finding_line" => ExtractFindingLine { line },
    "extract.finding_bot_line" => ExtractFindingBotLine { line },
    "extract.finding_event" => ExtractFindingEvent { line },
    "extract.finding_quote" => ExtractFindingQuote { line },
    "extract.finding_no_member" => ExtractFindingNoMember { member },
    "extract.finding_not_subject" => ExtractFindingNotSubject { member },
    "extract.finding_name_not_quoted" => ExtractFindingNameNotQuoted { name },
    "extract.finding_name_empty" => ExtractFindingNameEmpty {},
    "extract.finding_name_too_long" => ExtractFindingNameTooLong {},
    "extract.finding_name_shared" => ExtractFindingNameShared { name },
    "extract.finding_predicate" => ExtractFindingPredicate { predicate },
    "extract.finding_empty_object" => ExtractFindingEmptyObject {},
    "extract.finding_empty_text" => ExtractFindingEmptyText {},
    "extract.finding_term_not_written" => ExtractFindingTermNotWritten { term },
}

impl Text {
    /// The text with its slots filled.
    pub fn render(&self) -> String {
        let Some(template) = TEXTS.get(self.key()) else {
            return self.key().to_owned();
        };
        let mut out = template.clone();
        for (name, value) in self.slots() {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }
}

/// Shorthand for `text.render()`.
pub fn say(text: Text) -> String {
    text.render()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn slots_in(text: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            let name = &rest[open + 1..open + close];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                out.insert(name.to_owned());
            }
            rest = &rest[open + close + 1..];
        }
        out
    }

    #[test]
    fn the_file_and_the_code_name_the_same_texts_with_the_same_slots() {
        let table: toml::Table = SOURCE.parse().unwrap();
        assert!(!table.is_empty());
        let file = flatten(SOURCE);
        let schema = Text::schema();
        let code: BTreeSet<&str> = schema.iter().map(|(k, _)| *k).collect();
        let in_file: BTreeSet<&str> = file.keys().map(String::as_str).collect();
        assert_eq!(
            in_file.difference(&code).collect::<Vec<_>>(),
            Vec::<&&str>::new(),
            "texts in the file that no code uses"
        );
        assert_eq!(
            code.difference(&in_file).collect::<Vec<_>>(),
            Vec::<&&str>::new(),
            "texts the code names that the file lacks"
        );
        for (key, slots) in schema {
            let declared: BTreeSet<String> = slots.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(slots_in(&file[key]), declared, "{key}");
        }
    }

    #[test]
    fn texts_are_english_and_render_their_slots() {
        for text in flatten(SOURCE).values() {
            assert!(
                text.chars()
                    .all(|c| !('\u{2E80}'..='\u{9FFF}').contains(&c)),
                "{text}"
            );
        }
        assert_eq!(
            Text::SendMessageLimit { max: "4".into() }.render(),
            "at most 4 messages can be sent per run"
        );
        assert_eq!(say(Text::StaySilentResult {}), "staying silent");
    }
}
