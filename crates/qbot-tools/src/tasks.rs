//! The five group-task tools. They are thin: every rule lives in `qbot_sched::TaskService`,
//! which the owner's `/tasks` command uses too.

use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Effect, Tool, ToolCx, ToolError, ToolOutput, Trigger};
use qbot_context::RefusalReason;
use qbot_core::{TimerId, UnixMillis};
use qbot_sched::{Origin, TaskError, TaskService, Timer, TimerState, When};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

/// Tasks per page in listings. A presentation size, not a limit on tasks.
const PAGE: usize = 5;

/// RFC 3339 in UTC.
pub(crate) fn format_time(at: UnixMillis) -> String {
    jiff::Timestamp::from_millisecond(at.get())
        .map_or_else(|_| format!("{}ms", at.get()), |t| t.to_string())
}

/// An RFC 3339 instant; the offset is required.
fn parse_time(text: &str) -> Result<UnixMillis, ToolError> {
    let parsed: jiff::Timestamp = text.parse().map_err(|_| {
        ToolError::InvalidArguments(say(Text::TasksRunAtFormat {
            text: format!("{text:?}"),
        }))
    })?;
    Ok(UnixMillis::new(parsed.as_millisecond()))
}

fn when(run_at: Option<&str>, delay_seconds: Option<u64>) -> Result<Option<When>, ToolError> {
    match (run_at, delay_seconds) {
        (Some(_), Some(_)) => Err(ToolError::InvalidArguments(say(Text::TasksBothTimes {}))),
        (Some(at), None) => Ok(Some(When::At(parse_time(at)?))),
        (None, Some(secs)) => Ok(Some(When::After(Duration::from_secs(secs)))),
        (None, None) => Ok(None),
    }
}

fn describe(timer: &Timer) -> String {
    let state = say(match &timer.state {
        TimerState::Pending => Text::TasksPending {},
        TimerState::Claimed { .. } => Text::TasksRunning {},
        TimerState::Done(_) => Text::TasksDone {},
        TimerState::Cancelled => Text::TasksCancelled {},
        TimerState::Interrupted => Text::TasksInterrupted {},
    });
    say(Text::TasksLine {
        id: timer.id.get().to_string(),
        state,
        due: format_time(timer.due_at),
        intent: timer.wake().map_or("", |w| w.intent.as_str()).to_owned(),
    })
}

fn map_error(error: TaskError) -> ToolError {
    let invalid = |text| ToolError::InvalidArguments(say(text));
    match error {
        TaskError::EmptyIntent => invalid(Text::TasksEmptyIntent {}),
        TaskError::TooSoon { earliest } => invalid(Text::TasksTooSoon {
            earliest: format_time(earliest),
        }),
        TaskError::NothingToChange => invalid(Text::TasksNothingToChange {}),
        TaskError::NotFound => invalid(Text::TasksNotFound {}),
        TaskError::TooManyPending { limit } => ToolError::refused(
            RefusalReason::LimitReached,
            say(Text::TasksTooMany {
                limit: limit.to_string(),
            }),
        ),
        TaskError::ChainTooDeep { limit } => ToolError::refused(
            RefusalReason::LimitReached,
            say(Text::TasksChainDeep {
                limit: limit.to_string(),
            }),
        ),
        TaskError::NotPending => {
            ToolError::refused(RefusalReason::Conflict, say(Text::TasksNotPending {}))
        }
        TaskError::Storage(_) => ToolError::Unavailable(say(Text::TasksStorage {})),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScheduleArgs {
    pub intent: String,
    pub run_at: Option<String>,
    pub delay_seconds: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ScheduleTask(pub TaskService);

#[async_trait]
impl Tool for ScheduleTask {
    type Args = ScheduleArgs;
    const NAME: &'static str = "schedule_task";
    fn description(&self) -> String {
        say(Text::ScheduleTaskDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("intent", say(Text::ScheduleTaskParamIntent {})),
            ("run_at", say(Text::ScheduleTaskParamRunAt {})),
            ("delay_seconds", say(Text::ScheduleTaskParamDelaySeconds {})),
        ]
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    async fn call(&self, cx: &ToolCx<'_>, args: ScheduleArgs) -> Result<ToolOutput, ToolError> {
        let when = when(args.run_at.as_deref(), args.delay_seconds)?
            .ok_or_else(|| ToolError::InvalidArguments(say(Text::TasksNoTime {})))?;
        // A task created while handling a task continues that task's chain.
        let parent = match cx.trigger {
            Trigger::Wake { chain, .. } => Some(*chain),
            Trigger::Addressed { .. } | Trigger::Spontaneous => None,
        };
        let timer = self
            .0
            .create(cx.group, &args.intent, when, parent, Origin::Model)
            .await
            .map_err(map_error)?;
        Ok(ToolOutput::text(say(Text::ScheduleTaskScheduled {
            task: describe(&timer),
        })))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    pub page: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ListScheduledTasks(pub TaskService);

#[async_trait]
impl Tool for ListScheduledTasks {
    type Args = ListArgs;
    const NAME: &'static str = "list_tasks";
    fn description(&self) -> String {
        say(Text::ListTasksDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("page", say(Text::ListTasksParamPage {}))]
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, cx: &ToolCx<'_>, args: ListArgs) -> Result<ToolOutput, ToolError> {
        let page = args.page.unwrap_or(0) as usize;
        let tasks = self
            .0
            .list(cx.group, page * PAGE, PAGE + 1)
            .await
            .map_err(map_error)?;
        if tasks.is_empty() {
            return Ok(ToolOutput::text(say(if page == 0 {
                Text::ListTasksNone {}
            } else {
                Text::ListTasksEmptyPage {}
            })));
        }
        let more = tasks.len() > PAGE;
        let mut lines: Vec<String> = tasks.iter().take(PAGE).map(describe).collect();
        if more {
            lines.push(say(Text::ListTasksMore {
                page: (page + 1).to_string(),
            }));
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskRef {
    pub id: u64,
}

#[derive(Debug, Clone)]
pub struct GetScheduledTask(pub TaskService);

#[async_trait]
impl Tool for GetScheduledTask {
    type Args = TaskRef;
    const NAME: &'static str = "get_task";
    fn description(&self) -> String {
        say(Text::GetTaskDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("id", say(Text::GetTaskParamId {}))]
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, cx: &ToolCx<'_>, args: TaskRef) -> Result<ToolOutput, ToolError> {
        let timer = self
            .0
            .get(cx.group, TimerId::new(args.id))
            .await
            .map_err(map_error)?;
        Ok(ToolOutput::text(describe(&timer)))
    }
}

#[derive(Debug, Clone)]
pub struct CancelScheduledTask(pub TaskService);

#[async_trait]
impl Tool for CancelScheduledTask {
    type Args = TaskRef;
    const NAME: &'static str = "cancel_task";
    fn description(&self) -> String {
        say(Text::CancelTaskDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("id", say(Text::CancelTaskParamId {}))]
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    async fn call(&self, cx: &ToolCx<'_>, args: TaskRef) -> Result<ToolOutput, ToolError> {
        let timer = self
            .0
            .cancel(cx.group, TimerId::new(args.id))
            .await
            .map_err(map_error)?;
        Ok(ToolOutput::text(say(Text::CancelTaskCancelled {
            task: describe(&timer),
        })))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateArgs {
    pub id: u64,
    pub intent: Option<String>,
    pub run_at: Option<String>,
    pub delay_seconds: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct UpdateScheduledTask(pub TaskService);

#[async_trait]
impl Tool for UpdateScheduledTask {
    type Args = UpdateArgs;
    const NAME: &'static str = "update_task";
    fn description(&self) -> String {
        say(Text::UpdateTaskDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("id", say(Text::UpdateTaskParamId {})),
            ("intent", say(Text::UpdateTaskParamIntent {})),
            ("run_at", say(Text::UpdateTaskParamRunAt {})),
            ("delay_seconds", say(Text::UpdateTaskParamDelaySeconds {})),
        ]
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    async fn call(&self, cx: &ToolCx<'_>, args: UpdateArgs) -> Result<ToolOutput, ToolError> {
        let when = when(args.run_at.as_deref(), args.delay_seconds)?;
        let timer = self
            .0
            .update(
                cx.group,
                TimerId::new(args.id),
                args.intent.as_deref(),
                when,
            )
            .await
            .map_err(map_error)?;
        Ok(ToolOutput::text(say(Text::UpdateTaskUpdated {
            task: describe(&timer),
        })))
    }
}
