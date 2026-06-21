# forgejo-mcp-rs

[![CI](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions/workflows/ci.yml/badge.svg)](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions)

A [Model Context Protocol](https://modelcontextprotocol.io/) server for
[Forgejo](https://forgejo.org/) / [Codeberg](https://codeberg.org), written in Rust. It lets
an MCP client (Claude Code, Claude Desktop, …) read your forge — the authenticated user,
repositories, issues, and pull requests — over the Forgejo REST API.

> Status: **v0.1.0, read-only.** Seven read tools — the authenticated user, repositories,
> issues, pull requests, and repository search (with `state`/pagination filters) — over the
> Forgejo REST API. See [`SPECIFICATION.md`](SPECIFICATION.md) for the full plan; writes
> land in v0.2.

It's a thin tool layer over the [`forgejo-api`](https://crates.io/crates/forgejo-api) crate
(a maintained, typed Forgejo client) — an **independent implementation over the documented
API**, not a port of any other server. Building our own means the tool surface holding your
token is code you can read and audit.

## Build

```sh
cargo build --release      # binary at target/release/forgejo-mcp-rs
```

## Configure

The server is configured by environment variables:

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `FORGEJO_TOKEN` | **yes** | — | Forgejo/Codeberg access token. **Read-only scopes are enough.** |
| `FORGEJO_URL` | no | `https://codeberg.org` | Instance base URL. |

Mint a token at **Codeberg → Settings → Applications** (or your instance's equivalent). For
the current read-only tools, read scopes such as `read:repository`, `read:issue`, and
`read:user` suffice.

### Wire it into Claude Code

```sh
claude mcp add --scope user forgejo /path/to/target/release/forgejo-mcp-rs \
  --env FORGEJO_URL=https://codeberg.org \
  --env FORGEJO_TOKEN=your_token_here
```

### Or Claude Desktop

```json
{
  "mcpServers": {
    "forgejo": {
      "command": "/path/to/target/release/forgejo-mcp-rs",
      "env": { "FORGEJO_URL": "https://codeberg.org", "FORGEJO_TOKEN": "your_token_here" }
    }
  }
}
```

Logs go to **stderr** (stdout is the MCP transport); control verbosity with `RUST_LOG`, e.g.
`RUST_LOG=forgejo_mcp_rs=debug`.

## Tools

| Tool | Status | Notes |
|---|---|---|
| `whoami` | ✅ | The authenticated user (verifies the token) |
| `list_my_repos` | ✅ | Your repositories (first page) |
| `list_issues` / `get_issue` | ✅ | Issues in `owner/repo` (open by default) |
| `list_pull_requests` / `get_pull_request` | ✅ | Pull requests in `owner/repo` (open by default) |
| `search_repos` | ✅ | Repository search by keyword |

The list tools accept optional `state` (`open`/`closed`/`all`, on issues/PRs) and
`page`/`limit` pagination; sort order and other upstream filters aren't exposed yet. Write
tools (create issue / comment) are deferred to v0.2 — see the
[specification](SPECIFICATION.md).

## Security

The token is read from the environment only — never logged, never written to disk
(`forgejo-api` zeroizes it). v0.1 is read-only, so the server cannot modify your account.
Tool output is untrusted, repo-derived text — the server flags it as data, not instructions.
See [`SPECIFICATION.md`](SPECIFICATION.md#security-model).

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

MIT — see [`LICENSE`](LICENSE).
