# forgejo-mcp-rs

[![CI](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions/workflows/ci.yml/badge.svg)](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions)

A [Model Context Protocol](https://modelcontextprotocol.io/) server for
[Forgejo](https://forgejo.org/) and [Codeberg](https://codeberg.org). It lets
an MCP client (Claude Code, Claude Desktop, …) read your forge — the authenticated user,
repositories, issues, and pull requests — over the Forgejo REST API.

> Status: **read-only by default, with opt-in guarded writes (since v0.2).** Read tools across
> the forge — user, repos, issues, pull requests, search, orgs, notifications, comments,
> reviews, and Actions (CI) runs — plus guarded writes (`create_repo`, `edit_repo`,
> `create_branch`, `create_issue`, `create_pull_request`, `comment_on_issue`, `delete_repo`,
> `migrate_repo`, push-mirror
> management, and `dispatch_workflow`) gated behind a separate write token and a deliberate,
> time-boxed **write mode**. See [`SPECIFICATION.md`](SPECIFICATION.md) for the full design.

It speaks the Forgejo REST API directly through a small, in-house client (`src/forgejo/client.rs`,
over the shared `src/mcp_core/` transport) — an **independent implementation over the documented
API**, not a port of any other server. There is no third-party forge SDK in the trust path, so the
tool surface holding your token is code you can read and audit end to end.

Both servers speak **MCP protocol version 2026-07-28** (since v0.16) and negotiate down to
`2024-11-05`, so older clients keep working unchanged. They run over **stdio** and expose
**tools only** — no resources, prompts, sampling, or roots.

## Build

```sh
cargo build --release                        # both binaries under target/release/
cargo install --path .                       # install both to ~/.cargo/bin
cargo install --path . --bin woodpecker-mcp  # …or just one (--bin forgejo-mcp-rs for the other)
```

## Binaries

This crate ships **two** MCP servers as separate binaries that share the in-house REST/MCP core
(`src/mcp_core/`) as internal modules:

- **`forgejo-mcp-rs`** — the Forgejo / Codeberg server documented below.
- **`woodpecker-mcp`** — a companion server for [Woodpecker CI](https://woodpecker-ci.org/) when
  it runs alongside your Forgejo instance: list repos and pipelines, and (guarded) trigger, cancel,
  and restart pipelines. It uses `WOODPECKER_URL` and `WOODPECKER_TOKEN_READ_ONLY` /
  `WOODPECKER_TOKEN_WRITE`, and the same time-boxed write-mode elevation as the Forgejo server.
  Woodpecker addresses repositories by numeric id, so `lookup_repo` resolves an `owner/name` to it.

Each runs **independently** — same crate, but no runtime coupling. `woodpecker-mcp` needs only
`WOODPECKER_URL` and a Woodpecker token and works with or without the Forgejo server installed;
"companion" just means Woodpecker CI is usually deployed alongside Forgejo, not that either server
depends on the other. So you needn't install both: `cargo install --path .` builds both, or add
`--bin woodpecker-mcp` (or `--bin forgejo-mcp-rs`) to install just one. Enable whichever you need in
your MCP client — the Forgejo server is documented first, the Woodpecker server has its own section
([below](#woodpecker-server-woodpecker-mcp)).

## Configure

The server is configured by environment variables:

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `FORGEJO_TOKEN_READ_ONLY` | **yes** | — | Read token (or `FORGEJO_TOKEN`). **Read-only scopes are enough.** |
| `FORGEJO_TOKEN_WRITE` | no | — | Write/delete-scoped token. **Providing it enables the write tools**; omit it for a pure read-only server. |
| `FORGEJO_WRITE_MINUTES` | no | `10` | Default write-mode window (minutes, max 60). |
| `FORGEJO_MIRROR_TOKEN` | no | — | Credential `add_push_mirror` sends as the remote's password (e.g. a GitHub PAT). Kept out of the conversation — never passed as a tool argument. Omit if you only use `use_ssh=true` mirrors. |
| `FORGEJO_MIGRATE_TOKEN` | no | — | Credential `migrate_repo` sends to the **source** instance it reads from. Also never passed as a tool argument. Kept separate from `FORGEJO_MIRROR_TOKEN` on purpose — that one authenticates to a push *target*, so sharing a variable would send a credential to a host it was never issued for. Omit if you only migrate public repos. |
| `FORGEJO_URL` | no | `https://codeberg.org` | Instance base URL. |

Mint a token at **Codeberg → Settings → Applications** (or your instance's equivalent). For
the read tools, read scopes (`read:repository`, `read:issue`, `read:user`) suffice. The write
token needs `write:repository` (including delete, and the repo-admin push-mirror endpoints).

A **read token is mandatory**: the server refuses to start on a write token alone, and the
read token must be a *different* token from `FORGEJO_TOKEN_WRITE` — you can't shortcut by
reusing the write token for reads.

> **Forgejo 15+ token scoping.** Forgejo 15.0 tightened authorization on many repository
> APIs to match its fine-grained, repository-scoped access tokens. Classic broad tokens are
> unaffected, but if you mint a *repository-scoped* token it must actually carry the scopes
> above — in particular the repo-admin scope for the push-mirror tools, which otherwise return
> `403`. Scope the token to every repo you intend to reach.

### Write mode

The server is **read-only by default.** `create_repo` / `edit_repo` / `delete_repo` work only when (a)
`FORGEJO_TOKEN_WRITE` is configured **and** (b) you've deliberately entered **write mode** via
`enable_write_mode` — a time-boxed elevation (default 10 min, max 60) that slides forward on
each write and auto-reverts. `write_status` reports the state; `delete_repo` also requires a
`confirm` argument equal to `"owner/repo"`. See [`SPECIFICATION.md`](SPECIFICATION.md#write-mode-deliberate-time-boxed-elevation)
for the full design.

### Migration source token (optional)

Only needed if you'll use [`migrate_repo`](#moving-a-repo-between-instances) against a
**private** source. Public sources need no credential — skip this entirely.

Note the direction: every other variable here authenticates to *your* instance
(`FORGEJO_URL`). `FORGEJO_MIGRATE_TOKEN` authenticates to a **different** instance — the one
you're copying *from*.

1. Mint a token on the **source** instance (its Settings → Applications), not on your own.
2. Give it **read scopes only** — `read:repository`, plus `read:issue` if you're migrating
   issues and PRs. It never needs write: the migration only reads from the source.
3. Set it as `FORGEJO_MIGRATE_TOKEN` in the same `env` block as your other tokens.
4. Call `migrate_repo` with `auth_username` set (or `authenticate: true` for token-only
   forges like GitHub). Without one of those the token is not sent at all.

> **Understand where this token goes before you set it.** Your read/write tokens travel as an
> `Authorization` header to `FORGEJO_URL` and nowhere else. This one is different: it goes in
> the *request body*, and your destination instance then presents it to the source host on your
> behalf. The destination sees it in cleartext. That's inherent to any server-side migration
> API, not a choice this server makes — but it means you're extending trust to the destination
> operator. Scope the token to the single repository if your source supports it, and revoke it
> once the migration lands.
>
> With `mirror: true` the destination must retain the credential to keep re-fetching, so it
> will persist in that instance's database rather than being used once and discarded. Prefer a
> one-shot migration unless you actually want an ongoing pull mirror.

### Wire it into Claude Code

```sh
claude mcp add --scope user forgejo /path/to/target/release/forgejo-mcp-rs \
  --env FORGEJO_URL=https://codeberg.org \
  --env FORGEJO_TOKEN_READ_ONLY=your_read_token_here
# add --env FORGEJO_TOKEN_WRITE=… only if you want the (gated) write tools
# add --env FORGEJO_MIGRATE_TOKEN=… only to migrate from a *private* source instance
```

### Or Claude Desktop

```json
{
  "mcpServers": {
    "forgejo": {
      "command": "/path/to/target/release/forgejo-mcp-rs",
      "env": { "FORGEJO_URL": "https://codeberg.org", "FORGEJO_TOKEN_READ_ONLY": "your_read_token_here" }
    }
  }
}
```

Logs go to **stderr** (stdout is the MCP transport); control verbosity with `RUST_LOG`, e.g.
`RUST_LOG=forgejo_mcp_rs=debug`.

## Tools

| Tool |  | Notes |
|---|---|---|
| `whoami` | read | The authenticated user (verifies the token) |
| `version` | read | This MCP server's version + the connected Forgejo instance's version |
| `list_my_repos` | read | Your repositories (auto-paginated, slimmed) |
| `list_issues` / `get_issue` | read | Issues in `owner/repo` (open by default) |
| `list_pull_requests` / `get_pull_request` | read | Pull requests in `owner/repo` (open by default) |
| `get_repo` | read | One repository's details (incl. default branch and size in KiB), slimmed |
| `list_branches` | read | Branches in `owner/repo` (auto-paginated, slimmed to name/commit/protected) |
| `get_file_contents` | read | Read a file (decodes text) or list a directory (`owner/repo/path`, optional `ref`) |
| `search_repos` | read | Repository search by keyword |
| `list_orgs` | read | Organizations you belong to |
| `list_notifications` | read | Your notification threads, slimmed (`all=true` for read+unread) |
| `list_issue_comments` | read | Comments on an issue/PR (slimmed) |
| `list_pull_request_reviews` | read | Reviews on a PR — approve/request-changes/comment verdicts + summary bodies (inline comments as a count) |
| `list_workflow_runs` | read | Forgejo Actions (CI) runs in `owner/repo`, slimmed; filter by `head_sha`/`ref`/`status`/`event`/`workflow_id`. Outcome is in each run's `status` (no separate conclusion) |
| `get_workflow_run` | read | One workflow run by `run_id` (full detail) |
| `write_status` | read | Report write-mode state (token configured? active? minutes left?) |
| `enable_write_mode` / `disable_write_mode` |  | Enter/leave the time-boxed write mode |
| `create_repo` | **write** | Create a repo (defaults to private) |
| `migrate_repo` | **write** | Copy a repo in from **another** instance — the only tool that carries issues/PRs across instances. Async (poll `get_repo`); leaves the source untouched; credential from `FORGEJO_MIGRATE_TOKEN` |
| `edit_repo` | **write** | Edit repo settings — visibility, description, website, default branch, issues/PRs/wiki toggles, archive. Only provided fields change; no renames |
| `create_branch` | **write** | Create a branch (owner/repo/new_branch, optional old_ref) |
| `create_issue` | **write** | Create an issue (owner/repo/title, optional body) |
| `create_pull_request` | **write** | Open a PR (owner/repo/title/head/base, optional body) |
| `comment_on_issue` | **write** | Comment on an issue/PR (owner/repo/index/body) |
| `delete_repo` | **write** | Delete a repo (needs `confirm = "owner/repo"`) |
| `add_push_mirror` | **write** | Auto-push a repo to an external remote (e.g. a GitHub mirror); credential from `FORGEJO_MIRROR_TOKEN` or `use_ssh=true` |
| `list_push_mirrors` | **write** | List a repo's push mirrors (admin-scoped; secrets never returned) |
| `delete_push_mirror` | **write** | Remove a push mirror by `remote_name` |
| `sync_push_mirrors` | **write** | Trigger an immediate push-mirror sync |
| `dispatch_workflow` | **write** | Trigger an Actions workflow via `workflow_dispatch` (owner/repo/`workflow` file name/`ref`, optional `inputs`); returns the created run |

Read list tools accept optional `state` (`open`/`closed`/`all`) and `page`/`limit`. Called
with no paging, `list_my_repos` / `list_issues` / `list_pull_requests` auto-paginate the whole
set and return a `{ returned, total, truncated, items }` envelope; pass an explicit `page` or
`limit` for a single page, which returns `{ page, limit, returned, total, items }` instead
(`total` is `null` for `search_repos`, which reports no count). Repository, notification,
comment, and review results are slimmed to the fields that matter. The **write** tools require
write mode (above); editing existing issues/PRs is future work — see the
[specification](SPECIFICATION.md).

### Moving a repo between instances

`migrate_repo` wraps Forgejo's `POST /repos/migrate`, which you call on the **destination** —
`clone_addr` points at the source. Unlike a push mirror, which replicates git refs and nothing
else, this can bring the issues, PRs, labels, milestones, releases and wiki with it. Three
things to know:

- **Set `service`.** It defaults to `git`, a bare clone that copies refs only. Name the source
  forge (`gitea` for a Forgejo or Codeberg source — there is no `forgejo` value) to enable the
  API-based importer that the metadata flags depend on.
- **The content flags default to off.** `issues`, `pull_requests`, `labels`, `milestones`,
  `releases`, `wiki` and `lfs` are each opt-in, matching the API's own defaults.
- **It's asynchronous, and it's a copy.** The call returns a repo record immediately while the
  import runs in Forgejo's task queue, so poll `get_repo` to see it land. The source repository
  is never modified — retiring it is a separate, deliberate step.

Pass `mirror: true` to keep the result as a *pull* mirror that periodically re-fetches from the
source, instead of taking a one-shot copy.

For a private source, set `auth_username` (or `authenticate: true` for token-only forges) and the
server sends `FORGEJO_MIGRATE_TOKEN` as the credential. As with push mirrors, the token is never
a tool argument, so it stays out of the conversation. See
[Migration source token](#migration-source-token-optional) for how to mint and scope it — and
for where it ends up, which is not where the other tokens go.

## Woodpecker server (`woodpecker-mcp`)

The companion server for a [Woodpecker CI](https://woodpecker-ci.org/) instance running alongside
your Forgejo. It reads repositories and pipelines and, behind the same write mode, triggers,
cancels, and restarts them — a separate process with its own token and tool namespace.

### Configure

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `WOODPECKER_URL` | **yes** | — | Instance base URL, e.g. `https://ci.codeberg.org` (Codeberg's hosted Woodpecker) or your own. No default. |
| `WOODPECKER_TOKEN_READ_ONLY` | **yes** | — | Personal access token (or `WOODPECKER_TOKEN`). |
| `WOODPECKER_TOKEN_WRITE` | no | — | A **second, different** token that enables the pipeline write tools — see the note below. |
| `WOODPECKER_WRITE_MINUTES` | no | `10` | Default write-mode window (minutes, max 60). |

Mint a personal access token from your Woodpecker profile (user icon, top right). If your repos are
on Codeberg, its hosted Woodpecker is at `https://ci.codeberg.org` — log in with your Codeberg
account and copy the token from your profile page.

**Read vs write — a Woodpecker limitation.** Woodpecker issues **one token per user** and does *not*
scope it read-only vs write (your rights come from your forge repo access — pull vs push). But the
write tools here require `WOODPECKER_TOKEN_WRITE` to be a **different** token from the read one. So:

- **Read-only (recommended):** set `WOODPECKER_TOKEN_READ_ONLY` only, leave `WOODPECKER_TOKEN_WRITE`
  unset — the write tools stay disabled.
- **To enable trigger/cancel/restart:** you need a second, distinct token, which in practice means a
  **second Woodpecker/forge account** (a bot with push access), since one account yields only one
  token.

Write mode itself works exactly like the Forgejo server (`enable_write_mode` / `write_status`).

Woodpecker addresses repositories by a **numeric `repo_id`**, not `owner/name`. Use `lookup_repo`
(or read the `id` from `list_repos`) to resolve a name to its id, then pass that id to the other
tools.

### Wire it into Claude Code

```sh
claude mcp add --scope user woodpecker /path/to/target/release/woodpecker-mcp \
  --env WOODPECKER_URL=https://ci.example.org \
  --env WOODPECKER_TOKEN_READ_ONLY=your_read_token_here
# add --env WOODPECKER_TOKEN_WRITE=… only if you want the (gated) pipeline write tools
```

### Tools

| Tool |  | Notes |
|---|---|---|
| `whoami` | read | The authenticated user (verifies the token) |
| `list_repos` | read | Repositories you can access (auto-paginated); each item carries the numeric `id` |
| `lookup_repo` | read | Resolve `owner/name` → the repo record, including its numeric `id` |
| `get_repo` | read | One repository's details by `repo_id` |
| `list_pipelines` | read | A repo's pipeline runs by `repo_id`, newest first (auto-paginated); a run's outcome is its `status` |
| `get_pipeline` | read | One pipeline by `repo_id` + its per-repo `number` |
| `write_status` | read | Report write-mode state (token configured? active? minutes left?) |
| `enable_write_mode` / `disable_write_mode` |  | Enter/leave the time-boxed write mode |
| `trigger_pipeline` | **write** | Start a pipeline (`repo_id`, optional `branch` and `variables`) |
| `cancel_pipeline` | **write** | Cancel a running pipeline (`repo_id`/`number`) |
| `restart_pipeline` | **write** | Re-run a pipeline (`repo_id`/`number`); returns the new run |

## Security

The token is read from the environment only — never logged, never written to disk (the client
holds it in a zeroized buffer and marks the `Authorization` header sensitive). Read-only by
default, so the server cannot modify your account without a separate write token and write mode.
Tool output is untrusted, repo-derived text — the server flags it as data, not instructions.
See [`SPECIFICATION.md`](SPECIFICATION.md#security-model).

The two *remote* credentials — `FORGEJO_MIRROR_TOKEN` and `FORGEJO_MIGRATE_TOKEN` — get the same
in-process handling (environment only, zeroized, never a tool argument, never returned), but they
are not header credentials and so do not stay between you and your own instance: each is sent to
your instance in a request body and relayed onward to a third-party host, which sees it in
cleartext. That is how Forgejo's mirror and migration APIs work, not a choice this server makes.
Scope both narrowly and treat them as disclosed to the remote operator.

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

## History

Releases through v0.5 were built on the [`forgejo-api`](https://codeberg.org/Cyborus/forgejo-api)
crate by Cyborus. `forgejo-mcp-rs` now talks to the Forgejo REST API through its own small
client and carries no third-party forge SDK.

## License

MIT — see [LICENSE.md](LICENSE.md) for details.

## MCP registry

Ownership-verification token for the [MCP registry](https://registry.modelcontextprotocol.io)
(read from this crate's rendered README on crates.io):

> Registry ownership token: `mcp-name: io.github.brechanbech/forgejo-mcp-rs`
