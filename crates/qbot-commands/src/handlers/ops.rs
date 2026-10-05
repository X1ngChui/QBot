//! Owner diagnostics: `/runs` and `/logs`.

use qbot_context::{AssistantPart, Item, Outcome, Part};
use qbot_core::RunId;
use qbot_i18n::Msg;
use qbot_store::{RunRecord, TriggerKind};

use super::Reply;
use crate::args;
use crate::router::Cx;

fn trigger_label(cx: &Cx<'_>, record: &RunRecord) -> String {
    cx.t(&match record.trigger_kind {
        TriggerKind::Wake => Msg::RunsTriggerWake {},
        TriggerKind::Addressed => Msg::RunsTriggerAddressed {},
    })
}

fn end_label(cx: &Cx<'_>, record: &RunRecord) -> String {
    match (record.end, &record.error_class) {
        (Some(end), Some(class)) => format!("{} ({class})", end.as_str()),
        (Some(end), None) => end.as_str().to_owned(),
        (None, _) => cx.t(&Msg::RunsOpen {}),
    }
}

fn tokens(record: &RunRecord) -> u64 {
    let n = |v: Option<i64>| u64::try_from(v.unwrap_or(0)).unwrap_or(0);
    n(record.input_tokens) + n(record.output_tokens)
}

fn count(v: Option<i32>) -> u64 {
    u64::try_from(v.unwrap_or(0)).unwrap_or(0)
}

fn outcome_label(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok => "ok".into(),
        Outcome::Error(kind) => format!("error: {kind:?}").to_lowercase(),
        Outcome::Refused(reason) => format!("refused: {reason:?}").to_lowercase(),
        Outcome::Interrupted => "interrupted".into(),
    }
}

pub(crate) async fn runs(cx: &Cx<'_>) -> Reply {
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageRuns {}.into());
    }
    match cx.tokens().as_slice() {
        [] => {
            let limit = i64::try_from(cx.deps.settings.runs_max_rows).unwrap_or(i64::MAX);
            let records = cx.deps.runs.recent(cx.group(), limit).await?;
            if records.is_empty() {
                return Ok(cx.t(&Msg::RunsEmpty {}));
            }
            let mut lines = vec![cx.t(&Msg::RunsHeader {
                count: records.len() as u64,
            })];
            for record in &records {
                lines.push(cx.t(&Msg::RunsRow {
                    id: record.run.get(),
                    time: cx.short_time(record.started),
                    trigger: trigger_label(cx, record),
                    end: end_label(cx, record),
                    turns: count(record.turns),
                    tools: count(record.tool_calls),
                    sends: count(record.sends),
                    tokens: tokens(record),
                }));
            }
            lines.push(cx.t(&Msg::RunsFooter {}));
            Ok(cx.fit(&lines.join("\n")))
        }
        [id] => {
            let no_such = || Msg::RunsNoSuch {
                id: (*id).to_owned(),
            };
            let run = id.parse::<u64>().map_err(|_| no_such())?;
            let Some((record, items)) = cx.deps.runs.run(cx.group(), RunId::new(run)).await? else {
                return Err(no_such().into());
            };
            let mut lines = vec![cx.t(&Msg::RunsDetail {
                id: record.run.get(),
                time: cx.short_time(record.started),
                trigger: trigger_label(cx, &record),
                end: end_label(cx, &record),
                tokens: tokens(&record),
            })];
            for item in &items {
                match item {
                    Item::Chat(batch) => lines.push(cx.t(&Msg::RunsStepChat {
                        count: batch.lines().len() as u64,
                    })),
                    Item::Summary(summary) => lines.push(cx.t(&Msg::RunsStepSummary {
                        text: args::preview(&summary.text, 60),
                    })),
                    Item::Assistant(turn) => {
                        for part in turn.parts() {
                            match part {
                                AssistantPart::Text(text) if !text.trim().is_empty() => {
                                    lines.push(cx.t(&Msg::RunsStepText {
                                        text: args::preview(text.trim(), 80),
                                    }));
                                }
                                AssistantPart::Call(call) => lines.push(cx.t(&Msg::RunsStepCall {
                                    name: call.name.clone(),
                                    args: args::preview(&call.arguments.to_string(), 80),
                                })),
                                _ => {}
                            }
                        }
                    }
                    Item::ToolResult(result) => {
                        let text: Vec<&str> = result
                            .content
                            .iter()
                            .filter_map(|p| match p {
                                Part::Text(t) => Some(t.as_str()),
                                Part::Image { .. } => None,
                            })
                            .collect();
                        lines.push(cx.t(&Msg::RunsStepResult {
                            outcome: outcome_label(&result.outcome),
                            text: args::preview(&text.join(" ").replace('\n', " "), 80),
                        }));
                    }
                    Item::Instruction(_) | Item::Meta(_) => {}
                }
            }
            Ok(cx.fit(&lines.join("\n")))
        }
        _ => Err(Msg::UsageRuns {}.into()),
    }
}

pub(crate) async fn logs(cx: &Cx<'_>) -> Reply {
    if !cx.req.mentions.is_empty() {
        return Err(Msg::UsageLogs {}.into());
    }
    let limit = match cx.tokens().as_slice() {
        [] => usize::MAX,
        [n] => args::positive(n)? as usize,
        _ => return Err(Msg::UsageLogs {}.into()),
    };
    let lines = cx.deps.logs.recent(limit);
    if lines.is_empty() {
        return Ok(cx.t(&Msg::LogsEmpty {}));
    }
    // Newest last; when the reply has to be cut, keep the newest.
    let mut out = vec![cx.t(&Msg::LogsHeader {
        count: lines.len() as u64,
    })];
    out.extend(lines.iter().map(|line| {
        cx.t(&Msg::LogsLine {
            time: cx.short_time(line.at),
            level: line.level.clone(),
            text: args::preview(&line.text, 160),
        })
    }));
    let budget = cx.deps.settings.max_message_chars.saturating_sub(2);
    while out.len() > 2 && out.iter().map(|l| l.chars().count() + 1).sum::<usize>() > budget {
        out.remove(1);
    }
    Ok(cx.fit(&out.join("\n")))
}
