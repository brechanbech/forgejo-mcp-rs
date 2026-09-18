//! Tool implementations — thin wrappers over the in-house [`Forge`] REST client.
//!
//! Each function maps a Forgejo API call to a [`CallToolResult`]. The server's `#[tool]`
//! methods in [`crate::forgejo::server`] delegate here, so that file reads as an index of the
//! surface and the real work lives here. (Promote to a `tools/` directory once it grows.)
//!
//! The client returns raw API JSON ([`Value`]); full-resource endpoints pass it straight
//! through, while list endpoints that we slim (notifications, comments) deserialize into
//! local shapes first.

use crate::mcp_core::{
    decode, gather_all, gathered_result, into_items, json_result, paged_result, to_mcp,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::client::{Forge, RunFilters};

/// Returns the authenticated user — proof the token works.
pub async fn whoami(forge: &Forge) -> Result<CallToolResult, McpError> {
    let user = forge.user_get_current().await.map_err(to_mcp)?;
    json_result(&user)
}

/// Reports this MCP server's own version plus the version and flavor of the instance it talks
/// to.
///
/// The MCP-server version is compiled in (no network), so it's reported even if the instance
/// call fails — in which case `instance_version` carries the error text instead of a version
/// string, and `flavor` falls back to `forgejo`.
pub async fn version(forge: &Forge) -> Result<CallToolResult, McpError> {
    let mcp_server = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"));
    let instance = match forge.server_version().await {
        Ok(value) => value
            .get("version")
            .and_then(Value::as_str)
            .map_or_else(|| "unknown".to_owned(), ToOwned::to_owned),
        Err(e) => format!("unavailable: {e}"),
    };
    json_result(&serde_json::json!({
        "mcp_server": mcp_server,
        "instance_version": instance,
        "flavor": forge.flavor().await.as_str(),
        "url": forge.base_url(),
    }))
}

/// An item within a repository addressed by number (an issue or pull-request index).
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct RepoItemRef {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Issue or pull-request number.
    pub index: i64,
}

/// A repository addressed by owner and name.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct RepoRef {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
}

/// A workflow run addressed by its numeric run id within a repository.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct RunRef {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Workflow run id (the `id` field from `list_workflow_runs`).
    pub run_id: i64,
}

/// Parameters for listing branches in a repository.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListBranchesParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for reading a file's contents from a repository.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct FileContentsParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Path to the file within the repository, e.g. `src/main.rs`.
    pub path: String,
    /// Branch, tag, or commit to read from. Defaults to the repository's default branch.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    /// First line to return, 1-indexed and inclusive. Omit to start at the top of the file.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// Last line to return, 1-indexed and inclusive. Omit to read to the end of the file.
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// Parameters for listing issues or pull requests in a repository.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListItemsParams {
    /// Repository owner — user or organization, e.g. `brechanbech`.
    pub owner: String,
    /// Repository name, e.g. `forgejo-mcp-rs`.
    pub repo: String,
    /// Filter by state: `open` (default), `closed`, or `all`.
    #[serde(default)]
    pub state: Option<String>,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Pagination-only parameters (for listings without a state).
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct PageParams {
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `search_repos` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct SearchReposParams {
    /// Search query (keywords; matches repository names and, by default, descriptions).
    pub query: String,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Validates a state filter (`open`/`closed`/`all`), returning the canonical query value or a
/// clear `invalid_params`.
fn parse_state(state: &str) -> Result<&'static str, McpError> {
    match state.to_ascii_lowercase().as_str() {
        "open" => Ok("open"),
        "closed" => Ok("closed"),
        "all" => Ok("all"),
        other => Err(McpError::invalid_params(
            format!("state must be open, closed, or all (got '{other}')"),
            None,
        )),
    }
}

/// A slimmed repository — the fields worth returning from a list, dropping the ~80-field full
/// Forgejo repo object (nested `owner`, `permissions`, `internal_tracker`, dozens of URLs and
/// flags) so the complete set stays compact. Deserializes from the raw API object (unknown
/// fields ignored) and re-serializes with the same names, omitting any that are absent.
#[derive(Debug, serde::Deserialize, Serialize)]
struct RepoSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    full_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    private: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fork: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stars_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forks_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    watchers_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_issues_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_pr_counter: Option<i64>,
    /// Repository size on disk, in KiB (git data plus LFS), as reported by Forgejo.
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clone_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

/// Projects raw repository objects down to [`RepoSummary`]. Each is an all-optional shape, so a
/// well-formed object never fails to deserialize; anything that somehow does is skipped.
fn slim_repos(items: Vec<Value>) -> Vec<RepoSummary> {
    items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

/// Lists the authenticated user's repositories.
pub async fn list_my_repos(forge: &Forge, params: PageParams) -> Result<CallToolResult, McpError> {
    // With no explicit paging, walk every page so the caller gets the complete set; an
    // explicit page or limit opts back into single-page control.
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| Box::pin(forge.list_my_repos(Some(page), Some(limit))))
            .await
            .map_err(to_mcp)?;
        return gathered_result(&slim_repos(all.items), all.total, all.truncated);
    }
    // The list endpoints carry the full count in the `X-Total-Count` header.
    let (repos, total) = forge
        .list_my_repos(params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    paged_result(
        params.page,
        params.limit,
        total,
        &slim_repos(into_items(repos)),
    )
}

/// Gets one repository's details (slimmed to the same fields as `list_my_repos`).
pub async fn get_repo(forge: &Forge, params: RepoRef) -> Result<CallToolResult, McpError> {
    let repo = forge
        .get_repo(&params.owner, &params.repo)
        .await
        .map_err(to_mcp)?;
    let summary: RepoSummary = decode(repo)?;
    json_result(&summary)
}

/// A branch reduced to its name, head commit, and protection flag.
#[derive(Debug, Serialize)]
struct BranchSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protected: Option<bool>,
}

/// Projects raw branch objects down to [`BranchSummary`], pulling the head SHA out of the
/// nested `commit.id`.
fn slim_branches(items: Vec<Value>) -> Vec<BranchSummary> {
    items
        .into_iter()
        .map(|b| BranchSummary {
            name: b.get("name").and_then(Value::as_str).map(ToOwned::to_owned),
            commit: b
                .pointer("/commit/id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            protected: b.get("protected").and_then(Value::as_bool),
        })
        .collect()
}

/// Lists branches in `owner/repo` (auto-paginated unless an explicit page/limit is given).
pub async fn list_branches(
    forge: &Forge,
    params: ListBranchesParams,
) -> Result<CallToolResult, McpError> {
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| {
            Box::pin(forge.list_branches(&params.owner, &params.repo, Some(page), Some(limit)))
        })
        .await
        .map_err(to_mcp)?;
        return gathered_result(&slim_branches(all.items), all.total, all.truncated);
    }
    let (branches, total) = forge
        .list_branches(&params.owner, &params.repo, params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    paged_result(
        params.page,
        params.limit,
        total,
        &slim_branches(into_items(branches)),
    )
}

/// Applies an optional 1-indexed, inclusive line window to `text`.
///
/// Returns the selected text and the window actually applied, both clamped to the file's extent —
/// so `end_line` past the end simply stops at the last line. A `start_line` past the end is not an
/// error either: the file genuinely has no such line, and an empty slice alongside `total_lines`
/// says so more usefully than a failure. Only an inverted window (`start_line` after `end_line`)
/// is rejected, since no file could satisfy it.
///
/// The slice is re-joined with `\n`, so a file with CRLF endings comes back normalized and the
/// final line carries no trailing newline.
///
/// # Errors
/// `invalid_params` if both bounds are given and `start_line` is greater than `end_line`.
fn slice_lines(
    text: &str,
    start: Option<u32>,
    end: Option<u32>,
    total: usize,
) -> Result<(String, Option<(usize, usize)>), McpError> {
    if start.is_none() && end.is_none() {
        return Ok((text.to_owned(), None));
    }
    if let (Some(s), Some(e)) = (start, end)
        && s > e
    {
        return Err(McpError::invalid_params(
            format!("start_line {s} is after end_line {e}"),
            None,
        ));
    }
    let first = (start.unwrap_or(1) as usize).max(1);
    let last = end.map_or(total, |e| e as usize).min(total);
    if first > last {
        // Window starts past the end of the file: no lines, reported honestly.
        return Ok((String::new(), Some((first, last))));
    }
    let slice = text
        .lines()
        .skip(first - 1)
        .take(last - first + 1)
        .collect::<Vec<_>>()
        .join("\n");
    Ok((slice, Some((first, last))))
}

/// Reads a file's contents (or lists a directory) from `owner/repo`. For a file the base64 body
/// is decoded: UTF-8 text is returned inline; binary content is reported by size, not dumped.
pub async fn get_file_contents(
    forge: &Forge,
    params: FileContentsParams,
) -> Result<CallToolResult, McpError> {
    let raw = forge
        .get_contents(
            &params.owner,
            &params.repo,
            &params.path,
            params.git_ref.as_deref(),
        )
        .await
        .map_err(to_mcp)?;

    // A directory comes back as an array of entries; slim each to name/path/type.
    if let Value::Array(entries) = raw {
        let listing: Vec<Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.get("name"),
                    "path": e.get("path"),
                    "type": e.get("type"),
                })
            })
            .collect();
        return json_result(&serde_json::json!({ "type": "dir", "entries": listing }));
    }

    // A file carries its body as base64 in `content` (whitespace-wrapped on some instances).
    let mut out = serde_json::Map::new();
    out.insert("type".to_owned(), Value::String("file".to_owned()));
    for key in ["path", "sha", "size"] {
        if let Some(v) = raw.get(key) {
            out.insert(key.to_owned(), v.clone());
        }
    }
    let decoded = match (
        raw.get("encoding").and_then(Value::as_str),
        raw.get("content").and_then(Value::as_str),
    ) {
        (Some("base64"), Some(c)) => {
            let stripped: String = c.chars().filter(|ch| !ch.is_whitespace()).collect();
            BASE64.decode(stripped).ok()
        }
        _ => None,
    };
    match decoded {
        Some(bytes) => match String::from_utf8(bytes) {
            Ok(text) => {
                // `total_lines` is always reported, so a caller that asked for a window knows
                // how much file lies outside it and can page through the rest.
                let total_lines = text.lines().count();
                let (content, window) =
                    slice_lines(&text, params.start_line, params.end_line, total_lines)?;
                out.insert("encoding".to_owned(), Value::String("utf-8".to_owned()));
                out.insert("total_lines".to_owned(), Value::from(total_lines));
                if let Some((first, last)) = window {
                    out.insert("start_line".to_owned(), Value::from(first));
                    out.insert("end_line".to_owned(), Value::from(last));
                }
                out.insert("content".to_owned(), Value::String(content));
            }
            Err(e) => {
                out.insert("encoding".to_owned(), Value::String("binary".to_owned()));
                out.insert(
                    "note".to_owned(),
                    Value::String(format!(
                        "binary file ({} bytes); content omitted",
                        e.as_bytes().len()
                    )),
                );
            }
        },
        None => {
            out.insert(
                "note".to_owned(),
                Value::String("no decodable base64 content".to_owned()),
            );
        }
    }
    json_result(&Value::Object(out))
}

/// Lists issues in `owner/repo` (open issues by default).
pub async fn list_issues(
    forge: &Forge,
    params: ListItemsParams,
) -> Result<CallToolResult, McpError> {
    let state = params.state.as_deref().map(parse_state).transpose()?;
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| {
            Box::pin(forge.list_issues(&params.owner, &params.repo, state, Some(page), Some(limit)))
        })
        .await
        .map_err(to_mcp)?;
        return gathered_result(&all.items, all.total, all.truncated);
    }
    let (issues, total) = forge
        .list_issues(
            &params.owner,
            &params.repo,
            state,
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    paged_result(params.page, params.limit, total, &into_items(issues))
}

/// Gets one issue by index.
pub async fn get_issue(forge: &Forge, params: RepoItemRef) -> Result<CallToolResult, McpError> {
    let issue = forge
        .get_issue(&params.owner, &params.repo, params.index)
        .await
        .map_err(to_mcp)?;
    json_result(&issue)
}

/// Lists pull requests in `owner/repo` (open by default).
pub async fn list_pull_requests(
    forge: &Forge,
    params: ListItemsParams,
) -> Result<CallToolResult, McpError> {
    let state = params.state.as_deref().map(parse_state).transpose()?;
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| {
            Box::pin(forge.list_pull_requests(
                &params.owner,
                &params.repo,
                state,
                Some(page),
                Some(limit),
            ))
        })
        .await
        .map_err(to_mcp)?;
        return gathered_result(&all.items, all.total, all.truncated);
    }
    let (prs, total) = forge
        .list_pull_requests(
            &params.owner,
            &params.repo,
            state,
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    paged_result(params.page, params.limit, total, &into_items(prs))
}

/// Gets one pull request by index.
pub async fn get_pull_request(
    forge: &Forge,
    params: RepoItemRef,
) -> Result<CallToolResult, McpError> {
    let pr = forge
        .get_pull_request(&params.owner, &params.repo, params.index)
        .await
        .map_err(to_mcp)?;
    json_result(&pr)
}

/// Searches repositories by keyword.
pub async fn search_repos(
    forge: &Forge,
    params: SearchReposParams,
) -> Result<CallToolResult, McpError> {
    // `repo_search` returns `{ ok, data }` (no count header), so `total` is unknown here —
    // surface `data` in the same envelope for consistency.
    let results = forge
        .search_repos(&params.query, params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    let items = match results {
        Value::Object(mut map) => into_items(map.remove("data").unwrap_or(Value::Null)),
        _ => Vec::new(),
    };
    paged_result(params.page, params.limit, None, &items)
}

/// Lists the organizations the authenticated user belongs to.
pub async fn list_orgs(forge: &Forge, params: PageParams) -> Result<CallToolResult, McpError> {
    // Returns a bare array (no count header), so `total` is unknown here.
    let orgs = forge
        .list_orgs(params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    paged_result(params.page, params.limit, None, &into_items(orgs))
}

/// Parameters for the `list_notifications` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListNotificationsParams {
    /// Include read notifications too. Default: unread only.
    #[serde(default)]
    pub all: Option<bool>,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

// A loose notification shape. Forgejo's strict `NotificationThread` enums don't model every
// value the API emits (notably `StateType` has no `merged`, so a merged-PR notification would
// break a strict parse), so we deserialize the volatile fields as plain strings and ignore
// the rest (including the full embedded repository object).
#[derive(Debug, serde::Deserialize)]
struct LooseNotification {
    id: Option<i64>,
    unread: Option<bool>,
    repository: Option<LooseRepo>,
    subject: Option<LooseSubject>,
    updated_at: Option<String>,
}
#[derive(Debug, serde::Deserialize)]
struct LooseRepo {
    full_name: Option<String>,
}
#[derive(Debug, serde::Deserialize)]
struct LooseSubject {
    title: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    state: Option<String>,
    html_url: Option<String>,
    url: Option<String>,
}

/// A slimmed notification thread (the raw form embeds a full repository object each).
#[derive(Debug, Serialize)]
struct NotificationSummary {
    id: Option<i64>,
    unread: Option<bool>,
    repo: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    state: Option<String>,
    title: Option<String>,
    url: Option<String>,
    updated_at: Option<String>,
}

fn summarize_notification(n: LooseNotification) -> NotificationSummary {
    let subject = n.subject;
    NotificationSummary {
        id: n.id,
        unread: n.unread,
        repo: n.repository.and_then(|r| r.full_name),
        kind: subject.as_ref().and_then(|s| s.kind.clone()),
        state: subject.as_ref().and_then(|s| s.state.clone()),
        title: subject.as_ref().and_then(|s| s.title.clone()),
        url: subject
            .as_ref()
            .and_then(|s| s.html_url.clone().or_else(|| s.url.clone())),
        updated_at: n.updated_at,
    }
}

/// Lists the user's notification threads (unread by default; `all` includes read ones).
pub async fn list_notifications(
    forge: &Forge,
    params: ListNotificationsParams,
) -> Result<CallToolResult, McpError> {
    // No count header on this endpoint, so `total` is `None`.
    let raw = forge
        .list_notifications(params.all, params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    let threads: Vec<LooseNotification> = decode(raw)?;
    let items: Vec<NotificationSummary> = threads.into_iter().map(summarize_notification).collect();
    paged_result(params.page, params.limit, None, &items)
}

// A loose comment shape, capturing only the fields we surface.
#[derive(Debug, serde::Deserialize)]
struct RawComment {
    id: Option<i64>,
    user: Option<RawUser>,
    body: Option<String>,
    created_at: Option<String>,
    html_url: Option<String>,
}
#[derive(Debug, serde::Deserialize)]
struct RawUser {
    login: Option<String>,
}

/// A slimmed issue/PR comment (the raw form embeds a full user object each).
#[derive(Debug, Serialize)]
struct CommentSummary {
    id: Option<i64>,
    user: Option<String>,
    body: Option<String>,
    created_at: Option<String>,
    url: Option<String>,
}

fn summarize_comment(c: RawComment) -> CommentSummary {
    CommentSummary {
        id: c.id,
        user: c.user.and_then(|u| u.login),
        body: c.body,
        created_at: c.created_at,
        url: c.html_url,
    }
}

/// Parameters for the `list_issue_comments` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListCommentsParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Issue or pull-request number.
    pub index: i64,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Lists the comments on an issue or pull request.
pub async fn list_issue_comments(
    forge: &Forge,
    params: ListCommentsParams,
) -> Result<CallToolResult, McpError> {
    let (raw, total) = forge
        .list_issue_comments(
            &params.owner,
            &params.repo,
            params.index,
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    let comments: Vec<RawComment> = decode(raw)?;
    let items: Vec<CommentSummary> = comments.into_iter().map(summarize_comment).collect();
    paged_result(params.page, params.limit, total, &items)
}

// A loose review shape; `state` stays a string so unfamiliar values don't break parsing.
#[derive(Debug, serde::Deserialize)]
struct RawReview {
    id: Option<i64>,
    user: Option<RawUser>,
    body: Option<String>,
    /// e.g. `APPROVED`, `REQUEST_CHANGES`, `COMMENT`, `PENDING`.
    state: Option<String>,
    /// Whether the review counts toward the branch's review requirements.
    official: Option<bool>,
    /// Whether the review was dismissed.
    dismissed: Option<bool>,
    /// Whether the review is stale (made against an older commit).
    stale: Option<bool>,
    /// Count of inline (line-anchored) comments attached to this review.
    comments_count: Option<i64>,
    submitted_at: Option<String>,
    html_url: Option<String>,
}

/// A slimmed pull-request review (the raw form embeds a full user object each).
#[derive(Debug, Serialize)]
struct ReviewSummary {
    id: Option<i64>,
    user: Option<String>,
    body: Option<String>,
    state: Option<String>,
    official: Option<bool>,
    dismissed: Option<bool>,
    stale: Option<bool>,
    comments_count: Option<i64>,
    submitted_at: Option<String>,
    url: Option<String>,
}

fn summarize_review(r: RawReview) -> ReviewSummary {
    ReviewSummary {
        id: r.id,
        user: r.user.and_then(|u| u.login),
        body: r.body,
        state: r.state,
        official: r.official,
        dismissed: r.dismissed,
        stale: r.stale,
        comments_count: r.comments_count,
        submitted_at: r.submitted_at,
        url: r.html_url,
    }
}

/// Parameters for the `list_pull_request_reviews` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListReviewsParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Pull-request number.
    pub index: i64,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Lists the reviews on a pull request (approve / request-changes / comment verdicts and
/// their summary bodies). Inline line comments are reported only as `comments_count`.
pub async fn list_pull_request_reviews(
    forge: &Forge,
    params: ListReviewsParams,
) -> Result<CallToolResult, McpError> {
    let (raw, total) = forge
        .list_pull_request_reviews(
            &params.owner,
            &params.repo,
            params.index,
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    let reviews: Vec<RawReview> = decode(raw)?;
    let items: Vec<ReviewSummary> = reviews.into_iter().map(summarize_review).collect();
    paged_result(params.page, params.limit, total, &items)
}

/// Default cap on an unfiltered pull-request diff, in bytes. A whole diff is unbounded output —
/// the reason log retrieval is a non-goal — so it is truncated at a line boundary and flagged
/// rather than returned whole. Ask for one file at a time to stay under it.
const DIFF_MAX_BYTES: usize = 64 * 1024;

/// One file's entry in `list_pull_request_files`. Forgejo returns three URL fields per file
/// (`html_url`, `contents_url`, `raw_url`) that restate the path in three ways; they are dropped.
#[derive(Debug, serde::Deserialize, Serialize)]
struct ChangedFileSummary {
    filename: String,
    /// Present only on a rename — the path this file had before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    additions: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deletions: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changes: Option<i64>,
}

/// Parameters for listing a pull request's changed files.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListPullRequestFilesParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Pull request number within the repository.
    pub index: i64,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for reading a pull request's diff.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct PullRequestDiffParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Pull request number within the repository.
    pub index: i64,
    /// Return only this file's hunks. Matches either side of a rename, and must be the exact
    /// path as listed by `list_pull_request_files` (no globs). Omit for the whole diff.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Cap on the returned diff in bytes (default 65536), applied at a line boundary. Ignored
    /// when `file_path` selects a single file.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// Turns raw API file entries into the review-relevant summary, dropping the three URL fields.
fn slim_changed_files(items: Vec<Value>) -> Vec<ChangedFileSummary> {
    items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

/// Lists the files a pull request changes, with per-file line counts.
pub async fn list_pull_request_files(
    forge: &Forge,
    params: ListPullRequestFilesParams,
) -> Result<CallToolResult, McpError> {
    let (owner, repo, index) = (&params.owner, &params.repo, params.index);
    // With no explicit paging, walk every page so the caller gets the complete set; an
    // explicit page or limit opts back into single-page control.
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| {
            Box::pin(forge.list_pull_request_files(owner, repo, index, Some(page), Some(limit)))
        })
        .await
        .map_err(to_mcp)?;
        return gathered_result(&slim_changed_files(all.items), all.total, all.truncated);
    }
    let (raw, total) = forge
        .list_pull_request_files(owner, repo, index, params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    paged_result(
        params.page,
        params.limit,
        total,
        &slim_changed_files(into_items(raw)),
    )
}

/// Strips a unified diff's `a/` or `b/` prefix from a path, rejecting the `/dev/null` placeholder
/// that stands in for the missing side of an add or delete.
fn strip_diff_prefix(path: &str) -> Option<String> {
    if path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_owned(),
    )
}

/// One file's section of a unified diff.
#[derive(Debug)]
struct DiffSection {
    /// Every path this section names — both sides of a rename, so a caller matches on either.
    paths: Vec<String>,
    text: String,
}

/// Splits a unified diff into per-file sections.
///
/// A section starts at a `diff --git a/OLD b/NEW` header and runs to the next one. Paths come
/// from that header and from the `---` / `+++` lines that follow it, which name each side
/// unambiguously even when the header is hard to split (a path containing a space). Only the
/// lines *before* the first `@@` are scanned for those markers: inside a hunk, a removed line
/// whose content begins with `-- ` is itself rendered as `--- `, and would otherwise be mistaken
/// for a file header.
fn split_diff_sections(diff: &str) -> Vec<DiffSection> {
    let mut sections: Vec<DiffSection> = Vec::new();
    let mut in_hunks = false;

    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            let tokens: Vec<&str> = header.split_whitespace().collect();
            let mut paths: Vec<String> = Vec::new();
            if tokens.len() == 2 {
                // The two sides are identical for an ordinary edit and differ only on a rename,
                // so dedupe rather than listing the same path twice.
                for path in tokens.iter().filter_map(|t| strip_diff_prefix(t)) {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
            sections.push(DiffSection {
                paths,
                text: String::new(),
            });
            in_hunks = false;
        }

        let Some(section) = sections.last_mut() else {
            continue; // preamble before the first file header — not part of any section
        };

        if line.starts_with("@@") {
            in_hunks = true;
        } else if !in_hunks {
            for marker in ["--- ", "+++ "] {
                if let Some(rest) = line.strip_prefix(marker)
                    && let Some(path) = strip_diff_prefix(rest.trim_end())
                    && !section.paths.contains(&path)
                {
                    section.paths.push(path);
                }
            }
        }

        section.text.push_str(line);
        section.text.push('\n');
    }

    sections
}

/// Truncates `text` to at most `max_bytes`, cutting at a line boundary. Returns the text and
/// whether anything was dropped.
fn truncate_to_lines(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut out = String::new();
    for line in text.lines() {
        // +1 for the newline this line will carry.
        if out.len() + line.len() + 1 > max_bytes {
            return (out, true);
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, false)
}

/// Reads a pull request's unified diff, optionally narrowed to one file.
pub async fn get_pull_request_diff(
    forge: &Forge,
    params: PullRequestDiffParams,
) -> Result<CallToolResult, McpError> {
    let diff = forge
        .get_pull_request_diff(&params.owner, &params.repo, params.index)
        .await
        .map_err(to_mcp)?;

    if let Some(wanted) = params.file_path.as_deref() {
        let sections = split_diff_sections(&diff);
        let matched: String = sections
            .iter()
            .filter(|s| s.paths.iter().any(|p| p == wanted))
            .map(|s| s.text.as_str())
            .collect();
        if matched.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "no file matching {wanted} in this diff — call list_pull_request_files for \
                     the exact paths this pull request changes (the match is exact, not a glob)"
                ),
                None,
            ));
        }
        return json_result(&serde_json::json!({
            "index": params.index,
            "file_path": wanted,
            "bytes": matched.len(),
            "truncated": false,
            "diff": matched,
        }));
    }

    let max_bytes = params.max_bytes.unwrap_or(DIFF_MAX_BYTES);
    let (text, truncated) = truncate_to_lines(&diff, max_bytes);
    let mut out = serde_json::json!({
        "index": params.index,
        "bytes": text.len(),
        "total_bytes": diff.len(),
        "truncated": truncated,
        "diff": text,
    });
    if truncated && let Some(obj) = out.as_object_mut() {
        obj.insert(
            "note".to_owned(),
            Value::String(
                "diff truncated at the byte cap — call list_pull_request_files, then request one \
                 file at a time with file_path (or raise max_bytes)"
                    .to_owned(),
            ),
        );
    }
    json_result(&out)
}

// --- write tools (require write mode; see crate::forgejo::server) ---

/// Parameters for the `enable_write_mode` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct EnableWriteParams {
    /// How long write mode stays active, in minutes (default 10, hard-capped at 60). It
    /// also slides forward this far on each successful write, then auto-reverts.
    #[serde(default)]
    pub minutes: Option<u32>,
}

/// Parameters for the `create_repo` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CreateRepoParams {
    /// Name of the repository to create (under the authenticated user).
    pub name: String,
    /// Whether the repository is private. Defaults to private when omitted.
    #[serde(default)]
    pub private: Option<bool>,
    /// Optional description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Parameters for the `edit_repo` tool.
///
/// Every settings field is optional; only the ones provided are sent in the `PATCH`,
/// so everything else keeps its current value. Renaming is deliberately not exposed
/// (Codeberg renames are unreliable; see SPECIFICATION.md).
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct EditRepoParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Change visibility: `true` = private, `false` = public.
    #[serde(default)]
    pub private: Option<bool>,
    /// New description.
    #[serde(default)]
    pub description: Option<String>,
    /// New website URL.
    #[serde(default)]
    pub website: Option<String>,
    /// New default branch (must already exist).
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Enable or disable the issue tracker.
    #[serde(default)]
    pub has_issues: Option<bool>,
    /// Enable or disable pull requests.
    #[serde(default)]
    pub has_pull_requests: Option<bool>,
    /// Enable or disable the wiki.
    #[serde(default)]
    pub has_wiki: Option<bool>,
    /// Archive (`true`) or unarchive (`false`) the repository.
    #[serde(default)]
    pub archived: Option<bool>,
}

/// Parameters for the `delete_repo` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct DeleteRepoParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Safety guard: must be exactly `"owner/repo"`, or the delete is refused.
    pub confirm: String,
}

/// Source forges Forgejo's migration importer understands. `git` is a bare clone (refs only);
/// every other value unlocks the API-based importer that can carry issues, PRs and releases.
/// Use `gitea` for a Forgejo or Gitea source — there is no separate `forgejo` value.
const MIGRATE_SERVICES: [&str; 8] = [
    "git",
    "github",
    "gitea",
    "gitlab",
    "gogs",
    "onedev",
    "gitbucket",
    "codebase",
];

/// Parameters for the `migrate_repo` tool.
///
/// Mirrors Forgejo's `MigrateRepoOptions`, minus the credential: the source password/token is
/// supplied by the server via `FORGEJO_MIGRATE_TOKEN`, never as a tool argument.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct MigrateRepoParams {
    /// Source clone URL on the *other* instance, e.g.
    /// `https://codeberg.org/owner/repo.git`. Must be `http://` or `https://`.
    pub clone_addr: String,
    /// Name for the new repository on this instance.
    pub repo_name: String,
    /// User or organisation that will own the new repo. Defaults to the authenticated user.
    #[serde(default)]
    pub repo_owner: Option<String>,
    /// Source forge type: `git`, `github`, `gitea` (use this for Forgejo), `gitlab`, `gogs`,
    /// `onedev`, `gitbucket`, `codebase`. Defaults to `git`, which copies git refs only —
    /// name the real forge to carry issues, PRs and releases across.
    #[serde(default)]
    pub service: Option<String>,
    /// Description for the new repository.
    #[serde(default)]
    pub description: Option<String>,
    /// Visibility of the new repository. Defaults to private.
    #[serde(default)]
    pub private: Option<bool>,
    /// Username on the *source* instance. Setting it turns on authentication (the token comes
    /// from the server's `FORGEJO_MIGRATE_TOKEN`). Leave unset for a public source, or when the
    /// source authenticates by bare token — then set `authenticate = true` instead.
    #[serde(default)]
    pub auth_username: Option<String>,
    /// Send the server's `FORGEJO_MIGRATE_TOKEN` as the source credential. Implied when
    /// `auth_username` is set. Needed on its own for token-only auth, and for private
    /// GitHub/GitLab sources.
    #[serde(default)]
    pub authenticate: Option<bool>,
    /// Also migrate issues. Requires a non-`git` `service`. Defaults to false.
    #[serde(default)]
    pub issues: Option<bool>,
    /// Also migrate pull requests. Requires a non-`git` `service`. Defaults to false.
    #[serde(default)]
    pub pull_requests: Option<bool>,
    /// Also migrate labels. Requires a non-`git` `service`. Defaults to false.
    #[serde(default)]
    pub labels: Option<bool>,
    /// Also migrate milestones. Requires a non-`git` `service`. Defaults to false.
    #[serde(default)]
    pub milestones: Option<bool>,
    /// Also migrate releases (and their attachments). Requires a non-`git` `service`.
    /// Defaults to false.
    #[serde(default)]
    pub releases: Option<bool>,
    /// Also migrate the wiki. Defaults to false.
    #[serde(default)]
    pub wiki: Option<bool>,
    /// Also fetch Git LFS objects. Defaults to false.
    #[serde(default)]
    pub lfs: Option<bool>,
    /// Custom LFS endpoint, if the source doesn't serve LFS from the clone URL.
    #[serde(default)]
    pub lfs_endpoint: Option<String>,
    /// Keep the new repo as a *pull* mirror that periodically re-fetches from the source,
    /// instead of taking a one-shot copy. Defaults to false.
    #[serde(default)]
    pub mirror: Option<bool>,
    /// Pull-mirror refresh interval in Forgejo duration form (e.g. `8h0m0s`). Only meaningful
    /// with `mirror = true`.
    #[serde(default)]
    pub mirror_interval: Option<String>,
}

/// Creates a repository for the authenticated user (defaults to private).
pub async fn create_repo(
    forge: &Forge,
    params: CreateRepoParams,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::Map::new();
    body.insert("name".to_owned(), Value::String(params.name));
    body.insert(
        "private".to_owned(),
        Value::Bool(params.private.unwrap_or(true)),
    );
    if let Some(description) = params.description {
        body.insert("description".to_owned(), Value::String(description));
    }
    let repo = forge
        .create_repo(&Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&repo)
}

/// The `PATCH` body for `edit_repo`: exactly the fields the caller set, nothing else.
fn edit_repo_body(params: EditRepoParams) -> serde_json::Map<String, Value> {
    let fields = [
        ("private", params.private.map(Value::Bool)),
        ("description", params.description.map(Value::String)),
        ("website", params.website.map(Value::String)),
        ("default_branch", params.default_branch.map(Value::String)),
        ("has_issues", params.has_issues.map(Value::Bool)),
        (
            "has_pull_requests",
            params.has_pull_requests.map(Value::Bool),
        ),
        ("has_wiki", params.has_wiki.map(Value::Bool)),
        ("archived", params.archived.map(Value::Bool)),
    ];
    fields
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key.to_owned(), v)))
        .collect()
}

/// Edits repository settings; refuses a no-op call with nothing to change.
pub async fn edit_repo(forge: &Forge, params: EditRepoParams) -> Result<CallToolResult, McpError> {
    let (owner, repo) = (params.owner.clone(), params.repo.clone());
    let body = edit_repo_body(params);
    if body.is_empty() {
        return Err(McpError::invalid_params(
            "edit refused: provide at least one field to change (private, description, website, \
             default_branch, has_issues, has_pull_requests, has_wiki, archived)",
            None,
        ));
    }
    let updated = forge
        .edit_repo(&owner, &repo, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&updated)
}

/// Deletes a repository — guarded by an exact `owner/repo` confirmation.
pub async fn delete_repo(
    forge: &Forge,
    params: DeleteRepoParams,
) -> Result<CallToolResult, McpError> {
    let expected = format!("{}/{}", params.owner, params.repo);
    if params.confirm != expected {
        return Err(McpError::invalid_params(
            format!("delete refused: `confirm` must be exactly \"{expected}\""),
            None,
        ));
    }
    forge
        .delete_repo(&params.owner, &params.repo)
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({ "deleted": expected }))
}

/// Builds the `MigrateRepoOptions` body, validating the caller's input and splicing in the
/// server-held source credential. Split out from [`migrate_repo`] so the argument handling —
/// especially the credential rules — is unit-testable without a live instance.
fn migrate_repo_body(
    params: MigrateRepoParams,
    migrate_token: Option<&str>,
) -> Result<serde_json::Map<String, Value>, McpError> {
    // An http(s) clone address is what the migration API expects; anything else (ssh, git://,
    // a local path) is either rejected downstream or, worse, quietly treated as a local disk
    // path by the instance. Fail loudly here instead.
    let addr = params.clone_addr.trim();
    if !(addr.starts_with("https://") || addr.starts_with("http://")) {
        return Err(McpError::invalid_params(
            format!(
                "migrate refused: clone_addr must be an http:// or https:// URL, got \"{addr}\""
            ),
            None,
        ));
    }
    if params.repo_name.trim().is_empty() {
        return Err(McpError::invalid_params(
            "migrate refused: repo_name must not be empty".to_owned(),
            None,
        ));
    }
    if let Some(service) = params.service.as_deref()
        && !MIGRATE_SERVICES.contains(&service)
    {
        return Err(McpError::invalid_params(
            format!(
                "migrate refused: unknown service \"{service}\" — expected one of {} (use \
                 \"gitea\" for a Forgejo source)",
                MIGRATE_SERVICES.join(", ")
            ),
            None,
        ));
    }

    let mut body = serde_json::Map::new();
    body.insert("clone_addr".to_owned(), Value::String(addr.to_owned()));
    body.insert("repo_name".to_owned(), Value::String(params.repo_name));
    // Default to private, matching create_repo: a migrated repo is easier to publish later than
    // to un-publish.
    body.insert(
        "private".to_owned(),
        Value::Bool(params.private.unwrap_or(true)),
    );

    let optional_strings = [
        ("repo_owner", params.repo_owner),
        ("service", params.service),
        ("description", params.description),
        ("lfs_endpoint", params.lfs_endpoint),
        ("mirror_interval", params.mirror_interval),
    ];
    for (key, value) in optional_strings {
        if let Some(v) = value {
            body.insert(key.to_owned(), Value::String(v));
        }
    }

    let optional_flags = [
        ("issues", params.issues),
        ("pull_requests", params.pull_requests),
        ("labels", params.labels),
        ("milestones", params.milestones),
        ("releases", params.releases),
        ("wiki", params.wiki),
        ("lfs", params.lfs),
        ("mirror", params.mirror),
    ];
    for (key, value) in optional_flags {
        if let Some(v) = value {
            body.insert(key.to_owned(), Value::Bool(v));
        }
    }

    // Authentication is opt-in: most migrations read a public source and need no credential at
    // all. Naming a username implies it; `authenticate` alone covers token-only forges.
    let wants_auth = params.authenticate.unwrap_or(false) || params.auth_username.is_some();
    if wants_auth {
        let token = migrate_token.ok_or_else(|| {
            McpError::invalid_params(
                "no source credential configured: set FORGEJO_MIGRATE_TOKEN on the server to a \
                 token for the *source* instance, or drop auth_username/authenticate if the \
                 source repository is public"
                    .to_owned(),
                None,
            )
        })?;
        if let Some(username) = params.auth_username {
            body.insert("auth_username".to_owned(), Value::String(username));
        }
        body.insert("auth_token".to_owned(), Value::String(token.to_owned()));
    }

    Ok(body)
}

/// Migrates (copies) a repository from another forge into this instance, optionally bringing
/// issues, PRs, labels, milestones, releases and the wiki with it.
///
/// The source credential is supplied by the server via `FORGEJO_MIGRATE_TOKEN`
/// (`migrate_token`), never as a tool argument, so it stays out of the conversation.
pub async fn migrate_repo(
    forge: &Forge,
    migrate_token: Option<&str>,
    params: MigrateRepoParams,
) -> Result<CallToolResult, McpError> {
    let body = migrate_repo_body(params, migrate_token)?;
    // The Repository response carries no credential — safe to return verbatim.
    let repo = forge
        .migrate_repo(&Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&repo)
}

/// Parameters for the `create_issue` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CreateIssueParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Issue title.
    pub title: String,
    /// Issue body (Markdown). Optional.
    #[serde(default)]
    pub body: Option<String>,
}

/// Creates an issue in `owner/repo`.
pub async fn create_issue(
    forge: &Forge,
    params: CreateIssueParams,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::Map::new();
    body.insert("title".to_owned(), Value::String(params.title));
    if let Some(text) = params.body {
        body.insert("body".to_owned(), Value::String(text));
    }
    let issue = forge
        .create_issue(&params.owner, &params.repo, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&issue)
}

/// Parameters for the `create_branch` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CreateBranchParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Name for the new branch.
    pub new_branch: String,
    /// Existing branch, tag, or commit to branch from. Defaults to the repo's default branch.
    #[serde(default)]
    pub old_ref: Option<String>,
}

/// Creates a branch in `owner/repo`, optionally from a given ref.
pub async fn create_branch(
    forge: &Forge,
    params: CreateBranchParams,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::Map::new();
    body.insert(
        "new_branch_name".to_owned(),
        Value::String(params.new_branch),
    );
    if let Some(old) = params.old_ref {
        body.insert("old_ref_name".to_owned(), Value::String(old));
    }
    let branch = forge
        .create_branch(&params.owner, &params.repo, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&branch)
}

/// Parameters for the `create_pull_request` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CreatePullRequestParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Pull-request title.
    pub title: String,
    /// Source branch — the branch with your changes. For a cross-repo PR use `user:branch`.
    pub head: String,
    /// Target branch the changes should be merged into (e.g. `main`).
    pub base: String,
    /// Pull-request body (Markdown). Optional.
    #[serde(default)]
    pub body: Option<String>,
}

/// Opens a pull request in `owner/repo` from `head` into `base`.
pub async fn create_pull_request(
    forge: &Forge,
    params: CreatePullRequestParams,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::Map::new();
    body.insert("title".to_owned(), Value::String(params.title));
    body.insert("head".to_owned(), Value::String(params.head));
    body.insert("base".to_owned(), Value::String(params.base));
    if let Some(text) = params.body {
        body.insert("body".to_owned(), Value::String(text));
    }
    let pull = forge
        .create_pull_request(&params.owner, &params.repo, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&pull)
}

/// Parameters for the `comment_on_issue` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CommentOnIssueParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Issue or pull-request number.
    pub index: i64,
    /// Comment body (Markdown).
    pub body: String,
}

/// Adds a comment to an issue or pull request.
pub async fn comment_on_issue(
    forge: &Forge,
    params: CommentOnIssueParams,
) -> Result<CallToolResult, McpError> {
    let body = serde_json::json!({ "body": params.body });
    let raw = forge
        .comment_on_issue(&params.owner, &params.repo, params.index, &body)
        .await
        .map_err(to_mcp)?;
    let comment: RawComment = decode(raw)?;
    json_result(&summarize_comment(comment))
}

/// Default push-mirror sync interval (Forgejo duration syntax) when the caller omits one.
const DEFAULT_MIRROR_INTERVAL: &str = "8h0m0s";

/// Parameters for the `add_push_mirror` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct AddPushMirrorParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Target git URL to push to, e.g. `https://github.com/you/repo.git`.
    pub remote_address: String,
    /// Username on the remote (e.g. your GitHub username). Required for password auth; ignored
    /// when `use_ssh` is true.
    #[serde(default)]
    pub remote_username: Option<String>,
    /// Sync interval in Forgejo duration form (e.g. `8h0m0s`; `0` disables periodic sync).
    /// Defaults to `8h0m0s`.
    #[serde(default)]
    pub interval: Option<String>,
    /// Also push right after each push to this repo (near-real-time). Defaults to true.
    #[serde(default)]
    pub sync_on_commit: Option<bool>,
    /// Optional glob branch filter (e.g. `main,release/*`); empty mirrors all branches.
    #[serde(default)]
    pub branch_filter: Option<String>,
    /// Authenticate to the remote with an SSH key instead of a password token. When true no
    /// username/token is sent and the response carries a `public_key` to add on the remote.
    #[serde(default)]
    pub use_ssh: Option<bool>,
}

/// Parameters for the `delete_push_mirror` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct DeletePushMirrorParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// The mirror's `remote_name` (as reported by `list_push_mirrors`).
    pub remote_name: String,
}

/// Adds a push mirror to `owner/repo`. The push credential is supplied by the server via
/// `FORGEJO_MIRROR_TOKEN` (`mirror_token`), never as a tool argument, so it stays out of the
/// conversation; pass `use_ssh = true` to use key auth instead.
pub async fn add_push_mirror(
    forge: &Forge,
    mirror_token: Option<&str>,
    params: AddPushMirrorParams,
) -> Result<CallToolResult, McpError> {
    let use_ssh = params.use_ssh.unwrap_or(false);
    let mut body = serde_json::Map::new();
    body.insert(
        "remote_address".to_owned(),
        Value::String(params.remote_address),
    );
    body.insert(
        "interval".to_owned(),
        Value::String(
            params
                .interval
                .unwrap_or_else(|| DEFAULT_MIRROR_INTERVAL.to_owned()),
        ),
    );
    body.insert(
        "sync_on_commit".to_owned(),
        Value::Bool(params.sync_on_commit.unwrap_or(true)),
    );
    body.insert("use_ssh".to_owned(), Value::Bool(use_ssh));
    if let Some(filter) = params.branch_filter {
        body.insert("branch_filter".to_owned(), Value::String(filter));
    }
    // Password auth needs a username and the server-held credential; SSH auth needs neither
    // (Forgejo generates a deploy key, returned as `public_key` for you to add on the remote).
    if !use_ssh {
        let username = params.remote_username.ok_or_else(|| {
            McpError::invalid_params(
                "remote_username is required for password auth (or set use_ssh=true)".to_owned(),
                None,
            )
        })?;
        let token = mirror_token.ok_or_else(|| {
            McpError::invalid_params(
                "no push credential configured: set FORGEJO_MIRROR_TOKEN on the server to the \
                 remote's password/token (e.g. a GitHub PAT with contents:write), or pass \
                 use_ssh=true"
                    .to_owned(),
                None,
            )
        })?;
        body.insert("remote_username".to_owned(), Value::String(username));
        body.insert(
            "remote_password".to_owned(),
            Value::String(token.to_owned()),
        );
    }
    // The PushMirror response never includes the password — safe to return verbatim.
    let created = forge
        .add_push_mirror(&params.owner, &params.repo, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&created)
}

/// Lists the push mirrors on `owner/repo` (secrets are never part of the response).
pub async fn list_push_mirrors(forge: &Forge, params: RepoRef) -> Result<CallToolResult, McpError> {
    let (raw, total) = forge
        .list_push_mirrors(&params.owner, &params.repo, None, None)
        .await
        .map_err(to_mcp)?;
    paged_result(None, None, total, &into_items(raw))
}

/// Removes a push mirror from `owner/repo` by its `remote_name`.
pub async fn delete_push_mirror(
    forge: &Forge,
    params: DeletePushMirrorParams,
) -> Result<CallToolResult, McpError> {
    forge
        .delete_push_mirror(&params.owner, &params.repo, &params.remote_name)
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({ "deleted": params.remote_name }))
}

/// Triggers an immediate sync of every push mirror on `owner/repo`.
pub async fn sync_push_mirrors(forge: &Forge, params: RepoRef) -> Result<CallToolResult, McpError> {
    forge
        .sync_push_mirrors(&params.owner, &params.repo)
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({
        "sync_requested": format!("{}/{}", params.owner, params.repo),
    }))
}

// --- releases (and their downloadable assets) ---

/// Parameters for the `list_releases` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListReleasesParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// 1-based page number; omit (with `limit`) to auto-paginate the whole list.
    #[serde(default)]
    pub page: Option<u32>,
    /// Items per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A release addressed by its git tag.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ReleaseTagRef {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// The git tag the release is attached to, e.g. `v1.2.0`.
    pub tag: String,
}

/// A release addressed by its numeric id (the `id` from `list_releases` or `create_release`).
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ReleaseRef {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Release id.
    pub release_id: i64,
}

/// Parameters for the `create_release` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CreateReleaseParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Git tag for the release, e.g. `v1.2.0`. Must already exist unless `target_commitish`
    /// is given, in which case the tag is created at that commit.
    pub tag_name: String,
    /// Release title. Defaults to the tag name.
    #[serde(default)]
    pub name: Option<String>,
    /// Release notes (markdown).
    #[serde(default)]
    pub body: Option<String>,
    /// Commit, branch or ref to create the tag at when it does not exist yet.
    #[serde(default)]
    pub target_commitish: Option<String>,
    /// Publish as a draft (not visible to others). Defaults to false.
    #[serde(default)]
    pub draft: Option<bool>,
    /// Mark as a pre-release. Defaults to false.
    #[serde(default)]
    pub prerelease: Option<bool>,
}

/// Parameters for the `upload_release_asset` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct UploadReleaseAssetParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Release id to attach the file to.
    pub release_id: i64,
    /// Path to the local file to upload. Must resolve to a regular file inside the server's
    /// configured `FORGEJO_UPLOAD_ROOT`.
    pub file_path: String,
    /// Published filename. Defaults to the file's own name.
    #[serde(default)]
    pub name: Option<String>,
}

/// Parameters for the `delete_release_asset` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct DeleteReleaseAssetParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Release id the asset belongs to.
    pub release_id: i64,
    /// Attachment id, from `list_release_assets`.
    pub attachment_id: i64,
}

/// Where release-asset uploads may read from, and how large they may be.
///
/// Uploading is the only thing this server does that reads the local disk, and whatever it
/// reads becomes a publicly downloadable file — so the capability is opt-in and bounded rather
/// than implicit.
#[derive(Debug, Clone)]
pub struct UploadPolicy {
    /// Directory uploads must sit inside (`FORGEJO_UPLOAD_ROOT`). `None` disables uploading.
    pub root: Option<std::path::PathBuf>,
    /// Largest file that may be uploaded, in bytes (`FORGEJO_UPLOAD_MAX_MB`).
    pub max_bytes: u64,
}

/// One file attached to a release.
#[derive(Debug, Serialize)]
struct AssetSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    download_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    browser_download_url: Option<String>,
}

/// A release reduced to what identifies it and what can be downloaded from it.
///
/// The raw object also carries the full uploader `User` and the tarball/zipball URLs; those are
/// dropped here for the same reason the other list tools slim — a release list is otherwise
/// mostly boilerplate.
#[derive(Debug, Serialize)]
struct ReleaseSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prerelease: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    /// Attached files. Empty for a release that has none.
    assets: Vec<AssetSummary>,
}

/// Projects one raw asset object onto [`AssetSummary`].
fn slim_asset(value: &Value) -> AssetSummary {
    AssetSummary {
        id: value.get("id").and_then(Value::as_i64),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        size: value.get("size").and_then(Value::as_i64),
        download_count: value.get("download_count").and_then(Value::as_i64),
        browser_download_url: value
            .get("browser_download_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

/// Projects raw release objects down to [`ReleaseSummary`], flattening the uploader to a login.
fn slim_releases(items: Vec<Value>) -> Vec<ReleaseSummary> {
    items
        .into_iter()
        .map(|value: Value| ReleaseSummary {
            id: value.get("id").and_then(Value::as_i64),
            tag_name: value
                .get("tag_name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            draft: value.get("draft").and_then(Value::as_bool),
            prerelease: value.get("prerelease").and_then(Value::as_bool),
            published_at: value
                .get("published_at")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            html_url: value
                .get("html_url")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            author: value
                .get("author")
                .and_then(|a| a.get("login"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            assets: value
                .get("assets")
                .and_then(Value::as_array)
                .map(|assets| assets.iter().map(slim_asset).collect())
                .unwrap_or_default(),
        })
        .collect()
}

/// Lists a repository's releases, newest first.
pub async fn list_releases(
    forge: &Forge,
    params: ListReleasesParams,
) -> Result<CallToolResult, McpError> {
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| {
            Box::pin(forge.list_releases(&params.owner, &params.repo, Some(page), Some(limit)))
        })
        .await
        .map_err(to_mcp)?;
        return gathered_result(&slim_releases(all.items), all.total, all.truncated);
    }
    let (releases, total) = forge
        .list_releases(&params.owner, &params.repo, params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    paged_result(
        params.page,
        params.limit,
        total,
        &slim_releases(into_items(releases)),
    )
}

/// Gets one release by its git tag — the lookup that makes a release script idempotent, since
/// the tag is known before the release exists and the numeric id only after.
pub async fn get_release(forge: &Forge, params: ReleaseTagRef) -> Result<CallToolResult, McpError> {
    let release = forge
        .get_release_by_tag(&params.owner, &params.repo, &params.tag)
        .await
        .map_err(to_mcp)?;
    json_result(&release)
}

/// Lists the files attached to one release.
pub async fn list_release_assets(
    forge: &Forge,
    params: ReleaseRef,
) -> Result<CallToolResult, McpError> {
    let assets = forge
        .list_release_assets(&params.owner, &params.repo, params.release_id)
        .await
        .map_err(to_mcp)?;
    let items: Vec<AssetSummary> = into_items(assets).iter().map(slim_asset).collect();
    json_result(&serde_json::json!({
        "release_id": params.release_id,
        "returned": items.len(),
        "items": items,
    }))
}

/// Creates a release on an existing tag.
pub async fn create_release(
    forge: &Forge,
    params: CreateReleaseParams,
) -> Result<CallToolResult, McpError> {
    if params.tag_name.trim().is_empty() {
        return Err(McpError::invalid_params(
            "tag_name must not be empty".to_owned(),
            None,
        ));
    }
    let mut body = serde_json::Map::new();
    body.insert(
        "tag_name".to_owned(),
        Value::String(params.tag_name.clone()),
    );
    // Forgejo defaults an omitted name to the tag, but says so nowhere in the response — set it
    // explicitly so the created release reads the same on both forges.
    body.insert(
        "name".to_owned(),
        Value::String(params.name.unwrap_or(params.tag_name)),
    );
    if let Some(notes) = params.body {
        body.insert("body".to_owned(), Value::String(notes));
    }
    if let Some(target) = params.target_commitish {
        body.insert("target_commitish".to_owned(), Value::String(target));
    }
    body.insert(
        "draft".to_owned(),
        Value::Bool(params.draft.unwrap_or(false)),
    );
    body.insert(
        "prerelease".to_owned(),
        Value::Bool(params.prerelease.unwrap_or(false)),
    );

    let release = forge
        .create_release(&params.owner, &params.repo, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&release)
}

/// Reads a local file for upload, enforcing [`UploadPolicy`].
///
/// Split out from [`upload_release_asset`] because this is the part with teeth: what it returns
/// becomes a publicly downloadable file. The path is resolved through symlinks first and then
/// required to sit under the configured root, so neither a `..` traversal nor a link pointing
/// out of the tree escapes it. An unconfigured root refuses everything rather than falling back
/// to the working directory — an MCP server's cwd is whatever its client happened to launch it
/// from, which is no basis for deciding what may be published.
fn read_upload(
    policy: &UploadPolicy,
    file_path: &str,
    name: Option<String>,
) -> Result<(String, Vec<u8>), McpError> {
    let Some(configured_root) = policy.root.as_deref() else {
        return Err(McpError::invalid_params(
            "uploads are disabled: set FORGEJO_UPLOAD_ROOT on the MCP server to the directory \
             release assets may be read from, then restart it"
                .to_owned(),
            None,
        ));
    };
    let root = configured_root.canonicalize().map_err(|e| {
        McpError::internal_error(
            format!(
                "FORGEJO_UPLOAD_ROOT ({}) cannot be resolved: {e}",
                configured_root.display()
            ),
            None,
        )
    })?;
    let path = std::path::Path::new(file_path)
        .canonicalize()
        .map_err(|e| {
            McpError::invalid_params(format!("cannot resolve file_path {file_path}: {e}"), None)
        })?;
    if !path.starts_with(&root) {
        return Err(McpError::invalid_params(
            format!(
                "{} is outside FORGEJO_UPLOAD_ROOT ({})",
                path.display(),
                root.display()
            ),
            None,
        ));
    }

    let meta = std::fs::metadata(&path).map_err(|e| {
        McpError::invalid_params(format!("cannot stat {}: {e}", path.display()), None)
    })?;
    if !meta.is_file() {
        return Err(McpError::invalid_params(
            format!("{} is not a regular file", path.display()),
            None,
        ));
    }
    if meta.len() > policy.max_bytes {
        return Err(McpError::invalid_params(
            format!(
                "{} is {} bytes, over the {} byte limit — raise FORGEJO_UPLOAD_MAX_MB to upload it",
                path.display(),
                meta.len(),
                policy.max_bytes
            ),
            None,
        ));
    }

    let published = match name {
        Some(n) => n,
        None => path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("{} has no usable filename; pass `name`", path.display()),
                    None,
                )
            })?
            .to_owned(),
    };
    // Forgejo takes the published name verbatim, so keep path syntax out of it.
    if published.is_empty() || published.contains('/') || published.contains('\\') {
        return Err(McpError::invalid_params(
            format!("asset name {published:?} must be a plain filename"),
            None,
        ));
    }

    let bytes = std::fs::read(&path).map_err(|e| {
        McpError::invalid_params(format!("cannot read {}: {e}", path.display()), None)
    })?;
    Ok((published, bytes))
}

/// Uploads a local file as a release asset.
///
/// The file is read under [`UploadPolicy`]; see [`read_upload`] for why that is not optional.
pub async fn upload_release_asset(
    forge: &Forge,
    policy: &UploadPolicy,
    params: UploadReleaseAssetParams,
) -> Result<CallToolResult, McpError> {
    let (name, bytes) = read_upload(policy, &params.file_path, params.name)?;
    let uploaded = bytes.len();
    let asset = forge
        .upload_release_asset(&params.owner, &params.repo, params.release_id, &name, bytes)
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({
        "uploaded": name,
        "bytes": uploaded,
        "source": params.file_path,
        "asset": asset,
    }))
}

/// Removes one file from a release. Forgejo keeps same-named assets side by side, so replacing
/// an asset means deleting the old one first.
pub async fn delete_release_asset(
    forge: &Forge,
    params: DeleteReleaseAssetParams,
) -> Result<CallToolResult, McpError> {
    forge
        .delete_release_asset(
            &params.owner,
            &params.repo,
            params.release_id,
            params.attachment_id,
        )
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({
        "deleted": true,
        "release_id": params.release_id,
        "attachment_id": params.attachment_id,
    }))
}

// --- actions (CI) ---

/// Parameters for the `list_workflow_runs` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListWorkflowRunsParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Filter by head commit SHA — the most reliable way to find the run for a given push.
    #[serde(default)]
    pub head_sha: Option<String>,
    /// Filter by branch or tag ref, e.g. `refs/heads/main`.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    /// Filter by run status, e.g. `success`, `failure`, `running`, `waiting`.
    #[serde(default)]
    pub status: Option<String>,
    /// Filter by triggering event, e.g. `push`, `pull_request`, `workflow_dispatch`.
    #[serde(default)]
    pub event: Option<String>,
    /// Filter by workflow file name, e.g. `ci.yml`.
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A slimmed workflow run, in one shape regardless of which forge produced it.
///
/// Forgejo and Gitea model a run differently — Gitea copied GitHub's vocabulary, Forgejo kept
/// its own — so this is the union, filled from whichever spelling the instance used. Every
/// field is optional: a run missing one is reported without it rather than dropped.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct RunSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    /// Per-repository run counter — Forgejo's `index_in_repo`, Gitea's `run_number`.
    #[serde(skip_serializing_if = "Option::is_none")]
    run_number: Option<i64>,
    /// Forgejo's `title`, Gitea's `display_title`.
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// The run's outcome where it has one, else how far it has got. On Gitea, whose `status`
    /// only says `queued`/`in_progress`/`completed`, the terminal `conclusion` is promoted
    /// into this field so the same question can be asked of either forge.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    /// Gitea's raw `conclusion`, kept alongside `status` for callers that want the
    /// distinction. Never present on Forgejo, which has no such field.
    #[serde(skip_serializing_if = "Option::is_none")]
    conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    /// Workflow file name — Forgejo's `workflow_id`, or the basename of Gitea's `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
    /// Forgejo's `commit_sha`, Gitea's `head_sha`.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_sha: Option<String>,
    /// Forgejo's `prettyref`, Gitea's `head_branch`.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<String>,
    /// Forgejo's `created`, Gitea's `created_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<String>,
    /// Forgejo's `started`, Gitea's `started_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    started: Option<String>,
    /// Forgejo's `stopped`, Gitea's `completed_at`.
    #[serde(skip_serializing_if = "Option::is_none")]
    stopped: Option<String>,
}

/// Reads the first of `keys` that holds a non-empty string.
///
/// Both forges emit `""` for "not set yet" on the timestamp and conclusion fields, so an empty
/// string has to count as absent — otherwise a queued run reports a blank conclusion and
/// `status` would be overwritten with nothing.
fn first_str(run: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| run.get(*k).and_then(Value::as_str))
        .find(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Reads the first of `keys` that holds an integer.
fn first_i64(run: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| run.get(*k)?.as_i64())
}

/// Projects one raw workflow run — Forgejo's or Gitea's — onto [`RunSummary`].
///
/// Allow-listing the fields is what keeps the verbose parts out: the embedded `repository`
/// object, the whole `event_payload`, and `trigger_user`, which carries an email address.
fn normalize_run(run: &Value) -> RunSummary {
    let conclusion = first_str(run, &["conclusion"]);
    // Gitea packs both the workflow file and the ref into one `path` field; Forgejo reports
    // them separately as `workflow_id` and `prettyref`.
    let gitea_path = first_str(run, &["path"]);
    let (path_workflow, path_ref) = gitea_path.as_deref().map_or((None, None), |p| {
        let (workflow, git_ref) = split_gitea_path(p);
        (
            Some(workflow_basename(workflow)),
            git_ref.map(|r| pretty_ref(r).to_owned()),
        )
    });
    RunSummary {
        id: first_i64(run, &["id"]),
        run_number: first_i64(run, &["index_in_repo", "run_number"]),
        title: first_str(run, &["title", "display_title"]),
        // Gitea's `conclusion` is the outcome and its `status` merely the phase, so prefer the
        // conclusion once there is one. Forgejo has no conclusion and its `status` is already
        // the outcome.
        status: conclusion.clone().or_else(|| first_str(run, &["status"])),
        conclusion,
        event: first_str(run, &["event"]),
        workflow: first_str(run, &["workflow_id"]).or(path_workflow),
        commit_sha: first_str(run, &["commit_sha", "head_sha"]),
        // `head_branch` is populated only for branch runs — it is null for tags and pull
        // requests — so the ref embedded in `path` is the reliable fallback.
        git_ref: first_str(run, &["prettyref", "head_branch"]).or(path_ref),
        html_url: first_str(run, &["html_url"]),
        created: first_str(run, &["created", "created_at"]),
        started: first_str(run, &["started", "started_at"]),
        stopped: first_str(run, &["stopped", "completed_at"]),
    }
}

/// Splits Gitea's workflow `path` into its file name and the ref the run was for.
///
/// Despite the name, this is not a filesystem path: Gitea reports
/// `test-pr.yml@refs/pull/1117/head`, or `release-nightly.yml@refs/heads/main` for a branch
/// run — the workflow file, an `@`, and the fully-qualified ref. Confirmed against live runs on
/// `gitea.com`; reading it as a path yields `head` as the "workflow", which is what the first
/// live probe of this code returned.
fn split_gitea_path(path: &str) -> (&str, Option<&str>) {
    match path.split_once('@') {
        Some((workflow, git_ref)) => (workflow, Some(git_ref)),
        None => (path, None),
    }
}

/// Shortens a fully-qualified ref to the form Forgejo's `prettyref` reports — `refs/heads/main`
/// and `refs/tags/v1.0` become `main` and `v1.0`.
///
/// A pull-request ref (`refs/pull/1117/head`) has no short form and is left whole, which at
/// least says plainly that the run belonged to a pull request.
fn pretty_ref(git_ref: &str) -> &str {
    git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref)
}

/// Reduces a workflow file reference to its bare name. Gitea reports `ci.yml` directly, but
/// this keeps a directory-prefixed value from leaking through if that ever changes.
fn workflow_basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// Projects raw workflow-run objects down to [`RunSummary`], skipping any that aren't objects.
fn slim_runs(items: &[Value]) -> Vec<RunSummary> {
    items
        .iter()
        .filter(|v| v.is_object())
        .map(normalize_run)
        .collect()
}

/// Lists workflow runs in `owner/repo`, optionally filtered.
///
/// The endpoint returns a `{ workflow_runs, total_count }` wrapper with no `X-Total-Count`
/// header (confirmed against the live Forgejo API), so this unwraps the body like
/// `search_repos` rather than auto-paginating via `gather_all`. Both forges use that wrapper;
/// the runs inside it differ, which [`normalize_run`] reconciles.
///
/// A `404` usually means the repository has Actions disabled, not that there are no runs —
/// with one Gitea-only exception: because a workflow filter becomes a path segment there, an
/// unknown `workflow_id` also 404s (`workflow "ci.yml" not found`), where Forgejo returns an
/// empty list. Both confirmed live.
pub async fn list_workflow_runs(
    forge: &Forge,
    params: ListWorkflowRunsParams,
) -> Result<CallToolResult, McpError> {
    let filters = RunFilters {
        head_sha: params.head_sha,
        git_ref: params.git_ref,
        status: params.status,
        event: params.event,
        workflow: params.workflow_id,
    };
    let body = forge
        .list_workflow_runs(
            &params.owner,
            &params.repo,
            &filters,
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    let (items, total) = match body {
        Value::Object(mut map) => (
            into_items(map.remove("workflow_runs").unwrap_or(Value::Null)),
            map.get("total_count")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok()),
        ),
        _ => (Vec::new(), None),
    };
    paged_result(params.page, params.limit, total, &slim_runs(&items))
}

/// Gets one workflow run by id (full object, not slimmed).
pub async fn get_workflow_run(forge: &Forge, params: RunRef) -> Result<CallToolResult, McpError> {
    let run = forge
        .get_workflow_run(&params.owner, &params.repo, params.run_id)
        .await
        .map_err(to_mcp)?;
    json_result(&run)
}

/// Parameters for the `dispatch_workflow` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct DispatchWorkflowParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Workflow file name as it appears in `.forgejo/workflows/`, `.gitea/workflows/` or
    /// `.github/workflows/`, e.g. `ci.yml`. This server exposes no list-workflows tool
    /// (Forgejo has no such endpoint) — read the directory with `get_file_contents` if you
    /// don't know the name. The workflow must declare an `on: workflow_dispatch` trigger.
    pub workflow: String,
    /// Git ref to run on — a branch or tag, e.g. `main`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Optional `workflow_dispatch` inputs (key/value), matching the workflow's `inputs:`.
    #[serde(default)]
    pub inputs: Option<serde_json::Map<String, Value>>,
}

/// Triggers a `workflow_dispatch` run.
///
/// Forgejo answers with the run it created (via its `return_run_info` extension) and that is
/// passed straight through. Gitea answers `204 No Content`, so there is nothing to return but
/// the acknowledgement — hence the synthesized object, which says plainly that the run was
/// accepted and that finding it means listing runs.
pub async fn dispatch_workflow(
    forge: &Forge,
    params: DispatchWorkflowParams,
) -> Result<CallToolResult, McpError> {
    let run = forge
        .dispatch_workflow(
            &params.owner,
            &params.repo,
            &params.workflow,
            &params.git_ref,
            params.inputs,
        )
        .await
        .map_err(to_mcp)?;
    if run.is_null() {
        return json_result(&serde_json::json!({
            "dispatched": true,
            "workflow": params.workflow,
            "ref": params.git_ref,
            "note": "The instance accepted the dispatch but returned no run details. \
                     Call list_workflow_runs with this workflow and ref to find the run.",
        }));
    }
    json_result(&run)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the system temp dir, removed when the guard drops. Each upload
    /// test needs a real tree on disk: the guard resolves symlinks and `..` through the actual
    /// filesystem, so it cannot be tested against synthetic paths.
    struct TempTree(std::path::PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "forgejo-mcp-upload-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            // Canonicalize the root itself: on macOS the temp dir sits under `/var`, a symlink to
            // `/private/var`, and an uncanonicalized root would make these tests pass on that
            // difference rather than on the behaviour under test.
            Self(dir.canonicalize().unwrap())
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }

        fn write(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn policy(root: Option<&std::path::Path>, max_bytes: u64) -> UploadPolicy {
        UploadPolicy {
            root: root.map(std::path::Path::to_path_buf),
            max_bytes,
        }
    }

    #[test]
    fn upload_is_refused_when_no_root_is_configured() {
        let tree = TempTree::new("noroot");
        let file = tree.write("asset.tar.gz", b"payload");

        let err = read_upload(&policy(None, 1024), file.to_str().unwrap(), None)
            .expect_err("an unset root must refuse, not fall back to something permissive");
        assert!(
            err.message.contains("FORGEJO_UPLOAD_ROOT"),
            "the error should name the variable to set: {}",
            err.message
        );
    }

    #[test]
    fn a_file_inside_the_root_is_read() {
        let tree = TempTree::new("inside");
        let file = tree.write("dist/asset.tar.gz", b"payload");

        let (name, bytes) = read_upload(
            &policy(Some(tree.path()), 1024),
            file.to_str().unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(name, "asset.tar.gz", "the name defaults to the file's own");
        assert_eq!(bytes, b"payload");
    }

    #[test]
    fn an_explicit_name_overrides_the_filename() {
        let tree = TempTree::new("rename");
        let file = tree.write("asset.tar.gz", b"payload");

        let (name, _) = read_upload(
            &policy(Some(tree.path()), 1024),
            file.to_str().unwrap(),
            Some("xmcp-1.0.0-aarch64.tar.gz".to_owned()),
        )
        .unwrap();
        assert_eq!(name, "xmcp-1.0.0-aarch64.tar.gz");
    }

    #[test]
    fn a_path_outside_the_root_is_refused() {
        let root = TempTree::new("root");
        let outside = TempTree::new("outside");
        let secret = outside.write("id_rsa", b"-----BEGIN PRIVATE KEY-----");

        let err = read_upload(
            &policy(Some(root.path()), 1024),
            secret.to_str().unwrap(),
            None,
        )
        .expect_err("a file outside the root must be refused");
        assert!(
            err.message.contains("outside FORGEJO_UPLOAD_ROOT"),
            "unexpected message: {}",
            err.message
        );
    }

    /// The traversal case the `starts_with` check exists for: a path that *textually* begins
    /// with the root but climbs back out of it.
    #[test]
    fn a_dotdot_traversal_out_of_the_root_is_refused() {
        let root = TempTree::new("traverse-root");
        let outside = TempTree::new("traverse-out");
        let _secret = outside.write("id_rsa", b"key");

        let climbing = format!(
            "{}/../{}/id_rsa",
            root.path().display(),
            outside.path().file_name().unwrap().to_str().unwrap()
        );

        let err = read_upload(&policy(Some(root.path()), 1024), &climbing, None)
            .expect_err("`..` must not escape the root");
        assert!(
            err.message.contains("outside FORGEJO_UPLOAD_ROOT"),
            "unexpected message: {}",
            err.message
        );
    }

    /// Canonicalizing before the check is what catches this: the path sits inside the root but
    /// the file it names does not.
    #[test]
    #[cfg(unix)]
    fn a_symlink_pointing_out_of_the_root_is_refused() {
        let root = TempTree::new("symlink-root");
        let outside = TempTree::new("symlink-out");
        let secret = outside.write("id_rsa", b"key");
        let link = root.path().join("innocent.tar.gz");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let err = read_upload(
            &policy(Some(root.path()), 1024),
            link.to_str().unwrap(),
            None,
        )
        .expect_err("a symlink out of the root must be refused");
        assert!(
            err.message.contains("outside FORGEJO_UPLOAD_ROOT"),
            "unexpected message: {}",
            err.message
        );
    }

    #[test]
    fn a_file_over_the_size_limit_is_refused() {
        let tree = TempTree::new("toobig");
        let file = tree.write("big.bin", &[0u8; 2048]);

        let err = read_upload(
            &policy(Some(tree.path()), 1024),
            file.to_str().unwrap(),
            None,
        )
        .expect_err("an oversized file must be refused");
        assert!(
            err.message.contains("FORGEJO_UPLOAD_MAX_MB"),
            "the error should say how to raise the limit: {}",
            err.message
        );
    }

    #[test]
    fn a_directory_is_not_uploadable() {
        let tree = TempTree::new("dir");
        std::fs::create_dir_all(tree.path().join("subdir")).unwrap();

        let err = read_upload(
            &policy(Some(tree.path()), 1024),
            tree.path().join("subdir").to_str().unwrap(),
            None,
        )
        .expect_err("a directory must be refused");
        assert!(
            err.message.contains("not a regular file"),
            "unexpected message: {}",
            err.message
        );
    }

    #[test]
    fn a_published_name_may_not_carry_path_syntax() {
        let tree = TempTree::new("badname");
        let file = tree.write("asset.tar.gz", b"payload");

        for bad in ["../escape", "dir/asset.tar.gz", ""] {
            let err = read_upload(
                &policy(Some(tree.path()), 1024),
                file.to_str().unwrap(),
                Some(bad.to_owned()),
            )
            .expect_err("a name with path syntax must be refused");
            assert!(
                err.message.contains("plain filename"),
                "unexpected message for {bad:?}: {}",
                err.message
            );
        }
    }

    #[test]
    fn a_missing_file_reports_the_path() {
        let tree = TempTree::new("missing");
        let missing = tree.path().join("nope.tar.gz");

        let err = read_upload(
            &policy(Some(tree.path()), 1024),
            missing.to_str().unwrap(),
            None,
        )
        .expect_err("a missing file must be refused");
        assert!(
            err.message.contains("cannot resolve file_path"),
            "unexpected message: {}",
            err.message
        );
    }

    #[test]
    fn releases_are_slimmed_to_their_identity_and_assets() {
        let raw = serde_json::json!([{
            "id": 42,
            "tag_name": "v3.3.1",
            "name": "xojo-mcp 3.3.1",
            "draft": false,
            "prerelease": false,
            "published_at": "2026-09-18T10:00:00Z",
            "html_url": "https://codeberg.org/brechanbech/xojo-mcp/releases/tag/v3.3.1",
            "author": { "login": "brechanbech", "email": "hidden@example.com" },
            "tarball_url": "https://codeberg.org/…/v3.3.1.tar.gz",
            "assets": [{
                "id": 7,
                "name": "xmcp-3.3.1-aarch64-apple-darwin.tar.gz",
                "size": 532_480,
                "download_count": 0,
                "browser_download_url": "https://codeberg.org/…/xmcp.tar.gz",
                "uuid": "dropped"
            }]
        }]);

        let slimmed = slim_releases(into_items(raw));
        assert_eq!(slimmed.len(), 1);
        let release = &slimmed[0];
        assert_eq!(release.id, Some(42));
        assert_eq!(release.tag_name.as_deref(), Some("v3.3.1"));
        assert_eq!(
            release.author.as_deref(),
            Some("brechanbech"),
            "the uploader should flatten to a login, dropping the rest of the User object"
        );
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].id, Some(7));
        assert_eq!(release.assets[0].size, Some(532_480));

        // The serialized form is what reaches the model: no email, no tarball boilerplate.
        let json = serde_json::to_string(&slimmed).unwrap();
        assert!(!json.contains("hidden@example.com"), "author email leaked");
        assert!(!json.contains("tarball_url"), "tarball boilerplate leaked");
    }

    #[test]
    fn a_release_without_assets_serializes_an_empty_list() {
        let raw = serde_json::json!([{ "id": 1, "tag_name": "v0.1.0" }]);
        let slimmed = slim_releases(into_items(raw));
        assert!(slimmed[0].assets.is_empty());
    }

    #[test]
    fn edit_repo_body_contains_exactly_the_set_fields() {
        let params = EditRepoParams {
            owner: "brechanbech".to_owned(),
            repo: "rpn42s-mcp-rs".to_owned(),
            private: Some(false),
            description: None,
            website: None,
            default_branch: Some("main".to_owned()),
            has_issues: None,
            has_pull_requests: None,
            has_wiki: None,
            archived: None,
        };
        let body = edit_repo_body(params);
        assert_eq!(
            Value::Object(body),
            serde_json::json!({ "private": false, "default_branch": "main" })
        );
    }

    #[test]
    fn edit_repo_body_is_empty_when_nothing_is_set() {
        let params = EditRepoParams {
            owner: "o".to_owned(),
            repo: "r".to_owned(),
            private: None,
            description: None,
            website: None,
            default_branch: None,
            has_issues: None,
            has_pull_requests: None,
            has_wiki: None,
            archived: None,
        };
        assert!(edit_repo_body(params).is_empty());
    }

    /// A minimal, valid set of migrate params; tests tweak the fields they care about.
    fn migrate_params(clone_addr: &str) -> MigrateRepoParams {
        MigrateRepoParams {
            clone_addr: clone_addr.to_owned(),
            repo_name: "repo".to_owned(),
            repo_owner: None,
            service: None,
            description: None,
            private: None,
            auth_username: None,
            authenticate: None,
            issues: None,
            pull_requests: None,
            labels: None,
            milestones: None,
            releases: None,
            wiki: None,
            lfs: None,
            lfs_endpoint: None,
            mirror: None,
            mirror_interval: None,
        }
    }

    #[test]
    fn migrate_repo_body_sends_only_set_fields_and_defaults_to_private() {
        let mut params = migrate_params("https://codeberg.org/brechanbech/sec-mcp.git");
        params.service = Some("gitea".to_owned());
        params.issues = Some(true);
        params.wiki = Some(false);
        let body = migrate_repo_body(params, None).unwrap();
        assert_eq!(
            Value::Object(body),
            serde_json::json!({
                "clone_addr": "https://codeberg.org/brechanbech/sec-mcp.git",
                "repo_name": "repo",
                "private": true,
                "service": "gitea",
                "issues": true,
                "wiki": false,
            })
        );
    }

    #[test]
    fn migrate_repo_body_rejects_a_non_http_clone_addr() {
        for addr in [
            "git@codeberg.org:o/r.git",
            "git://host/r.git",
            "/etc/passwd",
        ] {
            assert!(
                migrate_repo_body(migrate_params(addr), None).is_err(),
                "{addr} should be refused"
            );
        }
    }

    #[test]
    fn migrate_repo_body_rejects_an_unknown_service() {
        let mut params = migrate_params("https://example.com/o/r.git");
        // A plausible-looking mistake: there is no "forgejo" service, it's "gitea".
        params.service = Some("forgejo".to_owned());
        assert!(migrate_repo_body(params, None).is_err());
    }

    #[test]
    fn migrate_repo_body_omits_credentials_unless_auth_is_requested() {
        let body =
            migrate_repo_body(migrate_params("https://example.com/o/r.git"), Some("tok")).unwrap();
        assert!(body.get("auth_token").is_none(), "public source: no token");
        assert!(body.get("auth_username").is_none());
    }

    #[test]
    fn migrate_repo_body_splices_in_the_server_token_when_auth_is_requested() {
        // A username implies authentication...
        let mut params = migrate_params("https://example.com/o/r.git");
        params.auth_username = Some("brechanbech".to_owned());
        let body = migrate_repo_body(params, Some("tok")).unwrap();
        assert_eq!(body["auth_username"], "brechanbech");
        assert_eq!(body["auth_token"], "tok");

        // ...and so does `authenticate` alone, for token-only forges.
        let mut params = migrate_params("https://example.com/o/r.git");
        params.authenticate = Some(true);
        let body = migrate_repo_body(params, Some("tok")).unwrap();
        assert_eq!(body["auth_token"], "tok");
        assert!(body.get("auth_username").is_none());
    }

    #[test]
    fn migrate_repo_body_fails_when_auth_is_requested_without_a_configured_token() {
        let mut params = migrate_params("https://example.com/o/r.git");
        params.auth_username = Some("brechanbech".to_owned());
        assert!(migrate_repo_body(params, None).is_err());
    }

    #[test]
    fn slim_repos_keeps_summary_fields_and_drops_the_rest() {
        let raw = vec![serde_json::json!({
            "full_name": "brechanbech/sec-mcp",
            "description": "MCP server for SEC EDGAR data",
            "language": "Rust",
            "stars_count": 0,
            "forks_count": 0,
            "open_issues_count": 0,
            "size": 101,
            "html_url": "https://codeberg.org/brechanbech/sec-mcp",
            // Verbose/nested fields that must NOT survive the slim:
            "owner": { "login": "brechanbech", "email": "person@example.com" },
            "internal_tracker": { "enable_time_tracker": true },
            "permissions": { "admin": true }
        })];
        let slim = slim_repos(raw);
        assert_eq!(slim.len(), 1);

        let v = serde_json::to_value(&slim[0]).unwrap();
        assert_eq!(v["full_name"], "brechanbech/sec-mcp");
        assert_eq!(v["language"], "Rust");
        assert_eq!(v["stars_count"], 0);
        assert_eq!(v["size"], 101);
        // Nested objects (and the owner's email) are dropped.
        assert!(v.get("owner").is_none());
        assert!(v.get("internal_tracker").is_none());
        assert!(v.get("permissions").is_none());
        // Absent optional fields are omitted, not serialized as null.
        assert!(v.get("private").is_none());
    }

    #[test]
    fn a_forgejo_run_normalizes_and_drops_the_verbose_fields() {
        let raw = vec![serde_json::json!({
            "id": 42,
            "index_in_repo": 7,
            "title": "CI",
            "status": "success",
            "event": "push",
            "workflow_id": "ci.yml",
            "commit_sha": "deadbeef",
            "prettyref": "main",
            "html_url": "https://codeberg.org/o/r/actions/runs/7",
            "created": "2026-07-06T00:00:00Z",
            "started": "2026-07-06T00:00:01Z",
            "stopped": "2026-07-06T00:01:00Z",
            // Verbose/nested fields that must NOT survive the slim:
            "repository": { "full_name": "o/r", "private": true },
            "trigger_user": { "login": "brechanbech", "email": "person@example.com" },
            "event_payload": "{...large json...}"
        })];
        let slim = slim_runs(&raw);
        assert_eq!(slim.len(), 1);

        let v = serde_json::to_value(&slim[0]).unwrap();
        assert_eq!(v["id"], 42);
        assert_eq!(v["run_number"], 7);
        assert_eq!(v["status"], "success");
        assert_eq!(v["workflow"], "ci.yml");
        assert_eq!(v["ref"], "main");
        assert_eq!(v["commit_sha"], "deadbeef");
        assert_eq!(v["created"], "2026-07-06T00:00:00Z");
        assert_eq!(v["stopped"], "2026-07-06T00:01:00Z");
        // Forgejo has no conclusion field, so none is invented.
        assert!(v.get("conclusion").is_none());
        // Nested/verbose fields (and the trigger user's email) are dropped.
        assert!(v.get("repository").is_none());
        assert!(v.get("trigger_user").is_none());
        assert!(v.get("event_payload").is_none());
    }

    #[test]
    fn a_gitea_run_normalizes_onto_the_same_shape() {
        // Field-for-field a real `gitea.com` response (gitea/tea run 1657, a branch push),
        // trimmed to the keys that matter. Note `path`: it is NOT a filesystem path.
        let raw = vec![serde_json::json!({
            "id": 42,
            "run_number": 7,
            "display_title": "CI",
            "status": "completed",
            "conclusion": "success",
            "event": "push",
            "path": "ci.yml@refs/heads/main",
            "head_branch": "main",
            "head_sha": "deadbeef",
            "html_url": "https://gitea.com/o/r/actions/runs/7",
            "created_at": "2026-07-06T00:00:00Z",
            "started_at": "2026-07-06T00:00:01Z",
            "completed_at": "2026-07-06T00:01:00Z",
            "repository": { "full_name": "o/r", "private": true },
            "actor": { "login": "brechanbech", "email": "person@example.com" },
            "pull_requests": [{ "number": 1 }]
        })];
        let slim = slim_runs(&raw);
        let v = serde_json::to_value(&slim[0]).unwrap();

        // Same keys, same values as the Forgejo run above — that is the whole point.
        assert_eq!(v["id"], 42);
        assert_eq!(v["run_number"], 7);
        assert_eq!(v["title"], "CI");
        assert_eq!(v["workflow"], "ci.yml");
        assert_eq!(v["ref"], "main");
        assert_eq!(v["commit_sha"], "deadbeef");
        assert_eq!(v["created"], "2026-07-06T00:00:00Z");
        assert_eq!(v["stopped"], "2026-07-06T00:01:00Z");
        // The conclusion is promoted into `status`, and also kept verbatim.
        assert_eq!(v["status"], "success");
        assert_eq!(v["conclusion"], "success");
        assert!(v.get("repository").is_none());
        assert!(v.get("actor").is_none());
        assert!(v.get("pull_requests").is_none());
    }

    #[test]
    fn a_gitea_pull_request_run_recovers_its_ref_from_the_path() {
        // Real shape from gitea.com (gitea/tea run 1664): `head_branch` is null on a pull
        // request run, so the ref has to come out of `path`. Reading `path` as a filesystem
        // path — the bug the first live probe caught — reported the workflow as "head".
        let raw = vec![serde_json::json!({
            "id": 920_800,
            "run_number": 1664,
            "path": "test-pr.yml@refs/pull/1117/head",
            "head_branch": null,
            "status": "completed",
            "conclusion": "success",
        })];
        let v = serde_json::to_value(&slim_runs(&raw)[0]).unwrap();
        assert_eq!(v["workflow"], "test-pr.yml");
        // No short form exists for a pull-request ref, so it stays whole and says so.
        assert_eq!(v["ref"], "refs/pull/1117/head");
    }

    #[test]
    fn a_gitea_tag_run_recovers_a_short_ref_from_the_path() {
        // Real shape from gitea.com (gitea/tea run 1662): `head_branch` is null for a tag too.
        let raw = vec![serde_json::json!({
            "id": 5,
            "path": "release-tag.yml@refs/tags/v0.16.0",
            "head_branch": null,
            "status": "completed",
            "conclusion": "success",
        })];
        let v = serde_json::to_value(&slim_runs(&raw)[0]).unwrap();
        assert_eq!(v["workflow"], "release-tag.yml");
        assert_eq!(v["ref"], "v0.16.0");
    }

    #[test]
    fn an_unfinished_gitea_run_reports_its_phase_not_a_blank_conclusion() {
        // Gitea sends "" rather than null for a conclusion that doesn't exist yet; an empty
        // string must not displace the status or appear as a field.
        let raw = vec![serde_json::json!({
            "id": 8,
            "status": "in_progress",
            "conclusion": "",
            "completed_at": "",
        })];
        let v = serde_json::to_value(&slim_runs(&raw)[0]).unwrap();
        assert_eq!(v["status"], "in_progress");
        assert!(v.get("conclusion").is_none());
        assert!(v.get("stopped").is_none());
    }

    #[test]
    fn a_run_that_is_not_an_object_is_skipped() {
        let raw = vec![Value::String("nonsense".to_owned()), serde_json::json!({})];
        // The string is dropped; the empty object survives as an all-absent summary.
        assert_eq!(slim_runs(&raw).len(), 1);
    }

    #[test]
    fn a_gitea_workflow_path_reduces_to_the_file_name_dispatch_expects() {
        assert_eq!(workflow_basename(".gitea/workflows/ci.yml"), "ci.yml");
        assert_eq!(
            workflow_basename(".github/workflows/release.yml"),
            "release.yml"
        );
        assert_eq!(workflow_basename("ci.yml"), "ci.yml");
    }

    // --- bounded file reads ---

    const SAMPLE: &str = "one\ntwo\nthree\nfour\nfive\n";

    #[test]
    fn no_bounds_returns_the_whole_file_and_no_window() {
        let (text, window) = slice_lines(SAMPLE, None, None, 5).unwrap();
        assert_eq!(text, SAMPLE, "unbounded read is byte-identical");
        assert!(window.is_none(), "no window to report");
    }

    #[test]
    fn a_window_slices_and_reports_what_it_applied() {
        let (text, window) = slice_lines(SAMPLE, Some(2), Some(4), 5).unwrap();
        assert_eq!(text, "two\nthree\nfour");
        assert_eq!(window, Some((2, 4)));

        // One bound alone: start-only runs to the end, end-only starts at line 1.
        let (text, window) = slice_lines(SAMPLE, Some(4), None, 5).unwrap();
        assert_eq!(text, "four\nfive");
        assert_eq!(window, Some((4, 5)));
        let (text, window) = slice_lines(SAMPLE, None, Some(2), 5).unwrap();
        assert_eq!(text, "one\ntwo");
        assert_eq!(window, Some((1, 2)));
    }

    #[test]
    fn bounds_clamp_to_the_file_rather_than_failing() {
        // end_line past the end stops at the last line.
        let (text, window) = slice_lines(SAMPLE, Some(4), Some(999), 5).unwrap();
        assert_eq!(text, "four\nfive");
        assert_eq!(window, Some((4, 5)));

        // start_line past the end is empty, not an error — the window says why.
        let (text, window) = slice_lines(SAMPLE, Some(99), None, 5).unwrap();
        assert!(text.is_empty());
        assert_eq!(window, Some((99, 5)));

        // start_line 0 is nonsense in a 1-indexed scheme; treat it as line 1.
        let (text, _) = slice_lines(SAMPLE, Some(0), Some(1), 5).unwrap();
        assert_eq!(text, "one");
    }

    #[test]
    fn an_inverted_window_is_refused() {
        let err = slice_lines(SAMPLE, Some(4), Some(2), 5).unwrap_err();
        assert!(
            err.to_string().contains("after end_line"),
            "message names the problem: {err}"
        );
    }

    // --- pull request diffs ---

    /// Two files, the second a rename, plus a hunk body containing a line that *looks* like a
    /// `---` file marker once the diff prefixes a `-` to it.
    const DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 fn main() {
--- not a header, just a removed line
+    println!(\"hi\");
 }
diff --git a/old/name.rs b/new/name.rs
similarity index 90%
rename from old/name.rs
rename to new/name.rs
--- a/old/name.rs
+++ b/new/name.rs
@@ -1 +1 @@
-old
+new
";

    #[test]
    fn diff_splits_into_one_section_per_file() {
        let sections = split_diff_sections(DIFF);
        assert_eq!(sections.len(), 2);
        assert!(sections[0].text.starts_with("diff --git a/src/main.rs"));
        assert!(sections[1].text.starts_with("diff --git a/old/name.rs"));
        // Every line is accounted for — nothing dropped on the floor.
        let rejoined: String = sections.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rejoined, DIFF);
    }

    #[test]
    fn a_rename_matches_on_either_side() {
        let sections = split_diff_sections(DIFF);
        assert!(sections[1].paths.contains(&"old/name.rs".to_owned()));
        assert!(sections[1].paths.contains(&"new/name.rs".to_owned()));
    }

    #[test]
    fn a_removed_line_that_looks_like_a_header_is_not_read_as_one() {
        let sections = split_diff_sections(DIFF);
        // "--- not a header, just a removed line" sits inside the hunk; it must not become a path.
        assert_eq!(
            sections[0].paths,
            vec!["src/main.rs".to_owned()],
            "only the real path, deduped across the a/ and b/ sides"
        );
    }

    #[test]
    fn truncation_cuts_at_a_line_boundary() {
        let (text, truncated) = truncate_to_lines(SAMPLE, 9);
        assert!(truncated);
        assert_eq!(text, "one\ntwo\n", "whole lines only, never a split line");

        let (text, truncated) = truncate_to_lines(SAMPLE, 1024);
        assert!(!truncated);
        assert_eq!(text, SAMPLE);
    }

    #[test]
    fn changed_files_slim_to_the_review_relevant_fields() {
        let raw = serde_json::json!([{
            "filename": "src/main.rs",
            "status": "changed",
            "additions": 2,
            "deletions": 1,
            "changes": 3,
            // The three URL fields restate the path; they must not survive.
            "html_url": "https://codeberg.org/o/r/src/commit/deadbeef/src/main.rs",
            "contents_url": "https://codeberg.org/api/v1/repos/o/r/contents/src/main.rs",
            "raw_url": "https://codeberg.org/o/r/raw/commit/deadbeef/src/main.rs"
        }]);
        let files = slim_changed_files(into_items(raw));
        assert_eq!(files.len(), 1);

        let v = serde_json::to_value(&files[0]).unwrap();
        assert_eq!(v["filename"], "src/main.rs");
        assert_eq!(v["additions"], 2);
        assert!(v.get("html_url").is_none());
        assert!(v.get("contents_url").is_none());
        assert!(v.get("raw_url").is_none());
        // Absent on a non-rename, and omitted rather than serialized as null.
        assert!(v.get("previous_filename").is_none());
    }

    /// A real diff, fetched verbatim from `codeberg.org/api/v1` (`forgejo/forgejo` PR
    /// 14137, trimmed to the first hunk of each file). Guards the parser against drifting
    /// away from what Forgejo actually serves — note the context text trailing the `@@`
    /// header, and the `index` line between the header and the `---` marker.
    const REAL_DIFF: &str = r"diff --git a/go.mod b/go.mod
index 9707e6d220..779439f1d5 100644
--- a/go.mod
+++ b/go.mod
@@ -262,3 +262,5 @@ replace github.com/mholt/archiver/v3 => code.forgejo.org/forgejo/archiver/v3 v3.
 replace github.com/gliderlabs/ssh => code.forgejo.org/forgejo/ssh v0.0.0-20241211213324-5fc306ca0616
 
 replace git.sr.ht/~mariusor/go-xsd-duration => code.forgejo.org/forgejo/go-xsd-duration v0.0.0-20220703122237-02e73435a078
+
+replace code.forgejo.org/xorm/xorm => code.forgejo.org/xorm/xorm v1.4.2-0.20260827232307-8552ec01718a
diff --git a/go.sum b/go.sum
index cc10f213c2..d3bf724104 100644
--- a/go.sum
+++ b/go.sum
@@ -40,8 +40,8 @@ code.forgejo.org/go-chi/captcha v1.0.3 h1:ii3VrlhSJeSyJA2GD/UvjY3tbzGB0ZH/og1ZPV
 code.forgejo.org/go-chi/captcha v1.0.3/go.mod h1:YXw47044t3pHWdigYyn+NMnccv0Y9h69kDwpMsCO2C4=
 code.forgejo.org/go-chi/session v1.0.4 h1:WQ1NaVxcCpxYwCliEGypKclZnOCjh3p1fk8XciJc62U=
 code.forgejo.org/go-chi/session v1.0.4/go.mod h1:+sSTiomM5C8AUPtxZyTENIbcTz22kcVottKO0lnmDRk=
";

    #[test]
    fn parses_a_real_forgejo_diff() {
        let sections = split_diff_sections(REAL_DIFF);
        assert_eq!(sections.len(), 2, "one section per changed file");
        assert_eq!(sections[0].paths, vec!["go.mod".to_owned()]);
        assert_eq!(sections[1].paths, vec!["go.sum".to_owned()]);

        // Selecting one file yields that file's hunks and nothing from its neighbour.
        let only_mod: String = sections
            .iter()
            .filter(|s| s.paths.iter().any(|p| p == "go.mod"))
            .map(|s| s.text.as_str())
            .collect();
        assert!(
            only_mod.contains("+replace code.forgejo.org/xorm/xorm"),
            "keeps the added line from go.mod"
        );
        assert!(!only_mod.contains("go-chi/captcha"), "no bleed from go.sum");
    }
}
