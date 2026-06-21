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

A single binary crate. The tool layer sits over a small, in-house Forgejo REST client
(`src/forge/`) built on `reqwest` — the ~14 endpoints we touch, no third-party forge SDK.

```
forge::Forge   →  in-house REST client (auth, ~14 endpoints, pagination)  ← src/forge/
   ↑
forgejo-mcp-rs  →  #[tool] methods mapping to the calls we use            ← this crate
```

- `src/main.rs` — `#[tokio::main]`; logs to **stderr** (stdout is the MCP stdio transport);
  builds the server from the environment; serves over stdio.
- `src/forge/` — `Forge`, the REST client: base-URL/`api/v1` joining, a zeroized token sent as
  `Authorization: token …`, one `request()` helper, and a typed method per endpoint returning
  raw JSON (`serde_json::Value`). `error.rs` holds `ForgeError` (config / transport / non-2xx
  status / decode), which knows whether a failure is the caller's (4xx) or ours.
- `src/server.rs` — `ForgejoMcp { tool_router, forgejo: Arc<Forge> }`; `from_env()`;
  the `#[tool_router]` block; the `ServerHandler` with instructions.
- `src/tools.rs` — `to_mcp(ForgeError)` error mapping and the tool functions; the server's
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
  file, never logged. The client keeps the token in a `Zeroizing<String>` (wiped on drop) and
  marks the `Authorization` header value sensitive so it stays out of any debug output.
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
| `list_issue_comments` | **done** | Comments on an issue/PR, slimmed. |

Each tool returns the relevant Forgejo API JSON, pretty-printed. Full-resource endpoints pass
the raw response straight through; the slimmed tools (notifications, comments) reshape it.

The list tools accept optional `state` (`open`/`closed`/`all`, on issues and pull requests)
and `page` / `limit` pagination (sent as query parameters). An invalid `state` is rejected
with `invalid_params` before any request is made. Each list tool returns a
`{ page, limit, returned, total, items }` envelope; `total` comes from the endpoint's
`X-Total-Count` header (the client parses it when present), so the caller can tell whether
more pages remain. `search_repos`, `list_orgs`, and `list_notifications` report `total: null`
(those endpoints send no count header).

`list_notifications` returns **slimmed** summaries (`id`, `repo`, `type`, `state`, `title`,
`unread`, `url`, `updated_at`) — the raw threads embed a full repository object each. We
deserialize each thread into a **loose** local struct that keeps the volatile fields (notably
`state`) as plain strings, so a value like a merged-PR notification can't break the parse.
Because we own the response shape, there is no strict-enum gap to work around. Comments are
slimmed the same way.

**Limitations (planned refinements):** sort order and the other query filters (labels,
milestones, author, …) aren't exposed yet, and the per-item output of the passthrough list
tools is the full API object, not a slimmed summary.

### v0.2 — write mode & repo management

Require `FORGEJO_TOKEN_WRITE` + active write mode (see the security model).

| Tool | Status | Purpose |
|---|---|---|
| `write_status` | **done** | Report mode state (read tool; always available). |
| `enable_write_mode` | **done** | Elevate to write mode for a sliding, capped window. |
| `disable_write_mode` | **done** | Return to read-only immediately. |
| `create_repo` | **done** | Create a repo for the authenticated user (defaults to private). |
| `create_issue` | **done** | Create an issue in `owner/repo` (title required, optional body). |
| `comment_on_issue` | **done** | Comment on an issue/PR (`owner/repo/index/body`). |
| `delete_repo` | **done** | Delete a repo (guarded by an exact `owner/repo` `confirm`). |

**Deferred:** `edit_repo` (rename/visibility/archive) — `EditRepoOption` has 20+ no-`Default`
fields and Codeberg renames are unreliable; not yet needed. `comment_on_issue` and other
issue/PR writes are also future work.

## Error handling

`ForgeError`s map to MCP errors in `tools::to_mcp`, keyed off `ForgeError::is_caller_error`:
an HTTP 4xx (bad token, not found, bad request) becomes `invalid_params` (the caller's
problem); config, transport, 5xx, and decode failures become `internal_error`.

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

- Not a full Forgejo SDK — the in-house `forge` client covers only the ~14 endpoints the
  assistant needs, not the whole REST surface.
- **Local git operations are out of scope.** Clients with shell access (Claude Code) already
  run `git` directly; this server is about the *remote* forge API.
- No webhooks, admin, or CI-control tooling in v0.1.

### Tried and dropped: CI status

A `ci_status` ("did my CI pass?") tool was attempted and removed — **Codeberg's API can't
deliver it usefully today**. The combined commit-status endpoint
(`repo_get_combined_status_by_ref`) returns an empty `state: ""` / `total_count: 0` for
Forgejo-Actions repos (Actions don't populate commit statuses), and the Actions-runs
endpoints (`/actions/runs`, `/actions/tasks`) 404 on Codeberg's Forgejo version. So there is
no API that reports a Forgejo-Actions run's pass/fail. Revisit if/when Codeberg exposes the
Actions-runs API. (Aside: that empty `state: ""` is exactly the sort of value a strict typed
client rejects; our loose parsing wouldn't choke on it, but there's still no useful status to
return.)

### Why the in-house client (dropping `forgejo-api`)

Through v0.5, this crate was a thin layer over [`forgejo-api`](https://codeberg.org/Cyborus/forgejo-api).
We hit three real, AI-independent gaps in it: `StateType` has no `merged`, `CommitStatusState`
rejects the empty `state: ""`, and `impl_from_response!` references the `soft_assert` crate
unqualified (a macro-hygiene gap that blocks expansion outside the crate). The upstream issue
reporting these was closed won't-fix — the maintainer doesn't accept AI-tooling-related
contributions, which is their call to make.

Rather than fork and carry patches against a crate whose author would prefer not to be part of
this, we removed the dependency. The tool surface only touches ~14 endpoints, all plain JSON,
and we were already reshaping much of the output into local types — so a small `reqwest`-based
client (`src/forge/`) is a proportionate replacement that we fully own and can audit. The
strict-enum gaps simply don't exist when we define the response shapes ourselves (loose where
it matters). It also shed `soft_assert` and a duplicate `thiserror` from the dependency tree.

## Milestones

1. **v0.1.0** — read-only surface, validated against live Codeberg, tagged. *(done)*
2. **v0.2.0** — write mode + repo management (`create_repo` / `delete_repo`) behind a separate
   write token and deliberate, time-boxed elevation.
3. Later — `edit_repo`, issue/PR writes, slimmed output, sort filters.
