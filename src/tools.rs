//! Tool implementations — thin wrappers over the in-house [`Forge`] REST client.
//!
//! Each function maps a Forgejo API call to a [`CallToolResult`]. The server's `#[tool]`
//! methods in [`crate::server`] delegate here, so that file reads as an index of the
//! surface and the real work lives here. (Promote to a `tools/` directory once it grows.)
//!
//! The client returns raw API JSON ([`Value`]); full-resource endpoints pass it straight
//! through, while list endpoints that we slim (notifications, comments) deserialize into
//! local shapes first.

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::forge::{Forge, ForgeError};

/// Maps a [`ForgeError`] to an MCP error. Caller-side rejections (bad token, missing repo,
/// validation — any 4xx) are `invalid_params`; transport, 5xx, and decode failures are ours
/// (`internal_error`).
// By value so it reads as a point-free `.map_err(to_mcp)`; the body only needs a borrow.
#[allow(clippy::needless_pass_by_value)]
fn to_mcp(err: ForgeError) -> McpError {
    if err.is_caller_error() {
        McpError::invalid_params(err.to_string(), None)
    } else {
        McpError::internal_error(err.to_string(), None)
    }
}

/// Deserializes a raw API [`Value`] into a local shape, mapping a mismatch to an internal
/// error (an unexpected response shape is our problem, not the caller's).
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, McpError> {
    serde_json::from_value(value)
        .map_err(|e| McpError::internal_error(format!("unexpected response shape: {e}"), None))
}

/// Unwraps a JSON array into its elements (anything else becomes empty).
fn into_items(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// Serializes a value as pretty JSON in a successful tool result.
pub(crate) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Wraps a page of list results with pagination metadata, so the caller can tell where it
/// is and whether more remain. `total` is the full count when the endpoint reports it.
fn paged_result<T: Serialize>(
    page: Option<u32>,
    limit: Option<u32>,
    total: Option<usize>,
    items: &[T],
) -> Result<CallToolResult, McpError> {
    json_result(&serde_json::json!({
        "page": page.unwrap_or(1),
        "limit": limit,
        "returned": items.len(),
        "total": total,
        "items": items,
    }))
}

/// Returns the authenticated user — proof the token works.
pub async fn whoami(forge: &Forge) -> Result<CallToolResult, McpError> {
    let user = forge.user_get_current().await.map_err(to_mcp)?;
    json_result(&user)
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

/// Lists the authenticated user's repositories.
pub async fn list_my_repos(forge: &Forge, params: PageParams) -> Result<CallToolResult, McpError> {
    // The list endpoints carry the full count in the `X-Total-Count` header.
    let (repos, total) = forge
        .list_my_repos(params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    paged_result(params.page, params.limit, total, &into_items(repos))
}

/// Lists issues in `owner/repo` (open issues by default).
pub async fn list_issues(
    forge: &Forge,
    params: ListItemsParams,
) -> Result<CallToolResult, McpError> {
    let state = params.state.as_deref().map(parse_state).transpose()?;
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

// --- write tools (require write mode; see crate::server) ---

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
