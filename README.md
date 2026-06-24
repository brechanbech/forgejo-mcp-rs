# forgejo-mcp-rs

[![CI](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions/workflows/ci.yml/badge.svg)](https://codeberg.org/brechanbech/forgejo-mcp-rs/actions)

A [Model Context Protocol](https://modelcontextprotocol.io/) server for
[Forgejo](https://forgejo.org/) / [Codeberg](https://codeberg.org), written in Rust. It lets
an MCP client (Claude Code, Claude Desktop, …) read your forge — the authenticated user,
repositories, issues, and pull requests — over the Forgejo REST API.

> Status: **read-only by default, with opt-in guarded writes (since v0.2).** Read tools across
> the forge — user, repos, issues, pull requests, search, orgs, notifications, comments, and
> reviews — plus guarded writes (`create_repo`, `create_issue`, `create_pull_request`,
> `comment_on_issue`, `delete_repo`) gated behind a separate write token and a deliberate, time-boxed **write
> mode**. See [`SPECIFICATION.md`](SPECIFICATION.md) for the full design.

It speaks the Forgejo REST API directly through a small, in-house client (`src/forge/`) — an
**independent implementation over the documented API**, not a port of any other server. There
is no third-party forge SDK in the trust path, so the tool surface holding your token is code
you can read and audit end to end.

## Build

```sh
cargo build --release      # binary at target/release/forgejo-mcp-rs
```

## Configure

The server is configured by environment variables:

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `FORGEJO_TOKEN_READ_ONLY` | **yes** | — | Read token (or `FORGEJO_TOKEN`). **Read-only scopes are enough.** |
| `FORGEJO_TOKEN_WRITE` | no | — | Write/delete-scoped token. **Providing it enables the write tools**; omit it for a pure read-only server. |
| `FORGEJO_WRITE_MINUTES` | no | `10` | Default write-mode window (minutes, max 60). |
| `FORGEJO_URL` | no | `https://codeberg.org` | Instance base URL. |

Mint a token at **Codeberg → Settings → Applications** (or your instance's equivalent). For
the read tools, read scopes (`read:repository`, `read:issue`, `read:user`) suffice. The write
token needs `write:repository` (including delete).

A **read token is mandatory**: the server refuses to start on a write token alone, and the
read token must be a *different* token from `FORGEJO_TOKEN_WRITE` — you can't shortcut by
reusing the write token for reads.

### Write mode

The server is **read-only by default.** `create_repo` / `delete_repo` work only when (a)
`FORGEJO_TOKEN_WRITE` is configured **and** (b) you've deliberately entered **write mode** via
`enable_write_mode` — a time-boxed elevation (default 10 min, max 60) that slides forward on
each write and auto-reverts. `write_status` reports the state; `delete_repo` also requires a
`confirm` argument equal to `"owner/repo"`. See [`SPECIFICATION.md`](SPECIFICATION.md#write-mode-deliberate-time-boxed-elevation)
for the full design (and the honest "guardrail, not sandbox" caveat).

### Wire it into Claude Code

```sh
claude mcp add --scope user forgejo /path/to/target/release/forgejo-mcp-rs \
  --env FORGEJO_URL=https://codeberg.org \
  --env FORGEJO_TOKEN_READ_ONLY=your_read_token_here
# add --env FORGEJO_TOKEN_WRITE=… only if you want the (gated) write tools
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

| Tool | Status | Notes |
|---|---|---|
| `whoami` | ✅ read | The authenticated user (verifies the token) |
| `list_my_repos` | ✅ read | Your repositories (auto-paginated, slimmed) |
| `list_issues` / `get_issue` | ✅ read | Issues in `owner/repo` (open by default) |
| `list_pull_requests` / `get_pull_request` | ✅ read | Pull requests in `owner/repo` (open by default) |
| `search_repos` | ✅ read | Repository search by keyword |
| `list_orgs` | ✅ read | Organizations you belong to |
| `list_notifications` | ✅ read | Your notification threads, slimmed (`all=true` for read+unread) |
| `list_issue_comments` | ✅ read | Comments on an issue/PR (slimmed) |
| `list_pull_request_reviews` | ✅ read | Reviews on a PR — approve/request-changes/comment verdicts + summary bodies (inline comments as a count) |
| `write_status` | ✅ read | Report write-mode state (token configured? active? minutes left?) |
| `enable_write_mode` / `disable_write_mode` | ✅ | Enter/leave the time-boxed write mode |
| `create_repo` | ✅ **write** | Create a repo (defaults to private) |
| `create_issue` | ✅ **write** | Create an issue (owner/repo/title, optional body) |
| `create_pull_request` | ✅ **write** | Open a PR (owner/repo/title/head/base, optional body) |
| `comment_on_issue` | ✅ **write** | Comment on an issue/PR (owner/repo/index/body) |
| `delete_repo` | ✅ **write** | Delete a repo (needs `confirm = "owner/repo"`) |

Read list tools accept optional `state` (`open`/`closed`/`all`) and `page`/`limit`. Called
with no paging, `list_my_repos` / `list_issues` / `list_pull_requests` auto-paginate the whole
set and return a `{ returned, total, truncated, items }` envelope; pass an explicit `page` or
`limit` for a single page, which returns `{ page, limit, returned, total, items }` instead
(`total` is `null` for `search_repos`, which reports no count). Repository, notification,
comment, and review results are slimmed to the fields that matter. The **write** tools require
write mode (above); `edit_repo` and editing existing issues/PRs are future work — see the
[specification](SPECIFICATION.md).

## Security

The token is read from the environment only — never logged, never written to disk (the client
holds it in a zeroized buffer and marks the `Authorization` header sensitive). Read-only by
default, so the server cannot modify your account without a separate write token and write mode.
Tool output is untrusted, repo-derived text — the server flags it as data, not instructions.
See [`SPECIFICATION.md`](SPECIFICATION.md#security-model).

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
