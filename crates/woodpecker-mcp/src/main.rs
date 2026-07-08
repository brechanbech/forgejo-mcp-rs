//! Entry point for the Woodpecker CI MCP server.
//!
//! Skeleton only. The extraction of the shared scaffolding into [`mcp_core`] is complete; this
//! crate will grow a `Woodpecker` client (Bearer auth, `api/` prefix) and its tool set in a
//! later step. For now it exists so the workspace member list is stable and the shared crate is
//! proven reusable from a second binary.

fn main() {
    mcp_core::init_tracing("woodpecker_mcp=info");
    tracing::info!(target: "woodpecker_mcp.startup", version = env!("CARGO_PKG_VERSION"));
    eprintln!(
        "woodpecker-mcp is a skeleton: the endpoint set and tools are not implemented yet. \
         The shared REST client, write-mode elevation, and helpers live in mcp-core and are \
         ready to build on."
    );
}
