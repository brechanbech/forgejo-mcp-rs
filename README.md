# forgejo-mcp-rs workspace

[![CI](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions/workflows/ci.yml/badge.svg)](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions)

A Cargo workspace of [Model Context Protocol](https://modelcontextprotocol.io/) servers for
self-hosted forge infrastructure, sharing one small in-house REST/MCP core. Each server is an
independent binary — load whichever you need in your MCP client; they share no runtime state.

## Crates

| Crate | Kind | What it is |
|---|---|---|
| [`forgejo-mcp`](crates/forgejo-mcp) | binary (`forgejo-mcp-rs`) | MCP server for **Forgejo / Codeberg** — user, repos, issues, pull requests, search, notifications, reviews, and Actions (CI), with opt-in guarded writes and push-mirror management. See its [README](crates/forgejo-mcp/README.md). |
| [`woodpecker-mcp`](crates/woodpecker-mcp) | binary (`woodpecker-mcp`) | MCP server for **Woodpecker CI** — repos and pipelines, with guarded trigger/cancel/restart. |
| [`mcp-core`](crates/mcp-core) | library (`forgejo-mcp-core`) | Shared scaffolding: a thin `RestClient` (Bearer/token auth), the time-boxed write-mode `Elevation` gate, pagination, and result helpers. |

## Build & install

```sh
cargo build --release                        # all binaries under target/release/
cargo install --path crates/forgejo-mcp      # installs `forgejo-mcp-rs` to ~/.cargo/bin
cargo install --path crates/woodpecker-mcp   # installs `woodpecker-mcp`
```

Each server is configured entirely by environment variables (`FORGEJO_*` / `WOODPECKER_*`) and
speaks the MCP stdio transport — see each crate's README for the variables and the client wiring.

## Quality checks

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check          # cargo install cargo-deny
```

CI runs the same on [Codeberg Forgejo Actions](.forgejo/workflows/ci.yml); a tracked
[`.githooks/pre-push`](.githooks/pre-push) hook mirrors it locally
(`git config core.hooksPath .githooks`).

## License

MIT — see [LICENSE.md](LICENSE.md) for details.
