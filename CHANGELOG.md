# Changelog

Notable changes to `forgejo-mcp-rs`. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html), where a **0.x** minor bump is the
breaking-change slot.

Design *rationale* for each release lives in [`SPECIFICATION.md`](SPECIFICATION.md) — this file
records what changed, that one records why.

## [0.20.2] — 2026-09-18

### Added

- **`edit_repo` can set every repository unit**, not three of seven: `has_releases`,
  `has_actions`, `has_packages` and `has_projects` join `has_issues`, `has_pull_requests` and
  `has_wiki`. The gap was not academic — `has_releases` is exactly what the release tools added
  in 0.20.1 need, so the one setting that unblocks them was the one setting unreachable through
  this server, and turning it on meant leaving for the web UI. `has_actions` gates the workflow
  tools the same way.

### Fixed

- **A release 404 no longer reads as "no such release".** A repository with the Releases unit
  switched off returns `404` from *every* release endpoint — reads included, anonymously
  included — which is indistinguishable from a tag that simply has no release yet. Taken at face
  value, `get_release`'s 404 sent a caller straight into `create_release`, which then 404'd for
  a reason nothing had named. `get_release` now reports both readings and points at `get_repo`'s
  `has_releases`. Found live against a repository whose releases unit was off.

## [0.20.1] — 2026-09-18

### Added

- **Release tools — `list_releases`, `get_release`, `list_release_assets` (read) and
  `create_release`, `upload_release_asset`, `delete_release_asset` (write).** The Forgejo API
  has had full release CRUD all along; this server just never wrapped it, so publishing a build
  meant dropping out to `curl` with a token in the environment. `get_release` addresses a
  release by *tag* rather than id, which is what lets a release script be idempotent: the tag is
  known before the release exists, the numeric id only after. `delete_release_asset` exists
  because Forgejo keeps same-named assets side by side instead of replacing them, so re-running
  a release needs the old one removed first.
- **`FORGEJO_UPLOAD_ROOT` and `FORGEJO_UPLOAD_MAX_MB`.** `upload_release_asset` is the only tool
  that reads the local disk, and what it reads becomes publicly downloadable — so it is off
  until a root is configured, confined to that root with the path resolved through symlinks
  before the check (neither `..` nor a symlink escapes), limited to regular files under a size
  ceiling, and restricted to plain filenames, which Forgejo takes verbatim. There is
  deliberately no fallback to the working directory: an MCP server's cwd is whatever its client
  launched it from.
- **`RestClient::post_multipart`.** The release-asset upload is the one endpoint on this surface
  that is not JSON going in. `send` was split into `begin`/`finish` so the multipart path gets
  identical auth treatment rather than a second copy of the sensitive-header handling.
- **A `Forgejo vs Gitea` section in the README** — the field-by-field comparison for workflow
  runs, the per-flavor request differences, and the three details that are invisible in the
  OpenAPI specs and only show up in live responses (`path` carries a ref, `head_branch` is null
  for tags and pull requests, unset values are `""` rather than null). Written down because the
  knowledge was only in Rust doc comments and test fixtures, where a second consumer of these
  APIs could not find it, and re-deriving it from the specs produces the wrong answer.

### Changed

- **The Actions tool description and the server instructions no longer undersell the
  divergence.** Both said to read the outcome from `status`, "which on Gitea carries the run's
  conclusion" — true, and misleading: it reads as though one field differs, when a workflow run
  shares only `id`, `event` and `html_url` between the two forges. A consumer that took it at
  face value would decode a Gitea run into a near-empty object without any error, since every
  other field is optional. They now say plainly that the output is a translation and that its
  field names are this server's own.

## [0.20.0] — 2026-09-15

Gitea support. The server now talks to Gitea as well as Forgejo and Codeberg, from one binary,
with the flavor detected automatically.

Almost nothing had to change to get there: of the ~25 endpoints this server calls, only the
Actions (CI) ones differ between the two forges. Issues, pull requests, diffs, PR files,
branches, contents, search, orgs, notifications, push mirrors, and migration are identical in
path, method, and response shape, and were already working against Gitea untested.

### Added

- **Automatic flavor detection.** The instance is classified once, lazily, from `GET /version`,
  and only the Actions tools consult the result. Detection never fails the request: an
  unreachable instance falls back to Forgejo and is not cached, so a later call retries rather
  than living with a guess made during an outage.
- **`FORGEJO_FLAVOR`** — `forgejo`, `gitea`, or `auto` (the default) to pin the flavor when
  detection guesses wrong. An unrecognized value fails startup with a message naming the
  accepted ones, rather than being silently ignored.
- **`flavor` in the `version` tool's output**, so a client can see which forge it reached.
- **The write-mode tools now name the instance they apply to.** `enable_write_mode`,
  `disable_write_mode` and `write_status` all return an `instance` field, and the elevation note
  names it in prose — "Write mode is active on `https://gitea.com/` for 5 min" — because the note
  is what a model repeats back to the user. With several of these servers configured against
  different forges, each has its own independent write mode, so an unqualified "write mode is on"
  does not say which forge just became writable. `enable_write_mode` reports the `flavor` too.
  `write_status` deliberately does **not** detect it: that tool reports local elevation state and
  is exactly what you reach for when an instance is misbehaving, so it never blocks on a request
  to that instance and reports `flavor: null` until something else has settled it.

### Changed

- **`list_workflow_runs` normalizes both forges onto one run shape.** Gitea copied GitHub's
  vocabulary and Forgejo kept its own, so `head_sha`/`commit_sha`, `head_branch`/`prettyref`,
  `run_number`/`index_in_repo`, `display_title`/`title` and `created_at`/`created` are folded
  into one set of field names. Gitea's `conclusion` is promoted into `status` — where Forgejo
  already puts the outcome — and kept verbatim alongside it, so "did this run pass?" is the
  same question on both.
- **Run summary field names changed** as a consequence: `index_in_repo` → `run_number`,
  `prettyref` → `ref`, `workflow_id` → `workflow`, and a `conclusion` field appears on Gitea.
  Forgejo's `commit_sha`, `title`, `status`, `created`, `started` and `stopped` keep their
  names. This is the breaking part of the release.
- **`version` tool: `forgejo` → `instance_version`.** The old key was simply wrong on a Gitea
  instance.
- **Run filters are translated per flavor.** Forgejo's `ref` (fully qualified) becomes Gitea's
  `branch` (bare), and a workflow filter moves from Forgejo's `workflow_id` query parameter to
  Gitea's separate `…/actions/workflows/{file}/runs` path. Translating matters because an
  unknown query parameter is ignored rather than rejected — sending Forgejo's spelling to Gitea
  would have silently returned *unfiltered* runs.
- **`dispatch_workflow` adapts to the reply.** `return_run_info` is a Forgejo extension, so it
  is omitted on Gitea, which answers `204 No Content`; the tool then returns an acknowledgement
  naming the workflow and ref, and points the caller at `list_workflow_runs` to find the run.
- Tool descriptions and the server's instructions no longer claim Forgejo-only facts, in
  particular that a run has no `conclusion` field.

### Security

- **rustls 0.23.41 → 0.23.45** (lockfile only), clearing RUSTSEC-2026-0285: TLS 1.3 handshake
  messages were accepted across encryption-level boundaries. Unrelated to the Gitea work, but
  it was failing `cargo deny` on the branch.

### Notes

- **Gitea's `path` is not a filesystem path.** It is `ci.yml@refs/heads/main` — the workflow
  file, an `@`, and the ref — and it is the only reliable source of the ref, because
  `head_branch` is null for tag and pull-request runs. Reading it as a path (an early version of
  this release did) reported the workflow as `head`. Caught by live testing, not by the specs.
- **An unknown `workflow_id` behaves differently per forge**: Forgejo returns an empty list,
  Gitea returns `404 workflow "x.yml" not found`, because the filter is a path segment there.
  The tool description now distinguishes that from the 404 that means Actions is disabled.
- `get_workflow_run` still returns the instance's full, unmodified run object, so it remains
  shaped differently on each forge by design. `list_workflow_runs` is the normalized view.
- Gitea requires a token for the Actions API even on public repositories.
- **Verified live against `gitea.com`** with a read-only token: flavor detection, the
  `FORGEJO_FLAVOR` override, startup rejection of a bad override, the whole read surface
  against a private repo, and `list_workflow_runs` / `get_workflow_run` against a public repo
  with 864 real runs — unfiltered and filtered by `workflow_id`, `ref`, `status` and
  `head_sha`, across branch, tag and pull-request runs.
- **`dispatch_workflow` on Gitea is still unverified**: testing it means triggering CI on a repo
  one does not own. See [`SPECIFICATION.md`](SPECIFICATION.md) for exactly what that leaves
  unproven.

## [0.19.0] — 2026-08-28

Bounded reads. Every read tool should be incapable of dumping unbounded text into a model's
context; the list tools already were, via the auto-paginator's item cap. File reads and diffs
were not.

### Added

- **`start_line` / `end_line` on `get_file_contents`** — a 1-indexed, inclusive line window, with
  `total_lines` now always reported so a caller can page through a large file instead of pulling
  it whole. Bounds clamp rather than fail: `end_line` past the end stops at the last line, and a
  `start_line` past the end returns an empty slice with the window echoed. Only an inverted
  window (`start_line` after `end_line`) is an error.
- **`list_pull_request_files`** — the files a pull request changes, with `additions` /
  `deletions` / `changes` and `previous_filename` on a rename. Forgejo's three per-file URL
  fields are dropped. Auto-paginates to the complete set when `page` and `limit` are both
  omitted, matching `list_my_repos` and the other list tools; pass either to take single-page
  control. The auto-paginator's 1000-item cap still applies and surfaces as `truncated`.
- **`get_pull_request_diff`** — a pull request's unified diff. `file_path` narrows it to one
  file, matching either side of a rename, which is the intended path for review work. Without it
  the whole diff is truncated at 64 KiB (`max_bytes` overrides) at a line boundary and flagged
  `truncated`, with a note naming `list_pull_request_files`.
- `RestClient::get_text` — Forgejo serves `.diff` as `text/plain`, not JSON. `request()` was
  split into a raw `send()` plus a JSON parse layered on it; the existing verbs are unchanged.

### Changed

- The Non-goals entry on CI logs was **factually wrong** and has been corrected. It claimed
  "Forgejo exposes no repo-level endpoint" for logs; Forgejo v16 does serve per-job logs, and
  they can be read bounded and resumable. The real objection was only ever to *unbounded* logs.
  Still not implemented, but now recorded as a candidate rather than an impossibility.

### Notes

- Forgejo's `/pulls/{index}/files` does **not** return the `patch` field GitHub's equivalent
  does, so the file list cannot carry hunks and the second call to `get_pull_request_diff` is
  unavoidable rather than a design choice. Verified against `codeberg.org/api/v1`.
- The diff parser only scans for `---` / `+++` path markers *before* the first `@@`. Inside a
  hunk, a removed line whose content begins with `-- ` is rendered as `--- ` and would otherwise
  be misread as a file header. Covered by a unit test, alongside a fixture of real fetched
  Codeberg diff output.
- Verified end-to-end through the server against live Codeberg: file counts and rename metadata
  match the raw API, file-scoped diffs carry no bleed from neighbouring files, truncation lands on
  a line boundary with an exact `total_bytes`, and the line window clamps and errors as specified.
  The rename match was confirmed on **both** sides of a pure rename (`forgejo/forgejo` PR 13957),
  which is the case with no `---`/`+++` markers at all — so it exercises the `diff --git` header
  parse on its own.

## [0.18.0] — 2026-08-22

### Removed

- **The `woodpecker-mcp` binary.** The companion Woodpecker CI server that shipped in this crate
  from v0.13.0 now lives in its own repository and crate:
  [`woodpecker-mcp`](https://codeberg.org/brechanbech/woodpecker-mcp). Its tool surface did not
  change in the move. If you were using it, install it from the new crate and repoint your MCP
  client; `WOODPECKER_*` configuration is unchanged. This crate is a single binary again, so
  `cargo install --path .` no longer needs `--bin`.

  Woodpecker is not a Forgejo component — it drives Gitea, GitHub, GitLab, and Bitbucket equally,
  and that server only ever called Woodpecker's own API, never Forgejo's. Bundling it here was
  packaging, not coupling, and it hid the server from every Woodpecker user not running Forgejo.

- `mcp_core::Auth` — with only the Forgejo client left, the credential scheme is no longer a
  choice; `Authorization: token <t>` is hardcoded in `RestClient`.
- `RestClient::post_none` — used only by the Woodpecker pipeline-restart endpoint.

### Changed

- `list_tools` is no longer an `async fn` (nothing in it awaits); it returns a ready future.
  This clears a `clippy::unused_async_trait_impl` failure that a newer clippy had started
  reporting on the previous code.
- Documentation throughout no longer describes this as a two-server crate.

## [0.17.0] — 2026-07-29

### Added

- **`migrate_repo`** — copy a repository in from **another** forge or instance, wrapping
  `POST /repos/migrate`. This is the only tool that carries issues, pull requests, labels,
  milestones, releases and the wiki *across* instances; push mirrors replicate git refs and
  nothing else. Called on the destination, with `clone_addr` naming the source. Write-mode
  gated, like every other write tool.
- **`FORGEJO_MIGRATE_TOKEN`** — optional credential for a private migration source. Follows
  `FORGEJO_MIRROR_TOKEN`'s handling (server environment only, `Zeroizing`, never a tool
  argument, never logged or returned) but is deliberately a **separate variable**: the mirror
  token authenticates to a push *target*, this one to a source being *read from*, and those are
  generally different hosts. Sharing one variable would send a credential to a host it was never
  issued for.

### Changed

- Security section of the README no longer describes all tokens as header credentials. That is
  accurate for the read/write tokens, but `FORGEJO_MIRROR_TOKEN` and `FORGEJO_MIGRATE_TOKEN`
  travel in a request body and are relayed onward to a third-party host, which sees them in
  cleartext — a materially different trust posture, now stated as such.

### Notes

- `migrate_repo` is **asynchronous**: the call returns a placeholder repository while the import
  runs in Forgejo's task queue. Poll `get_repo` to confirm it landed.
- It **copies**; the source repository is never modified. Retiring the original stays a separate,
  deliberate act.
- `service` defaults to `git` (a bare clone, refs only). Name the source forge — `gitea` for a
  Forgejo or Codeberg source, there is no `forgejo` value — to engage the importer the metadata
  flags depend on. Unknown values are rejected locally with the valid list.
- Content flags (`issues`, `pull_requests`, `labels`, `milestones`, `releases`, `wiki`, `lfs`)
  are opt-in, matching the API's own defaults. `private` is the one override: it defaults to
  `true`, as in `create_repo`.
- **Not yet exercised against a live instance.** Unit tests cover body construction, validation
  and the credential rules; tool registration and the write-mode gate were smoke-tested over
  stdio. The `POST /repos/migrate` round-trip, the async import settling, and whether
  `service: "gitea"` really carries issues and PRs remain unverified.

## [0.16.0] — 2026-07-29

### Changed

- Migrated to **rmcp 3** / MCP protocol **2026-07-28**. No tool changes; the surface is
  identical to 0.15.0. `Content` became `ContentBlock`, `list_tools` is overridden in both
  servers to attach the now-required `ttlMs` / `cacheScope` hints, `server_info` is set
  explicitly (both servers had been identifying themselves as `rmcp 3.0.0`), and `base64` moved
  to 0.23 to match rmcp's.
- Documentation no longer pins a specific Forgejo instance version.

## [0.15.0] — 2026-07-24

### Added

- **`edit_repo`** — write-mode `PATCH /repos/{owner}/{repo}` with a partial `EditRepoOption`:
  visibility, description, website, default branch, issues/PRs/wiki toggles, archived. Only the
  fields provided are sent; a call with nothing to change is refused rather than issuing a no-op
  `PATCH`. Renaming is deliberately not exposed — Codeberg renames are unreliable.

## [0.14.0] — 2026-07-09

### Added

- Repository size (KiB) exposed in `get_repo` and `list_my_repos`.

## [0.12.0] — 2026-07-07

### Added

- Forgejo Actions (CI) tools: `list_workflow_runs`, `get_workflow_run` (both read-only) and
  `dispatch_workflow` (write-mode).

## [0.11.0] — 2026-06-25

### Added

- Push-mirror management: `add_push_mirror`, `list_push_mirrors`, `delete_push_mirror`,
  `sync_push_mirrors`, all write-mode gated, with the remote credential taken from
  `FORGEJO_MIRROR_TOKEN` rather than a tool argument.

## [0.10.1] — 2026-06-24

### Changed

- `reqwest` switched to rustls with bundled roots; `CDLA-Permissive-2.0` allowed in
  `cargo deny`.

## Earlier releases

Versions before 0.10.1 — including the split into a Cargo workspace, the Woodpecker CI server,
the collapse back into one crate with two binaries, and the move off the third-party
`forgejo-api` crate at v0.5 — are recorded in the git history and in
[`SPECIFICATION.md`](SPECIFICATION.md).
