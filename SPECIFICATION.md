# forgejo-mcp-rs — Specification

This is the source of truth for the project: what it is, how it's built, and what it
exposes. Code and README track this document.

## Purpose

A [Model Context Protocol](https://modelcontextprotocol.io/) server, in Rust, that lets an
MCP client (Claude Code, Claude Desktop, …) inspect a [Forgejo](https://forgejo.org/)
instance — primarily [Codeberg](https://codeberg.org) — over its REST API: the authenticated
user, repositories, issues, and pull requests. **v0.1 is read-only.**

It exists so the assistant can read repo/issue/PR context directly while you work, without
shell-scripting `curl` against the API or trusting a pre-built third-party server with your
token. It is an **independent implementation over the documented Forgejo API**, not a port
of any existing server.

## Architecture

A single binary crate — a thin tool layer over the [`forgejo-api`](https://crates.io/crates/forgejo-api)
crate (a maintained, typed, swagger-generated Forgejo client). No bespoke HTTP code.

```
forgejo-api   →  typed Forgejo client (auth, endpoints, pagination)   ← upstream
   ↑
forgejo-mcp-rs  →  #[tool] methods mapping to the calls we use         ← this crate
```

- `src/main.rs` — `#[tokio::main]`; logs to **stderr** (stdout is the MCP stdio transport);
  builds the server from the environment; serves over stdio.
- `src/server.rs` — `ForgejoMcp { tool_router, forgejo: Arc<Forgejo> }`; `from_env()`;
  the `#[tool_router]` block; the `ServerHandler` with instructions.
- `src/tools.rs` — `to_mcp(ForgejoError)` error mapping and the tool functions; the server's
  `#[tool]` methods delegate here. (Promoted to a `tools/` directory once it grows.)

Built on `rmcp 1.7`. Conventions (lints, CI, pre-push, deny/clippy config) mirror the
sibling `kicad-mcp-rs` project.

## Configuration

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `FORGEJO_TOKEN` | **yes** | — | Forgejo/Codeberg access token (read-only scopes suffice). |
| `FORGEJO_URL` | no | `https://codeberg.org` | Instance base URL. |
| `RUST_LOG` | no | `forgejo_mcp_rs=info` | Tracing filter (logs go to stderr). |

The server refuses to start without `FORGEJO_TOKEN`, with a clear message.

## Security model

- The token is read **from the environment only** — never a CLI argument, never written to
  a file, never logged. `forgejo-api` zeroizes it after building request headers.
- **Read-only first.** v0.1 exposes no write tools, so a leaked or over-shared client cannot
  modify your account. Mint a **read-scoped** token.
- **Untrusted output.** Tool results are repository-derived text (issue/PR titles and bodies,
  repo names, user content). The `ServerHandler` instructions flag this: the client/model
  must treat it as data, never as instructions (indirect prompt-injection defense is the
  client's responsibility; the server simply does not amplify it).
- `unsafe` code is forbidden crate-wide.

## Tool surface

### v0.1 — read-only

| Tool | Status | Purpose |
|---|---|---|
| `whoami` | **done** | The authenticated user (verifies the token). |
| `list_my_repos` | **done** | Repositories owned by the token's user (first page). |
| `list_issues` | **done** | Issues in `owner/repo` (open by default). |
| `get_issue` | **done** | One issue by number. |
| `list_pull_requests` | **done** | Pull requests in `owner/repo` (open by default). |
| `get_pull_request` | **done** | One pull request by number. |
| `search_repos` | **done** | Search repositories by keyword. |

Each tool returns the relevant `forgejo-api` struct(s) serialized as pretty JSON.

The list tools accept optional `state` (`open`/`closed`/`all`, on issues and pull requests)
and `page` / `limit` pagination (via `forgejo-api`'s `Request::page` / `page_size`). An
invalid `state` is rejected with `invalid_params` before any request is made.

**v0.1 limitations (planned refinements):** sort order and the other upstream query filters
(labels, milestones, author, …) aren't exposed yet, and output is the full upstream
struct(s), not slimmed summaries.

### v0.2 — writes (not yet built)

`create_issue`, `comment_on_issue`, and similar. These require write-scoped tokens and are
deliberately deferred until the read surface is validated.

## Error handling

`forgejo-api` errors map to MCP errors in `tools::to_mcp`: an HTTP 4xx (bad token, not found,
bad request) becomes `invalid_params` (the caller's problem); everything else becomes
`internal_error`.

## Concurrency & testing

The server handles concurrent requests (rmcp's default). The read tools share no mutable
state, so parallel calls are safe — there is no per-file serialization concern (unlike a
file-mutating server).

One testing caveat, **not** a server limitation: a slow upstream call (Codeberg's repo
search can take ~6 s) is cut off only if the client closes stdin while the request is still
in flight — on disconnect, rmcp drains in-flight responses for ~5 s, then quits. Real MCP
clients keep the stdio connection open for the whole session, so this affects only ad-hoc
`printf … | forgejo-mcp-rs` testing. When testing that way, keep stdin open (or test through
a real client) so slow responses can return.

## Non-goals

- Not a full Forgejo SDK — only the read surface the assistant needs. `forgejo-api` is the
  SDK; this crate is the MCP adaptor.
- **Local git operations are out of scope.** Clients with shell access (Claude Code) already
  run `git` directly; this server is about the *remote* forge API.
- No webhooks, admin, or CI-control tooling in v0.1.

## Milestones

1. **v0.1.0** — read-only surface (the table above), validated against live Codeberg, tagged.
2. **v0.2.0** — selected write tools behind explicit write-scoped tokens.
