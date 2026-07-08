//! Shared scaffolding for the forge MCP servers.
//!
//! This crate holds everything the Forgejo and Woodpecker servers have in common, so each
//! server crate is reduced to its endpoint set and tool definitions:
//!
//! - [`RestClient`] — a thin REST-over-`reqwest` client, parameterized by [`Auth`] scheme and
//!   API prefix ([`RestConfig`]).
//! - [`ApiError`] — the client's error type, with [`ApiError::is_caller_error`] driving MCP
//!   error mapping.
//! - [`Elevation`] — time-boxed, sliding write-mode elevation, generic over the write client.
//! - result/pagination helpers: [`json_result`], [`to_mcp`], [`decode`], [`paged_result`],
//!   [`gather_all`], …
//! - [`init_tracing`] — stderr structured logging (stdout is the MCP stdio transport).

mod elevation;
mod error;
mod helpers;
mod rest;

pub use elevation::Elevation;
pub use error::ApiError;
pub use helpers::{
    Gathered, PageFetch, decode, gather_all, gathered_result, into_items, json_result,
    paged_result, to_mcp,
};
pub use rest::{Auth, RestClient, RestConfig, paging};

use tracing_subscriber::EnvFilter;

/// Configures `tracing` to write structured logs to stderr, honoring `RUST_LOG`.
///
/// `default_directive` is the `EnvFilter` used when `RUST_LOG` is unset, e.g.
/// `"forgejo_mcp_rs=info"`. Logs MUST go to stderr because stdout carries the MCP stdio
/// transport's JSON-RPC frames.
pub fn init_tracing(default_directive: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
