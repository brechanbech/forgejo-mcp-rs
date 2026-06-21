//! Tool implementations — thin wrappers over the `forgejo-api` client.
//!
//! Each function maps a Forgejo API call to a [`CallToolResult`]. The server's `#[tool]`
//! methods in [`crate::server`] delegate here, so that file reads as an index of the
//! surface and the real work lives here. (Promote to a `tools/` directory once it grows.)

use forgejo_api::structs::{
    IssueListIssuesQuery, IssueListIssuesQueryState, RepoListPullRequestsQuery,
    RepoListPullRequestsQueryState, RepoSearchQuery, UserCurrentListReposQuery,
};
use forgejo_api::{ApiErrorKind, Forgejo, ForgejoError};
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
fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
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
    // The list endpoints return `(pagination headers, items)`; we surface just the items.
    let mut req = forgejo.user_current_list_repos(UserCurrentListReposQuery::default());
    if let Some(page) = params.page {
        req = req.page(page);
    }
    if let Some(limit) = params.limit {
        req = req.page_size(limit);
    }
    let (_, repos) = req.await.map_err(to_mcp)?;
    json_result(&repos)
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
    let (_, issues) = req.await.map_err(to_mcp)?;
    json_result(&issues)
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
    let (_, prs) = req.await.map_err(to_mcp)?;
    json_result(&prs)
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
    let results = req.await.map_err(to_mcp)?;
    json_result(&results)
}
