//! How a command ends other than by succeeding.

use qbot_i18n::Msg;
use qbot_memory::{AliasTextError, IdentityError, LinkError, MAX_ALIAS_CHARS};
use qbot_sched::TaskError;

use crate::ports::AdminError;

#[derive(Debug)]
pub enum Fail {
    /// A reply that is the answer: a usage error, a refusal, "nothing there".
    Say(Msg),
    /// A storage problem; whether the action happened is not known.
    Storage(String),
}

impl From<Msg> for Fail {
    fn from(msg: Msg) -> Self {
        Fail::Say(msg)
    }
}

impl From<AdminError> for Fail {
    fn from(error: AdminError) -> Self {
        Fail::Storage(error.0)
    }
}

pub fn link_msg(error: &LinkError) -> Msg {
    match error {
        LinkError::SelfLink => Msg::LinkSelf {},
        LinkError::AlreadyLinked => Msg::LinkAlready {},
        LinkError::Busy => Msg::LinkBusy {},
        LinkError::NoInvitation => Msg::LinkNoInvitation {},
        LinkError::Expired => Msg::LinkExpired {},
        LinkError::NotTheTarget => Msg::LinkNotTarget {},
        LinkError::OutOfOrder => Msg::LinkOutOfOrder {},
        LinkError::Stale => Msg::LinkStale {},
    }
}

impl From<IdentityError> for Fail {
    fn from(error: IdentityError) -> Self {
        match error {
            IdentityError::Name(AliasTextError::Empty) => Fail::Say(Msg::NameEmptyName {}),
            IdentityError::Name(AliasTextError::TooLong) => Fail::Say(Msg::NameTooLong {
                max: u32::try_from(MAX_ALIAS_CHARS).unwrap_or(u32::MAX),
            }),
            IdentityError::NameTaken => Fail::Say(Msg::NameTaken {
                name: String::new(),
            }),
            IdentityError::NotLinked => Fail::Say(Msg::NotLinked {}),
            IdentityError::UnknownAccount => Fail::Say(Msg::UnknownAccount {}),
            IdentityError::Link(link) => Fail::Say(link_msg(&link)),
            IdentityError::Backend(message) => Fail::Storage(message),
        }
    }
}

/// Task errors with the time formatted by the caller (it knows the zone).
pub fn task_fail(error: TaskError, earliest: impl Fn(qbot_core::UnixMillis) -> String) -> Fail {
    match error {
        TaskError::EmptyIntent => Fail::Say(Msg::TasksEmptyIntent {}),
        TaskError::TooSoon { earliest: at } => Fail::Say(Msg::TasksTooSoon {
            earliest: earliest(at),
        }),
        TaskError::TooManyPending { limit } => Fail::Say(Msg::TasksTooMany {
            limit: u64::try_from(limit).unwrap_or(u64::MAX),
        }),
        TaskError::ChainTooDeep { limit } => Fail::Say(Msg::TasksChainDeep { limit }),
        TaskError::NotFound | TaskError::NotPending => Fail::Say(Msg::TasksNotFound {}),
        TaskError::NothingToChange => Fail::Say(Msg::TasksNothingToChange {}),
        TaskError::Storage(message) => Fail::Storage(message),
    }
}
