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

A **companion Woodpecker CI server** (`woodpecker-mcp`) ships in the same crate for the common
self-hosted arrangement where Woodpecker runs alongside Forgejo: it inspects repositories and
pipelines and, under the same time-boxed write mode, triggers / cancels / restarts them. It is a
second binary — separate process, separate token, separate tool namespace — reusing the shared
core, not a Forgejo feature. The two are decoupled at the process level but bundled in one crate
because, in practice, they are deployed together (see the collapse rationale in Milestones).
**Either binary runs standalone**: `woodpecker-mcp` has no runtime dependency on the Forgejo server,
and `cargo install --bin <name>` installs just one — the bundling is packaging, not coupling.

## Architecture

One crate, `forgejo-mcp-rs`, providing **two MCP server binaries** that share an in-house
REST/MCP core as internal modules. The Forgejo server is the primary; the Woodpecker server is a
companion for instances that run [Woodpecker CI](https://woodpecker-ci.org/) alongside Forgejo.
They run as **separate processes with separate tokens and tool namespaces** — not one server with
two toolsets — so a token for one never sits in the other's process.

```
mcp_core    →  shared transport: RestClient (Auth::{Token,Bearer} + api-prefix),   ← src/mcp_core/
               ApiError, the Elevation<C> write-mode gate, pagination & helpers
   ↑
forgejo     →  Forge(RestClient) + #[tool] methods  →  forgejo-mcp-rs binary        ← src/forgejo/
woodpecker  →  Woodpecker(RestClient) + #[tool] methods  →  woodpecker-mcp binary   ← src/woodpecker/
```

- `src/lib.rs` — declares the three modules. `mcp_core` is `pub(crate)`: it is internal
  scaffolding, not a public library surface — the crate exists to provide the two binaries.
- `src/mcp_core/` — the shared, forge-agnostic core, built on `reqwest`:
  - `rest.rs` — `RestClient`: base-URL/api-prefix joining, a zeroized token presented per an
    `Auth` scheme (`token …` for Forgejo, `Bearer …` for Woodpecker), one `request()` helper, and
    `get` / `get_list` / `post` / `post_none` / `post_empty` / `delete` verbs returning raw JSON
    (`serde_json::Value`).
  - `error.rs` — `ApiError` (config / transport / non-2xx status / decode), which knows whether a
    failure is the caller's (4xx) or ours.
  - `elevation.rs` — `Elevation<C>`, the generic time-boxed write-mode gate (see Security model),
    reused by both servers over their respective write clients.
  - `helpers.rs` — `to_mcp(ApiError)` mapping, `json_result`, the auto-paginator (`gather_all`),
    and the paged/gathered result envelopes.
  - `mod.rs` — re-exports and `init_tracing`.
- `src/forgejo/` — the Forgejo server: `client.rs` (`Forge`, the `api/v1/` + `Authorization: token`
  endpoint set), `tools.rs` (tool functions), `server.rs` (`ForgejoMcp { tool_router, forgejo,
  elevation, mirror_token }`, its `#[tool_router]`, and the `ServerHandler`), and `mod.rs::serve()`
  (the stdio entry point).
- `src/woodpecker/` — the Woodpecker server, same shape: `client.rs` (`Woodpecker`, `api/` +
  `Authorization: Bearer`, repos keyed by numeric `repo_id` with a `lookup/{owner}/{name}`
  resolver), `tools.rs`, `server.rs` (`WoodpeckerMcp`), and `mod.rs::serve()`.
- `src/bin/{forgejo,woodpecker}.rs` — thin `#[tokio::main]` wrappers that call the respective
  module `serve()`. Logs go to **stderr** (stdout is the MCP stdio transport).

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

The **Woodpecker server** (`woodpecker-mcp`) takes the analogous variables — same read/write
discipline, different names:

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `WOODPECKER_URL` | **yes** | — | Instance base URL — e.g. `https://ci.codeberg.org` (Codeberg's hosted Woodpecker) or a self-hosted one. No default. |
| `WOODPECKER_TOKEN_READ_ONLY` | **yes** | — | Personal access token. `WOODPECKER_TOKEN` is accepted as a fallback. |
| `WOODPECKER_TOKEN_WRITE` | no | — | A second, *different* token; its presence enables the pipeline write tools (see the token-model caveat in v0.13). |
| `WOODPECKER_WRITE_MINUTES` | no | `10` | Default write-mode window, clamped to `1..=60`. |

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

This mechanism is the generic `Elevation<C>` gate in `mcp_core`; **both servers use it** — the
Forgejo write tools and the Woodpecker pipeline-action tools are gated identically (the examples
below are Forgejo's). Even with a write token present, a server **starts read-only** and writes
are refused until the model deliberately elevates — "sudo with a timeout":

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
| `list_my_repos` | **done** | Repositories owned by the token's user (auto-paginated, slimmed). |
| `get_repo` | **done** | One repository's details (incl. default branch and size in KiB), slimmed. |
| `list_branches` | **done** | Branches in `owner/repo` (auto-paginated, slimmed to name/commit/protected). |
| `get_file_contents` | **done** | Read a file (decodes text) or list a directory (`owner/repo/path`, optional `ref`). |
| `list_issues` | **done** | Issues in `owner/repo` (open by default). |
| `get_issue` | **done** | One issue by number. |
| `list_pull_requests` | **done** | Pull requests in `owner/repo` (open by default). |
| `get_pull_request` | **done** | One pull request by number. |
| `search_repos` | **done** | Search repositories by keyword. |
| `list_orgs` | **done** | Organizations the user belongs to. |
| `list_notifications` | **done** | Notification threads, slimmed (`all` includes read). |
| `list_issue_comments` | **done** | Comments on an issue/PR, slimmed. |
| `list_workflow_runs` | **done** | Actions runs in `owner/repo`, slimmed; filter by `head_sha`/`ref`/`status`/`event`/`workflow_id`. |
| `get_workflow_run` | **done** | One Actions run by id (a run's outcome is its `status`). |
| `dispatch_workflow` | **done** | Write-mode. Trigger a `workflow_dispatch` run by workflow file name. |

Each tool returns the relevant Forgejo API JSON, pretty-printed. Full-resource endpoints pass
the raw response straight through; the slimmed tools (repositories, branches, notifications,
comments, workflow runs) reshape it into a compact local struct.

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

(The per-version tables above cover the original v0.1/v0.2 surface; the intervening tools —
`create_branch`, `create_pull_request`, `list_pull_request_reviews`, and the push-mirror set —
landed across v0.3–v0.11 and are listed in the README's current tool table rather than here.)

### v0.12 — Forgejo Actions (CI)

| Tool | Status | Purpose |
|---|---|---|
| `list_workflow_runs` | **done** | Runs in `owner/repo`, slimmed; filter by `head_sha`/`ref`/`status`/`event`/`workflow_id`. A run's outcome is its `status` (no separate `conclusion`). |
| `get_workflow_run` | **done** | One run by internal `id` (not `index_in_repo`, despite the web URL). |
| `dispatch_workflow` | **done** | Write-mode. Trigger a `workflow_dispatch` run, keyed by workflow file name (no list-workflows API — discover via `get_file_contents`). |

`list_workflow_runs` unwraps the endpoint's `{ workflow_runs, total_count }` body (no
`X-Total-Count` header), so it does not auto-paginate. A `404` from these endpoints means the
repo has the Actions unit disabled, not that there are no runs. Verified end-to-end against a
live dispatched run on Codeberg's Forgejo 15.

### v0.13 — the `woodpecker-mcp` companion server

A second binary (see Architecture and Purpose) targeting Woodpecker CI. Woodpecker authenticates
with `Authorization: Bearer`, prefixes its API with `api/`, addresses repositories by numeric
`repo_id`, and paginates with `page` / `perPage` returning bare arrays (no `X-Total-Count`, so the
auto-paginator ends on a short page). The write tools reuse the shared `Elevation` gate.

| Tool | Status | Purpose |
|---|---|---|
| `whoami` | **done** | The authenticated user (verifies the token). |
| `list_repos` | **done** | Repos the user can access (auto-paginated). |
| `lookup_repo` | **done** | Resolve `owner/name` → the repo record, incl. the numeric `id` the other tools need. |
| `get_repo` | **done** | One repository by `repo_id`. |
| `list_pipelines` | **done** | A repo's pipeline runs, newest first (a run's outcome is its `status`). |
| `get_pipeline` | **done** | One pipeline by its per-repo number. |
| `write_status` / `enable_write_mode` / `disable_write_mode` | **done** | Write-mode state and elevation (as in the Forgejo server). |
| `trigger_pipeline` | **done** | Write-mode. Start a pipeline (optional `branch`, `variables`). |
| `cancel_pipeline` | **done** | Write-mode. Cancel a running pipeline. |
| `restart_pipeline` | **done** | Write-mode. Re-run a pipeline; returns the new run. |

Endpoint shapes were taken from Woodpecker's `server/router/api.go`, not assumed. The list tools
currently pass pipeline/repo JSON straight through (no slimming yet). There is no Woodpecker
`version`/instance tool yet.

**Token-model caveat.** Woodpecker issues **one PAT per user** and does not scope it read-only vs
write — a user's rights come from their forge repo access (pull/push). That does not fit this
server's rule that the read and write tokens must differ: a single token yields a read-only server,
and enabling the write tools needs a *distinct* `WOODPECKER_TOKEN_WRITE`, i.e. a second (bot)
account. The wart is inherited from reusing Forgejo's fine-grained-token model; relaxing the
must-differ rule for Woodpecker (letting one token back both read and write) is a candidate
refinement. Note also that Codeberg **hosts** a public Woodpecker at `ci.codeberg.org` (its
recommended CI), so for Codeberg repos `WOODPECKER_URL` is `https://ci.codeberg.org`, not a
self-hosted instance.

## Error handling

`ApiError`s map to MCP errors in `mcp_core::to_mcp`, keyed off `ApiError::is_caller_error`:
an HTTP 4xx (bad token, not found, bad request) becomes `invalid_params` (the caller's
problem); config, transport, 5xx, and decode failures become `internal_error`. (In the forge
clients the type is re-exported as `ForgeError` / `WoodpeckerError` for readability.)

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

- Not a full Forgejo (or Woodpecker) SDK — the in-house `mcp_core` client covers only the ~30
  endpoints the two servers touch, not the whole REST surface of either.
- **Local git operations are out of scope.** Clients with shell access (Claude Code) already
  run `git` directly; this server is about the *remote* forge API.
- No webhooks or admin tooling. CI control is limited to dispatch (Forgejo) and
  trigger/cancel/restart (Woodpecker) — with **no log or artifact retrieval**, as neither forge
  exposes a repo-level endpoint for it.

### CI status: dropped via commit-status, later solved via the Actions-runs API (v0.12.0)

An early `ci_status` ("did my CI pass?") tool built on the combined commit-status endpoint
(`repo_get_combined_status_by_ref`) was removed: that endpoint returns an empty `state: ""` /
`total_count: 0` for Forgejo-Actions repos, because Actions don't populate commit statuses.
(Aside: that empty `state: ""` is exactly the sort of value a strict typed client rejects; our
loose parsing wouldn't choke on it, but there was still no useful status to return.)

The earlier belief that the Actions-runs endpoints themselves 404 on Codeberg was **wrong** —
they 404 only when a repo has the Actions unit *disabled*, not because Forgejo lacks them.
Codeberg's Forgejo 15 exposes `/actions/runs`, `/actions/runs/{id}`, and
`/actions/workflows/{file}/dispatches`, verified live against the real API. So as of **v0.12.0**
the server has proper CI tooling: `list_workflow_runs` (a run's pass/fail is its `status` field —
there is no separate `conclusion`) and `get_workflow_run` (read), plus `dispatch_workflow`
(write-mode, keyed by workflow file name since there is no list-workflows API). Residual gap:
Forgejo exposes no repo-level **logs or artifacts** endpoint, so the tools can report *that* a
run failed and link to it, but can't retrieve the log text programmatically.

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
client (then `src/forge/`, now the shared `src/mcp_core/` + `src/forgejo/client.rs`) is a
proportionate replacement that we fully own and can audit. The
strict-enum gaps simply don't exist when we define the response shapes ourselves (loose where
it matters). It also shed `soft_assert` and a duplicate `thiserror` from the dependency tree.

## Milestones

1. **v0.1.0** — read-only surface, validated against live Codeberg, tagged. *(done)*
2. **v0.2.0** — write mode + repo management (`create_repo` / `delete_repo`) behind a separate
   write token and deliberate, time-boxed elevation. *(done)*
3. **v0.3–v0.11** — dropped `forgejo-api` for the in-house client; added `create_branch`,
   `create_pull_request`, `comment_on_issue`, `list_pull_request_reviews`, and the push-mirror
   set. *(done)*
4. **v0.12.0** — Forgejo Actions (CI): `list_workflow_runs`, `get_workflow_run`,
   `dispatch_workflow`. *(done)*
5. **v0.13.0** — extracted the shared `mcp_core` (generic `RestClient` + `Elevation<C>`) and added
   the companion `woodpecker-mcp` server. Briefly a three-crate workspace (with a published
   `forgejo-mcp-core`), then **collapsed to one crate + two binaries** once it was clear Woodpecker
   only runs in tandem with Forgejo and crates.io is the project's only real discovery surface — one
   legible crate beats a trio with an internal helper crate on display. *(done)*
6. Later — `edit_repo`, issue/PR writes, slimmed Woodpecker/passthrough output, sort filters, a
   Woodpecker `version`/instance tool.
