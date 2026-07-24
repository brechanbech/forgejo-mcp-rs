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

use super::client::Forge;

/// Returns the authenticated user — proof the token works.
pub async fn whoami(forge: &Forge) -> Result<CallToolResult, McpError> {
    let user = forge.user_get_current().await.map_err(to_mcp)?;
    json_result(&user)
}

/// Reports this MCP server's own version plus the Forgejo instance version it talks to.
///
/// The MCP-server version is compiled in (no network), so it's reported even if the instance
/// call fails — in which case `forgejo` carries the error text instead of a version string.
pub async fn version(forge: &Forge) -> Result<CallToolResult, McpError> {
    let mcp_server = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"));
    let forgejo = match forge.server_version().await {
        Ok(value) => value
            .get("version")
            .and_then(Value::as_str)
            .map_or_else(|| "unknown".to_owned(), ToOwned::to_owned),
        Err(e) => format!("unavailable: {e}"),
    };
    json_result(&serde_json::json!({
        "mcp_server": mcp_server,
        "forgejo": forgejo,
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
                out.insert("encoding".to_owned(), Value::String("utf-8".to_owned()));
                out.insert("content".to_owned(), Value::String(text));
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

/// A slimmed workflow run — the fields worth surfacing from a verbose `ActionRun`. All-optional
/// so a well-formed run never fails to deserialize. Note there is no `conclusion` field: the
/// terminal outcome (success/failure/…) lives in `status`.
#[derive(Debug, Serialize, serde::Deserialize)]
struct RunSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_in_repo: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prettyref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stopped: Option<String>,
}

/// Projects raw workflow-run objects down to [`RunSummary`], skipping any that don't fit.
fn slim_runs(items: Vec<Value>) -> Vec<RunSummary> {
    items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

/// Lists workflow runs in `owner/repo`, optionally filtered.
///
/// The endpoint returns a `{ workflow_runs, total_count }` wrapper with no `X-Total-Count`
/// header (confirmed against the live API), so this unwraps the body like `search_repos`
/// rather than auto-paginating via `gather_all`. A `404` from Forgejo here usually means the
/// repository has Actions disabled, not that there are no runs.
pub async fn list_workflow_runs(
    forge: &Forge,
    params: ListWorkflowRunsParams,
) -> Result<CallToolResult, McpError> {
    let mut filters: Vec<(&'static str, String)> = Vec::new();
    for (key, value) in [
        ("head_sha", &params.head_sha),
        ("ref", &params.git_ref),
        ("status", &params.status),
        ("event", &params.event),
        ("workflow_id", &params.workflow_id),
    ] {
        if let Some(value) = value {
            filters.push((key, value.clone()));
        }
    }
    let body = forge
        .list_workflow_runs(
            &params.owner,
            &params.repo,
            params.page,
            params.limit,
            &filters,
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
    paged_result(params.page, params.limit, total, &slim_runs(items))
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
    /// Workflow file name as it appears in `.forgejo/workflows/` or `.github/workflows/`,
    /// e.g. `ci.yml`. There is no list-workflows API — read the directory with
    /// `get_file_contents` if you don't know it. The workflow must declare an
    /// `on: workflow_dispatch` trigger.
    pub workflow: String,
    /// Git ref to run on — a branch or tag, e.g. `main`.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Optional `workflow_dispatch` inputs (key/value), matching the workflow's `inputs:`.
    #[serde(default)]
    pub inputs: Option<serde_json::Map<String, Value>>,
}

/// Triggers a `workflow_dispatch` run and returns the created run (`return_run_info`).
pub async fn dispatch_workflow(
    forge: &Forge,
    params: DispatchWorkflowParams,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::Map::new();
    body.insert("ref".to_owned(), Value::String(params.git_ref));
    body.insert("return_run_info".to_owned(), Value::Bool(true));
    if let Some(inputs) = params.inputs {
        body.insert("inputs".to_owned(), Value::Object(inputs));
    }
    let run = forge
        .dispatch_workflow(
            &params.owner,
            &params.repo,
            &params.workflow,
            &Value::Object(body),
        )
        .await
        .map_err(to_mcp)?;
    json_result(&run)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn slim_runs_keeps_summary_fields_and_drops_the_rest() {
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
        let slim = slim_runs(raw);
        assert_eq!(slim.len(), 1);

        let v = serde_json::to_value(&slim[0]).unwrap();
        assert_eq!(v["id"], 42);
        assert_eq!(v["status"], "success");
        assert_eq!(v["workflow_id"], "ci.yml");
        assert_eq!(v["prettyref"], "main");
        // Nested/verbose fields (and the trigger user's email) are dropped.
        assert!(v.get("repository").is_none());
        assert!(v.get("trigger_user").is_none());
        assert!(v.get("event_payload").is_none());
    }
}
