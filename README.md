# forgejo-mcp-rs

[![CI](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions/workflows/ci.yml/badge.svg)](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions)

A [Model Context Protocol](https://modelcontextprotocol.io/) server for
[Forgejo](https://forgejo.org/), [Codeberg](https://codeberg.org) and
[Gitea](https://about.gitea.com/). It lets an MCP client (Claude Code, Claude Desktop, …)
read your forge — the authenticated user, repositories, issues, and pull requests — over the
Forgejo/Gitea REST API.

> Status: **read-only by default, with opt-in guarded writes (since v0.2).** Read tools across
> the forge — user, repos, issues, pull requests, search, orgs, notifications, comments,
> reviews, and Actions (CI) runs — plus guarded writes (`create_repo`, `edit_repo`,
> `create_branch`, `create_issue`, `create_pull_request`, `comment_on_issue`, `delete_repo`,
> `migrate_repo`, push-mirror
> management, and `dispatch_workflow`) gated behind a separate write token and a deliberate,
> time-boxed **write mode**. See [`SPECIFICATION.md`](SPECIFICATION.md) for the full design.

It speaks the Forgejo REST API directly through a small, in-house client (`src/forgejo/client.rs`,
over the `src/mcp_core/` transport) — an **independent implementation over the documented
API**, not a port of any other server. There is no third-party forge SDK in the trust path, so the
tool surface holding your token is code you can read and audit end to end.

The server speaks **MCP protocol version 2026-07-28** (since v0.16) and negotiates down to
`2024-11-05`, so older clients keep working unchanged. It runs over **stdio** and exposes
**tools only** — no resources, prompts, sampling, or roots.

## Build

```sh
cargo build --release   # target/release/forgejo-mcp-rs
cargo install --path .  # install to ~/.cargo/bin
```

## Woodpecker CI

A companion server for [Woodpecker CI](https://woodpecker-ci.org/) shipped in this crate as a
second `woodpecker-mcp` binary from v0.13.0 through v0.17.0. **It now lives in its own repository
and crate:** [`woodpecker-mcp`](https://codeberg.org/brechanbech/woodpecker-mcp).

Woodpecker is its own system — it drives Gitea, GitHub, GitLab, and Bitbucket as readily as
Forgejo, and that server never called the Forgejo API at all — so bundling it here made it
invisible to everyone not running Forgejo. Nothing about it changed in the move; if you were using
the `woodpecker-mcp` binary from this crate, install it from the new crate instead and point your
MCP client at the new path.

## Configure

The server is configured by environment variables:

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `FORGEJO_TOKEN_READ_ONLY` | **yes** | — | Read token (or `FORGEJO_TOKEN`). **Read-only scopes are enough.** |
| `FORGEJO_TOKEN_WRITE` | no | — | Write/delete-scoped token. **Providing it enables the write tools**; omit it for a pure read-only server. |
| `FORGEJO_WRITE_MINUTES` | no | `10` | Default write-mode window (minutes, max 60). |
| `FORGEJO_MIRROR_TOKEN` | no | — | Credential `add_push_mirror` sends as the remote's password (e.g. a GitHub PAT). Kept out of the conversation — never passed as a tool argument. Omit if you only use `use_ssh=true` mirrors. |
| `FORGEJO_MIGRATE_TOKEN` | no | — | Credential `migrate_repo` sends to the **source** instance it reads from. Also never passed as a tool argument. Kept separate from `FORGEJO_MIRROR_TOKEN` on purpose — that one authenticates to a push *target*, so sharing a variable would send a credential to a host it was never issued for. Omit if you only migrate public repos. |
| `FORGEJO_UPLOAD_ROOT` | no | — | Directory `upload_release_asset` may read files from. **Uploading is disabled while this is unset** — see [Release assets](#release-assets). |
| `FORGEJO_UPLOAD_MAX_MB` | no | `100` | Ceiling on one uploaded asset (MiB). The file is read into memory to be sent. |
| `FORGEJO_URL` | no | `https://codeberg.org` | Instance base URL. |
| `FORGEJO_FLAVOR` | no | `auto` | `forgejo`, `gitea`, or `auto` to detect from the instance version. Only the Actions (CI) tools consult it — see [Forgejo and Gitea](#forgejo-and-gitea). |

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

### Forgejo and Gitea

Forgejo began as a Gitea fork, and the REST surface is still very nearly the same one. Of the
~25 endpoints this server calls, **only the Actions (CI) ones differ.** Issues, pull requests,
diffs, PR files, branches, file contents, search, orgs, notifications, push mirrors, and
migration are identical in path, method, and response shape on both, so they need no special
handling and get none.

The flavor is detected once, lazily, from the instance's `GET /version` — and only the Actions
tools ever ask. Set `FORGEJO_FLAVOR` to `forgejo` or `gitea` to pin it if the detection ever
guesses wrong on your instance.

> The detection reads oddly on purpose: **Forgejo names Gitea in its own version string**
> (Codeberg reports `16.0.0-dev-741-6f391573+gitea-1.22.0`) to advertise API compatibility,
> while Gitea never names itself (`1.27.0+dev-954-g1f3981a301`). So a `gitea-` marker means
> Forgejo. Failing that marker, the major version decides: Gitea is still 1.x, Forgejo
> renumbered to 7 and beyond.

Where the two forges differ, and what the server does about it:

| | Forgejo | Gitea | Handling |
|---|---|---|---|
| Run filter by git ref | `ref`, fully qualified | `branch`, bare name | Translated; `refs/heads/main` works on both |
| Run filter by workflow | `workflow_id` query parameter | a separate `…/actions/workflows/{file}/runs` path | Translated |
| Run outcome | `status` alone | `status` (phase) plus `conclusion` | Normalized: `conclusion` is promoted into `status`, and also kept verbatim |
| Run field names | `commit_sha`, `prettyref`, `index_in_repo`, `title`, `created` | `head_sha`, `head_branch`, `run_number`, `display_title`, `created_at` | Normalized to one shape |
| Workflow file and ref | separate: `workflow_id` and `prettyref` | combined into `path`, as `ci.yml@refs/heads/main` | Split apart; the ref is shortened (`main`, `v0.16.0`) and a pull-request ref kept whole |
| Unknown workflow filter | empty list | `404 workflow "x.yml" not found` | Surfaced as-is; the tool description says which is which |
| `workflow_dispatch` reply | the created run (`return_run_info`) | `204 No Content` | Forgejo's run is passed through; on Gitea the tool returns an acknowledgement and tells the caller to find the run with `list_workflow_runs` |
| Workflow directory | `.forgejo/workflows`, `.github/workflows` | `.gitea/workflows`, `.github/workflows` | Mentioned in the `dispatch_workflow` tool description |

Translating rather than sending both spellings is deliberate: an unknown query parameter is
*ignored*, not rejected, so sending Forgejo's `ref` to Gitea would silently return **unfiltered**
runs — a filter that looks applied but isn't.

Gitea's `path` deserves its own warning, because the name lies: it is **not** a filesystem
path. It is the workflow file, an `@`, and the fully-qualified ref — `ci.yml@refs/heads/main`,
or `test-pr.yml@refs/pull/1117/head`. It is also the only reliable source of the ref, since
`head_branch` is populated for branch runs and null for tags and pull requests.

Two more things regardless of flavor. Gitea requires a token for the Actions API even on public
repositories. And `get_workflow_run` deliberately returns the instance's full, unmodified run
object, so that one *is* shaped differently on each forge; use `list_workflow_runs` when you
want the normalized view.

### Write mode

The server is **read-only by default.** `create_repo` / `edit_repo` / `delete_repo` work only when (a)
`FORGEJO_TOKEN_WRITE` is configured **and** (b) you've deliberately entered **write mode** via
`enable_write_mode` — a time-boxed elevation (default 10 min, max 60) that slides forward on
each write and auto-reverts. `write_status` reports the state; `delete_repo` also requires a
`confirm` argument equal to `"owner/repo"`.

`enable_write_mode`, `disable_write_mode` and `write_status` all report the `instance` they
apply to, and the elevation note names it in prose, so the announcement reads "write mode is
active on `https://gitea.com/`" rather than a bare "write mode is on". That matters once you
run **more than one of these servers at once** — a Codeberg one and a Gitea one, say — where
each has its own independent write mode and an unqualified warning tells you nothing about
which forge is now writable. `write_status` deliberately never calls the instance, so its
`flavor` is `null` until something else has detected it. See [`SPECIFICATION.md`](SPECIFICATION.md#write-mode-deliberate-time-boxed-elevation)
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

Point `FORGEJO_URL` at a Gitea instance and everything works the same way — the flavor is
detected for you:

```json
{
  "mcpServers": {
    "gitea": {
      "command": "/path/to/target/release/forgejo-mcp-rs",
      "env": { "FORGEJO_URL": "https://gitea.com", "FORGEJO_TOKEN_READ_ONLY": "your_read_token_here" }
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
| `version` | read | This MCP server's version, the connected instance's version, and its `flavor` (`forgejo` or `gitea`) |
| `list_my_repos` | read | Your repositories (auto-paginated, slimmed) |
| `list_issues` / `get_issue` | read | Issues in `owner/repo` (open by default) |
| `list_pull_requests` / `get_pull_request` | read | Pull requests in `owner/repo` (open by default) |
| `get_repo` | read | One repository's details (incl. default branch and size in KiB), slimmed |
| `list_branches` | read | Branches in `owner/repo` (auto-paginated, slimmed to name/commit/protected) |
| `get_file_contents` | read | Read a file (decodes text) or list a directory (`owner/repo/path`, optional `ref`). Optional `start_line`/`end_line` take a 1-indexed inclusive window, clamped to the file; `total_lines` is always reported |
| `search_repos` | read | Repository search by keyword |
| `list_orgs` | read | Organizations you belong to |
| `list_notifications` | read | Your notification threads, slimmed (`all=true` for read+unread) |
| `list_issue_comments` | read | Comments on an issue/PR (slimmed) |
| `list_pull_request_reviews` | read | Reviews on a PR — approve/request-changes/comment verdicts + summary bodies (inline comments as a count) |
| `list_pull_request_files` | read | Files a PR changes (auto-paginated), with per-file additions/deletions and rename info. Forgejo omits the hunks here — use `get_pull_request_diff` for content |
| `get_pull_request_diff` | read | A PR's unified diff. `file_path` narrows it to one file (matching either side of a rename); otherwise truncated at 64 KiB, raise with `max_bytes` |
| `list_workflow_runs` | read | Actions (CI) runs in `owner/repo`, slimmed to one shape on both forges; filter by `head_sha`/`ref`/`status`/`event`/`workflow_id`. Outcome is in each run's `status` |
| `get_workflow_run` | read | One workflow run by `run_id` (full detail) |
| `list_releases` | read | A repo's releases, newest first (auto-paginated, slimmed to identity + assets) |
| `get_release` | read | One release by git **tag** — the lookup that makes a release script idempotent |
| `list_release_assets` | read | A release's attached files (`id`, name, size, download URL) by `release_id` |
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
| `create_release` | **write** | Create a release on a tag (`tag_name`; `target_commitish` creates the tag when it does not exist) |
| `upload_release_asset` | **write** | Attach a **local file** to a release. Confined to `FORGEJO_UPLOAD_ROOT`; disabled entirely when that is unset |
| `delete_release_asset` | **write** | Remove one asset by `attachment_id` — needed to replace a same-named file, which Forgejo would otherwise keep alongside |
| `dispatch_workflow` | **write** | Trigger an Actions workflow via `workflow_dispatch` (owner/repo/`workflow` file name/`ref`, optional `inputs`); returns the created run on Forgejo, an acknowledgement on Gitea |

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

### Release assets

Publishing a build is three calls: look the tag up with `get_release`, `create_release` if that
404s, then `upload_release_asset` per file. Re-running is safe as long as you delete a
same-named asset first — Forgejo keeps both otherwise, rather than replacing.

`upload_release_asset` is the only tool that reads the local disk, and whatever it reads becomes
a publicly downloadable file. So it is confined rather than trusted:

- **Off unless configured.** With `FORGEJO_UPLOAD_ROOT` unset, every upload is refused. There is
  deliberately no fallback to the working directory: an MCP server's cwd is whatever its client
  happened to launch it from, which is no basis for deciding what may be published.
- **Confined to that root.** The path is resolved through symlinks *before* the check, so
  neither a `..` traversal nor a symlink pointing out of the tree escapes it.
- **Bounded.** Regular files only, at most `FORGEJO_UPLOAD_MAX_MB` (default 100).
- **Plain names only.** The published name defaults to the file's own and may not contain path
  separators, since Forgejo takes it verbatim.

Point the root at the tree you actually release from, not at `$HOME`:

```jsonc
"env": {
  "FORGEJO_UPLOAD_ROOT": "/Users/you/Developer",
  "FORGEJO_UPLOAD_MAX_MB": "100"
}
```

## Forgejo vs Gitea

The server talks to both from one binary, detecting the flavor from `GET /version`. Of the ~25
endpoints it calls, the two forges agree on all but Actions — but where they disagree, they
disagree completely, so this is the reference for what gets translated and what does not.

Everything below was checked against the two published specs, `codeberg.org/swagger.v1.json` and
`gitea.com/swagger.v1.json`, compared definition by definition. The three points marked *live*
could not have come from the specs at all: they type the fields in question as bare strings.

### A workflow run is a different object on each forge

Gitea copied GitHub's vocabulary, Forgejo kept its own. The definitions are not even named alike
— Forgejo's `ActionRun` against Gitea's `ActionWorkflowRun` — and of the fields worth reading,
only `id`, `event` and `html_url` are spelled the same:

| what it is | Forgejo | Gitea | this server |
|---|---|---|---|
| run counter | `index_in_repo` | `run_number` | `run_number` |
| title | `title` | `display_title` | `title` |
| outcome | `status` | `conclusion` | `status` |
| phase | — | `status` | folded into `status` |
| workflow file | `workflow_id` | file part of `path` | `workflow` |
| ref | `prettyref` | `head_branch`, or the ref in `path` | `ref` |
| commit | `commit_sha` | `head_sha` | `commit_sha` |
| who triggered it | `trigger_user` | `trigger_actor`, `actor` | dropped (carries an email) |
| times | `started`, `stopped`, `created` | `started_at`, `completed_at`, `created_at` | `started`, `stopped`, `created` |

Three things are visible only from live data:

- ***live*** — **`path` is not a path.** Gitea reports `ci.yml@refs/heads/main`: workflow file,
  `@`, fully qualified ref. Reading it as a file path yields `head` as the "workflow", which is
  what this server's first live probe returned.
- ***live*** — **`head_branch` is null for tag and pull-request runs**, so the ref has to come
  out of `path`.
- ***live*** — **unset means `""`, not null.** A running Gitea run has `conclusion: ""` and
  `completed_at: ""`. A consumer that decodes those as dates fails on the empty string, so a
  single queued run can fail a whole listing.

Gitea's `status` reports only `queued` / `in_progress` / `completed`, with the result arriving
separately in `conclusion` once the run ends. This server promotes the conclusion into `status`,
where Forgejo already puts the outcome, and keeps it verbatim alongside, so "did this pass?" is
one question on both.

**The output field names in the last column are this server's own**, not either forge's. Do not
read them as the wire format.

### Requests differ too

An unknown query parameter is *ignored* rather than rejected, so the wrong spelling returns
unfiltered results instead of an error:

| filter | Forgejo | Gitea |
|---|---|---|
| git ref | `ref`, fully qualified | `branch`, bare name |
| workflow file | `workflow_id` query | a `…/actions/workflows/{file}/runs` path |
| `head_sha`, `status`, `event` | same | same |

Unfiltered, the listing path is identical on both. `dispatch_workflow` differs as well:
`return_run_info` is a Forgejo extension and Gitea rejects unknown body fields on that endpoint,
so it is sent only to Forgejo — which is why Gitea answers with an acknowledgement rather than
the run.

### Everything else

Issues, pull requests, diffs, PR files, branches, contents, search, orgs, notifications, push
mirrors and migration are identical in path, method and response shape. Each forge does carry
fields the other lacks, but they are extras rather than disagreements — Forgejo has `pronouns` on
a user and `archive_download_count` on a release; Gitea has `time_estimate` on an issue and
`branch_count` on a repository.

The one real exception outside Actions is the contents endpoint, where each forge has a
last-commit field the other does not:

| | Forgejo | Gitea |
|---|---|---|
| when it last changed | `last_commit_when` | `last_author_date`, `last_committer_date` |
| the commit message | — | `last_commit_message` |

This server does not read those fields, but anything rendering a file listing needs both
spellings or the date column comes out blank on one forge.

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

Per-release changes are in [`CHANGELOG.md`](CHANGELOG.md); the design rationale behind each is
in [`SPECIFICATION.md`](SPECIFICATION.md).

Releases through v0.5 were built on the [`forgejo-api`](https://codeberg.org/Cyborus/forgejo-api)
crate by Cyborus. `forgejo-mcp-rs` now talks to the Forgejo REST API through its own small
client and carries no third-party forge SDK.

The companion Woodpecker CI server that shipped here as a second binary from v0.13.0 to v0.17.0
moved to its own repository at v0.18.0 —
[`woodpecker-mcp`](https://codeberg.org/brechanbech/woodpecker-mcp).

## License

MIT — see [LICENSE.md](LICENSE.md) for details.

## MCP registry

Ownership-verification token for the [MCP registry](https://registry.modelcontextprotocol.io)
(read from this crate's rendered README on crates.io):

> Registry ownership token: `mcp-name: io.github.brechanbech/forgejo-mcp-rs`
