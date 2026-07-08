//! Binary entry point for the Woodpecker CI MCP server.
//!
//! A thin wrapper: the server construction and stdio serve loop live in
//! [`forgejo_mcp_rs::woodpecker::serve`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    forgejo_mcp_rs::woodpecker::serve().await
}
