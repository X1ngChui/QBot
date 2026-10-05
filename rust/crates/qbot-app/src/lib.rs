//! The composition root: configuration in, a running bot out.

pub mod import;
pub mod logs;
pub mod rebuild;
mod report_sink;
mod run;

pub use run::{Options, RunError, run};
