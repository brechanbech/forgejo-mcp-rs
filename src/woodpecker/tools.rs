//! Tool implementations — thin wrappers over the in-house [`Woodpecker`] REST client.
//!
//! Each function maps a Woodpecker API call to a [`CallToolResult`]. The server's `#[tool]`
//! methods in [`crate::woodpecker::server`] delegate here, so that file reads as an index of the surface and
//! the real work lives here. Full-resource endpoints pass the raw API JSON straight through; list
//! endpoints auto-paginate to completion by default (see [`crate::mcp_core::gather_all`]).

use crate::mcp_core::{gather_all, gathered_result, into_items, json_result, paged_result, to_mcp};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde_json::Value;

use super::client::Woodpecker;

/// Page-size used per request while auto-paginating list endpoints — matches Woodpecker's default
/// `perPage`, and [`crate::mcp_core::gather_all`] verifies the effective size from the first page anyway.
const AUTO_PER_PAGE: u32 = 50;

/// Pagination parameters shared by the list tools. Omit both to auto-paginate the whole list;
/// pass either to fetch a single page.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct PageParams {
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page (Woodpecker's `perPage`; server default 50).
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// A repository addressed by its numeric Woodpecker id (from `list_repos` or `lookup_repo`).
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct RepoRef {
    /// Numeric Woodpecker repository id.
    pub repo_id: i64,
}

/// A repository addressed by its full name, to resolve to a numeric id.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct LookupRepoParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub name: String,
}

/// Parameters for listing a repository's pipelines.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListPipelinesParams {
    /// Numeric Woodpecker repository id.
    pub repo_id: i64,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page (Woodpecker's `perPage`; server default 50).
    #[serde(default)]
    pub per_page: Option<u32>,
}

/// A pipeline addressed by its per-repo number within a repository.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct PipelineRef {
    /// Numeric Woodpecker repository id.
    pub repo_id: i64,
    /// Pipeline number within the repository (the `number` field from `list_pipelines`).
    pub number: i64,
}

/// Parameters for triggering a new pipeline.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct TriggerPipelineParams {
    /// Numeric Woodpecker repository id.
    pub repo_id: i64,
    /// Branch to run on. Defaults to the repository's default branch when omitted.
    #[serde(default)]
    pub branch: Option<String>,
    /// Optional pipeline variables (string values), passed through to the run.
    #[serde(default)]
    pub variables: Option<serde_json::Map<String, Value>>,
}

/// Returns the authenticated user — proof the token works.
pub async fn whoami(wp: &Woodpecker) -> Result<CallToolResult, McpError> {
    let user = wp.self_user().await.map_err(to_mcp)?;
    json_result(&user)
}

/// Lists repositories the authenticated user has access to. Auto-paginates unless a `page` or
/// `per_page` is given.
pub async fn list_repos(wp: &Woodpecker, params: PageParams) -> Result<CallToolResult, McpError> {
    if params.page.is_none() && params.per_page.is_none() {
        let all = gather_all(|page, per_page| Box::pin(wp.list_repos(Some(page), Some(per_page))))
            .await
            .map_err(to_mcp)?;
        return gathered_result(&all.items, all.total, all.truncated);
    }
    let (repos, total) = wp
        .list_repos(params.page, params.per_page.or(Some(AUTO_PER_PAGE)))
        .await
        .map_err(to_mcp)?;
    paged_result(params.page, params.per_page, total, &into_items(repos))
}

/// Resolves a repository's `owner/name` to its record (which carries the numeric `id`).
pub async fn lookup_repo(
    wp: &Woodpecker,
    params: LookupRepoParams,
) -> Result<CallToolResult, McpError> {
    let repo = wp
        .lookup_repo(&params.owner, &params.name)
        .await
        .map_err(to_mcp)?;
    json_result(&repo)
}

/// Gets one repository's details by numeric id.
pub async fn get_repo(wp: &Woodpecker, params: RepoRef) -> Result<CallToolResult, McpError> {
    let repo = wp.get_repo(params.repo_id).await.map_err(to_mcp)?;
    json_result(&repo)
}

/// Lists a repository's pipeline runs. Auto-paginates unless a `page` or `per_page` is given.
pub async fn list_pipelines(
    wp: &Woodpecker,
    params: ListPipelinesParams,
) -> Result<CallToolResult, McpError> {
    let repo_id = params.repo_id;
    if params.page.is_none() && params.per_page.is_none() {
        let all = gather_all(|page, per_page| {
            Box::pin(wp.list_pipelines(repo_id, Some(page), Some(per_page)))
        })
        .await
        .map_err(to_mcp)?;
        return gathered_result(&all.items, all.total, all.truncated);
    }
    let (pipelines, total) = wp
        .list_pipelines(
            repo_id,
            params.page,
            params.per_page.or(Some(AUTO_PER_PAGE)),
        )
        .await
        .map_err(to_mcp)?;
    paged_result(params.page, params.per_page, total, &into_items(pipelines))
}

/// Gets one pipeline by its per-repo number.
pub async fn get_pipeline(
    wp: &Woodpecker,
    params: PipelineRef,
) -> Result<CallToolResult, McpError> {
    let pipeline = wp
        .get_pipeline(params.repo_id, params.number)
        .await
        .map_err(to_mcp)?;
    json_result(&pipeline)
}

/// Triggers a new pipeline and returns the created run.
pub async fn trigger_pipeline(
    wp: &Woodpecker,
    params: TriggerPipelineParams,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::Map::new();
    if let Some(branch) = params.branch {
        body.insert("branch".to_owned(), Value::String(branch));
    }
    if let Some(variables) = params.variables {
        body.insert("variables".to_owned(), Value::Object(variables));
    }
    let run = wp
        .create_pipeline(params.repo_id, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    json_result(&run)
}

/// Cancels a running pipeline.
pub async fn cancel_pipeline(
    wp: &Woodpecker,
    params: PipelineRef,
) -> Result<CallToolResult, McpError> {
    wp.cancel_pipeline(params.repo_id, params.number)
        .await
        .map_err(to_mcp)?;
    json_result(&serde_json::json!({
        "cancelled": true,
        "repo_id": params.repo_id,
        "number": params.number,
    }))
}

/// Restarts (re-runs) a pipeline and returns the new run.
pub async fn restart_pipeline(
    wp: &Woodpecker,
    params: PipelineRef,
) -> Result<CallToolResult, McpError> {
    let run = wp
        .restart_pipeline(params.repo_id, params.number)
        .await
        .map_err(to_mcp)?;
    json_result(&run)
}
