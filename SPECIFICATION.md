# forgejo-mcp-rs — Specification

This is the source of truth for the project: what it is, how it's built, and what it
exposes. Code and README track this document.

## Purpose

A [Model Context Protocol](https://modelcontextprotocol.io/) server, in Rust, that lets an
MCP client (Claude Code, Claude Desktop, …) inspect a [Forgejo](https://forgejo.org/)
instance — primarily [Codeberg](https://codeberg.org) — over its REST API: the authenticated
user, repositories, issues, and pull requests. **Read-only by default**; repository writes
(create/delete) are available only via a separate write token and a deliberate, time-boxed
write mode (v0.2).

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
| `FORGEJO_TOKEN_READ_ONLY` | **yes**\* | — | Read token (read-only scopes suffice). `FORGEJO_TOKEN` is accepted as a fallback. |
| `FORGEJO_TOKEN_WRITE` | no | — | Write/delete-scoped token. **Its presence enables the write tools**; absent ⇒ the server is read-only. |
| `FORGEJO_WRITE_MINUTES` | no | `10` | Default write-mode window, clamped to `1..=60`. |
| `FORGEJO_URL` | no | `https://codeberg.org` | Instance base URL. |
| `RUST_LOG` | no | `forgejo_mcp_rs=info` | Tracing filter (logs go to stderr). |

\* A read token is required (under either name); the server refuses to start without one. A
**write token alone is refused** — reads must use a dedicated read-only token, even though a
write token could technically read — and the read token **must differ** from
`FORGEJO_TOKEN_WRITE` (no reusing the write token in the read slot). The server can't verify a
token's *scope* without probing, so this is a structural guard (presence + distinctness), not
a scope check.

## Security model

- Tokens are read **from the environment only** — never a CLI argument, never written to a
  file, never logged. `forgejo-api` zeroizes them after building request headers.
- **Read-only by default.** Reads use `FORGEJO_TOKEN`. Writes use a *separate*
  `FORGEJO_TOKEN_WRITE`; if it isn't configured, the write tools refuse permanently — so
  *providing the second token is the opt-in* to any destructive capability.
- **Untrusted output.** Tool results are repository-derived text (issue/PR titles and bodies,
  repo names, user content). The `ServerHandler` instructions flag this: the client/model
  must treat it as data, never as instructions (indirect prompt-injection defense is the
  client's responsibility; the server simply does not amplify it).
- `unsafe` code is forbidden crate-wide.

### Write mode (deliberate, time-boxed elevation)

Even with a write token present, the server **starts read-only** and writes are refused until
the model deliberately elevates — "sudo with a timeout":

- `enable_write_mode(minutes?)` activates write mode for `minutes` (default `FORGEJO_WRITE_MINUTES`,
  **hard-capped at 60** — there is deliberately no permanent mode). `disable_write_mode` ends it.
- The window **slides**: each successful write extends it by the same length; otherwise it
  **auto-reverts** to read-only. Expiry is checked lazily on each call (no background timer).
- Write tools (`create_repo`, `delete_repo`) refuse with `invalid_params` if the write token
  is absent or write mode is inactive. `delete_repo` additionally requires a `confirm` argument
  exactly equal to `"owner/repo"`.
- It is **loud**: `write_status` reports the state anytime, every write result notes the
  remaining window, and the instructions tell the model to announce elevation and actions.

**Honest scope:** both tokens live in the process's memory throughout, so this is a
*deliberate-action guardrail* (like a `sudo` timestamp), **not a sandbox** — a fully
compromised process would still hold both tokens. A true boundary would need a separate
token-broker process, which is out of scope.

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
| `list_orgs` | **done** | Organizations the user belongs to. |
| `list_notifications` | **done** | Notification threads, slimmed (`all` includes read). |

Each tool returns the relevant `forgejo-api` struct(s) serialized as pretty JSON.

The list tools accept optional `state` (`open`/`closed`/`all`, on issues and pull requests)
and `page` / `limit` pagination (via `forgejo-api`'s `Request::page` / `page_size`). An
invalid `state` is rejected with `invalid_params` before any request is made. Each list tool
returns a `{ page, limit, returned, total, items }` envelope (the `total` comes from the
endpoint's count header — `CountHeader`), so the caller can tell whether more pages remain.
`search_repos` reports `total: null` (its `SearchResults` carries no count).

`list_notifications` returns **slimmed** summaries (`id`, `repo`, `type`, `state`, `title`,
`unread`, `url`, `updated_at`) — the raw threads embed a full repository object each. It also
deserializes into a **loose** local type via `forgejo-api`'s `response_type`, because the
crate's strict `StateType` enum lacks `merged`, which would otherwise fail any page
containing a merged-PR notification. (`total: null` there too, since that swaps out the count
header.)

**Limitations (planned refinements):** sort order and the other upstream query filters
(labels, milestones, author, …) aren't exposed yet, and the per-item output is the full
upstream struct, not a slimmed summary.

### v0.2 — write mode & repo management

Require `FORGEJO_TOKEN_WRITE` + active write mode (see the security model).

| Tool | Status | Purpose |
|---|---|---|
| `write_status` | **done** | Report mode state (read tool; always available). |
| `enable_write_mode` | **done** | Elevate to write mode for a sliding, capped window. |
| `disable_write_mode` | **done** | Return to read-only immediately. |
| `create_repo` | **done** | Create a repo for the authenticated user (defaults to private). |
| `create_issue` | **done** | Create an issue in `owner/repo` (title required, optional body). |
| `delete_repo` | **done** | Delete a repo (guarded by an exact `owner/repo` `confirm`). |

**Deferred:** `edit_repo` (rename/visibility/archive) — `EditRepoOption` has 20+ no-`Default`
fields and Codeberg renames are unreliable; not yet needed. `comment_on_issue` and other
issue/PR writes are also future work.

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

1. **v0.1.0** — read-only surface, validated against live Codeberg, tagged. *(done)*
2. **v0.2.0** — write mode + repo management (`create_repo` / `delete_repo`) behind a separate
   write token and deliberate, time-boxed elevation.
3. Later — `edit_repo`, issue/PR writes, slimmed output, sort filters.
