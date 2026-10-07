//! The composition root: configuration in, a running bot out.

pub mod logs;
mod report_sink;
mod run;

pub use run::{Options, RunError, run};
