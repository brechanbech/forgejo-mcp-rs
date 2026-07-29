# Changelog

Notable changes to `forgejo-mcp-rs`. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html), where a **0.x** minor bump is the
breaking-change slot.

Design *rationale* for each release lives in [`SPECIFICATION.md`](SPECIFICATION.md) — this file
records what changed, that one records why.

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
