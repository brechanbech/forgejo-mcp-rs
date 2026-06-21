//! Tool implementations — thin wrappers over the `forgejo-api` client.
//!
//! Each function maps a Forgejo API call to a [`CallToolResult`]. The server's `#[tool]`
//! methods in [`crate::server`] delegate here, so that file reads as an index of the
//! surface and the real work lives here. (Promote to a `tools/` directory once it grows.)

use forgejo_api::structs::{
    Comment, CreateIssueCommentOption, CreateIssueOption, CreateRepoOption, IssueGetCommentsQuery,
    IssueListIssuesQuery, IssueListIssuesQueryState, NotifyGetListQuery, RepoListPullRequestsQuery,
    RepoListPullRequestsQueryState, RepoSearchQuery, UserCurrentListReposQuery,
};
use forgejo_api::{ApiErrorKind, CountHeader, Forgejo, ForgejoError};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maps a `forgejo-api` error to an MCP error. Forge-level rejections (bad token, missing
/// repo, validation, …) and any other client-side 4xx are the caller's problem
/// (`invalid_params`); transport errors and server-side 5xx are `internal_error`.
// By value so it reads as a point-free `.map_err(to_mcp)`; the body only needs a borrow.
#[allow(clippy::needless_pass_by_value)]
fn to_mcp(err: ForgejoError) -> McpError {
    let caller_error = match &err {
        // Structured API errors are almost all 4xx; only a 5xx hiding in `Other` is internal.
        ForgejoError::ApiError(api) => {
            !matches!(&api.kind, ApiErrorKind::Other(code) if code.is_server_error())
        }
        ForgejoError::UnexpectedStatusCode(code) => code.is_client_error(),
        _ => false,
    };
    if caller_error {
        McpError::invalid_params(err.to_string(), None)
    } else {
        McpError::internal_error(err.to_string(), None)
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
pub async fn whoami(forgejo: &Forgejo) -> Result<CallToolResult, McpError> {
    let user = forgejo.user_get_current().await.map_err(to_mcp)?;
    json_result(&user)
}

/// An item within a repository addressed by number (an issue or pull-request index).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepoItemRef {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Issue or pull-request number.
    pub index: i64,
}

/// Parameters for listing issues or pull requests in a repository.
#[derive(Debug, Deserialize, JsonSchema)]
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
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PageParams {
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for the `search_repos` tool.
#[derive(Debug, Deserialize, JsonSchema)]
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

/// Parses a state filter (`open`/`closed`/`all`) for issues, or a clear `invalid_params`.
fn issue_state(state: &str) -> Result<IssueListIssuesQueryState, McpError> {
    match state.to_ascii_lowercase().as_str() {
        "open" => Ok(IssueListIssuesQueryState::Open),
        "closed" => Ok(IssueListIssuesQueryState::Closed),
        "all" => Ok(IssueListIssuesQueryState::All),
        other => Err(McpError::invalid_params(
            format!("state must be open, closed, or all (got '{other}')"),
            None,
        )),
    }
}

/// Parses a state filter (`open`/`closed`/`all`) for pull requests.
fn pr_state(state: &str) -> Result<RepoListPullRequestsQueryState, McpError> {
    match state.to_ascii_lowercase().as_str() {
        "open" => Ok(RepoListPullRequestsQueryState::Open),
        "closed" => Ok(RepoListPullRequestsQueryState::Closed),
        "all" => Ok(RepoListPullRequestsQueryState::All),
        other => Err(McpError::invalid_params(
            format!("state must be open, closed, or all (got '{other}')"),
            None,
        )),
    }
}

/// Lists the authenticated user's repositories.
pub async fn list_my_repos(
    forgejo: &Forgejo,
    params: PageParams,
) -> Result<CallToolResult, McpError> {
    // The list endpoints return `(pagination headers, items)`; the headers carry the total.
    let mut req = forgejo.user_current_list_repos(UserCurrentListReposQuery::default());
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    let (headers, repos) = req.await.map_err(to_mcp)?;
    paged_result(params.page, params.limit, headers.count(), &repos)
}

/// Lists issues in `owner/repo` (open issues by default).
pub async fn list_issues(
    forgejo: &Forgejo,
    params: ListItemsParams,
) -> Result<CallToolResult, McpError> {
    let query = IssueListIssuesQuery {
        state: params.state.as_deref().map(issue_state).transpose()?,
        ..IssueListIssuesQuery::default()
    };
    let mut req = forgejo.issue_list_issues(&params.owner, &params.repo, query);
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    let (headers, issues) = req.await.map_err(to_mcp)?;
    paged_result(params.page, params.limit, headers.count(), &issues)
}

/// Gets one issue by index.
pub async fn get_issue(forgejo: &Forgejo, params: RepoItemRef) -> Result<CallToolResult, McpError> {
    let issue = forgejo
        .issue_get_issue(&params.owner, &params.repo, params.index)
        .await
        .map_err(to_mcp)?;
    json_result(&issue)
}

/// Lists pull requests in `owner/repo` (open by default).
pub async fn list_pull_requests(
    forgejo: &Forgejo,
    params: ListItemsParams,
) -> Result<CallToolResult, McpError> {
    let query = RepoListPullRequestsQuery {
        state: params.state.as_deref().map(pr_state).transpose()?,
        ..RepoListPullRequestsQuery::default()
    };
    let mut req = forgejo.repo_list_pull_requests(&params.owner, &params.repo, query);
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    let (headers, prs) = req.await.map_err(to_mcp)?;
    paged_result(params.page, params.limit, headers.count(), &prs)
}

/// Gets one pull request by index.
pub async fn get_pull_request(
    forgejo: &Forgejo,
    params: RepoItemRef,
) -> Result<CallToolResult, McpError> {
    let pr = forgejo
        .repo_get_pull_request(&params.owner, &params.repo, params.index)
        .await
        .map_err(to_mcp)?;
    json_result(&pr)
}

/// Searches repositories by keyword.
pub async fn search_repos(
    forgejo: &Forgejo,
    params: SearchReposParams,
) -> Result<CallToolResult, McpError> {
    let query = RepoSearchQuery {
        q: Some(params.query),
        ..RepoSearchQuery::default()
    };
    let mut req = forgejo.repo_search(query);
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    // `repo_search` returns `SearchResults { data, ok }` (no count header), so `total` is
    // unknown here — surface the items in the same envelope for consistency.
    let results = req.await.map_err(to_mcp)?;
    let items = results.data.unwrap_or_default();
    paged_result(params.page, params.limit, None, &items)
}

/// Lists the organizations the authenticated user belongs to.
pub async fn list_orgs(forgejo: &Forgejo, params: PageParams) -> Result<CallToolResult, McpError> {
    let mut req = forgejo.org_list_current_user_orgs();
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    // Returns `Vec<Organization>` (no count header), so `total` is unknown here.
    let orgs = req.await.map_err(to_mcp)?;
    paged_result(params.page, params.limit, None, &orgs)
}

/// Parameters for the `list_notifications` tool.
#[derive(Debug, Deserialize, JsonSchema)]
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

// A loose notification shape we deserialize into directly. `forgejo-api`'s strict
// `NotificationThread` fails the whole page if any item has a value its enums don't model
// (notably `StateType` has no `merged`, so a merged-PR notification breaks the list). We
// capture only the fields we surface, with the volatile ones as plain strings, and ignore
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
// `forgejo_api::impl_from_response!` would do this, but it references the `soft_assert` crate
// unqualified (a macro-hygiene gap), so it can't expand outside `forgejo-api`. Hand-roll it.
impl forgejo_api::FromResponse for LooseNotification {
    fn from_response(
        response: forgejo_api::ApiResponse,
        has_body: bool,
    ) -> Result<Self, forgejo_api::StructureError> {
        if !has_body {
            return Err(forgejo_api::StructureError::EmptyResponse);
        }
        serde_json::from_slice(response.body()).map_err(|e| forgejo_api::StructureError::Serde {
            e,
            contents: response.body().clone(),
        })
    }
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
    forgejo: &Forgejo,
    params: ListNotificationsParams,
) -> Result<CallToolResult, McpError> {
    let query = NotifyGetListQuery {
        all: params.all,
        ..NotifyGetListQuery::default()
    };
    // `response_type` swaps the strict `(headers, Vec<NotificationThread>)` for our loose
    // `Vec<LooseNotification>` (so `total` is unavailable — hence `None` below).
    let mut req = forgejo
        .notify_get_list(query)
        .response_type::<Vec<LooseNotification>>();
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    let threads = req.await.map_err(to_mcp)?;
    let items: Vec<NotificationSummary> = threads.into_iter().map(summarize_notification).collect();
    paged_result(params.page, params.limit, None, &items)
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

fn summarize_comment(c: Comment) -> CommentSummary {
    CommentSummary {
        id: c.id,
        user: c.user.and_then(|u| u.login),
        body: c.body,
        created_at: c.created_at.map(|d| d.to_string()),
        url: c.html_url.map(|u| u.to_string()),
    }
}

/// Parameters for the `list_issue_comments` tool.
#[derive(Debug, Deserialize, JsonSchema)]
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
    forgejo: &Forgejo,
    params: ListCommentsParams,
) -> Result<CallToolResult, McpError> {
    let mut req = forgejo.issue_get_comments(
        &params.owner,
        &params.repo,
        params.index,
        IssueGetCommentsQuery::default(),
    );
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    let (headers, comments) = req.await.map_err(to_mcp)?;
    let items: Vec<CommentSummary> = comments.into_iter().map(summarize_comment).collect();
    paged_result(params.page, params.limit, headers.count(), &items)
}

// --- write tools (require write mode; see crate::server) ---

/// Parameters for the `enable_write_mode` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnableWriteParams {
    /// How long write mode stays active, in minutes (default 10, hard-capped at 60). It
    /// also slides forward this far on each successful write, then auto-reverts.
    #[serde(default)]
    pub minutes: Option<u32>,
}

/// Parameters for the `create_repo` tool.
#[derive(Debug, Deserialize, JsonSchema)]
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
#[derive(Debug, Deserialize, JsonSchema)]
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
    forgejo: &Forgejo,
    params: CreateRepoParams,
) -> Result<CallToolResult, McpError> {
    // CreateRepoOption has no Default, so every field is set explicitly.
    let option = CreateRepoOption {
        name: params.name,
        private: params.private.or(Some(true)),
        description: params.description,
        auto_init: None,
        default_branch: None,
        gitignores: None,
        issue_labels: None,
        license: None,
        object_format_name: None,
        readme: None,
        template: None,
        trust_model: None,
    };
    let repo = forgejo
        .create_current_user_repo(option)
        .await
        .map_err(to_mcp)?;
    json_result(&repo)
}

/// Deletes a repository — guarded by an exact `owner/repo` confirmation.
pub async fn delete_repo(
    forgejo: &Forgejo,
    params: DeleteRepoParams,
) -> Result<CallToolResult, McpError> {
    let expected = format!("{}/{}", params.owner, params.repo);
    if params.confirm != expected {
        return Err(McpError::invalid_params(
            format!("delete refused: `confirm` must be exactly \"{expected}\""),
            None,
        ));
    }
    forgejo
        .repo_delete(&params.owner, &params.repo)
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({ "deleted": expected }))
}

/// Parameters for the `create_issue` tool.
#[derive(Debug, Deserialize, JsonSchema)]
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
    forgejo: &Forgejo,
    params: CreateIssueParams,
) -> Result<CallToolResult, McpError> {
    // CreateIssueOption has no Default, so every field is set explicitly.
    let option = CreateIssueOption {
        title: params.title,
        body: params.body,
        assignee: None,
        assignees: None,
        closed: None,
        due_date: None,
        labels: None,
        milestone: None,
        r#ref: None,
    };
    let issue = forgejo
        .issue_create_issue(&params.owner, &params.repo, option)
        .await
        .map_err(to_mcp)?;
    json_result(&issue)
}

/// Parameters for the `comment_on_issue` tool.
#[derive(Debug, Deserialize, JsonSchema)]
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
    forgejo: &Forgejo,
    params: CommentOnIssueParams,
) -> Result<CallToolResult, McpError> {
    let option = CreateIssueCommentOption {
        body: params.body,
        updated_at: None,
    };
    let comment = forgejo
        .issue_create_comment(&params.owner, &params.repo, params.index, option)
        .await
        .map_err(to_mcp)?;
    json_result(&summarize_comment(comment))
}
