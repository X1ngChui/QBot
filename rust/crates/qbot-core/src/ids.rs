//! Nominal identifier types. Platform ids are parsed once at the protocol boundary and are
//! never interchangeable with each other or with plain integers afterwards.

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("{kind} must be positive, got {value}")]
    NotPositive { kind: &'static str, value: i64 },
    #[error("call id must be non-empty and contain no whitespace")]
    BadCallId,
}

macro_rules! positive_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            pub fn new(value: i64) -> Result<Self, IdError> {
                if value > 0 {
                    Ok(Self(value))
                } else {
                    Err(IdError::NotPositive { kind: $kind, value })
                }
            }

            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let value = i64::deserialize(d)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

positive_id!(
    /// A QQ group number.
    GroupId, "group id"
);
positive_id!(
    /// A QQ account number (a person's login, not the holder entity behind it).
    AccountId, "account id"
);
positive_id!(
    /// A platform message id, stable across the archive and tool arguments.
    MessageId, "message id"
);

/// A per-group member number. Zero is the bot itself; humans are positive and assigned once, on
/// first appearance in the group, and never renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemberNo(u32);

impl MemberNo {
    pub const BOT: MemberNo = MemberNo(0);

    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub const fn is_bot(self) -> bool {
        self.0 == 0
    }
}

/// Identifier of one agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(u64);

impl RunId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Identifier of one durable timer (a group task or a background job).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimerId(u64);

impl TimerId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Identifier shared by a task and every follow-up it schedules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChainId(u64);

impl ChainId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Dense position of an item inside one run's transcript, starting at zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemSeq(u32);

impl ItemSeq {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Identifier pairing a tool call with its result, unique within one transcript.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CallId(String);

impl CallId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() || value.chars().any(char::is_whitespace) {
            return Err(IdError::BadCallId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CallId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_positive_platform_ids() {
        assert!(GroupId::new(0).is_err());
        assert!(AccountId::new(-5).is_err());
        assert_eq!(MessageId::new(7).map(MessageId::get), Ok(7));
    }

    #[test]
    fn deserialization_enforces_the_same_rules() {
        assert!(serde_json::from_str::<GroupId>("0").is_err());
        assert!(serde_json::from_str::<CallId>("\"a b\"").is_err());
        assert!(serde_json::from_str::<CallId>("\"call-1\"").is_ok());
    }

    #[test]
    fn bot_member_number_is_zero() {
        assert!(MemberNo::BOT.is_bot());
        assert!(!MemberNo::new(3).is_bot());
    }
}
