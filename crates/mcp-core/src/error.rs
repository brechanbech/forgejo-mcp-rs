//! Error type for the in-house REST client shared by the forge servers.

use std::fmt;

/// An error from a REST call.
#[derive(Debug)]
pub enum ApiError {
    /// Building the client or a request URL failed (bad base URL, TLS backend, …).
    Config(String),
    /// The HTTP request never completed (DNS, connect, TLS, timeout). Our problem, not the
    /// caller's.
    Transport(reqwest::Error),
    /// The API returned a non-success status. `body` is the (possibly JSON) error payload,
    /// surfaced verbatim because the server's messages are useful ("token does not have …").
    Status {
        code: reqwest::StatusCode,
        body: String,
    },
    /// A 2xx response body could not be deserialized into the expected shape.
    Decode(serde_json::Error),
}

impl ApiError {
    /// Whether this is the caller's fault (a 4xx) rather than ours (config / transport / 5xx /
    /// decode). Drives the MCP error mapping in [`crate::to_mcp`].
    #[must_use]
    pub fn is_caller_error(&self) -> bool {
        matches!(self, ApiError::Status { code, .. } if code.is_client_error())
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Config(msg) => write!(f, "{msg}"),
            ApiError::Transport(e) => write!(f, "request failed: {e}"),
            ApiError::Status { code, body } => {
                let reason = code.canonical_reason().unwrap_or("unknown");
                if body.is_empty() {
                    write!(f, "the server returned {} {reason}", code.as_u16())
                } else {
                    write!(f, "the server returned {} {reason}: {body}", code.as_u16())
                }
            }
            ApiError::Decode(e) => write!(f, "decoding the response failed: {e}"),
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ApiError::Transport(e) => Some(e),
            ApiError::Decode(e) => Some(e),
            ApiError::Config(_) | ApiError::Status { .. } => None,
        }
    }
}
