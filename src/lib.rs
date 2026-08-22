//! Library backing the `forgejo-mcp-rs` MCP server binary.
//!
//! Two modules, one crate:
//! - [`mcp_core`] — internal scaffolding: a thin REST client (token auth), the time-boxed
//!   write-mode [`Elevation`](mcp_core::Elevation) gate, pagination, and result helpers.
//! - [`forgejo`] — the Forgejo / Codeberg MCP server itself.
//!
//! The binary in `src/bin/forgejo.rs` is a thin `#[tokio::main]` wrapper over
//! [`forgejo::serve`]; the substance lives here so `cargo test` can reach it.
//!
//! A companion Woodpecker CI server shipped alongside this one, as a second binary, from v0.13.0
//! through v0.17.0. It now lives in its own repository and crate:
//! <https://codeberg.org/brechanbech/woodpecker-mcp>.

pub mod forgejo;

// Internal shared scaffolding — not part of the public API (the crate exists to provide the
// server binary, not a library surface).
pub(crate) mod mcp_core;
