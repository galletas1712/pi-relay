use std::fmt;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::validate_env_name;

const DEFAULT_CALLBACK_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
const MAX_CALLBACK_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum McpHttpAuthConfig {
    BearerEnv {
        env: String,
    },
    Oauth {
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        scopes: Option<Vec<String>>,
        #[serde(default)]
        resource: Option<String>,
        #[serde(default)]
        callback_port: Option<u16>,
        #[serde(default = "default_callback_timeout_ms")]
        callback_timeout_ms: u64,
    },
}

const fn default_callback_timeout_ms() -> u64 {
    DEFAULT_CALLBACK_TIMEOUT_MS
}

impl McpHttpAuthConfig {
    pub(super) fn validate(&self) -> Result<()> {
        match self {
            Self::BearerEnv { env } => validate_env_name(env),
            Self::Oauth {
                callback_port,
                callback_timeout_ms,
                ..
            } => {
                if callback_port == &Some(0) {
                    bail!("callback_port must be between 1 and 65535");
                }
                if *callback_timeout_ms == 0 || *callback_timeout_ms > MAX_CALLBACK_TIMEOUT_MS {
                    bail!("callback_timeout_ms must be between 1 and {MAX_CALLBACK_TIMEOUT_MS}");
                }
                Ok(())
            }
        }
    }

    pub(crate) fn bearer_env(&self) -> Option<&str> {
        match self {
            Self::BearerEnv { env } => Some(env),
            Self::Oauth { .. } => None,
        }
    }

    pub(crate) fn oauth(&self) -> Option<McpOAuthConfigRef<'_>> {
        match self {
            Self::BearerEnv { .. } => None,
            Self::Oauth {
                client_id,
                scopes,
                resource,
                callback_port,
                callback_timeout_ms,
            } => Some(McpOAuthConfigRef {
                client_id: client_id.as_deref(),
                scopes: scopes.as_deref(),
                resource: resource.as_deref(),
                callback_port: *callback_port,
                callback_timeout_ms: *callback_timeout_ms,
            }),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct McpOAuthConfigRef<'a> {
    pub(crate) client_id: Option<&'a str>,
    pub(crate) scopes: Option<&'a [String]>,
    pub(crate) resource: Option<&'a str>,
    pub(crate) callback_port: Option<u16>,
    pub(crate) callback_timeout_ms: u64,
}

impl fmt::Debug for McpHttpAuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BearerEnv { env } => formatter
                .debug_struct("BearerEnv")
                .field("env", env)
                .finish(),
            Self::Oauth {
                client_id,
                scopes,
                resource,
                callback_port,
                callback_timeout_ms,
            } => formatter
                .debug_struct("Oauth")
                .field("client_id", &client_id.as_ref().map(|_| "<redacted>"))
                .field(
                    "scope_count",
                    &scopes.as_ref().map(std::vec::Vec::len).unwrap_or_default(),
                )
                .field("resource", &resource.as_ref().map(|_| "<redacted>"))
                .field("callback_port", callback_port)
                .field("callback_timeout_ms", callback_timeout_ms)
                .finish(),
        }
    }
}
