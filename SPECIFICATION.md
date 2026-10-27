# forgejo-mcp-rs — Specification

This is the source of truth for the project: what it is, how it's built, and what it
exposes. Code and README track this document.

## Purpose

A [Model Context Protocol](https://modelcontextprotocol.io/) server, in Rust, that lets an
MCP client (Claude Code, Claude Desktop, …) inspect a [Forgejo](https://forgejo.org/)
instance — primarily [Codeberg](https://codeberg.org) — or a [Gitea](https://about.gitea.com/)
one (v0.20), over its REST API: the authenticated user, repositories, issues, and pull
requests. **Read-only by default**; repository writes
(create/delete) are available only via a separate write token and a deliberate, time-boxed
write mode (v0.2).

It exists so the assistant can read repo/issue/PR context directly while you work, without
shell-scripting `curl` against the API or trusting a pre-built third-party server with your
token. It is an **independent implementation over the documented Forgejo API**, not a port
of any existing server.

A **companion Woodpecker CI server** (`woodpecker-mcp`) shipped in this crate as a second binary
from v0.13.0 through v0.17.0, for the common arrangement where Woodpecker runs alongside Forgejo.
It moved out at v0.18.0 to its own repository and crate —
[`woodpecker-mcp`](https://codeberg.org/brechanbech/woodpecker-mcp) — because Woodpecker is not a
Forgejo component: it drives Gitea, GitHub, GitLab, and Bitbucket equally, that server never
called the Forgejo API at all, and bundling it here made it invisible to everyone not running
Forgejo. See the v0.18 section for the full rationale.

## Architecture

One crate, `forgejo-mcp-rs`, providing one MCP server binary over an in-house REST/MCP core
(`mcp_core`) kept as internal modules rather than a published crate.

```
mcp_core  →  transport: RestClient (token auth + api-prefix), ApiError,   ← src/mcp_core/
             the Elevation<C> write-mode gate, pagination & helpers
   ↑
forgejo   →  Forge(RestClient) + #[tool] methods  →  forgejo-mcp-rs binary   ← src/forgejo/
```

- `src/lib.rs` — declares the two modules. `mcp_core` is `pub(crate)`: it is internal scaffolding,
  not a public library surface — the crate exists to provide the server binary.
- `src/mcp_core/` — the endpoint-agnostic core, built on `reqwest`:
  - `rest.rs` — `RestClient`: base-URL/api-prefix joining, a zeroized token presented as
    `Authorization: token …`, one `request()` helper, and `get` / `get_list` / `post` / `patch` /
    `post_empty` / `delete` verbs returning raw JSON (`serde_json::Value`).
  - `error.rs` — `ApiError` (config / transport / non-2xx status / decode), which knows whether a
    failure is the caller's (4xx) or ours.
  - `elevation.rs` — `Elevation<C>`, the generic time-boxed write-mode gate (see Security model),
    parameterized with the Forgejo write client.
  - `helpers.rs` — `to_mcp(ApiError)` mapping, `json_result`, the auto-paginator (`gather_all`),
    and the paged/gathered result envelopes.
  - `mod.rs` — re-exports and `init_tracing`.
- `src/forgejo/` — the Forgejo server: `client.rs` (`Forge`, the `api/v1/` + `Authorization: token`
  endpoint set), `tools.rs` (tool functions), `server.rs` (`ForgejoMcp { tool_router, forgejo,
  elevation, mirror_token }`, its `#[tool_router]`, and the `ServerHandler`), and `mod.rs::serve()`
  (the stdio entry point).
- `src/bin/forgejo.rs` — a thin `#[tokio::main]` wrapper that calls `forgejo::serve()`. Logs go to
  **stderr** (stdout is the MCP stdio transport).

Built on `rmcp 3`, which speaks MCP protocol version **2026-07-28** and negotiates down to
`2024-11-05`. Conventions (lints, CI, pre-push, deny/clippy config) mirror the sibling
`kicad-mcp-rs` project.

Both servers are **stdio-only and tools-only** (`ServerCapabilities::enable_tools()`), which is
why the 2026-07-28 revision costs them so little: the removed session handshake, the
`Mcp-Session-Id` header, SSE resumability, and `subscriptions/listen` are all Streamable-HTTP
concerns, and every feature the revision deprecates — Roots, Sampling, Logging, HTTP+SSE,
Dynamic Client Registration — is one neither server ever used. Logging in particular already
follows the recommended migration: `init_tracing` writes to stderr, never `notifications/message`.

## Configuration

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `FORGEJO_TOKEN_READ_ONLY` | **yes**\* | — | Read token (read-only scopes suffice). `FORGEJO_TOKEN` is accepted as a fallback. |
| `FORGEJO_TOKEN_WRITE` | no | — | Write/delete-scoped token. **Its presence enables the write tools**; absent ⇒ the server is read-only. |
| `FORGEJO_WRITE_MINUTES` | no | `10` | Default write-mode window, clamped to `1..=60`. |
| `FORGEJO_URL` | no | `https://codeberg.org` | Instance base URL. |
| `FORGEJO_FLAVOR` | no | `auto` | `forgejo` / `gitea` / `auto`. Pins the instance flavor instead of detecting it; only the Actions tools consult it. An unrecognized value fails startup. |
| `RUST_LOG` | no | `forgejo_mcp_rs=info` | Tracing filter (logs go to stderr). |

\* A read token is required (under either name); the server refuses to start without one. A
**write token alone is refused** — reads must use a dedicated read-only token, even though a
write token could technically read — and the read token **must differ** from
`FORGEJO_TOKEN_WRITE` (no reusing the write token in the read slot). The server can't verify a
token's *scope* without probing, so this is a structural guard (presence + distinctness), not
a scope check.

**No Forgejo version is pinned.** `FORGEJO_URL` points at any Forgejo instance — Codeberg is
only the default, not an assumption — and the client targets the documented REST API rather
than a specific release. Where a version *does* matter it is called out inline (the Forgejo 15
token-scoping tightening, the Actions-runs endpoints); the `version` tool reports whatever the
connected instance is running, which is the reliable way to check rather than trusting a
version named in these docs.

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

This mechanism is the generic `Elevation<C>` gate in `mcp_core`. Even with a write token present,
the server **starts read-only** and writes are refused until the model deliberately elevates —
"sudo with a timeout":
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
| `list_issues` | **done** | Issues in `owner/repo` (open by default); issues only, and slimmed, since v0.22.0. |
| `get_issue` | **done** | One issue by number. Slimmed since v0.22.0, like the listing, plus the body. |
| `list_pull_requests` | **done** | Pull requests in `owner/repo` (open by default); `base`/`head` branch filters since v0.21.0. |
| `get_pull_request` | **done** | One pull request by number. Slimmed since v0.21.0, like the listing, plus description, reviewers and merge details. |
| `search_repos` | **done** | Search repositories by keyword. |
| `list_orgs` | **done** | Organizations the user belongs to. |
| `list_notifications` | **done** | Notification threads, slimmed (`all` includes read). |
| `list_issue_comments` | **done** | Comments on an issue/PR, slimmed. |
| `list_workflow_runs` | **done** | Actions runs in `owner/repo`, slimmed; filter by `head_sha`/`ref`/`status`/`event`/`workflow_id`. |
| `get_workflow_run` | **done** | One Actions run by id (a run's outcome is its `status`). |
| `dispatch_workflow` | **done** | Write-mode. Trigger a `workflow_dispatch` run by workflow file name. |
| `cancel_workflow_run` | **done** | Write-mode. Cancel a pending or running run (v0.21.0). |
| `rerun_workflow` | **done** | Write-mode. Rerun a finished run, or one job of it (v0.21.0; Forgejo 17+). |

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

**Deferred:** `edit_repo` was deferred here through v0.14 ("`EditRepoOption` has 20+
no-`Default` fields and Codeberg renames are unreliable") and landed in v0.15 — the first
concern dissolved because the tool builds a partial `PATCH` body from only the provided
fields; the second stands, so renaming is still not exposed. See the v0.15 section below.

(The per-version tables above cover the original v0.1/v0.2 surface; the intervening tools —
`create_branch`, `create_pull_request`, `list_pull_request_reviews`, and the push-mirror set —
landed across v0.3–v0.11 and are listed in the README's current tool table rather than here.)

### v0.12 — Forgejo Actions (CI)

> This section describes the surface as it stood at v0.12, when Forgejo was the only supported
> forge. **v0.20 generalized it to Gitea**, which does have a list-workflows API and does report
> a separate `conclusion`; see [v0.20](#v020--gitea-support) for what changed and what the run
> shape looks like now.

| Tool | Status | Purpose |
|---|---|---|
| `list_workflow_runs` | **done** | Runs in `owner/repo`, slimmed; filter by `head_sha`/`ref`/`status`/`event`/`workflow_id`. A run's outcome is its `status` (no separate `conclusion`). |
| `get_workflow_run` | **done** | One run by internal `id` (not `index_in_repo`, despite the web URL). |
| `dispatch_workflow` | **done** | Write-mode. Trigger a `workflow_dispatch` run, keyed by workflow file name (no list-workflows API — discover via `get_file_contents`). |
| `cancel_workflow_run` | **done** | Write-mode, v0.21.0. Cancel a pending or running run; answers with the run. |
| `rerun_workflow` | **done** | Write-mode, v0.21.0. Rerun a finished run, or with `job_id` one job and its dependents. Forgejo 17+; a missing endpoint is reported as such. |

`list_workflow_runs` unwraps the endpoint's `{ workflow_runs, total_count }` body (no
`X-Total-Count` header), so it does not auto-paginate. It always sends `page` and `limit`, defaulting to page 1 of 30, because Forgejo ignores
`limit` here without `page` and returns every run; see
[v0.21.0](#v0210--cancel-and-rerun-and-listings-that-answer-what-was-asked). A `404` from these endpoints means the
repo has the Actions unit disabled, not that there are no runs. Verified end-to-end against a
live dispatched run on Codeberg, then running Forgejo 15.

### v0.13 — the `woodpecker-mcp` companion server *(moved out in v0.18)*

> This server now lives in its own repository and crate:
> [`woodpecker-mcp`](https://codeberg.org/brechanbech/woodpecker-mcp). The section is kept for the
> record; see v0.18 below for why it moved.
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

### v0.15 — `edit_repo`

| Tool | Status | Purpose |
|---|---|---|
| `edit_repo` | **done** | Write-mode. `PATCH /repos/{owner}/{repo}` with a partial `EditRepoOption`: visibility (`private`), `description`, `website`, `default_branch`, `has_issues`/`has_pull_requests`/`has_wiki`, `archived`. |

Only the fields the caller provides are sent, so everything else keeps its current value; a
call with nothing to change is refused with `invalid_params` rather than issuing a no-op
`PATCH`. Renaming (`name`) is deliberately not exposed — Codeberg renames are unreliable
(the original reason this tool was deferred). The motivating use case: flipping a repo
created private (the `create_repo` default) to public without leaving the MCP session.

### v0.16 — MCP 2026-07-28 (rmcp 3)

No tool changes; the tool surface is identical to v0.15. What moved:

| Change | Why |
|---|---|
| `rmcp 1.7` → `3` | Protocol version 2026-07-28 support. `Content` was renamed `ContentBlock`; that rename is the entire mechanical cost of the migration. |
| `list_tools` overridden | 2026-07-28 (SEP-2549) requires `ttlMs` and `cacheScope` on `tools/list`; the `#[tool_handler]`-generated body leaves both `None`. Built by `mcp_core::tool_list_result`. |
| `server_info` set explicitly | `Implementation::from_build_env` expands `env!` inside *rmcp*, so the server was identifying itself as `rmcp 3.0.0`. Now `forgejo-mcp-rs` at the crate version. |
| `base64 0.22` → `0.23` | Matches rmcp's. Does **not** clear the `cargo deny` duplicate warning: `reqwest`/`hyper-util` still pin 0.22, so two versions remain in the tree until they update. |

**Cache hints.** `ttlMs` is one hour and `cacheScope` is `public`. Public is safe because the
tool set depends only on the binary, never on the caller or its token: write-mode elevation
gates the write tools at *call* time and never hides them from the listing, so the list does
not vary with elevation state either. Both fields are sent unconditionally rather than gated on
the negotiated version — they are additive, and rmcp models them as optional across versions,
so a pre-2026-07-28 client simply ignores them.

**Protocol version.** `get_info` still leans on `ServerInfo::default()` for `protocol_version`,
which is `ProtocolVersion::LATEST` — and in rmcp 3.0 that is still `2025-11-25`, not
`2026-07-28`. This only sets the *fallback*: negotiation accepts any known version, so a
2026-07-28 client is served 2026-07-28. Verified against both binaries over stdio, including
the fully stateless path (no `initialize` at all, just per-request `_meta`), where
`server/discover` returns all five supported versions and `tools/list` carries
`resultType: "complete"` alongside the cache hints.

### v0.17 — `migrate_repo`

| Tool | Status | Purpose |
|---|---|---|
| `migrate_repo` | **done** | Write-mode. `POST /repos/migrate` with a `MigrateRepoOptions`: copy a repository from another forge/instance into this one, optionally carrying issues, PRs, labels, milestones, releases, wiki and LFS. |

This closes the one real gap left by the push-mirror set: mirrors replicate git refs and
nothing else, so until now there was no way to move a repo's *metadata* between instances.
`/repos/migrate` is called on the destination, with `clone_addr` naming the source — the
inverse direction from a push mirror.

Design notes:

- **`service` gates the metadata.** It defaults to `git` (a bare clone, refs only); the
  API-based importer that the content flags depend on only engages for a named forge. The
  value for a Forgejo or Codeberg source is `gitea` — there is no `forgejo` variant, a
  plausible enough mistake that unknown services are rejected up front with the valid list
  rather than passed through to a confusing upstream 422.
- **Content flags stay opt-in**, matching the API's own defaults, rather than being switched
  on for the caller. `private` is the one place we override the API: it defaults to `true`,
  as in `create_repo`, since publishing later is easier than un-publishing.
- **`clone_addr` is validated as http(s).** An ssh or `git://` address, or a bare path, is
  refused locally — a path in particular would otherwise be read as a *local disk* path by
  the instance.
- **Asynchronous by nature.** The response is a placeholder repo while the import runs in
  Forgejo's task queue; the tool description tells the caller to poll `get_repo`. And it is a
  copy — the source is never touched, so retiring it stays a separate, deliberate act.
  *(Wrong: the API migration runs within the request. Corrected in
  [v0.24.0](#v0240--migrate_repo-sends-the-credential-where-each-importer-reads-it).)*

**Credential.** `FORGEJO_MIGRATE_TOKEN` follows `FORGEJO_MIRROR_TOKEN`'s pattern — server
environment, `Zeroizing`, never a tool argument — but is deliberately a *separate* variable
rather than a reuse of it. The mirror token authenticates to a push **target**; this one
authenticates to a source being **read from**, and they are generally different hosts. One
shared variable would mean a credential issued for host A being sent to host B on the caller's
say-so. Authentication is opt-in (`auth_username`, or `authenticate` alone for token-only
forges), since the common case — a public source — needs no credential at all.

### v0.18 — the Woodpecker server moves out

The `woodpecker-mcp` binary added at v0.13 left for its own repository and crate:
[`woodpecker-mcp`](https://codeberg.org/brechanbech/woodpecker-mcp). Its tool surface did not
change in the move; this crate is a single binary again.

**Why.** The v0.13 bundling reasoning was that Woodpecker only runs in tandem with Forgejo, so one
legible crate beat a trio. The first half of that turned out to be wrong: Woodpecker drives Gitea,
GitHub, GitLab, and Bitbucket as readily as Forgejo, and this repository's own Woodpecker server
never called the Forgejo API at all — it spoke only to Woodpecker's, which already normalizes the
forge behind it. So there was never a technical coupling, only a packaging one, and that packaging
actively hid the server from every Woodpecker user not on Forgejo. crates.io being the project's
discovery surface is exactly why that matters: nobody searching for a Woodpecker MCP server finds
one inside a crate named for Forgejo.

**The shared core went by copy, not by dependency.** Splitting the repository forced the question
`mcp_core` had been deferring: publish it as a crate both projects depend on, or duplicate it. It
was copied. Across v0.13–v0.17 `mcp_core` was touched by **four commits** — thin, static
scaffolding, not a living library, and publishing it would have imposed an API-stability
obligation out of all proportion to that, recreating the very three-crate arrangement v0.13
collapsed. A git dependency was not an option: crates.io rejects published crates carrying one,
which would have killed `cargo install` and the registry listing with it. The two copies are free
to diverge, and are expected to.

**What this crate shed.** With the Woodpecker server gone, `mcp_core` lost the pieces only it
used: the `Auth` scheme enum (this server is always `Authorization: token`, so the scheme is now
hardcoded) and the `post_none` verb. `list_tools` also stopped being an `async fn` — nothing in it
awaits — which a newer clippy had begun flagging.

### v0.19 — bounded reads: file windows and pull-request diffs

Three additions, all in service of one property: **a read tool should never be able to dump an
unbounded amount of text into the model's context.** The existing list tools already had this via
`gather_all`'s item cap; file reads and diffs did not.

| Tool | | Endpoint |
|---|---|---|
| `list_pull_request_files` | read | `GET /repos/{owner}/{repo}/pulls/{index}/files` |
| `get_pull_request_diff` | read | `GET /repos/{owner}/{repo}/pulls/{index}.diff` |

**`get_file_contents` gained a line window.** Optional `start_line` / `end_line` are 1-indexed and
inclusive, and `total_lines` is now always reported so a caller can page through a large file
rather than pulling it whole. Bounds **clamp** rather than fail: an `end_line` past the end stops
at the last line, and a `start_line` past the end returns an empty slice with the window echoed —
the file genuinely has no such line, and saying so is more useful than an error. Only an inverted
window (`start_line` after `end_line`) is rejected, since nothing could satisfy it. The slice is
re-joined with `\n`, so CRLF files come back normalized.

**Diffs are a two-step read.** `list_pull_request_files` gives the changed paths with per-file
counts; `get_pull_request_diff` then returns hunks. Passing `file_path` narrows to a single file
and is the intended path for review work. Without it, the whole diff is truncated at 64 KiB
(`max_bytes` overrides) at a line boundary, flagged `truncated`, with a note naming
`list_pull_request_files` — the same "cap and say so" contract `gather_all` uses.

`list_pull_request_files` auto-paginates like the other list tools when neither `page` nor `limit`
is given. That is a deliberate exception to the bounded-reads theme of this release: a file *list*
is a few dozen bytes per entry and is exactly the index a caller needs to decide what to read
next, whereas the diff it points at is the unbounded thing. The 1000-item cap still guards the
pathological case.

**Two API facts drove the shape.** Forgejo's `/files` endpoint does **not** carry the `patch`
field GitHub's equivalent returns, so the file list cannot supply hunks and a second call is
unavoidable — this is not a design preference. And `.diff` serves `text/plain`, not JSON, which is
why `mcp_core` grew [`RestClient::get_text`]; `request()` was split into a raw `send()` plus a JSON
parse on top, leaving every existing verb unchanged.

**Matching a file inside a diff.** `file_path` matches exactly (no globs) against either side of a
rename, so a caller need not know whether they hold the pre- or post-rename name. Paths are
harvested from the `diff --git` header *and* the `---` / `+++` markers, but only from the lines
before the first `@@`: inside a hunk, a removed line whose content starts with `-- ` is rendered as
`--- `, and would otherwise be misread as a file header. Both that trap and a real Codeberg diff
are covered by unit tests; the real-diff fixture is fetched output, not hand-written, so the parser
is pinned to what Forgejo actually serves.

### v0.20 — Gitea support

Forgejo is a Gitea fork, and for this server's purposes the fork barely diverged. Diffing the
two published OpenAPI specs (`gitea.com/swagger.v1.json` against `codeberg.org/swagger.v1.json`)
over the ~25 endpoints this server calls found **only the Actions (CI) ones differ**. Issues,
pull requests, diffs, PR files, branches, contents, search, orgs, notifications, push mirrors,
and migration match in path, method, and response shape. So Gitea support is not a port or an
abstraction layer; it is a handful of conditionals confined to the Actions calls.

That claim is scoped to *this server's* endpoints, and it is worth stating what it does not
cover. The contents endpoint also diverges — Forgejo dates a file's last change with
`last_commit_when`, Gitea with `last_author_date`/`last_committer_date`, and only Gitea carries
`last_commit_message` — which does not matter here because no tool reads those fields, but does
matter to anything rendering a file listing. The full field-by-field comparison, including the
parts that are only observable from live responses rather than from the specs, is in the
README's [Forgejo vs Gitea](README.md#forgejo-vs-gitea) section.

That shaped the design: no trait, no per-forge client, no second module. One `Flavor` enum, and
only the three Actions functions ever ask for it.

**Detecting the flavor.** From `GET /version`, lazily, cached in a `OnceLock` after first use.
The heuristic reads backwards and the code says so: **Forgejo names Gitea in its own version
string** — Codeberg reports `16.0.0-dev-741-6f391573+gitea-1.22.0`, older releases
`7.0.4+0-gitea-1.22.0` — to advertise API compatibility, while Gitea never names itself
(`1.27.0+dev-954-g1f3981a301`). A `gitea-` marker therefore means Forgejo. Failing that marker,
the major version decided: Gitea was still on 1.x, Forgejo renumbered to 7 and beyond, so 2+ was
Forgejo. *(Corrected in v0.21.0: gitea.com now reports `29.0.0`, so the major version is no
signal and only the marker counts — see [v0.21.0](#v0210--cancel-and-rerun-and-listings-that-answer-what-was-asked).)*
The live strings are pinned as constants in the client's unit tests.

Detection is lazy rather than done at startup for two reasons: `from_env` is synchronous and does
no network, and an instance that is briefly down should not prevent the server from starting.
Failure falls back to Forgejo — the historical assumption — and is deliberately **not cached**, so
a later call retries instead of living with a guess made during an outage. `FORGEJO_FLAVOR` pins
it outright for an instance the heuristic misreads.

A plain `std::sync::OnceLock` rather than an async-aware cell: a lost race costs one redundant
`GET /version` and nothing else, which is not worth adding `tokio/sync` for.

**The four differences, and why each is handled the way it is.**

*Filters are translated, not duplicated.* Forgejo filters runs by `ref` (fully qualified) and
`workflow_id` (a query parameter); Gitea uses `branch` (bare name) and moves the workflow filter
into a separate `…/actions/workflows/{file}/runs` path. The tempting shortcut is to send both
spellings and let each forge ignore the one it doesn't know. That is exactly wrong here: an
unknown query parameter is **ignored, not rejected**, so Forgejo's `ref` sent to Gitea would
silently return *unfiltered* runs — a filter that appears to work and doesn't. Translation is in
one pure function, `run_list_request`, so both branches are unit-testable without a network.

*A tag ref is left alone for Gitea.* `refs/heads/` is stripped for Gitea's `branch` filter, but
`refs/tags/v1` is passed through untouched. Gitea has no tag filter on this endpoint, so it
cannot match either way; rewriting it to `v1` would only disguise the miss as a branch that
doesn't exist.

*Gitea's `path` is not a path.* This is the one thing the spec diff could not have told us, and
the first live probe caught it. Gitea reports `path` as `test-pr.yml@refs/pull/1117/head` — the
workflow file, an `@`, and the fully-qualified ref — not a filesystem path. Reading it as one
(basename after the last `/`) reported the workflow as `head`. It is also the *only* reliable
source of the ref: `head_branch` is populated for branch runs and null for both tags and pull
requests, so the ref is recovered from `path` and then shortened to match Forgejo's `prettyref`
(`refs/heads/main` → `main`, `refs/tags/v0.16.0` → `v0.16.0`). A pull-request ref has no short
form and is left whole, which at least says plainly what kind of run it was.

*An unknown workflow filter diverges.* Because the workflow filter is a path segment on Gitea
and a query parameter on Forgejo, filtering by a workflow that doesn't exist returns `404
workflow "ci.yml" not found` on Gitea and an **empty list** on Forgejo. Both confirmed live. This
matters more than it looks: the tool description used to say a 404 means Actions is disabled, and
a model following that would draw the wrong conclusion. The description now distinguishes them.

*Runs are normalized onto one shape.* Gitea took GitHub's vocabulary (`head_sha`, `head_branch`,
`run_number`, `display_title`, `created_at`, and a `status`/`conclusion` split) while Forgejo kept
its own (`commit_sha`, `prettyref`, `index_in_repo`, `title`, `created`, and a single `status`
carrying the outcome). `list_workflow_runs` folds them together so that "did this run pass?" is
the same question on both: Gitea's `conclusion` is promoted into `status`, where Forgejo already
puts the outcome, and kept verbatim alongside it for callers that want the phase/outcome
distinction. The fields are allow-listed rather than filtered out, which is also what keeps the
embedded `repository` object, the whole `event_payload`, and the `trigger_user` email out of the
response. An empty string counts as absent, because both forges send `""` rather than null for a
conclusion or timestamp that doesn't exist yet — otherwise a queued run would report a blank
conclusion and lose its status. *(Corrected in v0.20.6: not always `""`. Forgejo reports a
queued run's `started` and `stopped` as the Unix epoch, rendered in the server's zone —
`1970-01-01T01:00:00+01:00` on Codeberg. Timestamps now treat the epoch, on either side of
midnight UTC, and Go's zero time `0001-01-01` as absent too.)*

This renamed the summary's fields (`index_in_repo` → `run_number`, `prettyref` → `ref`,
`workflow_id` → `workflow`), which is the breaking part of the release and the reason it is a
minor bump.

*`get_workflow_run` is deliberately left un-normalized.* It exists to return the instance's full,
unmodified run object, so it stays shaped differently on each forge. `list_workflow_runs` is the
normalized view; the tool descriptions say which is which. *(Reversed in
[v0.25.0](#v0250--every-write-answers-in-its-read-tools-shape): the full object was about
20 KB, an email address included.)*

*Dispatch adapts to the reply.* `return_run_info` — which makes the endpoint answer with the run
it just created — is a Forgejo extension, so it is omitted on Gitea, which replies `204 No
Content`. Rather than return a bare null, the tool synthesizes an acknowledgement naming the
workflow and ref and pointing the caller at `list_workflow_runs`, so a model isn't left guessing
whether the dispatch took.

**What is verified, and what isn't.** Everything below was driven through the installed binary
over stdio against live `gitea.com`, with a read-only token, on 15 September 2026:

- Flavor detection (`gitea.com` → `gitea`, reporting `1.27.0+dev-954-g1f3981a301`), the
  `FORGEJO_FLAVOR` override, and startup rejection of a bad override value.
- The read surface against a private repo: `whoami`, `list_my_repos`, `get_repo`,
  `list_branches`, `get_file_contents` (including a line window), `list_issues`,
  `list_pull_requests`, `list_orgs`, `search_repos`.
- The Actions read tools against a public repo with real history (`gitea/tea`, 864 runs):
  `list_workflow_runs` unfiltered and filtered by `workflow_id`, `ref`, `status` and
  `head_sha` — each returning the correct subset, which is what proves the per-flavor
  translation actually filters rather than being silently ignored — plus `get_workflow_run`.
  Normalization was checked against branch, tag and pull-request runs, the three cases that
  exercise the `path` split and the `head_branch` fallback.

**`dispatch_workflow` on Gitea remains unverified.** It needs write mode against a repo whose
workflows one is entitled to trigger, and firing someone else's CI to test a code path is not a
reasonable thing to do. The request body is unit-tested and the endpoint path is shared with
Forgejo, where dispatch is verified; what is unproven is Gitea's acceptance of that body and the
`204` handling. Treat it as v0.17's `migrate_repo` is treated: implemented and reviewed, not yet
proven.

### v0.20.1–v0.20.3 — releases and assets

| Tool | Status | Purpose |
|---|---|---|
| `list_releases` | **done** | A repo's releases newest first, each slimmed to what identifies it plus its asset names (v0.21.0; before, each asset in full). |
| `get_release` | **done** | One release **by git tag** — `GET /repos/{owner}/{repo}/releases/tags/{tag}`. Slimmed like `list_releases`, plus notes and `target_commitish` (v0.21.0; raw before). |
| `list_release_assets` | **done** | The files attached to one release, by `release_id`; returns the `attachment_id` that deletion takes. |
| `create_release` | **done** | Write-mode. `POST …/releases` with a `CreateReleaseOption`; the tag must already exist unless `target_commitish` creates it. |
| `edit_release` | **done** | Write-mode. `PATCH …/releases/{id}` with a partial `EditReleaseOption`: `name`, `body`, `tag_name`, `target_commitish`, `draft`, `prerelease`. |
| `upload_release_asset` | **done** | Write-mode. The one multipart endpoint, and the one local-disk read; confined to `FORGEJO_UPLOAD_ROOT`. |
| `delete_release_asset` | **done** | Write-mode. Detach one file by `attachment_id`. |

All seven paths are identical on Forgejo and Gitea, request and response shape alike, so not one
of them consults `Flavor` — releases are among the endpoints the fork did not touch.

**Addressed by tag, not by id.** `get_release` takes the tag because that is the only identifier
a release script has *before* the release exists; the numeric id arrives only with the release
itself. That makes publishing idempotent: look the tag up, `create_release` only if it 404s,
then upload. A lookup keyed on the id would force every caller to list releases and match by
hand, which is the same work done less reliably.

**Same-named assets accumulate rather than replace.** Forgejo attaches a second file under the
same name instead of overwriting, so a re-run silently produces two downloads with one name.
Hence `delete_release_asset`, and hence the instruction — in the tool description and the server
instructions both — to delete before re-uploading.

**Uploading is the only thing this server does that reads the local disk**, and whatever it
reads becomes a publicly downloadable file. So the capability is bounded rather than trusted,
via `UploadPolicy`:

- **Off unless configured.** With `FORGEJO_UPLOAD_ROOT` unset every upload is refused. There is
  deliberately no fallback to the working directory: an MCP server's cwd is whatever its client
  happened to launch it from, which is no basis for deciding what may be published.
- **Confined to that root**, with the path resolved through symlinks *before* the containment
  check, so neither a `..` traversal nor a symlink pointing out of the tree escapes it.
- **Bounded.** Regular files only, at most `FORGEJO_UPLOAD_MAX_MB` (default 100), and a
  published name carrying no path separators, since Forgejo takes it verbatim.

**The 404 that reads as something it is not** (v0.20.2). Every release endpoint returns `404`
when a repository has the Releases *unit* switched off — reads included, anonymously included.
Taken at face value that says "no release for that tag", which sends a caller straight into
`create_release`, which 404s in turn for a reason nothing has named. `release_404_hint` states
both readings and names the fix: check `has_releases` via `get_repo` — which reports the unit
toggles for exactly this reason — then turn the unit on with
`edit_repo`. That diagnosis is also why v0.20.2 widened `edit_repo` to reach *every* repository
unit — the one setting that unblocks the release tools was the one setting this server could not
reach, so the remedy it pointed at meant leaving for the web UI.

**`edit_release` sends only the fields it was given** (v0.20.3), on the same principle as
`edit_repo`: a `PATCH` filling in defaults for the rest would overwrite whatever the release
already said, so a call meaning to correct a title would take the release notes with it. Nothing
to change is refused with `invalid_params` rather than issuing a no-op `PATCH`. It exists because
release notes get written before a build finishes and are therefore routinely wrong once it has
— the motivating case was notes instructing readers to strip a quarantine flag from a tarball
that had since been signed and notarized — and the only alternative this server offered was
deleting the release and recreating it, which takes its assets with it.

### v0.20.4 — reading commits, keys, protection and CI

Six read tools, each answering a question the server could previously only gesture at. Every one
was probed against both forges before it was written; the divergences are recorded where they
matter rather than discovered by a caller.

- **`list_commits` / `get_commit`** — the server could name a branch's head and say nothing else
  about it. Slimmed hard: the wire objects carry base64 `signature` and `payload` blobs of a few
  hundred bytes to a couple of kilobytes *each*, plus `files` and `stats` on every listing entry.
  A thirty-commit page is tens of kilobytes nobody reads. `signature` is `verified`, `unverified`
  or `unsigned`, decided by whether a signature is present rather than by parsing `reason`, so an
  unrecognised `gpg.error.*` still classifies and "nobody signed this" stays distinct from "this
  did not verify".
- **`list_keys`** — SSH and GPG for an account, because "can this instance verify my commits"
  needs both. **Forgejo reports `verified` on an SSH key and Gitea does not** (Gitea's
  `PublicKey` carries `last_used_at` instead), so the field is optional and absent means
  unreported, never false. Defaulting it would assert a verified key was unverified on every
  Gitea instance.
- **`list_branch_protections`** — what a repository enforces per branch name or glob. The
  allowlists naming who may bypass each gate are dropped; the question is what is enforced.
  `enable_force_push` is Gitea-only and `apply_to_admins` Forgejo-only, both optional.
- **`get_commit_statuses`** — every status against a ref. Note this is a *transition history*:
  one job appears as queued, then running, then finished, so `gitea/tea`'s `main` carries twelve
  entries for two jobs.
- **`get_combined_status`** — the rolled-up verdict, restoring what v0.12 dropped. See the CI
  status note below for why the original objection expired.
- **`get_repo_tree`** — the whole tree in one request with `recursive`. `truncated` is reported
  even when false: the forge stops early on a large tree, and concluding a file is absent from a
  listing that stopped looking is unsound.

`list_repo_contents` was considered and rejected: `get_file_contents` already lists a directory,
and an empty path lists the root — confirmed against a live instance rather than assumed.

### v0.20.5 — Actions job logs

Two read tools that get from "this run failed" to "this is why". Forgejo 16 added the endpoints
(`GET …/actions/runs/{run_id}/jobs` and `GET …/actions/jobs/{job_id}/logs`), and Gitea serves
the same paths.

- **`list_workflow_jobs`** — the jobs of one run, normalized the way runs are. The two bodies
  differ: Forgejo answers with a bare array and no paging, Gitea with `{ jobs, total_count }`.
  Forgejo's listing is also sparse, with no timestamps, steps or URL (its v17 adds steps, but
  only on the single-job endpoint). Those fields are optional and stay absent on Forgejo rather
  than appearing empty. Gitea is asked for 50 jobs, its usual ceiling, and `truncated` is set if
  its `total_count` says that still fell short.
- **`get_job_log`** — part of one job's log, never the whole of it. The design points:
  - *Ranged, tail first.* The default is the last 32 KiB, because that is where a failure is.
    The window is fetched with `Range: bytes=-N`, which Codeberg honours with a `206` and a
    `Content-Range` giving the log's full length. Ceiling 1 MiB; a larger `max_bytes` is refused,
    not clamped.
  - *Whole lines.* A byte range starts and ends mid-line, and can split a UTF-8 character. Each
    request asks for **one extra byte before the window**, so the cut can tell whether the window
    opens on a line boundary (that byte is `\n`) or mid-line (drop up to the first newline). The
    end is trimmed back to the last newline unless the window reaches the end of the log. Only
    then are the bytes decoded.
  - *Paging without arithmetic.* The answer carries `start`/`end` in whole-log offsets, plus
    `earlier: { offset, max_bytes }` and `later: { offset }`. The `earlier` window ends exactly
    where the current one began, so walking back from the tail neither loses nor repeats a line.
    A unit test walks a whole log back that way and checks the result is byte-identical.
  - *A server that ignores `Range`* sends everything with a `200`. The client reports where the
    bytes it got sit in the whole, and the same cut runs locally, so the answer is identical
    either way. A `416` on a tail read can only mean an empty log (a job that has written nothing
    yet) and is answered as one. On an `offset` read it means the offset is past the end, which
    is reported as the caller's error.
  - *Stripped by default.* Probed on this repository's own CI: every line carries a 29-byte
    timestamp (about 36% of the log), and 411 of 529 lines carry ANSI colour codes. Both are
    removed: CSI sequences, and OSC sequences (Forgejo 17 renders OSC 8 hyperlinks).
    `timestamps=true` keeps the timestamps. `max_bytes` counts raw bytes, so the text returned
    is shorter than the window.

**Verified live on 23 October 2026**, through the installed binary over MCP:
- Codeberg (`brechanbech/forgejo-mcp-rs` run 7311558): the tail, then its `earlier` window,
  which ended at exactly the byte where the tail began.
- gitea.com (`gitea/tea` run 1118217, a failed run): the job listing, with per-step outcomes
  naming the failed step, and the log tail. Gitea's timestamps have the same format, and are
  stripped the same way.

One thing the probe showed: a job's tail is not always its error. On Gitea the last ~2 KiB of
a failed job is the checkout action's post-run cleanup, so a caller may need to page back with
`earlier`, or raise `max_bytes`, to reach the failing step's output.

Not taken up, deliberately: the run-wide ZIP (unbounded, no `Range`), and Forgejo 17's
`?step=`, `?q=` substring filter and `?format=ndjson`. Codeberg still runs a Forgejo 16 build,
which ignores `?step=` and would ignore `?q=` too, returning an unfiltered log to a caller that
believes it filtered. That is the same silent-filter trap the run filters were translated to
avoid. Worth revisiting once those parameters are live where this server is used.

### v0.21.0 — cancel and rerun, and listings that answer what was asked

Two write tools, both gated by write mode like `dispatch_workflow`:

- **`cancel_workflow_run`** — `POST …/actions/runs/{run_id}/cancel`, Forgejo 16+ and Gitea,
  same path. Codeberg has it (an unauthenticated `POST` gets `401`, not `404`).
- **`rerun_workflow`** — a finished run, or with `job_id` one job and the jobs that depend on it.
  Forgejo 17+ and Gitea. The whole-run path is shared; a single job is
  `…/actions/jobs/{job_id}/rerun` on Forgejo but `…/actions/runs/{run}/jobs/{job_id}/rerun` on
  Gitea, so `run_id` is required even with `job_id`.

**Both answer with the run, not an acknowledgement.** Forgejo answers both with an empty `204`,
and its cancel succeeds on a run that has already finished and leaves it alone, so "cancelled"
would not say whether anything was cancelled. The tool reads the run back and returns it in
`list_workflow_runs`' shape, so its `status` says what happened. Gitea answers with the run
itself, which is used instead of a second request; its answer to a job rerun is the job, so that
case reads the run back too. A failed read-back after a successful action is a note, not an
error, because the action did succeed.

**Rerun on Forgejo 16: try, don't check the version.** Codeberg reports
`16.0.0-dev-785-465b60ef+gitea-1.22.0`, a development build, so its version number does not say
reliably which routes exist. A `>= 17` check would refuse an instance that has the endpoint,
with no way round it. The request is made instead, which costs nothing when the route is
missing, because nothing runs. The two kinds of `404` can be told apart. Probed on Codeberg and
gitea.com on 24 October 2026, for `GET` and `POST` alike:

| request | answer |
|---|---|
| a path the router does not know (`…/runs/{id}/rerun` on Codeberg) | `404`, plain text `404 page not found` |
| a known path, unknown run (`…/runs/999999999`) | `404`, JSON `{"message":"run with id 999999999: resource does not exist",…}` |

A non-JSON `404`, or a `405`, is reported as "this instance has no API for rerunning a run",
naming the Forgejo release that added it and the version the instance reports. That version is
fetched only on this failure path. A JSON `404` is passed through as before. Cancel uses the same
check, for Forgejo before 16.

**Gitea detection, fixed.** gitea.com now reports `29.0.0+dev-160-g55186ca39b`: Gitea has left the
1.x line. The second signal in [v0.20](#v020--gitea-support)'s heuristic, "a major of 2 or more
is Forgejo", therefore classified gitea.com as Forgejo. Every Actions call there used Forgejo's
spellings. The run filters `ref` and `workflow_id` were silently ignored rather than rejected,
and `dispatch_workflow` sent the `return_run_info` field Gitea refuses. Now only the `gitea-`
marker means Forgejo, and anything without it is Gitea. A Forgejo build that dropped the marker
would be taken for Gitea; `FORGEJO_FLAVOR` pins it. The job-log verification of 23 October ran
under the wrong flavor. The paths it used are shared, and Forgejo's request for a job listing
merely omits Gitea's `limit`, so its result stands. It did not test what it was believed to.

**The run listing is always paged.** Forgejo honours `limit` on `…/actions/runs` only when
`page` is sent too. Without `page` it returns every run the repository has, and `limit` is
ignored. With `page` alone it pages at 30. Probed on Codeberg on 24 October 2026:

| query | this repository (41 runs) | `forgejo/forgejo` (210,515 runs) |
|---|---|---|
| none, or `limit=3` | all 41 | `504` from the gateway after 30 s |
| `page=1&limit=3` | 3 | 3 s for `limit=5` |
| `page=2` | the oldest 11, after the newest 30 on page 1 | — |

So until v0.21.0, `list_workflow_runs` could not list runs at all on a large Forgejo repository
unless the caller passed `page`, and on a small one it returned everything regardless of
`limit`. It now always sends both, defaulting to page 1 and a limit of 30, which is Forgejo's
own page size. The reply echoes the values actually sent. `run_list_request` takes them as
plain integers rather than options, so leaving one out no longer compiles. Releases, commits,
issues, pulls, branches and tags were probed the same way and honour `limit` alone, so the
shared `paging` helper is unchanged. Gitea honours `limit` alone here too, and sending `page`
costs it nothing.

**A run's trigger comes from `trigger_event`.** Testing the listing showed dispatched and
scheduled runs with no `event` at all. Forgejo has two fields. `event` is empty for those runs:
this repository's run 40, dispatched for the cancel test below, has `"event": ""` beside
`"trigger_event": "workflow_dispatch"`, and a push run has both. `trigger_event` is also what
Forgejo's `event` filter matches. The normalizer now reads `trigger_event` first and falls back
to `event`, which is the only field Gitea has.

**Releases report their asset names, not the assets.** `forgejo/forgejo` attaches 21 files to
every release: a binary, `.xz`, `.asc` and `.sha256` for each platform, plus the source tarball.
Measured on 15 of its releases on 24 October 2026:

| what is returned | size |
|---|---|
| the raw API response, which `get_release` used to pass through | 122 KB, about 8 KB a release |
| `list_releases` until now, each asset with id, name, size, download count and URL | 66 KB of assets |
| asset names alone | 11 KB |

`list_releases` now gives each release `asset_count` and `asset_names`. `get_release`,
`create_release` and `edit_release` give the same summary plus the notes (`body`, omitted when
empty) and `target_commitish`, so a single release has one shape whichever tool returned it.
Upload URLs, tarball links and the uploader's full user object were already dropped from the
listing and are now dropped here too. `list_release_assets` is where the per-asset detail lives,
including the `attachment_id` that `delete_release_asset` takes. The re-upload sequence therefore
gains a call when a same-named asset already exists: `get_release` shows the name is taken, and
`list_release_assets` supplies the id.

**`list_pull_requests` gains `base` and `head`,** so the tool can answer "does this branch
already have an open pull request?" without listing every pull request. Probed on 24 October
2026:

| query | Codeberg, `forgejo/forgejo` | gitea.com, `gitea/tea` |
|---|---|---|
| `state=all`, no filter | 10,350 | 748 |
| `base=…` | 147 for `v16.0/forgejo` | 748 (ignored) |
| `base_branch=…` | 10,350 (ignored) | 509 for `main` |
| `head=<branch>` | 1 | 748 (ignored) |
| `head=<owner>:<branch>` | 0 | — |

Each forge silently ignores the other's spelling, and Forgejo before 16 has neither, which is
the run filters' trap again. So the tool sends only the flavor's own spelling (`base` and
`head`, or `base_branch`) and then checks every pull request it got back against both filters.
If the check drops anything, the forge ignored a filter, and the page it sent is not the page
that was asked for. The tool then walks the whole list and filters it locally, as it always does
for a head filter on Gitea, which has none. The walk is capped at 1,000 like every auto-paginated
listing, and `truncated` says so; with the default `state=open` that is rarely reached.

Forgejo's `head` matches the head's `label`, not its `ref`. For an ordinary pull request the two
are the branch name, even from a fork. For an agit-flow one, `ref` is `refs/pull/{n}/head` and
the label is `user/topic`: `head=mahlzahn/fix_package_view_templates` finds #14984, and the bare
topic does not. The local check accepts either field, so it agrees with Forgejo's filter.
gitea.com's agit-flow pull requests carry the branch name in `label` the same way. Codeberg
rejects GitHub's `owner:branch` form with an empty answer, which would read as "no pull
request". The tool therefore accepts it, sends only the branch, and checks the owner against
`head.repo.owner.login` itself.

**"Open by default" is now sent rather than assumed.** Checking the slimmed listing on gitea.com
turned up merged pull requests in a call that gave no `state`. Both forges default `…/pulls` to
every state, though `…/issues` to open. Probed on 24 October 2026:

| | no `state` | `state=open` |
|---|---|---|
| Codeberg `forgejo/forgejo` pulls | 10,350 | 154 |
| gitea.com `gitea/tea` pulls | 748 | 14 |
| Codeberg `forgejo/forgejo` issues | 1,728 | 1,728 |
| gitea.com `gitea/tea` issues | 101 | 101 |

The tool had documented `open` as its default since v0.1 but sent nothing, so on either forge an
unqualified `list_pull_requests` walked closed pull requests too and stopped at the 1,000 cap.
`state_or_open` now sends `open` when the caller names no state, for issues as well, so the
default is the tool's and does not depend on the forge.

**Pull requests are slimmed.** The two answers that tried the new filters live were 12–15 KB
for a single pull request. The raw object embeds the base and head repositories in full, which is
the same object twice for an in-repository branch. It also embeds the author's and every
requested reviewer's full profile, email addresses included, and every label's colour and
description. `list_pull_requests` now gives each pull request number, title, state, `merged`,
`draft`, `mergeable`, `base`, `head`, `head_repo` (only when the head is in a fork), `head_sha`,
author, label names and its dates. `head` is the branch the `head` filter takes: the `ref`, or for
an agit-flow pull request its `label`. `head_sha` is what `list_workflow_runs` takes to find the
pull request's CI. `get_pull_request` and `create_pull_request` add the description, `merged_by`,
`merge_commit_sha`, `merge_base`, assignees, requested reviewers, milestone, comment counts and
`diff_url`. The diff itself stays with `list_pull_request_files` and `get_pull_request_diff`.
Issues still pass through raw and are next in line.

**Verified live on Codeberg on 24 October 2026**, through the installed binary over MCP and in
write mode. CI was dispatched on this repository (run 40, id 7968548) and cancelled at once;
the answer showed `status: cancelled`, five seconds after it was created. Cancelling the same
run a second time also succeeded and showed the same run unchanged, which is Forgejo's no-op on a
finished run. `rerun_workflow` on that run, for the whole run and with a `job_id`, came back
as "no API for rerunning a run / a job", naming v17 and Codeberg's reported version. It
changed nothing.

Not verified: rerun actually working, which needs a Forgejo 17 instance, and Codeberg has not
moved to 17 yet. On Gitea there is no account with Actions to test on (gitea.com's free tier
has no runners), and firing someone else's CI is not an option. Both tools are therefore
unproven on Gitea, as `dispatch_workflow` is, until a self-hosted Gitea with a runner exists.


### v0.22.0 — issues slimmed, and only issues

`list_issues`, `get_issue` and `create_issue` passed the raw issue object through. It carries
the author's full profile (24 fields, email included), the repository the issue belongs to,
every label's colour and description, and the whole body. Fifteen open `forgejo/forgejo` issues
came to 53 KB, 22 KB of it bodies. Now they follow the pull requests in
[v0.21.0](#v0210--cancel-and-rerun-and-listings-that-answer-what-was-asked):

- **`list_issues`** gives number, title, state, author, `original_author` (only for a migrated
  issue, whose `author` is then the migrating account), label names, assignee logins, milestone
  title, comment count and dates. There is no body: a listing is read for which issues there are,
  and the body is what `get_issue` is for.
- **`get_issue` and `create_issue`** add the body, `locked`, `due_date` and `ref`. They also add
  `pull_request: true` when the number belongs to a pull request. Both forges number issues and
  pull requests in one sequence and answer `…/issues/{n}` for either.

The same probe showed the listing was not only issues. Without `type=issues`, both forges list
pull requests here too, as issues with a `pull_request` object. On 24 October 2026:

| open, from `…/issues` | no `type` | `type=issues` | `type=pulls` |
|---|---|---|---|
| Codeberg `forgejo/forgejo` | 1,728 | 1,574 | 154 |
| gitea.com `gitea/tea` | 101 | 87 | 14 |

Half of the first 50 Codeberg returned were pull requests. `list_issues` now always sends
`type=issues`; `list_pull_requests` is the tool for the rest.

### v0.22.1 — tool annotations

`tools/list` carried a name, a description and a schema for each tool, and nothing else. MCP
tool annotations are how a client tells a read from a write without parsing prose, and each
hint left out means the alarming answer: a tool without `readOnlyHint` may write, one without
`destructiveHint` may destroy, one without `openWorldHint` reaches outside. So every one of the
53 tools looked like a destructive open-world write. Directory graders such as Glama scored the
descriptions as carrying "the full burden", and Anthropic's connector directory requires
`readOnlyHint` or `destructiveHint` on every tool.

Each `#[tool]` now sets a `title` and its hints:

| Hint | Tools |
|---|---|
| `readOnlyHint: true` | The 32 reads from the forge, `list_push_mirrors` included although it needs write mode, plus `write_status` |
| `destructiveHint: true` | What overwrites or removes: `edit_repo`, `edit_release`, `delete_repo`, `delete_push_mirror`, `delete_release_asset`, `sync_push_mirrors` (it overwrites the mirror's refs), `cancel_workflow_run`, `rerun_workflow` (it replaces the previous attempt) |
| `destructiveHint: false` | The writes that only add: the `create_*` tools, `migrate_repo`, `comment_on_issue`, `add_push_mirror`, `upload_release_asset`, `dispatch_workflow`, and the two write-mode switches |
| `idempotentHint: true` | The edits, the deletes, `sync_push_mirrors`, `cancel_workflow_run` (a second cancel was a no-op on Codeberg), and the write-mode switches |
| `openWorldHint: false` | `write_status`, `enable_write_mode` and `disable_write_mode`, which touch only the server's own state |

`tool_list_result` copies each `title` into `annotations.title`, where clients that predate
the top-level field (before protocol `2025-06-18`) look for it. Two tests hold the line: one
fails on a tool missing its title or any hint its kind needs, and one pins the destructive set,
so a new tool has to decide where it belongs.

### v0.23.0 — closing, merging, reviewing, and single-file commits

An agent could open an issue, a branch and a pull request, but not finish any of them: no
closing what it fixed, no merging what was approved, no removing the branch afterwards, and
no changing a file without a local clone. Six write tools fill that in, all behind write mode:

| Tool | Request | Hints |
|---|---|---|
| `edit_issue` | `PATCH …/issues/{n}`, then `PUT …/issues/{n}/labels` by name | destructive, idempotent |
| `merge_pull_request` | `POST …/pulls/{n}/merge` | destructive, idempotent |
| `create_pull_request_review` | `POST …/pulls/{n}/reviews` | neither |
| `delete_branch` | `DELETE …/branches/{b}` | destructive, idempotent |
| `create_or_update_file` | `POST` (create) or `PUT` (update) `…/contents/{path}` | destructive |
| `delete_file` | `DELETE …/contents/{path}`, with a body | destructive, idempotent |

Design notes:

- **Guards against overwriting unseen work.** An update or deletion takes the file's current
  `sha`, and a merge can take `head_sha`; the forge refuses a stale one (Codeberg 409 for a
  file and a merge; gitea.com 422 for a file, 409 for a merge). `head_sha` must be the full
  40 characters: both forges compare it exactly, so an abbreviated one would never match.
- **The merge body is spelled per flavor.** Forgejo takes `Do`, `MergeTitleField` and
  `MergeMessageField`; Gitea takes `do`, `merge_title_field` and `merge_message_field`. Both
  take `delete_branch_after_merge` and `head_commit_id`. Each squash merge was checked live
  for its title and body.
- **`edit_issue` sends only what was given.** `due_date: ""` becomes `unset_due_date: true`,
  `milestone: 0` removes the milestone, and `state` is checked as `open` or `closed` before
  any request. Labels go by name through their own endpoint. Both forges drop a name they
  don't know without an error, so the issue is read back and any missing name is returned in
  `labels_not_found`. A pull request's number works too, which is how one is closed unmerged.
- **Paths are encoded segment by segment.** Request URLs are built by joining onto the API
  root, so a `#` in a file name became a fragment (`get_file_contents` on
  `docs/notes #1 Ä.md` read `docs/notes `), and `..` would resolve into another endpoint.
  `encode_repo_path` percent-encodes each segment, keeps only the unreserved set and `/`, and
  refuses empty, `.` and `..` segments. Branch names go through it too.
- **A missing branch is reported as missing.** Codeberg answers `DELETE` on a branch that does
  not exist with an empty 500 and gitea.com with a bare 404. After either, the branch is looked
  up; when that 404s as well, the error says there was no such branch and nothing was deleted.

Verified live on Codeberg and gitea.com, 24–27 October 2026: file create, update, stale-sha
refusal and delete (including the `#` path); a comment review; a squash merge with title,
message, `delete_branch` and `head_sha`, and its refusal on a wrong `head_sha`; branch
deletion, including the refusal of the default branch; issue title, assignees, due date,
closing, and closing a pull request unmerged. Not live: clearing a due date with `""`, which
has a unit test only.

**`migrate_repo`, first live run (27 October 2026).** A private gitea.com repository to
Codeberg, with `service: gitea` and every content flag on, got an empty 500 and left an empty
repository marked as migrating, which never finished and had to be deleted. The cause is
unknown, and `migrate_repo` remains unverified. The repository was moved by `git push` instead.
*(Found and fixed in
[v0.24.0](#v0240--migrate_repo-sends-the-credential-where-each-importer-reads-it).)*

### v0.24.0 — `migrate_repo` sends the credential where each importer reads it

The failed migration of v0.23.0 was bisected live on 27 October 2026, gitea.com to Codeberg,
in sixteen runs:

| Source | `service` | Credential sent | Result |
|---|---|---|---|
| public | gitea | none | copied |
| public or private | gitea | `auth_token`, all six content flags on or off | copied, issues and PRs intact |
| public or private | gitea | `auth_username` + `auth_token`, even git data only | empty 500 |
| private | git | `auth_username` + `auth_token` | 422 "Authentication failed" |
| public or private | gitea | a revoked `auth_token` | empty 500 |

Two bugs in how the token was sent, and one wrong assumption:

- **A username made the token unused.** Forgejo's Gitea importer, given `auth_username`,
  authenticates with it and `auth_password`, which we never sent. The source refused, and
  Codeberg reported that as an empty 500. This was the original failure.
- **A plain `git` clone never read `auth_token`.** Its URL is built from `auth_username` and
  `auth_password` alone, so `service: git` could not authenticate at all.
- **The migration is synchronous.** The API answers once the import has finished (five to ten
  seconds for a small repository), not with a placeholder to poll.

The token now goes where the service's importer reads it. For `git`, `onedev` and `codebase` it
is `auth_password`, with `auth_username` (default `oauth2` for `git`, which Forgejo, Gitea and
GitLab accept with a token; required for the other two). For `gitea`, `github`, `gitlab`,
`gogs` and `gitbucket` it is `auth_token` alone, and `auth_username` is refused before
anything is created, since it would make the importer ignore the token. That refusal is the
breaking change.

Every failure, the 500s and the 422 alike, left an empty repository under the target name,
which blocks a retry and counts toward the account's repository limit (Codeberg's is 100, and
the bisect hit it). `migrate_repo` now checks the name is free before it starts, so an existing
repository is never mistaken for a leftover, and looks it up again after a failure to name it
in the error. After a server error with a credential sent, the error also says the likeliest
causes: a source repository that does not exist (also an empty 500), or a token the source
refuses, whether revoked, out of scope, or limited to public repositories.

Verified live the same day: a username with `service: gitea` and an existing target name are
both refused with nothing created; `service: git` with the token clones the private source;
`service: gitea` with every content flag carries the merged pull request across; and a missing
source names the empty repository it left.

**rmcp 3.4 to 3.8.** Three of its changes reach a stdio server. `ProtocolVersion::LATEST` is
`2026-07-28` since 3.5, which `get_info` picks up through `Default`. Since 3.5.1 rmcp fills in
missing cache hints for a `2026-07-28` peer, but with `ttlMs: 0` and `cacheScope: private`, so
`list_tools` keeps its own hour, public. And 3.6 added `stdio_pipes`, which drives the pipes an
MCP client launches the server with through Tokio's reactor on Linux, rather than handing every
read and write to the blocking thread pool; on macOS, Windows, FreeBSD and for anything that is
not a pipe it falls back to `stdio`. The rest (authorization, Streamable HTTP, client fixes)
does not apply.

### v0.25.0 — every write answers in its read tool's shape

Four tools still handed the forge's object straight through, and they were the largest replies
the server gave:

| Tool | Passed through | Now |
|---|---|---|
| `get_workflow_run` | about 20 KB on Codeberg: the push `event_payload` as an escaped string, the repository twice, `trigger_user` with an email address | `list_workflow_runs`' run |
| `create_repo`, `edit_repo`, `migrate_repo` | about 3 KB: the owner's profile and email, merge settings, wiki URLs, the time tracker | `get_repo`'s repository |
| `create_branch` | the head commit in full: author, committer, verification | `list_branches`' branch |

`get_workflow_run` had been left raw on purpose (see v0.20), as the place to see a run as the
instance has it. In practice nothing in the raw run was worth its size: a run's jobs and logs
have their own tools, and the payload repeats the commit list. It now normalizes like the
listing, which is the breaking change.

The repository shape gains three fields, for `get_repo` too: `empty`, which tells a fresh
`create_repo` from one with commits; `mirror`; and `original_url`, kept only when it is not
empty, which names a migrated repository's source.

`add_push_mirror` and `list_push_mirrors` still pass through Forgejo's push-mirror object. It is
about ten fields and carries no credential, so there is nothing to cut.

## Error handling

`ApiError`s map to MCP errors in `mcp_core::to_mcp`, keyed off `ApiError::is_caller_error`:
an HTTP 4xx (bad token, not found, bad request) becomes `invalid_params` (the caller's
problem); config, transport, 5xx, and decode failures become `internal_error`. (In the Forgejo
client the type is re-exported as `ForgeError` for readability.)

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

## Releasing

crates.io is the distribution channel; the git remotes are Codeberg (canonical) and a GitHub
mirror, pushed together. A version is immutable once published — yanking hides it, nothing
deletes it, and a number can never be reused — so the order matters.

1. `cargo test` and `cargo clippy --all-targets` clean. The pre-push hook runs these plus
   `cargo deny`, so a push that lands has already passed them.
2. Bump `version` in **`Cargo.toml`**.
3. Bump `version` in **`server.json`** — *both* fields, the top-level one and the one under
   `packages[]`. Nothing enforces this: no script, no CI, no test reads the file, and it drifted
   from 0.20.0 to 0.20.4 unnoticed across four releases because of it. It ships inside the crate
   and feeds the MCP registry, so a stale value is published and advertised.
4. Move the CHANGELOG's `[Unreleased]` heading to the version and today's date.
5. Add a tool-surface section and a milestone entry to this file for anything new.
6. Commit, push, and let CI go green before publishing — a release whose last green run predates
   it by a dozen commits is a release nobody checked.
7. `cargo publish --dry-run`, which packages the crate and compiles *from the package* — the step
   that catches a crate depending on a file it does not ship.
8. `cargo publish`.
9. Publish to the [MCP registry](https://registry.modelcontextprotocol.io), **after** crates.io
   and not before: the registry verifies the namespace by fetching
   `mcp-name: io.github.brechanbech/forgejo-mcp-rs` from this crate's *rendered README on
   crates.io*, and `server.json` names a cargo version that has to exist there already.

   ```sh
   mcp-publisher validate          # checks server.json against the live registry
   mcp-publisher login github      # interactive; the io.github.brechanbech/* namespace
   mcp-publisher publish
   ```

   This is as easy to forget as the manifest itself was, and drifts the same way — `sec-mcp` sat
   at v0.4.4 on the registry while its own `server.json` said 0.5.2. A published crate whose
   registry entry names an older version tells every client to install the older one.

**No SLSA provenance step, deliberately.** Codeberg offers the OpenSSF *SLSA Generic generator*,
and it cannot work here: it is a GitHub Actions reusable workflow fetched from `github.com`,
which Forgejo will not resolve; it declares `permissions: id-token: write`, where Forgejo v15
enables OIDC with `enable-openid-connect` instead; and even if it ran, keyless signing binds the
identity to GitHub's OIDC issuer, so a Forgejo-issued token would not verify as a GitHub Actions
workflow identity — SLSA Build L3 also expects a hardened builder the ecosystem recognises, which
Codeberg's runners are not. The realistic route to provenance is running releases from the GitHub
mirror, which is not worth moving a Codeberg-canonical project for.

**crates.io Trusted Publishing** is the thing to watch instead: it replaces the API token with
OIDC, and supports GitHub Actions and GitLab.com today — not self-hosted GitLab, and not
Forgejo. crates.io has refactored for multiple providers, so Codeberg support is plausible later.
There is also a per-crate setting to *enforce* Trusted Publishing, which disables token
publishing entirely; do not enable it while releases run from Codeberg, as it would lock this
crate out of its own release path.

## Non-goals

- Not a full Forgejo SDK — the in-house `mcp_core` client covers only the ~52 endpoints this
  server touches, not the whole REST surface.
- **Local git operations are out of scope.** Clients with shell access (Claude Code) already
  run `git` directly; this server is about the *remote* forge API.
- No webhooks or admin tooling. CI control is limited to dispatching, cancelling and
  rerunning runs, and **artifacts and run-wide log archives are not retrieved**. Per-job logs
  *are*, since v0.20.5's `get_job_log` (see [Actions job logs](#v0205--actions-job-logs)): the
  objection was only ever to *unbounded* output, and a ranged read answers it. The run-wide
  `…/runs/{id}/logs` ZIP is still out, because it has no `Range` support and so cannot be bounded.

### CI status: dropped via commit-status, later solved via the Actions-runs API (v0.12.0)

An early `ci_status` ("did my CI pass?") tool built on the combined commit-status endpoint
(`repo_get_combined_status_by_ref`) was removed: that endpoint returns an empty `state: ""` /
`total_count: 0` for Forgejo-Actions repos, because Actions don't populate commit statuses.
(Aside: that empty `state: ""` is exactly the sort of value a strict typed client rejects; our
loose parsing wouldn't choke on it, but there was still no useful status to return.)

**Update, 27 September 2026 — the reason has expired.** Actions *do* populate commit statuses
now, on both forges. Probed live: Codeberg answers the combined endpoint with `state: "pending"`
and `total_count: 2` for this repository's own CI, and gitea.com answers `state: "success"` with
`total_count: 4` for `gitea/tea`. So the objection that killed `ci_status` no longer holds, and
v0.20.4 restores the capability as `get_combined_status` — plus `get_commit_statuses` for the
per-check detail. What the earlier tool got wrong was not the endpoint but the era.

The earlier belief that the Actions-runs endpoints themselves 404 on Codeberg was **wrong** —
they 404 only when a repo has the Actions unit *disabled*, not because Forgejo lacks them.
Forgejo exposes `/actions/runs`, `/actions/runs/{id}`, and
`/actions/workflows/{file}/dispatches`, verified live against the real API (on Codeberg's
Forgejo 15 at the time). So as of **v0.12.0**
the server has proper CI tooling: `list_workflow_runs` (a run's pass/fail is its `status` field —
there is no separate `conclusion`) and `get_workflow_run` (read), plus `dispatch_workflow`
(write-mode, keyed by workflow file name since there is no list-workflows API). Residual gap:
Forgejo exposes no repo-level **logs or artifacts** endpoint, so the tools can report *that* a
run failed and link to it, but can't retrieve the log text programmatically. (All of this is
Forgejo-specific; v0.20 added Gitea, where the run shape and the available endpoints differ.)
The logs half of that gap closed with Forgejo 16, which added per-job log and job-listing
endpoints; `list_workflow_jobs` and `get_job_log` use them.

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
   `forgejo-mcp-core`), then **collapsed to one crate + two binaries**. *(done; superseded by
   v0.18.0)*
6. **v0.16.0** — moved to `rmcp 3` / MCP protocol version 2026-07-28: `tools/list` cache hints
   and correct server identity in `server/discover`. No tool changes. *(done)*
7. **v0.17.0** — `migrate_repo`, the only tool that carries issues and PRs in from another
   instance. *(done)*
8. **v0.18.0** — the Woodpecker server moved out to its own repository and crate; this crate is a
   single binary again. *(done)*
9. **v0.19.0** — bounded reads: a line window on `get_file_contents`, plus
   `list_pull_request_files` and `get_pull_request_diff` for reviewing a change without pulling the
   whole diff. *(done)*
10. **v0.20.0** — Gitea support: automatic flavor detection, per-flavor Actions requests, and a
    normalized workflow-run shape across both forges. *(done; Actions tools not yet exercised
    against a live Gitea instance)*
11. **v0.20.1–v0.20.3** — releases: the seven release and asset tools, `edit_repo` widened to
    reach every repository unit (`has_releases` being the one the release tools need), and
    `edit_release` for correcting published notes without destroying a release's assets.
    *(done)*
12. **v0.20.4** — reading commits, keys, branch protection and CI status: `list_commits`,
    `get_commit`, `list_keys`, `list_branch_protections`, `get_commit_statuses`,
    `get_combined_status`, `get_repo_tree`. *(done)*
13. **v0.20.5** — Actions job logs: `list_workflow_jobs` and `get_job_log`, bounded by HTTP
    `Range`. *(done)*
14. **v0.21.0** — `cancel_workflow_run` and `rerun_workflow`, Gitea detection by the
    compatibility marker alone, a run listing that is always paged, releases that name
    their assets rather than carrying them, and pull requests filtered by branch and slimmed.
    *(done; cancel verified live on Codeberg, rerun awaits Forgejo 17)*
15. **v0.22.0** — issues slimmed like pull requests, and `list_issues` limited to issues.
    *(done)*
16. **v0.22.1** — every tool titled and annotated: read-only, destructive, idempotent and
    open-world hints. *(done)*
17. **v0.23.0** — `edit_issue`, `merge_pull_request`, `create_pull_request_review`,
    `delete_branch`, `create_or_update_file` and `delete_file`, with request paths encoded
    segment by segment. *(done; verified live on Codeberg and gitea.com)*
18. **v0.24.0** — rmcp 3.8 and `stdio_pipes`; `migrate_repo` sends the credential where each importer reads it, refuses
    a username for the token services, and names the empty repository a failure leaves.
    *(done; verified live gitea.com → Codeberg)*
19. **v0.25.0** — `get_workflow_run`, the repository writes and `create_branch` answer in
    their read tools' shapes. *(done)*
20. Later — sort filters on the issue lists, and binding the migrate and mirror credentials
    to their hosts.
