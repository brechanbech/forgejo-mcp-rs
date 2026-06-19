//! Tool implementations — thin wrappers over the `forgejo-api` client.
//!
//! Each function maps a Forgejo API call to a [`CallToolResult`]. The server's `#[tool]`
//! methods in [`crate::server`] delegate here, so that file reads as an index of the
//! surface and the real work lives here. (Promote to a `tools/` directory once it grows.)

use forgejo_api::{ApiErrorKind, Forgejo, ForgejoError};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;

/// Maps a `forgejo-api` error to an MCP error. Forge-level rejections (bad token, missing
/// repo, validation, …) and any other client-side 4xx are the caller's problem
/// (`invalid_params`); transport errors and server-side 5xx are `internal_error`.
// By value so it reads as a point-free `.map_err(to_mcp)`; the body only needs a borrow.
#[allow(clippy::needless_pass_by_value)]
fn to_mcp(err: ForgejoError) -> McpError {
    let caller_error = match &err {
        // Structured API errors are almost all 4xx; only a 5xx hiding in `Other` is internal.
        ForgejoError::ApiError(api) => {
            !matches!(&api.kind, ApiErrorKind::Other(code) if code.is_server_error())
        }
        ForgejoError::UnexpectedStatusCode(code) => code.is_client_error(),
        _ => false,
    };
    if caller_error {
        McpError::invalid_params(err.to_string(), None)
    } else {
        McpError::internal_error(err.to_string(), None)
    }
}

/// Serializes a value as pretty JSON in a successful tool result.
fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Returns the authenticated user — proof the token works.
pub async fn whoami(forgejo: &Forgejo) -> Result<CallToolResult, McpError> {
    let user = forgejo.user_get_current().await.map_err(to_mcp)?;
    json_result(&user)
}
