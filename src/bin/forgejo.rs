//! Binary entry point for the Forgejo / Codeberg MCP server.
//!
//! A thin wrapper: the server construction and stdio serve loop live in
//! [`forgejo_mcp_rs::forgejo::serve`].

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    forgejo_mcp_rs::forgejo::serve().await
}
