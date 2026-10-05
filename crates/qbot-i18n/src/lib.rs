//! Member-facing text.
//!
//! Everything a group member reads that the bot itself wrote (command replies, notices, reports)
//! is a typed [`Msg`], rendered through a [Fluent](https://projectfluent.org/) catalog. Code never
//! holds user-visible wording: `Msg` names the message and its arguments, and the catalog supplies
//! the words, including plural forms and anything else that depends on the language.
//!
//! - Ids are typed: a message that does not exist in the enum cannot be requested.
//! - Catalogs are validated against the enum when loaded: a missing or unknown message, a syntax
//!   error, or a reference to a variable the code does not supply is a startup error that lists
//!   every problem.
//! - English and Simplified Chinese are built in; a deployment can add or override any locale
//!   with `<locales_dir>/<tag>.ftl`.
//!
//! Text the *model* reads is not here: it lives with the prompts and uses ASCII markers.

mod catalog;
mod messages;

pub use catalog::{Locale, LocaleError, LocaleErrors, Locales};
pub use messages::{Arg, ArgKind, Msg, SCHEMA};
