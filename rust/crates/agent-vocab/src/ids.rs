use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct TurnId(pub u64);

impl TurnId {
    pub fn first() -> Self {
        Self(1)
    }

    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ActionId(pub u64);

impl ActionId {
    pub fn first() -> Self {
        Self(1)
    }

    pub fn take_next(next: &mut Self) -> Self {
        let current = *next;
        next.0 += 1;
        current
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

impl ToolCallId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn first() -> Self {
        Self("1".to_string())
    }

    pub fn from_u64(id: u64) -> Self {
        Self(id.to_string())
    }

    pub fn take_next(next: &mut Self) -> Self {
        let current = next.clone();
        let parsed = next.0.parse::<u64>().unwrap_or(0);
        next.0 = (parsed + 1).to_string();
        current
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ToolCallId {
    fn default() -> Self {
        Self::first()
    }
}

impl From<u64> for ToolCallId {
    fn from(value: u64) -> Self {
        Self::from_u64(value)
    }
}

impl From<&str> for ToolCallId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ToolCallId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
