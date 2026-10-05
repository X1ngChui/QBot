use std::time::Duration;

use qbot_i18n::{Locales, Msg};
use qbot_store::ReportData;

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
    let hit = (data.cached_tokens * 100)
        .checked_div(data.input_tokens)
        .unwrap_or(0);
    let mut lines = vec![
        t(Msg::ReportTitle {
            date: date.to_owned(),
        }),
        t(Msg::ReportRuns {
            runs: data.runs,
            addressed: data.runs_addressed,
            wake: data.runs_wake,
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
    lines.push(t(Msg::ReportModel {
        calls: data.model_calls,
        failed: data.failed_model_calls,
    }));
    lines.push(t(Msg::ReportTokens {
        input: data.input_tokens,
        cached: data.cached_tokens,
        output: data.output_tokens,
        hit,
    }));
    lines.push(t(Msg::ReportTools {
        calls: data.tool_calls,
        failed: data.tool_calls_not_ok,
    }));
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
