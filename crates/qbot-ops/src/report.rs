use std::time::Duration;

use qbot_i18n::{Locales, Msg};
use qbot_llm::{CallKind, Purpose};
use qbot_store::{ReportData, UsageLine, UsageSummary};

fn entries(locales: &Locales, pairs: &[(String, u64)]) -> String {
    let separator = locales.render(&Msg::ListSeparator {});
    pairs
        .iter()
        .map(|(label, count)| {
            locales.render(&Msg::ReportCountEntry {
                label: label.clone(),
                count: *count,
            })
        })
        .collect::<Vec<_>>()
        .join(&separator)
}

/// What a line's calls consumed: tokens for a model, input tokens for embeddings, credits for
/// web search and page reading; an unreported cache or credit count says so.
fn amounts(locales: &Locales, line: &UsageLine) -> String {
    let t = |msg: Msg| locales.render(&msg);
    match line.kind {
        CallKind::Model => t(Msg::CallsTokens {
            input: line.input_tokens,
            cache: match line.cached_tokens {
                Some(cached) => t(Msg::CallsCached {
                    cached,
                    hit: (cached * 100).checked_div(line.input_tokens).unwrap_or(0),
                }),
                None => t(Msg::CallsCacheUnreported {}),
            },
            output: line.output_tokens,
        }),
        CallKind::Embedding => t(Msg::CallsInputTokens {
            input: line.input_tokens,
        }),
        CallKind::Search | CallKind::Extract => match line.credits {
            Some(credits) => t(Msg::CallsCredits { credits }),
            None => t(Msg::CallsCreditsUnreported {}),
        },
    }
}

/// Every provider call of a period: one line per work and kind of call, then the total. The
/// same lines serve `/stats` and the daily report.
pub fn usage_lines(locales: &Locales, usage: &UsageSummary) -> Vec<String> {
    let t = |msg: Msg| locales.render(&msg);
    if usage.calls.is_empty() {
        return vec![t(Msg::CallsNone {})];
    }
    let mut lines: Vec<String> = usage
        .calls
        .iter()
        .map(|line| {
            t(Msg::CallsLine {
                purpose: t(match line.purpose {
                    Some(Purpose::Reply) => Msg::PurposeReply {},
                    Some(Purpose::Extraction) => Msg::PurposeExtraction {},
                    Some(Purpose::Picture) => Msg::PurposePicture {},
                    None => Msg::PurposeUnattributed {},
                }),
                kind: t(match line.kind {
                    CallKind::Model => Msg::CallKindModel {},
                    CallKind::Embedding => Msg::CallKindEmbedding {},
                    CallKind::Search => Msg::CallKindSearch {},
                    CallKind::Extract => Msg::CallKindExtract {},
                }),
                calls: line.calls,
                requests: line.requests,
                failed: line.failed,
                amounts: amounts(locales, line),
            })
        })
        .collect();
    let sum = |f: fn(&UsageLine) -> u64| usage.calls.iter().map(f).sum::<u64>();
    lines.push(t(Msg::CallsTotal {
        calls: sum(|l| l.calls),
        requests: sum(|l| l.requests),
        failed: sum(|l| l.failed),
        input: sum(|l| l.input_tokens),
        output: sum(|l| l.output_tokens),
        credits: sum(|l| l.credits.unwrap_or(0)),
    }));
    lines
}

/// The daily report as chat text. `backup_age` is `None` when no backup exists, and the whole
/// backup line is left out when backups are switched off (`backups_on` false).
pub fn render_report(
    locales: &Locales,
    date: &str,
    data: &ReportData,
    backups_on: bool,
    backup_age: Option<Duration>,
) -> String {
    let t = |msg: Msg| locales.render(&msg);
    let mut lines = vec![
        t(Msg::ReportTitle {
            date: date.to_owned(),
        }),
        t(Msg::ReportRuns {
            runs: data.runs,
            addressed: data.runs_addressed,
            wake: data.runs_wake,
            spontaneous: data.runs_spontaneous,
        }),
    ];
    if !data.run_ends.is_empty() {
        lines.push(t(Msg::ReportEnds {
            ends: entries(locales, &data.run_ends),
        }));
    }
    if !data.error_classes.is_empty() {
        lines.push(t(Msg::ReportErrors {
            errors: entries(locales, &data.error_classes),
        }));
    }
    lines.push(t(Msg::ReportTools {
        calls: data.usage.tool_calls,
        failed: data.usage.tool_calls_not_ok,
    }));
    lines.extend(usage_lines(locales, &data.usage));
    lines.push(t(Msg::ReportChat {
        messages: data.messages,
        groups: data.groups_active,
    }));
    lines.push(t(Msg::ReportGroups {
        new: data.groups_new,
        muted: data.groups_muted,
    }));
    lines.push(t(Msg::ReportMemory {
        episodes: data.episodes,
    }));
    lines.push(t(Msg::ReportJobs {
        pending: data.jobs_pending,
        failed: data.jobs_failed,
        interrupted: data.tasks_interrupted,
    }));
    if backups_on {
        lines.push(match backup_age {
            Some(age) => t(Msg::ReportBackupAge {
                hours: age.as_secs() / 3600,
            }),
            None => t(Msg::ReportBackupNone {}),
        });
    }
    lines.join("\n")
}
