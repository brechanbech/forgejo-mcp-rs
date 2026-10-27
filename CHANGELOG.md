# Changelog

Notable changes to `forgejo-mcp-rs`. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html), where a **0.x** minor bump is the
breaking-change slot.

Design *rationale* for each release lives in [`SPECIFICATION.md`](SPECIFICATION.md) — this file
records what changed, that one records why.

## [0.25.0] — 2026-10-27

### Changed
- **Breaking:** `get_workflow_run` answers in `list_workflow_runs`' normalized shape on both
  forges, not with the instance's raw run. The raw run was about 20 KB on Codeberg, with the
  whole push payload and the triggering user's email address.
- `create_repo`, `edit_repo` and `migrate_repo` answer in `get_repo`'s shape, and
  `create_branch` in `list_branches`'. They passed the forge's whole object through, with the
  owner's profile and email, merge settings, and the branch's full head commit.
- `get_repo` and the repository writes add `empty`, `mirror`, and `original_url` for a
  migrated repository.

## [0.24.0] — 2026-10-27

### Changed
- **Breaking:** `migrate_repo` refuses `auth_username` for the `gitea`, `github`, `gitlab`,
  `gogs` and `gitbucket` services. Their importers ignore the token once a username is given,
  which made every authenticated Gitea migration fail. Use `authenticate: true`.
- `migrate_repo` refuses a target name that already exists, before anything is created.
- rmcp 3.8 (from 3.4). The stdio transport is now `stdio_pipes`, which on Linux runs the
  client's pipes on Tokio's reactor rather than the blocking thread pool, and is plain stdio
  elsewhere.

### Fixed
- `migrate_repo` with `service: git` sends the token as the clone password (username
  `auth_username`, default `oauth2`); it could not authenticate before. `onedev` and
  `codebase` get it the same way and need `auth_username`.
- A failed `migrate_repo` names the empty repository it left behind, which has to be deleted
  before a retry, and after a server error with a credential sent, says the source likely
  refused it.
- The docs called the migration asynchronous and said to poll `get_repo`. It finishes within
  the call.

## [0.23.0] — 2026-10-27

### Added
- Six write tools, all behind write mode, for the steps an agent could not take
  before: close what it fixed, merge what was approved, clean up after itself.
  - `edit_issue` changes only the fields given: `state` (open/closed), `title`,
    `body`, `assignees`, `milestone`, `due_date`, and `labels` by name. It
    takes a pull request's number too, so it also closes a pull request
    without merging it.
  - `merge_pull_request` merges with a chosen `style`, an optional title and
    message, `delete_branch`, and a `head_sha` guard that refuses the merge
    when something was pushed after the caller looked. Forgejo and Gitea spell
    the merge fields differently (`Do`/`MergeTitleField` against
    `do`/`merge_title_field`), and each gets its own.
  - `create_pull_request_review` approves, requests changes or comments.
  - `delete_branch`.
  - `create_or_update_file` and `delete_file` commit one file change each.
    An update or a deletion takes the file's current `sha` and is refused
    when it is stale. Content is UTF-8 text; the base64 is done here.
- File paths and branch names are percent-encoded segment by segment, and an
  empty, `.` or `..` segment is refused: request URLs are built by joining
  onto the API root, which would resolve `..` into a different endpoint.

### Fixed
- `get_file_contents` read the wrong path for a file name with `#` in it, which
  became a URL fragment. Paths are now encoded.
- `delete_branch` on a branch that does not exist reported Codeberg's empty 500
  or gitea.com's bare 404. It now says there was no such branch and nothing was
  deleted.
- `edit_issue` names any label the forge did not apply in `labels_not_found`;
  both forges drop an unknown label name without an error.

## [0.22.1] — 2026-10-24

### Added
- Every tool carries a `title` and MCP tool annotations. The 33 tools that only
  read are `readOnlyHint: true`. The writes say whether they can overwrite or
  remove something (`destructiveHint`: the edits, deletes, push-mirror sync,
  cancel and rerun) and whether repeating them changes anything further
  (`idempotentHint`). Everything that reaches the forge is `openWorldHint: true`;
  the write-mode switches and `write_status` touch only the server's own state.
  The title is repeated in `annotations.title` for clients that predate the
  top-level field. Until now `tools/list` carried no annotations at all, so a
  client had to take every tool for a destructive open-world write.

## [0.22.0] — 2026-10-24

### Changed
- Issues are slimmed like pull requests. The raw object carried the author's
  full profile, email included, the repository and every label's colour, and
  15 open `forgejo/forgejo` issues came to 53 KB. `list_issues` gives number,
  title, state, author, label names, assignees, milestone, comment count and
  dates, without the body. `get_issue` and `create_issue` add the body,
  `locked`, `due_date` and `ref`, and `pull_request: true` when the number
  belongs to a pull request.

### Fixed
- `list_issues` listed pull requests as well. Both forges serve them as issues
  unless asked for `type=issues`. On `forgejo/forgejo`, 154 of the 1,728 open
  "issues" were pull requests, and half of the first 50 returned. The tool now
  always sends `type=issues`.

## [0.21.0] — 2026-10-24

### Added
- `cancel_workflow_run` and `rerun_workflow`, both in write mode. Cancel needs
  Forgejo 16+ and rerun Forgejo 17+; Gitea has both. `rerun_workflow` with
  `job_id` reruns one job and the jobs that depend on it.
- Both answer with the run as it now stands, in `list_workflow_runs`' shape.
  Forgejo's reply is an empty `204`, and its cancel succeeds on a run that has
  already finished, so only the run's `status` says what happened.
- Rerun does not check the version first. Codeberg reports a development build
  of Forgejo 16, so the version number does not say reliably which routes
  exist. An instance without the endpoint answers with a plain-text `404`,
  unlike the JSON `404` for an unknown run. That is reported as a missing API,
  with the version the instance reports, and nothing is rerun.
- `list_pull_requests` takes `base` and `head` branch filters. With
  `state=open`, it can answer whether a branch already has an open pull request
  without listing them all. `head` is a branch name, or `owner:branch` to
  require the fork as well. Forgejo 16+ filters by both. Gitea filters only by
  base, so there the head filter walks the list and filters it locally. Every
  answer is checked against both filters, because a forge that lacks one
  ignores it rather than refusing it.

### Changed
- Releases list their asset names instead of every asset in full.
  `list_releases` gives each release `asset_count` and `asset_names`, so 15
  `forgejo/forgejo` releases, with 21 files each, go from 66 KB of assets to
  11 KB. Each asset's id, size, download count and URL are in
  `list_release_assets`, which also gives the id `delete_release_asset` needs.
- `get_release`, `create_release` and `edit_release` return the same slim
  shape, plus the release notes and `target_commitish`, instead of the raw
  API object (about 8 KB per release on `forgejo/forgejo`).
- Pull requests are slimmed. The raw object was 12–15 KB, with both
  repositories embedded and the author's and reviewers' full profiles,
  including email addresses. `list_pull_requests` gives number, title, state,
  `merged`, `draft`, `mergeable`, `base`, `head`, `head_repo` (for a fork),
  `head_sha`, author, label names and dates. `get_pull_request` and
  `create_pull_request` add the description, `merged_by`, merge commit, merge
  base, assignees, requested reviewers, milestone, comment counts and
  `diff_url`.

### Fixed
- gitea.com was detected as Forgejo. It now reports version `29.0.0`, and
  detection still took any major version of 2 or more to mean Forgejo. On
  gitea.com, `list_workflow_runs` silently ignored its `ref` and `workflow`
  filters, and `dispatch_workflow` sent a field Gitea refuses. Detection now
  relies on the `+gitea-` compatibility marker that Forgejo puts in its version
  string, and nothing else. `FORGEJO_FLAVOR` still overrides it.
- `list_workflow_runs` on Forgejo ignored `limit` unless `page` was also
  given, and returned every run. On a large repository that never completed:
  `forgejo/forgejo`, with 210,515 runs, gets a `504` from Codeberg after 30
  seconds. The tool now always sends `page` and `limit`, defaulting to page 1
  of 30, and reports the values it sent.
- Dispatched and scheduled Forgejo runs reported no `event`. Forgejo leaves
  `event` empty for those runs and names the trigger in `trigger_event`, which
  is now read first.
- `list_pull_requests` without a `state` now returns open pull requests, as it
  always claimed to. It had been returning every state, because the tool sent
  no `state` and both forges default pull requests to all of them. On
  `forgejo/forgejo` that is 10,350 instead of 154, and the auto-paginated
  listing stopped at its 1,000-item cap. `state=open` is now sent explicitly,
  for issues too, though issues already defaulted to open.

### Security
- tokio 1.52.3 → 1.54.1, for RUSTSEC-2026-0386 (`AsyncFd::new` and related
  constructors violate I/O safety). This is a lockfile change only.

## [0.20.6] — 2026-10-23

### Fixed
- `list_workflow_runs` refuses an abbreviated `head_sha`. Both forges match
  the filter exactly, so `126a915` returned an empty list, which looks the same
  as "this commit has no runs". It now has to be the full 40-character SHA, or
  64 for a SHA-256 repository. Uppercase is lowercased rather than refused.
- A queued run no longer reports that it started and stopped in 1970. Forgejo
  sends "never" as the Unix epoch in the server's zone, not as an empty
  string. Run and job timestamps now treat the epoch and Go's zero time
  (`0001-01-01`) as absent.

## [0.20.5] — 2026-10-23

### Added
- `list_workflow_jobs` and `get_job_log`. The server could say *that* a CI run
  failed but not why. Forgejo 16 added per-job endpoints, and Gitea serves the
  same paths. List the run's jobs, then read the failed job's log.
- `get_job_log` returns the tail, the last 32 KiB by default, because that is
  where a failure is. It is fetched with an HTTP `Range` request, not
  downloaded whole, and cut to whole lines. `earlier` and `later` give the next
  window to ask for, so walking back from the tail loses and repeats nothing.
  `max_bytes` goes up to 1 MiB, and a larger request is refused.
- Colour codes and per-line timestamps are stripped. On this repository's own
  CI they are most lines and about a third of the bytes respectively.
  `timestamps=true` keeps the timestamps.
- `list_workflow_jobs` normalizes both forges like `list_workflow_runs`.
  Gitea's `conclusion` is promoted into `status`. Forgejo reports no
  timestamps or steps in this listing, and they stay absent there rather than
  appearing empty.

### Security
- TLS dependencies updated in the lockfile for four advisories published
  since 0.20.4: `rustls` 0.23.46 (RUSTSEC-2026-0346, `Debug` exposing session
  secrets; RUSTSEC-2026-0347, data sent to the outer server after an ECH
  rejection), `rustls-webpki` 0.103.16 (RUSTSEC-2026-0382, distrusted
  intermediates accepted) and `webpki-roots` 1.0.11 (RUSTSEC-2026-0381,
  distrusted emSign intermediates in the bundled roots). All semver-compatible;
  `cargo install` users pick them up by reinstalling.

## [0.20.4] — 2026-09-27

### Added
- `list_commits` and `get_commit`. The server could name a branch's head but
  could not read a single commit — no message, author or signature — which
  made questions about signing unanswerable without leaving the tools.
- Both are slimmed like the rest: sha, subject line, author, date and a
  signature verdict. The wire objects carry base64 `signature` and `payload`
  blobs of a few hundred bytes to a couple of kilobytes *each*, plus `files`
  and `stats` on every listing entry; a thirty-commit page is tens of
  kilobytes of material nobody reads.
- `signature` is `verified`, `unverified` or `unsigned`, decided by whether a
  signature is present rather than by the `reason` string, so an unrecognised
  `gpg.error.*` still classifies correctly. `signer` appears only on a
  verified commit and `reason` only on an unverified one — neither says
  anything about a commit nobody signed.

- `list_keys`. Both key kinds for an account in one answer, because "can this
  instance verify my commits" needs both. `verified` is the field that matters
  and is easy to miss: a key the forge holds but has not confirmed ownership of
  verifies nothing, and Gitea reports `gpg.error.no_gpg_keys_found` whether the
  key is absent or merely unverified — so the commit's reason cannot tell those
  apart and this can. GPG entries keep the addresses the key covers and whether
  each is activated, since a signature only verifies for a commit authored
  under an activated address.
- Key material is dropped: an SSH blob is a hundred-odd characters nobody
  compares by eye, and a GPG `public_key` is an armored block with `subkeys`
  nesting another copy of everything. Fingerprints and key ids identify them.

- `list_branch_protections`. What a repository enforces per branch name or
  glob, so "is signing required on main across all my repositories" is a
  script rather than forty trips through a web UI. An empty list means nothing
  is protected: a branch matching no rule is unprotected.
- Allowlists do not survive. The wire object carries a dozen arrays of
  usernames, teams and deploy keys naming who may bypass each gate; those
  answer who, and the question worth a tool is what is enforced.
- `enable_force_push` is Gitea-only and `apply_to_admins` Forgejo-only, both
  optional, absent meaning unreported. The first matters for signing: the
  signed-commit check leans on force push being refused, since it compares
  only the range a push introduces.

- `get_commit_statuses`. The individual checks reported against a ref, where
  `get_combined_status` gives only the rolled-up verdict. `target_url` is kept
  because neither forge exposes a logs API, so the link is often the only
  route to a failing run — both return it relative to the instance origin, so
  it needs the host prefixed. A ref carries one status per *transition*, not
  per check, so the newest entry for a context is its current state; the
  description says so, since reporting a pending check that finished ten
  minutes ago is the obvious way to misread it.
- `get_repo_tree`, with `recursive` to walk the whole tree in one request —
  "where is every file of this kind" without descending directory by
  directory. `truncated` is reported even when false: the forge stops early on
  a large tree, and concluding a file is absent from a truncated listing is
  unsound.
- `get_combined_status`, restoring what v0.12 dropped. An early `ci_status`
  tool on this endpoint was removed because it answered `state: ""` with
  `total_count: 0` for Actions repositories; that is no longer so on either
  forge — Codeberg answers `pending`/2 for this repository's own CI and
  gitea.com `success`/4 for `gitea/tea`. It is the tool for "did CI pass?",
  where `get_commit_statuses` returns the transition history instead.
- `list_repo_contents` was considered and not added: `get_file_contents`
  already lists a directory, and an empty path lists the root.

### Fixed
- `server.json` advertised 0.20.0 in both its version fields, having never been
  updated for 0.20.1 through 0.20.3. It ships inside the crate and feeds the
  MCP registry, so four releases went out describing themselves as an older
  one. Nothing referenced the file — no script, no CI, no test — which is how
  it drifted; SPECIFICATION.md now carries a release checklist that names it.

### Forge divergence
- `ContentsResponse` differs, which affects directory listings rather than the
  tools added here: Forgejo carries `last_commit_when`, Gitea carries
  `last_author_date`, `last_committer_date`, `last_commit_message`, `mode` and
  LFS fields instead. `CommitStatus`, `CombinedStatus` and `GitTreeResponse`
  are identical on both.
- **Gitea does not expose SSH key verification at all.** Forgejo's `PublicKey`
  carries `verified`; Gitea's carries `last_used_at` and no `verified`, so the
  state its web UI displays as "Verified Key" is absent from its API. Both
  instances were probed; the OpenAPI definitions disagree and only one of them
  was checked at first, which produced a wrong claim about what the tool could
  answer.
- Hence `verified` is optional and absent is reported as absent. Defaulting it
  to false would assert that a verified key is unverified on every Gitea
  instance. On Gitea neither this tool nor a commit's `reason` settles the
  question — `gpg.error.no_gpg_keys_found` covers both an absent key and an
  unverified one — and the web UI remains the only source.

### Notes
- Forgejo (codeberg.org) and Gitea (gitea.com) were probed directly: both
  endpoints return the same shape, field for field, including the
  verification block, so one summary serves both. The *semantics* differ —
  Gitea reports `gpg.error.no_gpg_keys_found` both for a key it does not hold
  and for one it holds but has not verified, which the tool description says
  so a caller does not over-read it.

## [0.20.3] — 2026-09-19

### Added

- **`edit_release`** — edit a release in place by `release_id`: `body` (the notes), `name`,
  `tag_name`, `target_commitish`, `draft` and `prerelease`. Only the fields passed are sent, so
  omitting one leaves it unchanged; a PATCH that filled in defaults for the rest would blank
  whatever the release already said. Release notes written before a build is finished are
  routinely wrong once it is — the case that prompted this was notes telling readers to clear a
  quarantine flag from a tarball that had since been signed and notarized — and the only route
  this server offered was deleting the release and recreating it, which takes its assets with
  it. `PATCH /repos/{owner}/{repo}/releases/{id}` is identical on Forgejo and Gitea, request and
  response alike, so it needs no per-flavor translation.

### Fixed

- **`get_repo` now reports the unit toggles** — `has_issues`, `has_pull_requests`, `has_wiki`,
  `has_releases`, `has_actions`, `has_packages`, `has_projects`. 0.20.2 taught the release-404
  error to say "check `has_releases` via `get_repo`", but the repo read never returned that
  field, so the advice named a dead end: the only way to answer the question was the web UI, the
  very thing the hint was added to avoid. The toggles live on a new `RepoDetail` shape rather
  than in `RepoSummary`, so a repository *listing* does not grow seven booleans per row.

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
