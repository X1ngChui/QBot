//! The five group-task tools. They are thin: every rule lives in `qbot_sched::TaskService`,
//! which the owner's `/tasks` command uses too.

use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Effect, Tool, ToolCx, ToolError, ToolOutput, Trigger};
use qbot_context::RefusalReason;
use qbot_core::{TimerId, UnixMillis};
use qbot_sched::{
    LastRun, Origin, Repeat, Rule, TaskError, TaskService, TimeOfDay, Timer, TimerOutcome,
    TimerState, Weekdays, When, interval_text,
};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

/// Tasks per page in listings. A presentation size, not a limit on tasks.
const PAGE: usize = 5;

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

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RepeatKind {
    Interval,
    Daily,
    Weekly,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    fn code(self) -> &'static str {
        match self {
            Weekday::Mon => "mon",
            Weekday::Tue => "tue",
            Weekday::Wed => "wed",
            Weekday::Thu => "thu",
            Weekday::Fri => "fri",
            Weekday::Sat => "sat",
            Weekday::Sun => "sun",
        }
    }
}

/// The repeat arguments shared by `schedule_task` and `update_task`, flat so each has its own
/// description.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct RepeatArgs {
    pub repeat: Option<RepeatKind>,
    pub repeat_minutes: Option<u64>,
    pub repeat_time: Option<String>,
    pub repeat_weekdays: Option<Vec<Weekday>>,
}

impl RepeatArgs {
    /// The rule the arguments describe; `None` when `repeat` is absent. Each kind takes exactly
    /// its own fields, so a stray field is an error rather than silently ignored.
    fn rule(&self) -> Result<Option<Rule>, ToolError> {
        let invalid = |text| ToolError::InvalidArguments(say(text));
        let (minutes, time, days) = (
            self.repeat_minutes,
            self.repeat_time.as_deref(),
            self.repeat_weekdays.as_deref(),
        );
        let time_of_day = |text: &str| {
            text.parse::<TimeOfDay>().map_err(|()| {
                invalid(Text::TasksRepeatTimeFormat {
                    text: format!("{text:?}"),
                })
            })
        };
        let Some(kind) = self.repeat else {
            if minutes.is_some() || time.is_some() || days.is_some() {
                return Err(invalid(Text::TasksRepeatFields {}));
            }
            return Ok(None);
        };
        Ok(Some(match (kind, minutes, time, days) {
            (RepeatKind::Interval, Some(minutes), None, None) => minutes
                .checked_mul(60)
                .map(Duration::from_secs)
                .and_then(Rule::interval)
                .ok_or_else(|| invalid(Text::TasksRepeatMinutes {}))?,
            (RepeatKind::Interval, ..) => return Err(invalid(Text::TasksRepeatIntervalFields {})),
            (RepeatKind::Daily, None, Some(time), None) => Rule::Daily(time_of_day(time)?),
            (RepeatKind::Daily, ..) => return Err(invalid(Text::TasksRepeatDailyFields {})),
            (RepeatKind::Weekly, None, Some(time), Some(days)) => Rule::Weekly(
                Weekdays::from_codes(days.iter().map(|d| d.code()))
                    .ok_or_else(|| invalid(Text::TasksRepeatWeeklyFields {}))?,
                time_of_day(time)?,
            ),
            (RepeatKind::Weekly, ..) => return Err(invalid(Text::TasksRepeatWeeklyFields {})),
        }))
    }
}

/// The repeat parameters' descriptions; `repeat` itself is described by each tool.
fn repeat_parameters() -> [(&'static str, String); 3] {
    [
        (
            "repeat_minutes",
            say(Text::ScheduleTaskParamRepeatMinutes {}),
        ),
        ("repeat_time", say(Text::ScheduleTaskParamRepeatTime {})),
        (
            "repeat_weekdays",
            say(Text::ScheduleTaskParamRepeatWeekdays {}),
        ),
    ]
}

/// A repeating task is made only on a member's request: in a run a member's message started.
/// A task's own run, or a look at the chat on the bot's own, cannot start an endless series.
fn check_requested(cx: &ToolCx<'_>) -> Result<(), ToolError> {
    match cx.trigger {
        Trigger::Addressed { .. } => Ok(()),
        Trigger::Wake { .. } | Trigger::Spontaneous => Err(ToolError::refused(
            RefusalReason::NotAllowed,
            say(Text::TasksRepeatNotRequested {}),
        )),
    }
}

/// A repeating task is changed or cancelled only on a member's request, as it is started: a
/// task's own run or a spontaneous one must not rewrite or end a series a member asked for. A
/// one-shot task is left to the model as before.
async fn check_target(tasks: &TaskService, cx: &ToolCx<'_>, id: TimerId) -> Result<(), ToolError> {
    if matches!(cx.trigger, Trigger::Addressed { .. }) {
        return Ok(());
    }
    let target = tasks
        .get(cx.group, id)
        .await
        .map_err(|e| map_error(cx, e))?;
    if target.wake().is_some_and(|w| w.repeat.is_some()) {
        return check_requested(cx);
    }
    Ok(())
}

fn rule_text(repeat: &Repeat) -> String {
    let zone = repeat.zone.name().to_owned();
    say(match repeat.rule {
        Rule::Interval(period) => Text::TasksRepeatInterval {
            interval: interval_text(period),
        },
        Rule::Daily(time) => Text::TasksRepeatDaily {
            time: time.to_string(),
            zone,
        },
        Rule::Weekly(days, time) => Text::TasksRepeatWeekly {
            days: days.codes().collect::<Vec<_>>().join(", "),
            time: time.to_string(),
            zone,
        },
    })
}

fn outcome_text(outcome: &TimerOutcome) -> String {
    match outcome {
        TimerOutcome::Ran(end) => end.as_str().to_owned(),
        TimerOutcome::SkippedMuted => say(Text::TasksSkippedMuted {}),
        TimerOutcome::JobOk => "ok".to_owned(),
        TimerOutcome::JobFailed(why) => why.clone(),
    }
}

fn describe(cx: &ToolCx<'_>, timer: &Timer) -> String {
    let state = say(match &timer.state {
        TimerState::Pending => Text::TasksPending {},
        TimerState::Claimed { .. } => Text::TasksRunning {},
        TimerState::Done(_) => Text::TasksDone {},
        TimerState::Cancelled => Text::TasksCancelled {},
        TimerState::Interrupted => Text::TasksInterrupted {},
    });
    let (id, due) = (timer.id.get().to_string(), cx.renderer.time(timer.due_at));
    let Some(spec) = timer.wake() else {
        return say(Text::TasksLine {
            id,
            state,
            due,
            intent: String::new(),
        });
    };
    let Some(repeat) = &spec.repeat else {
        return say(Text::TasksLine {
            id,
            state,
            due,
            intent: spec.intent.clone(),
        });
    };
    let mut line = say(Text::TasksLineRepeating {
        id,
        state,
        rule: rule_text(repeat),
        due,
        intent: spec.intent.clone(),
    });
    if let Some(LastRun { due_at, outcome }) = &spec.last {
        line.push_str("; ");
        line.push_str(&say(Text::TasksLastRun {
            due: cx.renderer.time(*due_at),
            result: outcome_text(outcome),
        }));
    }
    line
}

fn map_error(cx: &ToolCx<'_>, error: TaskError) -> ToolError {
    let invalid = |text| ToolError::InvalidArguments(say(text));
    match error {
        TaskError::EmptyIntent => invalid(Text::TasksEmptyIntent {}),
        TaskError::TooSoon { earliest } => invalid(Text::TasksTooSoon {
            earliest: cx.renderer.time(earliest),
        }),
        TaskError::IntervalTooShort { min } => invalid(Text::TasksIntervalTooShort {
            min: (min.as_secs() / 60).to_string(),
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
    #[serde(flatten)]
    pub repeat: RepeatArgs,
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
        let mut params = vec![
            ("intent", say(Text::ScheduleTaskParamIntent {})),
            ("run_at", say(Text::ScheduleTaskParamRunAt {})),
            ("delay_seconds", say(Text::ScheduleTaskParamDelaySeconds {})),
            ("repeat", say(Text::ScheduleTaskParamRepeat {})),
        ];
        params.extend(repeat_parameters());
        params
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    async fn call(&self, cx: &ToolCx<'_>, args: ScheduleArgs) -> Result<ToolOutput, ToolError> {
        let when = when(args.run_at.as_deref(), args.delay_seconds)?;
        // A task created while handling a task continues that task's chain.
        let parent = match cx.trigger {
            Trigger::Wake { chain, .. } => Some(*chain),
            Trigger::Addressed { .. } | Trigger::Spontaneous => None,
        };
        let created = match args.repeat.rule()? {
            Some(rule) => {
                check_requested(cx)?;
                self.0
                    .create_repeating(cx.group, &args.intent, rule, when, parent, Origin::Model)
                    .await
            }
            None => {
                let when =
                    when.ok_or_else(|| ToolError::InvalidArguments(say(Text::TasksNoTime {})))?;
                self.0
                    .create(cx.group, &args.intent, when, parent, Origin::Model)
                    .await
            }
        };
        let timer = created.map_err(|e| map_error(cx, e))?;
        Ok(ToolOutput::text(say(Text::ScheduleTaskScheduled {
            task: describe(cx, &timer),
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
            .map_err(|e| map_error(cx, e))?;
        if tasks.is_empty() {
            return Ok(ToolOutput::text(say(if page == 0 {
                Text::ListTasksNone {}
            } else {
                Text::ListTasksEmptyPage {}
            })));
        }
        let more = tasks.len() > PAGE;
        let mut lines: Vec<String> = tasks.iter().take(PAGE).map(|t| describe(cx, t)).collect();
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
            .map_err(|e| map_error(cx, e))?;
        Ok(ToolOutput::text(describe(cx, &timer)))
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
        check_target(&self.0, cx, TimerId::new(args.id)).await?;
        let timer = self
            .0
            .cancel(cx.group, TimerId::new(args.id))
            .await
            .map_err(|e| map_error(cx, e))?;
        let task = describe(cx, &timer);
        Ok(ToolOutput::text(say(
            if timer.wake().is_some_and(|w| w.repeat.is_some()) {
                Text::CancelTaskCancelledRepeating { task }
            } else {
                Text::CancelTaskCancelled { task }
            },
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateArgs {
    pub id: u64,
    pub intent: Option<String>,
    pub run_at: Option<String>,
    pub delay_seconds: Option<u64>,
    #[serde(flatten)]
    pub repeat: RepeatArgs,
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
        let mut params = vec![
            ("id", say(Text::UpdateTaskParamId {})),
            ("intent", say(Text::UpdateTaskParamIntent {})),
            ("run_at", say(Text::UpdateTaskParamRunAt {})),
            ("delay_seconds", say(Text::UpdateTaskParamDelaySeconds {})),
            ("repeat", say(Text::UpdateTaskParamRepeat {})),
        ];
        params.extend(repeat_parameters());
        params
    }
    fn effect(&self) -> Effect {
        Effect::Write
    }
    async fn call(&self, cx: &ToolCx<'_>, args: UpdateArgs) -> Result<ToolOutput, ToolError> {
        let when = when(args.run_at.as_deref(), args.delay_seconds)?;
        let rule = args.repeat.rule()?;
        if rule.is_some() {
            check_requested(cx)?;
        }
        check_target(&self.0, cx, TimerId::new(args.id)).await?;
        let timer = self
            .0
            .update(
                cx.group,
                TimerId::new(args.id),
                args.intent.as_deref(),
                when,
                rule,
            )
            .await
            .map_err(|e| map_error(cx, e))?;
        Ok(ToolOutput::text(say(Text::UpdateTaskUpdated {
            task: describe(cx, &timer),
        })))
    }
}
