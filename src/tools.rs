//! Tool implementations — thin wrappers over the `forgejo-api` client.
//!
//! Each function maps a Forgejo API call to a [`CallToolResult`]. The server's `#[tool]`
//! methods in [`crate::server`] delegate here, so that file reads as an index of the
//! surface and the real work lives here. (Promote to a `tools/` directory once it grows.)

use forgejo_api::structs::{
    IssueListIssuesQuery, RepoListPullRequestsQuery, RepoSearchQuery, UserCurrentListReposQuery,
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

/// A repository, by owner and name.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepoRef {
    /// Repository owner — user or organization, e.g. `brechanbech`.
    pub owner: String,
    /// Repository name, e.g. `forgejo-mcp-rs`.
    pub repo: String,
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

/// Parameters for the `search_repos` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchReposParams {
    /// Search query (keywords; matches repository names and, by default, descriptions).
    pub query: String,
}

/// Lists the authenticated user's repositories (first page).
pub async fn list_my_repos(forgejo: &Forgejo) -> Result<CallToolResult, McpError> {
    // The list endpoints return `(pagination headers, items)`; we surface just the items.
    let (_, repos) = forgejo
        .user_current_list_repos(UserCurrentListReposQuery::default())
        .await
        .map_err(to_mcp)?;
    json_result(&repos)
}

/// Lists issues in `owner/repo` (open issues by default).
pub async fn list_issues(forgejo: &Forgejo, params: RepoRef) -> Result<CallToolResult, McpError> {
    let (_, issues) = forgejo
        .issue_list_issues(&params.owner, &params.repo, IssueListIssuesQuery::default())
        .await
        .map_err(to_mcp)?;
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
    params: RepoRef,
) -> Result<CallToolResult, McpError> {
    let (_, prs) = forgejo
        .repo_list_pull_requests(
            &params.owner,
            &params.repo,
            RepoListPullRequestsQuery::default(),
        )
        .await
        .map_err(to_mcp)?;
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
    let results = forgejo.repo_search(query).await.map_err(to_mcp)?;
    json_result(&results)
}
