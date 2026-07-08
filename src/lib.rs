//! Library backing the Forgejo and Woodpecker MCP server binaries.
//!
//! Three modules, one crate:
//! - [`mcp_core`] — shared, in-house scaffolding: a thin REST client (token/Bearer auth), the
//!   time-boxed write-mode [`Elevation`](mcp_core::Elevation) gate, pagination, and result
//!   helpers.
//! - [`forgejo`] — the Forgejo / Codeberg MCP server (`forgejo-mcp-rs` binary).
//! - [`woodpecker`] — the companion Woodpecker CI MCP server (`woodpecker-mcp` binary).
//!
//! The two servers are separate binaries — separate processes, tokens, and tool namespaces — that
//! share [`mcp_core`] as internal modules rather than as a published crate.

pub mod forgejo;
pub mod woodpecker;

// Internal shared scaffolding — not part of the public API (the crate exists to provide the two
// server binaries, not a library surface).
pub(crate) mod mcp_core;
