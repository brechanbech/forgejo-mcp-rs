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

`list_workflow_runs` unwraps the endpoint's `{ workflow_runs, total_count }` body (no
`X-Total-Count` header), so it does not auto-paginate. A `404` from these endpoints means the
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
the major version decides: Gitea is still on 1.x, Forgejo renumbered to 7 and beyond, so 2+ is
Forgejo. Both live strings are pinned as constants in the client's unit tests.

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
conclusion and lose its status.

This renamed the summary's fields (`index_in_repo` → `run_number`, `prettyref` → `ref`,
`workflow_id` → `workflow`), which is the breaking part of the release and the reason it is a
minor bump.

*`get_workflow_run` is deliberately left un-normalized.* It exists to return the instance's full,
unmodified run object, so it stays shaped differently on each forge. `list_workflow_runs` is the
normalized view; the tool descriptions say which is which.

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
| `list_releases` | **done** | A repo's releases newest first, each slimmed to what identifies it plus its downloadable assets. |
| `get_release` | **done** | One release **by git tag** — `GET /repos/{owner}/{repo}/releases/tags/{tag}`. |
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
both readings and names the fix: check `has_releases` via `get_repo`, then turn the unit on with
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

## Non-goals

- Not a full Forgejo SDK — the in-house `mcp_core` client covers only the ~30 endpoints this
  server touches, not the whole REST surface.
- **Local git operations are out of scope.** Clients with shell access (Claude Code) already
  run `git` directly; this server is about the *remote* forge API.
- No webhooks or admin tooling. CI control is limited to `dispatch_workflow`, and **logs and
  artifacts are not retrieved** — but note the reason has changed. This was justified by "Forgejo
  exposes no repo-level endpoint for it", which is no longer accurate: Forgejo v16 serves per-job
  logs, and `goern/forgejo-mcp` reads them bounded and resumable (`offset` + `max_bytes`,
  defaulting to the tail) after enumerating a run's jobs — the run-wide ZIP endpoint is the one
  with no `Range` support. So the real objection was only ever to *unbounded* logs, and that is
  solvable. Reading a failed job's tail is a candidate, not an impossibility. Unverified here
  against a live v16 instance.

### CI status: dropped via commit-status, later solved via the Actions-runs API (v0.12.0)

An early `ci_status` ("did my CI pass?") tool built on the combined commit-status endpoint
(`repo_get_combined_status_by_ref`) was removed: that endpoint returns an empty `state: ""` /
`total_count: 0` for Forgejo-Actions repos, because Actions don't populate commit statuses.
(Aside: that empty `state: ""` is exactly the sort of value a strict typed client rejects; our
loose parsing wouldn't choke on it, but there was still no useful status to return.)

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
12. Later — issue/PR writes, sort filters on the issue lists, bounded Actions job logs (see
    Non-goals), and slimming what still passes through raw.
