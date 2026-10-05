use std::str::FromStr;

use jiff::Timestamp;
use qbot_core::{TimerId, UnixMillis};
use qbot_i18n::Msg;
use qbot_sched::{Origin, Timer, TimerOutcome, TimerState, When};

use super::Reply;
use crate::args;
use crate::fail::{Fail, task_fail};
use crate::router::Cx;

/// Tasks per `/tasks list` page. A display choice: a page must fit in one message.
const PAGE_SIZE: usize = 5;
const PREVIEW_CHARS: usize = 60;

fn state_text(cx: &Cx<'_>, timer: &Timer) -> String {
    cx.t(&match &timer.state {
        TimerState::Pending => Msg::TasksStatePending {},
        TimerState::Claimed { .. } => Msg::TasksStateRunning {},
        TimerState::Done(outcome) if outcome.is_failure() => Msg::TasksStateFailed {},
        TimerState::Done(_) => Msg::TasksStateDone {},
        TimerState::Cancelled => Msg::TasksStateCancelledState {},
        TimerState::Interrupted => Msg::TasksStateInterrupted {},
    })
}

fn task_text(cx: &Cx<'_>, timer: &Timer, preview: bool) -> String {
    let intent = timer.wake().map(|w| w.intent.clone()).unwrap_or_default();
    let intent = if preview {
        args::preview(&intent, PREVIEW_CHARS)
    } else {
        intent
    };
    let mut text = cx.t(&Msg::TasksDetail {
        id: timer.id.get().to_string(),
        state: state_text(cx, timer),
        due: cx.iso_time(timer.due_at),
        intent,
    });
    if let (false, TimerState::Done(outcome)) = (preview, &timer.state) {
        let outcome = match outcome {
            TimerOutcome::Ran(end) => end.as_str().to_owned(),
            TimerOutcome::SkippedMuted => cx.t(&Msg::TasksSkippedMuted {}),
            TimerOutcome::JobOk => "ok".to_owned(),
            TimerOutcome::JobFailed(reason) => reason.clone(),
        };
        text.push('\n');
        text.push_str(&cx.t(&Msg::TasksDetailOutcome { outcome }));
    }
    text
}

fn parse_id(text: &str) -> Result<TimerId, Fail> {
    text.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(TimerId::new)
        .ok_or_else(|| Msg::TasksBadId {}.into())
}

fn map<'a>(cx: &'a Cx<'_>) -> impl Fn(qbot_sched::TaskError) -> Fail + 'a {
    move |error| task_fail(error, |at| cx.iso_time(at))
}

/// `--at TIME` or `--in DURATION` pairs, at most one of them.
fn time_options(tokens: &[&str]) -> Result<Option<When>, Fail> {
    if !tokens.len().is_multiple_of(2) {
        return Err(Msg::TasksOptionValue {}.into());
    }
    let (mut at, mut within) = (None, None);
    for pair in tokens.chunks(2) {
        let slot = match pair[0] {
            "--at" => &mut at,
            "--in" => &mut within,
            _ => return Err(Msg::TasksOptionDupe {}.into()),
        };
        if slot.replace(pair[1]).is_some() {
            return Err(Msg::TasksOptionDupe {}.into());
        }
    }
    match (at, within) {
        (Some(_), Some(_)) => Err(Msg::TasksAtInConflict {}.into()),
        (Some(at), None) => {
            let parsed = Timestamp::from_str(at).map_err(|_| Msg::TasksBadTime {})?;
            Ok(Some(When::At(UnixMillis::new(parsed.as_millisecond()))))
        }
        (None, Some(within)) => Ok(Some(When::After(
            args::duration(within).ok_or(Msg::DurationInvalid {})?,
        ))),
        (None, None) => Ok(None),
    }
}

pub(crate) async fn tasks(cx: &Cx<'_>) -> Reply {
    if !cx.req.mentions.is_empty() {
        return Err(Msg::TasksNoMentions {}.into());
    }
    let (header, content) = args::body(&cx.req.args);
    let mut tokens: Vec<&str> = header.split_whitespace().collect();
    let action = if tokens.is_empty() {
        "list"
    } else {
        tokens.remove(0)
    };
    let group = cx.group();
    match action {
        "list" => {
            if tokens.len() > 1 || content.is_some() {
                return Err(Msg::UsageTasksList {}.into());
            }
            let page = tokens.first().map_or(Ok(1), |p| args::positive(p))?;
            let offset = usize::try_from(page - 1).unwrap_or(0) * PAGE_SIZE;
            let mut found = cx
                .deps
                .tasks
                .list(group, offset, PAGE_SIZE + 1)
                .await
                .map_err(map(cx))?;
            if found.is_empty() {
                return Ok(cx.t(&if page == 1 {
                    Msg::TasksListEmpty {}
                } else {
                    Msg::TasksListPageEmpty {}
                }));
            }
            let more = found.len() > PAGE_SIZE;
            found.truncate(PAGE_SIZE);
            let mut blocks = vec![cx.t(&Msg::TasksListHeader {
                page: page.to_string(),
            })];
            blocks.extend(found.iter().map(|t| task_text(cx, t, true)));
            if more {
                blocks.push(cx.t(&Msg::TasksListNext {
                    page: (page + 1).to_string(),
                }));
            }
            blocks.push(cx.t(&Msg::TasksListFooter {}));
            Ok(cx.fit(&blocks.join("\n\n")))
        }
        "show" | "cancel" => {
            let [id] = tokens.as_slice() else {
                return Err(Msg::UsageTasksOne {
                    action: action.to_owned(),
                }
                .into());
            };
            if content.is_some() {
                return Err(Msg::UsageTasksOne {
                    action: action.to_owned(),
                }
                .into());
            }
            let id = parse_id(id)?;
            if action == "show" {
                let timer = cx.deps.tasks.get(group, id).await.map_err(map(cx))?;
                return Ok(cx.fit(&cx.t(&Msg::TasksShown {
                    task: task_text(cx, &timer, false),
                })));
            }
            match cx.deps.tasks.cancel(group, id).await {
                Ok(timer) => Ok(cx.fit(&cx.t(&Msg::TasksCancelled {
                    task: task_text(cx, &timer, false),
                }))),
                Err(qbot_sched::TaskError::NotFound | qbot_sched::TaskError::NotPending) => {
                    Err(Msg::TasksNotFoundCancel {}.into())
                }
                Err(other) => Err(map(cx)(other)),
            }
        }
        "add" => {
            let when = time_options(&tokens)?;
            let content = content.map(str::to_owned);
            if content.as_deref() == Some("") {
                return Err(Msg::TasksEmptyContent {}.into());
            }
            let (Some(when), Some(content)) = (when, content) else {
                return Err(Msg::UsageTasksAdd {}.into());
            };
            let timer = cx
                .deps
                .tasks
                .create(group, &content, when, None, Origin::Owner)
                .await
                .map_err(map(cx))?;
            Ok(cx.fit(&cx.t(&Msg::TasksCreated {
                task: task_text(cx, &timer, false),
            })))
        }
        "edit" => {
            if tokens.is_empty() {
                return Err(Msg::UsageTasksEdit {}.into());
            }
            let id = parse_id(tokens.remove(0))?;
            let when = time_options(&tokens)?;
            if content == Some("") {
                return Err(Msg::TasksEmptyContent {}.into());
            }
            match cx.deps.tasks.update(group, id, content, when).await {
                Ok(timer) => Ok(cx.fit(&cx.t(&Msg::TasksEdited {
                    task: task_text(cx, &timer, false),
                }))),
                Err(qbot_sched::TaskError::NotFound | qbot_sched::TaskError::NotPending) => {
                    Err(Msg::TasksNotFoundEdit {}.into())
                }
                Err(other) => Err(map(cx)(other)),
            }
        }
        _ => Err(Msg::TasksUnknownAction {}.into()),
    }
}
