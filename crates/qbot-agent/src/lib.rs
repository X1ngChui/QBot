//! The agent runtime: the run loop, the tool contract and executor, and the supervisor that
//! admits runs. It depends only on the ports in [`env`], never on a gateway or a database.

pub mod env;
pub mod executor;
pub mod run;
pub mod sim;
pub mod supervisor;
pub mod tool;

pub use env::{
    Archive, ArchiveCursor, ArchivedLine, Chain, ContextSource, Delivered, Delivery, DeliveryError,
    Directory, EnvError, GroupPolicy, HistoryQuery, OpenedContext, OutSegment, Recap, RecapWhen,
    RunLog, RunSummary, TextQuery, Trigger, UsageDetail, UsageEvent, UsageSink,
};
pub use executor::{Executed, RunLimits, execute_turn};
pub use run::{RunDeps, RunInput, RunReport, execute};
pub use supervisor::{
    Rejected, Reservation, RunHandle, Supervisor, SupervisorConfig, TriggerRequest,
};
pub use tool::{
    ChatView, DuplicateTool, Effect, ErasedTool, Participant, RunState, Tool, ToolCx, ToolError,
    ToolOutput, ToolSet,
};
