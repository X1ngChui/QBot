//! The concrete tools the model can call, built on the tool contract in `qbot_agent` and the
//! task service in `qbot_sched`.

mod memory;
mod people;
mod query;
mod search;
mod send;
mod tasks;
mod web;

use std::sync::Arc;

use qbot_agent::{Archive, Delivery, DuplicateTool, ToolSet};
use qbot_sched::TaskService;

pub use memory::{ReadEpisode, ReadEpisodeArgs, RecallArgs, RecallEpisodes, add_memory_tools};
pub use people::{LookupArgs, LookupMember};
pub use query::parse_query;
pub use search::{SearchArgs, SearchHistory, SearchSettings};
pub use send::{SendArgs, SendMessage, StaySilent, StaySilentArgs};
pub use tasks::{
    CancelScheduledTask, GetScheduledTask, ListArgs, ListScheduledTasks, ScheduleArgs,
    ScheduleTask, TaskRef, UpdateArgs, UpdateScheduledTask,
};
pub use web::{READ_URL_MAX_CHARS, ReadUrl, ReadUrlArgs, WebSearchArgs, WebSearchTool};

/// Deployment settings of the standard tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSettings {
    /// Messages one run may send: a product rule against flooding the group.
    pub max_sends_per_run: u32,
    pub search: SearchSettings,
}

/// The tool set of a normal run. Fixed for the whole run.
pub fn standard_tools(
    delivery: Arc<dyn Delivery>,
    archive: Arc<dyn Archive>,
    tasks: TaskService,
    settings: ToolSettings,
) -> Result<ToolSet, DuplicateTool> {
    ToolSet::new()
        .with(SendMessage::new(delivery, settings.max_sends_per_run))?
        .with(StaySilent)?
        .with(SearchHistory::new(archive, settings.search))?
        .with(ScheduleTask(tasks.clone()))?
        .with(ListScheduledTasks(tasks.clone()))?
        .with(GetScheduledTask(tasks.clone()))?
        .with(UpdateScheduledTask(tasks.clone()))?
        .with(CancelScheduledTask(tasks))
}
