//! Entry point for the Woodpecker CI MCP server.
//!
//! Initializes structured logging to stderr (stdout is reserved for the MCP stdio transport),
//! builds the server from the environment, and serves until the client disconnects.

mod client;
mod server;
mod tools;

use anyhow::Context as _;
use rmcp::{ServiceExt as _, transport::stdio};

use crate::server::WoodpeckerMcp;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    mcp_core::init_tracing("woodpecker_mcp=info");

    tracing::info!(target: "woodpecker_mcp.startup", version = env!("CARGO_PKG_VERSION"));

    // stdio transport: JSON-RPC frames over stdin/stdout, so all logs MUST go to stderr.
    let service = WoodpeckerMcp::from_env()?
        .serve(stdio())
        .await
        .context("failed to start MCP service on stdio")?;

    service
        .waiting()
        .await
        .context("MCP service ended with error")?;
    Ok(())
}
