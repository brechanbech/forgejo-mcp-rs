//! The Forgejo / Codeberg MCP server.
//!
//! [`server::ForgejoMcp`] holds the API client and registers the tools; [`client`] is the thin
//! REST client and [`tools`] the tool implementations. [`serve`] wires it to the stdio transport.

pub(crate) mod client;
pub(crate) mod server;
pub(crate) mod tools;

use anyhow::Context as _;
use rmcp::{ServiceExt as _, transport::stdio};

use server::ForgejoMcp;

/// Runs the Forgejo MCP server on the stdio transport until the client disconnects.
///
/// Initializes structured logging to stderr (stdout is reserved for the MCP stdio transport),
/// builds the server from the environment, and serves.
///
/// # Errors
/// Fails if the server can't be built from the environment or the MCP service ends with an error.
pub async fn serve() -> anyhow::Result<()> {
    crate::mcp_core::init_tracing("forgejo_mcp_rs=info");

    tracing::info!(target: "forgejo_mcp.startup", version = env!("CARGO_PKG_VERSION"));

    // stdio transport: JSON-RPC frames over stdin/stdout, so all logs MUST go to stderr.
    let service = ForgejoMcp::from_env()?
        .serve(stdio())
        .await
        .context("failed to start MCP service on stdio")?;

    service
        .waiting()
        .await
        .context("MCP service ended with error")?;
    Ok(())
}
