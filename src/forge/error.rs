//! Error type for the in-house Forgejo REST client.

use std::fmt;

/// An error from a Forgejo REST call.
#[derive(Debug)]
pub enum ForgeError {
    /// Building the client or a request URL failed (bad base URL, TLS backend, …).
    Config(String),
    /// The HTTP request never completed (DNS, connect, TLS, timeout). Our problem, not the
    /// caller's.
    Transport(reqwest::Error),
    /// The API returned a non-success status. `body` is the (possibly JSON) error payload,
    /// surfaced verbatim because Forgejo's messages are useful ("token does not have …").
    Status {
        code: reqwest::StatusCode,
        body: String,
    },
    /// A 2xx response body could not be deserialized into the expected shape.
    Decode(serde_json::Error),
}

impl ForgeError {
    /// Whether this is the caller's fault (a 4xx) rather than ours (config / transport / 5xx /
    /// decode). Drives the MCP error mapping in [`crate::tools`].
    #[must_use]
    pub fn is_caller_error(&self) -> bool {
        matches!(self, ForgeError::Status { code, .. } if code.is_client_error())
    }
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::Config(msg) => write!(f, "{msg}"),
            ForgeError::Transport(e) => write!(f, "request failed: {e}"),
            ForgeError::Status { code, body } => {
                let reason = code.canonical_reason().unwrap_or("unknown");
                if body.is_empty() {
                    write!(f, "Forgejo returned {} {reason}", code.as_u16())
                } else {
                    write!(f, "Forgejo returned {} {reason}: {body}", code.as_u16())
                }
            }
            ForgeError::Decode(e) => write!(f, "decoding the response failed: {e}"),
        }
    }
}

impl std::error::Error for ForgeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ForgeError::Transport(e) => Some(e),
            ForgeError::Decode(e) => Some(e),
            ForgeError::Config(_) | ForgeError::Status { .. } => None,
        }
    }
}
