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
    ByteRange, decode, gather_all, gathered_result, into_items, json_result, paged_result, to_mcp,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::StatusCode;
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::client::{
    Flavor, Forge, ForgeError, IssueFilters, PullFilters, PullMerge, RunFilters, encode_repo_path,
};

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

/// Parameters for the `list_issues` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListIssuesParams {
    /// Repository owner — user or organization, e.g. `brechanbech`.
    pub owner: String,
    /// Repository name, e.g. `forgejo-mcp-rs`.
    pub repo: String,
    /// Filter by state: `open` (default), `closed`, or `all`.
    #[serde(default)]
    pub state: Option<String>,
    /// Order: `latest` (the default, newest first), `oldest`, `recentupdate`, `leastupdate`,
    /// `mostcomment`, `leastcomment`, `nearduedate`, `farduedate`, or `relevance` (with
    /// `search`). Forgejo only: Gitea's issue listing has no sort, so anything but `latest` is
    /// refused there.
    #[serde(default)]
    pub sort: Option<String>,
    /// Only issues carrying any of these label names.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Only issues in any of these milestones, by name (or id).
    #[serde(default)]
    pub milestones: Option<Vec<String>>,
    /// Only issues opened by this login.
    #[serde(default)]
    pub author: Option<String>,
    /// Only issues assigned to this login.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Only issues mentioning this login.
    #[serde(default)]
    pub mentioned: Option<String>,
    /// Only issues matching this text.
    #[serde(default)]
    pub search: Option<String>,
    /// Only issues updated at or after this time (RFC 3339, e.g. `2026-10-01T00:00:00Z`).
    #[serde(default)]
    pub since: Option<String>,
    /// Only issues updated before this time (RFC 3339).
    #[serde(default)]
    pub before: Option<String>,
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

/// The state to send for a listing: the caller's, or `open`. Always sent, never left to the
/// forge: both default `…/pulls` to every state, though `…/issues` to open. Probed on
/// 24 October 2026, an unqualified listing returned all 10,350 of `forgejo/forgejo`'s pull
/// requests on Codeberg rather than its 154 open ones, and all 748 of `gitea/tea`'s on gitea.com
/// rather than 14.
fn state_or_open(state: Option<&str>) -> Result<&'static str, McpError> {
    state.map_or(Ok("open"), parse_state)
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
    repo_detail_result(repo)
}

/// Answers with a raw repository object in [`RepoDetail`]'s shape. `create_repo`, `edit_repo`
/// and `migrate_repo` get the same object back as `get_repo` does, owner profile (email
/// included), merge settings and wiki URLs and all, and answer with what `get_repo` would.
fn repo_detail_result(raw: Value) -> Result<CallToolResult, McpError> {
    json_result(&repo_detail(raw)?)
}

fn repo_detail(raw: Value) -> Result<RepoDetail, McpError> {
    let mut detail: RepoDetail = decode(raw)?;
    // Every repository carries `original_url`; it is empty unless the repo was migrated.
    detail.original_url = detail.original_url.filter(|u| !u.is_empty());
    Ok(detail)
}

/// One repository in detail: the list summary plus the unit toggles.
///
/// The toggles are the read side of `edit_repo`'s unit flags, and the answer that
/// [`release_404_hint`] sends a caller looking for. A unit that is off 404s its whole endpoint
/// family rather than returning empty results, so "is it switched off?" has to be a question a
/// caller can actually ask — a hint naming a field no tool returns is a dead end.
///
/// They stay out of [`RepoSummary`] on purpose: a repo *listing* does not need seven booleans
/// per row, and the point of the summary is that the whole set stays compact.
#[derive(Debug, serde::Deserialize, Serialize)]
struct RepoDetail {
    #[serde(flatten)]
    summary: RepoSummary,
    /// True until the first commit: how a fresh `create_repo` differs from a migrated or
    /// pushed-to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mirror: Option<bool>,
    /// Where a migrated repository was copied from.
    #[serde(skip_serializing_if = "Option::is_none")]
    original_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_issues: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_pull_requests: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_wiki: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_releases: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_actions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_packages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_projects: Option<bool>,
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

/// The rolled-up verdict for a ref: one state, plus the checks behind it.
#[derive(Debug, Serialize)]
struct CombinedStatusSummary {
    /// `success`, `pending`, `failure`, `error` or `warning`. Empty on an
    /// instance that reports nothing for the ref, in which case it is dropped
    /// rather than returned blank.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
    total_count: i64,
    /// The latest state per check, not the full transition history that
    /// `get_commit_statuses` returns.
    statuses: Vec<CommitStatusSummary>,
}

fn slim_combined_status(raw: &Value) -> CombinedStatusSummary {
    CombinedStatusSummary {
        state: raw
            .get("state")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned),
        sha: raw
            .get("sha")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        total_count: raw.get("total_count").and_then(Value::as_i64).unwrap_or(0),
        statuses: raw
            .get("statuses")
            .and_then(Value::as_array)
            .map(|list| slim_commit_statuses(list))
            .unwrap_or_default(),
    }
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CombinedStatusParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// The commit, branch or tag to judge.
    #[serde(rename = "ref")]
    pub git_ref: String,
}

/// The one-line answer to "did CI pass for this ref".
pub async fn get_combined_status(
    forge: &Forge,
    params: CombinedStatusParams,
) -> Result<CallToolResult, McpError> {
    let raw = forge
        .get_combined_status(&params.owner, &params.repo, &params.git_ref)
        .await
        .map_err(to_mcp)?;
    json_result(&slim_combined_status(&raw))
}

/// One status reported against a commit: a CI run, a linter, whatever posted
/// it. `creator` and the timestamps are dropped — what matters is which check
/// this was and how it went.
///
/// Note that a ref accumulates one of these per *transition*, not one per
/// check: a single job appears as queued, then running, then finished. The
/// newest entry for a given `context` is its current state.
#[derive(Debug, Serialize)]
struct CommitStatusSummary {
    /// The check's name, e.g. a workflow job.
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
    /// `success`, `pending`, `failure`, `error` or `warning`.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Where the run can be read, when the reporter supplied one. Neither
    /// forge exposes a logs API, so this link is often the only way to the
    /// detail — and both return it **relative** to the instance origin
    /// (`/owner/repo/actions/runs/…`), so it needs the host prefixed before
    /// it will resolve. Probed on codeberg.org and gitea.com alike.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

fn slim_commit_statuses(items: &[Value]) -> Vec<CommitStatusSummary> {
    items
        .iter()
        .map(|s| CommitStatusSummary {
            context: s
                .get("context")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(ToOwned::to_owned),
            status: s
                .get("status")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            description: s
                .get("description")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(ToOwned::to_owned),
            target_url: s
                .get("target_url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
                .map(ToOwned::to_owned),
            updated_at: s
                .get("updated_at")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        })
        .collect()
}

/// One entry in a git tree. `url` is dropped — it is derivable and repeats the
/// sha on every line of what may be thousands.
#[derive(Debug, Serialize)]
struct TreeEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// `blob` for a file, `tree` for a directory, `commit` for a submodule.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

#[derive(Debug, Serialize)]
struct TreeListing {
    entries: Vec<TreeEntry>,
    total_count: Option<i64>,
    /// The forge stopped early. Anything concluded from a truncated tree —
    /// "there is no such file" above all — is unsound.
    truncated: bool,
}

fn slim_tree(raw: &Value) -> TreeListing {
    let entries = raw
        .get("tree")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|e| TreeEntry {
                    path: e.get("path").and_then(Value::as_str).map(ToOwned::to_owned),
                    kind: e.get("type").and_then(Value::as_str).map(ToOwned::to_owned),
                    size: e.get("size").and_then(Value::as_i64),
                    sha: e.get("sha").and_then(Value::as_str).map(ToOwned::to_owned),
                })
                .collect()
        })
        .unwrap_or_default();
    TreeListing {
        entries,
        total_count: raw.get("total_count").and_then(Value::as_i64),
        truncated: raw
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CommitStatusesParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// The commit, branch or tag whose statuses to read.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct RepoTreeParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Tree to read: a branch, tag or commit. Defaults to the repository's
    /// default branch.
    #[serde(default)]
    pub sha: Option<String>,
    /// Walk the whole tree rather than one level. Off by default, since a
    /// large repository returns thousands of entries.
    #[serde(default)]
    pub recursive: Option<bool>,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Lists every status reported against a commit.
pub async fn get_commit_statuses(
    forge: &Forge,
    params: CommitStatusesParams,
) -> Result<CallToolResult, McpError> {
    let (statuses, total) = forge
        .list_commit_statuses(
            &params.owner,
            &params.repo,
            &params.git_ref,
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    paged_result(
        params.page,
        params.limit,
        total,
        &slim_commit_statuses(&into_items(statuses)),
    )
}

/// Reads a repository's git tree.
pub async fn get_repo_tree(
    forge: &Forge,
    params: RepoTreeParams,
) -> Result<CallToolResult, McpError> {
    let sha = params.sha.as_deref().unwrap_or("HEAD");
    let raw = forge
        .get_repo_tree(
            &params.owner,
            &params.repo,
            sha,
            params.recursive.unwrap_or(false),
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    json_result(&slim_tree(&raw))
}

/// One branch protection rule, trimmed to the gates rather than the
/// allowlists.
///
/// The wire object carries a dozen arrays of usernames, teams and deploy keys
/// naming who may bypass each gate. Those answer "who", and the question worth
/// a tool is "what is enforced" — so the `enable_*` flags survive and the
/// membership lists do not.
#[derive(Debug, Serialize)]
struct BranchProtectionSummary {
    /// The branch name or glob the rule matches. A branch matching no rule is
    /// unprotected.
    #[serde(skip_serializing_if = "Option::is_none")]
    rule_name: Option<String>,
    /// Rejects pushes that are unsigned **or unverifiable**. Checked against
    /// the commits a push introduces, not the branch's history, so turning it
    /// on does not invalidate what is already there.
    #[serde(skip_serializing_if = "Option::is_none")]
    require_signed_commits: Option<bool>,
    /// Whether direct pushes are allowed at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_push: Option<bool>,
    /// True when only the allowlisted may push — the names are omitted here.
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_push_whitelist: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_approvals: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_status_check: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_on_outdated_branch: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    block_on_rejected_reviews: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protected_file_patterns: Option<String>,
    /// Gitea only — Forgejo does not report it. Absent means unreported, not
    /// false. Relevant to signing: the signed-commit check leans on force
    /// push being refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_force_push: Option<bool>,
    /// Forgejo only — Gitea does not report it. Absent means unreported.
    #[serde(skip_serializing_if = "Option::is_none")]
    apply_to_admins: Option<bool>,
}

fn slim_branch_protections(items: &[Value]) -> Vec<BranchProtectionSummary> {
    items
        .iter()
        .map(|r| BranchProtectionSummary {
            rule_name: r
                .get("rule_name")
                .or_else(|| r.get("branch_name"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            require_signed_commits: r.get("require_signed_commits").and_then(Value::as_bool),
            enable_push: r.get("enable_push").and_then(Value::as_bool),
            enable_push_whitelist: r.get("enable_push_whitelist").and_then(Value::as_bool),
            required_approvals: r.get("required_approvals").and_then(Value::as_i64),
            enable_status_check: r.get("enable_status_check").and_then(Value::as_bool),
            block_on_outdated_branch: r.get("block_on_outdated_branch").and_then(Value::as_bool),
            block_on_rejected_reviews: r.get("block_on_rejected_reviews").and_then(Value::as_bool),
            protected_file_patterns: r
                .get("protected_file_patterns")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .map(ToOwned::to_owned),
            enable_force_push: r.get("enable_force_push").and_then(Value::as_bool),
            apply_to_admins: r.get("apply_to_admins").and_then(Value::as_bool),
        })
        .collect()
}

/// Lists a repository's branch protection rules.
pub async fn list_branch_protections(
    forge: &Forge,
    params: RepoRef,
) -> Result<CallToolResult, McpError> {
    let (rules, total) = forge
        .list_branch_protections(&params.owner, &params.repo)
        .await
        .map_err(to_mcp)?;
    paged_result(
        None,
        None,
        total,
        &slim_branch_protections(&into_items(rules)),
    )
}

/// One SSH key, trimmed. The key material itself is dropped — the fingerprint
/// identifies it and the blob is a hundred-odd characters nobody compares by
/// eye.
#[derive(Debug, Serialize)]
struct SshKeySummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_type: Option<String>,
    /// Whether the key has been verified with a signed token. A key that is
    /// merely present does not verify commits — the forge shows "Verified
    /// Key" only once this is true, and until then commits signed with it read
    /// as unverified.
    ///
    /// Optional because **Gitea does not expose it**: its `PublicKey` carries
    /// `last_used_at` where Forgejo's carries `verified`, so the state exists
    /// in Gitea's web UI and nowhere in its API. Absent must therefore read as
    /// "not reported", never as false — defaulting it would assert a verified
    /// key was unverified on every Gitea instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    read_only: Option<bool>,
}

/// One GPG key, trimmed. `public_key` is an armored blob and `subkeys` nests
/// another copy of all of this; neither survives.
#[derive(Debug, Serialize)]
struct GpgKeySummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    can_sign: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    /// The addresses this key is good for, and whether each is activated. A
    /// signature only verifies for a commit authored under an activated
    /// address, so this is half the answer to "why is my commit unverified".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    emails: Vec<GpgEmail>,
}

#[derive(Debug, Serialize)]
struct GpgEmail {
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified: Option<bool>,
}

/// Both key lists for one account.
#[derive(Debug, Serialize)]
struct KeyListing {
    ssh: Vec<SshKeySummary>,
    gpg: Vec<GpgKeySummary>,
}

fn slim_ssh_keys(items: &[Value]) -> Vec<SshKeySummary> {
    items
        .iter()
        .map(|k| SshKeySummary {
            id: k.get("id").and_then(Value::as_i64),
            title: k
                .get("title")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            fingerprint: k
                .get("fingerprint")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            key_type: k
                .get("key_type")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            verified: k.get("verified").and_then(Value::as_bool),
            read_only: k.get("read_only").and_then(Value::as_bool),
        })
        .collect()
}

fn slim_gpg_keys(items: &[Value]) -> Vec<GpgKeySummary> {
    items
        .iter()
        .map(|k| GpgKeySummary {
            id: k.get("id").and_then(Value::as_i64),
            key_id: k
                .get("key_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            verified: k.get("verified").and_then(Value::as_bool),
            can_sign: k.get("can_sign").and_then(Value::as_bool),
            expires_at: k
                .get("expires_at")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            emails: k
                .get("emails")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|e| GpgEmail {
                            email: e
                                .get("email")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            verified: e.get("verified").and_then(Value::as_bool),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListKeysParams {
    /// Whose keys to list. Omit for the authenticated user, which is the only
    /// list that reliably carries `verified`.
    #[serde(default)]
    pub username: Option<String>,
}

/// Lists an account's SSH and GPG keys, both kinds in one answer.
pub async fn list_keys(forge: &Forge, params: ListKeysParams) -> Result<CallToolResult, McpError> {
    let username = params.username.as_deref();
    let (ssh, _) = forge
        .list_ssh_keys(username, None, Some(50))
        .await
        .map_err(to_mcp)?;
    let (gpg, _) = forge
        .list_gpg_keys(username, None, Some(50))
        .await
        .map_err(to_mcp)?;
    json_result(&KeyListing {
        ssh: slim_ssh_keys(&into_items(ssh)),
        gpg: slim_gpg_keys(&into_items(gpg)),
    })
}

/// One commit, trimmed to what a reader needs.
///
/// The wire object carries `signature` and `payload` — base64 blobs running
/// from a few hundred bytes to a couple of kilobytes *each*, per commit — plus
/// `files` and `stats` on every listing entry. A thirty-commit page is tens of
/// kilobytes of material nobody reads. What survives is the identity of the
/// commit, who wrote it, and whether the forge could vouch for its signature.
#[derive(Debug, Serialize)]
struct CommitSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
    /// First line only. A body can run to paragraphs and the subject is what
    /// identifies a commit in a list.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    /// "verified", "unverified" or "unsigned" — see [`signature_state`].
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    /// Who the forge says signed it, when it could tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,
    /// Why an existing signature did not verify: a `gpg.error.*` key on both
    /// forges. Gitea reports `no_gpg_keys_found` both for a key it does not
    /// have and for one it has but has not verified, so it does not
    /// distinguish those two.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Which of the three states a commit's signature is in.
///
/// Keyed off the presence of a signature rather than the `reason` string: an
/// unrecognised `gpg.error.*` still classifies correctly, and "no signature"
/// is a different claim from "a signature that did not verify".
fn signature_state(verification: Option<&Value>) -> Option<String> {
    let v = verification?;
    let signed = v
        .get("signature")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !signed {
        return Some("unsigned".into());
    }
    let verified = v.get("verified").and_then(Value::as_bool).unwrap_or(false);
    Some(if verified { "verified" } else { "unverified" }.into())
}

/// Trims commit objects from either endpoint — a listing entry and a single
/// commit have the same shape on both Forgejo and Gitea.
fn slim_commits(items: &[Value]) -> Vec<CommitSummary> {
    items.iter().map(slim_commit).collect()
}

fn slim_commit(c: &Value) -> CommitSummary {
    let verification = c.pointer("/commit/verification");
    let state = signature_state(verification);
    // Only name a signer when the forge vouched for one; on an unverified
    // commit the field is null and on an unsigned one it means nothing.
    let signer = match state.as_deref() {
        Some("verified") => verification
            .and_then(|v| v.pointer("/signer/name"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    };
    let reason = match state.as_deref() {
        Some("unverified") => verification
            .and_then(|v| v.get("reason"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    };
    CommitSummary {
        sha: c.get("sha").and_then(Value::as_str).map(ToOwned::to_owned),
        summary: c
            .pointer("/commit/message")
            .and_then(Value::as_str)
            .map(|m| m.lines().next().unwrap_or(m).trim().to_owned()),
        author: c
            .pointer("/commit/author/name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        email: c
            .pointer("/commit/author/email")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        date: c
            .pointer("/commit/author/date")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        signature: state,
        signer,
        reason,
    }
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListCommitsParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Branch, tag or commit to start from. Defaults to the repository's
    /// default branch. A ref the forge cannot resolve answers 404.
    #[serde(default)]
    pub sha: Option<String>,
    /// Limit to commits touching this path.
    #[serde(default)]
    pub path: Option<String>,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct GetCommitParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// The commit to fetch. A full id is safest; a branch or tag name also
    /// resolves.
    pub sha: String,
}

/// Lists commits on a ref (auto-paginated unless an explicit page/limit is given).
pub async fn list_commits(
    forge: &Forge,
    params: ListCommitsParams,
) -> Result<CallToolResult, McpError> {
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| {
            Box::pin(forge.list_commits(
                &params.owner,
                &params.repo,
                params.sha.as_deref(),
                params.path.as_deref(),
                Some(page),
                Some(limit),
            ))
        })
        .await
        .map_err(to_mcp)?;
        return gathered_result(&slim_commits(&all.items), all.total, all.truncated);
    }
    let (commits, total) = forge
        .list_commits(
            &params.owner,
            &params.repo,
            params.sha.as_deref(),
            params.path.as_deref(),
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    paged_result(
        params.page,
        params.limit,
        total,
        &slim_commits(&into_items(commits)),
    )
}

/// Gets one commit, slimmed to the same fields a listing entry carries.
pub async fn get_commit(
    forge: &Forge,
    params: GetCommitParams,
) -> Result<CallToolResult, McpError> {
    let commit = forge
        .get_commit(&params.owner, &params.repo, &params.sha)
        .await
        .map_err(to_mcp)?;
    json_result(&slim_commit(&commit))
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
    // `/` and the empty path both mean the repository root; anything else is encoded, since
    // a `#` or `?` in a file name would otherwise cut the request path short.
    let trimmed = params.path.trim_matches('/');
    let path = if trimmed.is_empty() {
        String::new()
    } else {
        repo_path_param("path", trimmed)?
    };
    let raw = forge
        .get_contents(
            &params.owner,
            &params.repo,
            &path,
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

/// An issue reduced to what a list of them is read for.
///
/// The raw object carries the author's full profile (24 fields, email included), the repository
/// it belongs to, every label's colour and description, and the whole body. Fifteen open issues
/// of `forgejo/forgejo` came to 53 KB, 22 KB of it bodies. The body is what `get_issue` is for;
/// a listing is read for which issues there are and where they stand.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct IssueSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// `open` or `closed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    /// Who wrote it on the forge it was migrated from, when it was migrated — `author` is then
    /// the account that ran the migration.
    #[serde(skip_serializing_if = "Option::is_none")]
    original_author: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    assignees: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    milestone: Option<String>,
    /// Comment count.
    #[serde(skip_serializing_if = "Option::is_none")]
    comments: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<String>,
}

/// One issue as `get_issue` and `create_issue` report it: the summary, plus what only matters
/// when looking at a single one.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct IssueDetail {
    #[serde(flatten)]
    summary: IssueSummary,
    /// The description, as Markdown. Absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    due_date: Option<String>,
    /// The branch or tag the issue is tied to, when one is set.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    git_ref: Option<String>,
    /// Present, and `true`, when the number is a pull request's: the forges number both in one
    /// sequence and answer `…/issues/{n}` for either. `get_pull_request` has its branches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pull_request: Option<bool>,
}

/// Projects one raw issue — Forgejo's or Gitea's, which agree on this shape — onto
/// [`IssueSummary`].
fn slim_issue(issue: &Value) -> IssueSummary {
    IssueSummary {
        number: issue.get("number").and_then(Value::as_i64),
        title: text_at(issue, "/title"),
        state: text_at(issue, "/state"),
        author: text_at(issue, "/user/login"),
        original_author: text_at(issue, "/original_author"),
        labels: names_at(issue, "/labels", "name"),
        assignees: names_at(issue, "/assignees", "login"),
        milestone: text_at(issue, "/milestone/title"),
        comments: issue.get("comments").and_then(Value::as_i64),
        created_at: first_time(issue, &["created_at"]),
        updated_at: first_time(issue, &["updated_at"]),
        closed_at: first_time(issue, &["closed_at"]),
        html_url: text_at(issue, "/html_url"),
    }
}

/// Projects one raw issue onto [`IssueDetail`].
fn issue_detail(issue: &Value) -> IssueDetail {
    IssueDetail {
        summary: slim_issue(issue),
        body: text_at(issue, "/body"),
        locked: issue.get("is_locked").and_then(Value::as_bool),
        due_date: first_time(issue, &["due_date"]),
        git_ref: text_at(issue, "/ref"),
        pull_request: issue
            .get("pull_request")
            .is_some_and(|p| !p.is_null())
            .then_some(true),
    }
}

/// The issue sort orders, in Forgejo's spelling. `latest` is the default and is not sent.
///
/// Both forges ignore a sort value they do not know and list newest first, so a misspelt one
/// would look like it worked. Probed on Codeberg and gitea.com, 27 October 2026.
const ISSUE_SORTS: [&str; 9] = [
    "latest",
    "oldest",
    "recentupdate",
    "leastupdate",
    "mostcomment",
    "leastcomment",
    "nearduedate",
    "farduedate",
    "relevance",
];

/// The pull-request sort orders, spelled alike on both forges. `latest` is ours, for the
/// default newest-first order, so that both listings share one word for it; it is not sent.
const PULL_SORTS: [&str; 8] = [
    "latest",
    "oldest",
    "recentupdate",
    "recentclose",
    "leastupdate",
    "mostcomment",
    "leastcomment",
    "priority",
];

/// Checks a sort order against the ones a listing takes, and drops `latest`, the default.
fn sort_param<'a>(sort: Option<&'a str>, allowed: &[&str]) -> Result<Option<&'a str>, McpError> {
    match sort {
        None | Some("latest") => Ok(None),
        Some(s) if allowed.contains(&s) => Ok(Some(s)),
        Some(s) => Err(McpError::invalid_params(
            format!(
                "unknown sort \"{s}\": expected one of {}. The forge would ignore it and list \
                 newest first",
                allowed.join(", ")
            ),
            None,
        )),
    }
}

/// Whether an issue answers the label and milestone filters asked for: any of the labels, and
/// any of the milestones, by name or id. Names compare case-insensitively.
fn issue_matches(issue: &Value, labels: &[String], milestones: &[String]) -> bool {
    let labels_ok = labels.is_empty()
        || issue
            .get("labels")
            .and_then(Value::as_array)
            .is_some_and(|have| {
                have.iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str))
                    .any(|name| labels.iter().any(|want| want.eq_ignore_ascii_case(name)))
            });
    let milestones_ok = milestones.is_empty()
        || issue.get("milestone").is_some_and(|m| {
            let title = m.get("title").and_then(Value::as_str);
            let id = m.get("id").and_then(Value::as_i64).map(|id| id.to_string());
            milestones.iter().any(|want| {
                title.is_some_and(|t| want.eq_ignore_ascii_case(t)) || id.as_deref() == Some(want)
            })
        });
    labels_ok && milestones_ok
}

/// Refuses a listing in which the forge dropped a label or milestone filter.
///
/// Both forges discard a label or milestone name they do not know, and with none of them known
/// drop the filter altogether and list everything, so a typo would come back as every issue in
/// the repository. Probed on Codeberg and gitea.com, 27 October 2026. An issue that answers
/// neither filter shows the forge did that.
fn check_issue_filters(
    items: &[Value],
    labels: &[String],
    milestones: &[String],
) -> Result<(), McpError> {
    let Some(stray) = items
        .iter()
        .find(|issue| !issue_matches(issue, labels, milestones))
    else {
        return Ok(());
    };
    let (what, names) = if issue_matches(stray, labels, &[]) {
        ("milestones", milestones)
    } else {
        ("labels", labels)
    };
    Err(McpError::invalid_params(
        format!(
            "no {what} named {} in this repository: the forge drops names it does not know, \
             and with none known it would have listed every issue. Check the spelling against \
             get_issue's {what}",
            names.join(", ")
        ),
        None,
    ))
}

/// Lists issues in `owner/repo` (open issues by default).
pub async fn list_issues(
    forge: &Forge,
    params: ListIssuesParams,
) -> Result<CallToolResult, McpError> {
    let state = Some(state_or_open(params.state.as_deref())?);
    let sort = sort_param(params.sort.as_deref(), &ISSUE_SORTS)?;
    if let Some(sort) = sort
        && forge.flavor().await == Flavor::Gitea
    {
        return Err(McpError::invalid_params(
            format!(
                "sort \"{sort}\" refused: Gitea's issue listing has no sort and always lists \
                 newest first, so it would be ignored. Omit sort, or order the results yourself"
            ),
            None,
        ));
    }
    let labels = params.labels.unwrap_or_default();
    let milestones = params.milestones.unwrap_or_default();
    let filters = IssueFilters {
        state,
        sort,
        labels: &labels,
        milestones: &milestones,
        created_by: params.author.as_deref(),
        assigned_by: params.assignee.as_deref(),
        mentioned_by: params.mentioned.as_deref(),
        q: params.search.as_deref(),
        since: params.since.as_deref(),
        before: params.before.as_deref(),
    };
    if params.page.is_none() && params.limit.is_none() {
        let all = gather_all(|page, limit| {
            Box::pin(forge.list_issues(
                &params.owner,
                &params.repo,
                &filters,
                Some(page),
                Some(limit),
            ))
        })
        .await
        .map_err(to_mcp)?;
        check_issue_filters(&all.items, &labels, &milestones)?;
        let items: Vec<IssueSummary> = all.items.iter().map(slim_issue).collect();
        return gathered_result(&items, all.total, all.truncated);
    }
    let (issues, total) = forge
        .list_issues(
            &params.owner,
            &params.repo,
            &filters,
            params.page,
            params.limit,
        )
        .await
        .map_err(to_mcp)?;
    let issues = into_items(issues);
    check_issue_filters(&issues, &labels, &milestones)?;
    let items: Vec<IssueSummary> = issues.iter().map(slim_issue).collect();
    paged_result(params.page, params.limit, total, &items)
}

/// Gets one issue by index.
pub async fn get_issue(forge: &Forge, params: RepoItemRef) -> Result<CallToolResult, McpError> {
    let issue = forge
        .get_issue(&params.owner, &params.repo, params.index)
        .await
        .map_err(to_mcp)?;
    json_result(&issue_detail(&issue))
}

/// Parameters for the `list_pull_requests` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct ListPullRequestsParams {
    /// Repository owner — user or organization, e.g. `brechanbech`.
    pub owner: String,
    /// Repository name, e.g. `forgejo-mcp-rs`.
    pub repo: String,
    /// Filter by state: `open` (default), `closed`, or `all`.
    #[serde(default)]
    pub state: Option<String>,
    /// Only pull requests into this base (target) branch, e.g. `main`.
    #[serde(default)]
    pub base: Option<String>,
    /// Only pull requests from this head (source) branch: a bare branch name, e.g.
    /// `fix/login`, or `owner:branch` to require the fork it comes from.
    #[serde(default)]
    pub head: Option<String>,
    /// Order: `latest` (the default, newest first), `oldest`, `recentupdate`, `recentclose`,
    /// `leastupdate`, `mostcomment`, `leastcomment` or `priority`.
    #[serde(default)]
    pub sort: Option<String>,
    /// Only pull requests opened by this login.
    #[serde(default)]
    pub author: Option<String>,
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// A head-branch filter as the caller gave it: a branch, optionally qualified by the owner of
/// the repository it comes from, GitHub-style. Git refuses `:` in a branch name, so the first
/// one can only be that separator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeadFilter<'a> {
    owner: Option<&'a str>,
    branch: &'a str,
}

impl<'a> HeadFilter<'a> {
    fn parse(head: &'a str) -> Result<Self, McpError> {
        let (owner, branch) = match head.split_once(':') {
            Some((owner, branch)) => (Some(owner), branch),
            None => (None, head),
        };
        if branch.is_empty() || owner.is_some_and(str::is_empty) {
            return Err(McpError::invalid_params(
                format!("head must be a branch name or owner:branch, not {head:?}"),
                None,
            ));
        }
        Ok(Self { owner, branch })
    }
}

/// Whether a raw pull request matches the base and head filters.
///
/// Run over every answer, not just where the forge cannot filter: a forge that does not know a
/// filter ignores it rather than refusing it, so an unfiltered answer would otherwise pass for a
/// filtered one. The head branch is compared with both `ref` and `label`. They agree for an
/// ordinary pull request, but an agit-flow one reports `ref` as `refs/pull/{n}/head` and only `label`
/// names the branch — `user/topic` on Forgejo, which is also what its own `head` filter matches.
fn pull_matches(pr: &Value, base: Option<&str>, head: Option<&HeadFilter<'_>>) -> bool {
    let field = |pointer: &str| pr.pointer(pointer).and_then(Value::as_str);
    let base_ok = base.is_none_or(|b| field("/base/ref") == Some(b));
    let head_ok = head.is_none_or(|h| {
        let branch_ok =
            field("/head/ref") == Some(h.branch) || field("/head/label") == Some(h.branch);
        let owner_ok = h.owner.is_none_or(|o| {
            field("/head/repo/owner/login").is_some_and(|login| login.eq_ignore_ascii_case(o))
        });
        branch_ok && owner_ok
    });
    base_ok && head_ok
}

/// A pull request reduced to what a list of them is read for.
///
/// The raw object is 12–15 KB on Codeberg and gitea.com: it embeds the base and head
/// repositories in full (the same object twice for a branch in the same repository), the
/// author's and every requested reviewer's full profile, email addresses included, and every
/// label's colour and description. None of that answers "which pull requests, from where to
/// where, in what state".
#[derive(Debug, Serialize, PartialEq, Eq)]
struct PullSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// `open` or `closed`; a merged pull request is `closed` with `merged: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft: Option<bool>,
    /// Whether it can be merged as it stands. Only meaningful while it is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    mergeable: Option<bool>,
    /// Target branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    /// Source branch: the head's `ref`, or for an agit-flow pull request, whose `ref` is
    /// `refs/pull/{n}/head`, its `label` — what the `head` filter takes either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    /// The repository the head branch lives in, when it is not the base repository: a fork.
    #[serde(skip_serializing_if = "Option::is_none")]
    head_repo: Option<String>,
    /// Commit at the tip of the head branch — what `list_workflow_runs` takes as `head_sha`.
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    closed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merged_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<String>,
}

/// One pull request as `get_pull_request` and `create_pull_request` report it: the summary,
/// plus what only matters when looking at a single one.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct PullDetail {
    #[serde(flatten)]
    summary: PullSummary,
    /// The description, as Markdown. Absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merged_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_base: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    assignees: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    requested_reviewers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    milestone: Option<String>,
    /// Comment count, not counting review comments.
    #[serde(skip_serializing_if = "Option::is_none")]
    comments: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review_comments: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_url: Option<String>,
}

/// Reads a non-empty string at a JSON pointer.
fn text_at(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// The `field` of each object in the array at `pointer` — label names, user logins.
fn names_at(value: &Value, pointer: &str, field: &str) -> Vec<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get(field).and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Projects one raw pull request — Forgejo's or Gitea's, which agree on this shape — onto
/// [`PullSummary`].
fn slim_pull(pr: &Value) -> PullSummary {
    let head_ref = text_at(pr, "/head/ref");
    let head = match head_ref {
        Some(r) if r.starts_with("refs/pull/") => text_at(pr, "/head/label").or(Some(r)),
        other => other,
    };
    let base_repo = text_at(pr, "/base/repo/full_name");
    let head_repo = text_at(pr, "/head/repo/full_name").filter(|h| Some(h) != base_repo.as_ref());
    PullSummary {
        number: pr.get("number").and_then(Value::as_i64),
        title: text_at(pr, "/title"),
        state: text_at(pr, "/state"),
        merged: pr.get("merged").and_then(Value::as_bool),
        draft: pr.get("draft").and_then(Value::as_bool),
        mergeable: pr.get("mergeable").and_then(Value::as_bool),
        base: text_at(pr, "/base/ref"),
        head,
        head_repo,
        head_sha: text_at(pr, "/head/sha"),
        author: text_at(pr, "/user/login"),
        labels: names_at(pr, "/labels", "name"),
        created_at: first_time(pr, &["created_at"]),
        updated_at: first_time(pr, &["updated_at"]),
        closed_at: first_time(pr, &["closed_at"]),
        merged_at: first_time(pr, &["merged_at"]),
        html_url: text_at(pr, "/html_url"),
    }
}

/// Projects one raw pull request onto [`PullDetail`].
fn pull_detail(pr: &Value) -> PullDetail {
    PullDetail {
        summary: slim_pull(pr),
        body: text_at(pr, "/body"),
        merged_by: text_at(pr, "/merged_by/login"),
        merge_commit_sha: text_at(pr, "/merge_commit_sha"),
        merge_base: text_at(pr, "/merge_base"),
        assignees: names_at(pr, "/assignees", "login"),
        requested_reviewers: names_at(pr, "/requested_reviewers", "login"),
        milestone: text_at(pr, "/milestone/title"),
        comments: pr.get("comments").and_then(Value::as_i64),
        review_comments: pr.get("review_comments").and_then(Value::as_i64),
        diff_url: text_at(pr, "/diff_url"),
    }
}

/// Lists pull requests in `owner/repo` (open by default), optionally by base and head branch.
///
/// Where the forge filters, the answer is checked against the filters and passed on. Where it
/// cannot — Gitea has no head filter, and Forgejo before 16 neither — the whole list is walked
/// and filtered here instead, whatever `page`/`limit` asked for: a filtered page cut from an
/// unfiltered list would be neither the page nor the filter the caller meant. The walk stops at
/// the usual cap, and `truncated` says so.
pub async fn list_pull_requests(
    forge: &Forge,
    params: ListPullRequestsParams,
) -> Result<CallToolResult, McpError> {
    let state = Some(state_or_open(params.state.as_deref())?);
    let head = params.head.as_deref().map(HeadFilter::parse).transpose()?;
    let base = params.base.as_deref();
    let author = params.author.as_deref();
    let filters = PullFilters {
        state,
        base,
        head: head.map(|h| h.branch),
        sort: sort_param(params.sort.as_deref(), &PULL_SORTS)?,
        poster: author,
    };
    // Both forges filter by author (confirmed on Codeberg and gitea.com), but it is checked
    // like the branch filters, so a forge that ignored it is walked rather than believed.
    let matches = |pr: &Value| {
        pull_matches(pr, base, head.as_ref())
            && author.is_none_or(|a| {
                pr.pointer("/user/login")
                    .and_then(Value::as_str)
                    .is_some_and(|login| login.eq_ignore_ascii_case(a))
            })
    };

    let paged = params.page.is_some() || params.limit.is_some();
    let gitea_head = head.is_some() && forge.flavor().await == Flavor::Gitea;
    if paged && !gitea_head {
        let (prs, total) = forge
            .list_pull_requests(
                &params.owner,
                &params.repo,
                &filters,
                params.page,
                params.limit,
            )
            .await
            .map_err(to_mcp)?;
        let items = into_items(prs);
        if items.iter().all(matches) {
            let slim: Vec<PullSummary> = items.iter().map(slim_pull).collect();
            return paged_result(params.page, params.limit, total, &slim);
        }
        // The forge ignored a filter, so this is not the page asked for: walk the list instead.
    }

    let all = gather_all(|page, limit| {
        Box::pin(forge.list_pull_requests(
            &params.owner,
            &params.repo,
            &filters,
            Some(page),
            Some(limit),
        ))
    })
    .await
    .map_err(to_mcp)?;
    let walked = all.items.len();
    let items: Vec<PullSummary> = all
        .items
        .iter()
        .filter(|pr| matches(pr))
        .map(slim_pull)
        .collect();
    // The forge's count is of what it sent. Once filtering here has dropped some of that, the
    // count is of what is left — known only if the walk was not cut short.
    let total = if items.len() == walked {
        all.total
    } else if all.truncated {
        None
    } else {
        Some(items.len())
    };
    gathered_result(&items, total, all.truncated)
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
    json_result(&pull_detail(&pr))
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
    /// Enable or disable Releases. While off, every release endpoint 404s — including reads,
    /// and including anonymous ones — so this is what to set when release tools report a
    /// repository that plainly exists as missing.
    #[serde(default)]
    pub has_releases: Option<bool>,
    /// Enable or disable Actions (CI). The workflow tools fail the same way the release tools
    /// do while this is off: the endpoints are gone, not empty.
    #[serde(default)]
    pub has_actions: Option<bool>,
    /// Enable or disable the package registry.
    #[serde(default)]
    pub has_packages: Option<bool>,
    /// Enable or disable project boards.
    #[serde(default)]
    pub has_projects: Option<bool>,
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

/// Services whose importer authenticates with a username and password, so the server's token
/// goes in `auth_password`. A plain `git` clone builds its URL from those two fields and never
/// reads `auth_token`; the `onedev` and `codebase` importers take no token at all. Every other
/// service authenticates by `auth_token`.
const MIGRATE_PASSWORD_SERVICES: [&str; 3] = ["git", "onedev", "codebase"];

/// The username a plain `git` clone sends with the token as its password when the caller names
/// none. Forgejo, Gitea and GitLab accept a token under it over HTTP basic auth.
const MIGRATE_DEFAULT_CLONE_USER: &str = "oauth2";

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
    /// Username on the *source* instance, sent with the token as its password. Only for `git`
    /// (optional, defaults to `oauth2`), `onedev` and `codebase` (required); refused for the
    /// other services, which authenticate by token alone and ignore the token when given a
    /// username. Setting it turns on authentication.
    #[serde(default)]
    pub auth_username: Option<String>,
    /// Send the server's `FORGEJO_MIGRATE_TOKEN` as the source credential: as a token for
    /// `github`, `gitea`, `gitlab`, `gogs` and `gitbucket`, as the clone password for `git`.
    /// Leave unset for a public source.
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
    repo_detail_result(repo)
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
        ("has_releases", params.has_releases.map(Value::Bool)),
        ("has_actions", params.has_actions.map(Value::Bool)),
        ("has_packages", params.has_packages.map(Value::Bool)),
        ("has_projects", params.has_projects.map(Value::Bool)),
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
    repo_detail_result(updated)
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

    let service = params.service.clone().unwrap_or_else(|| "git".to_owned());

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
    // all. Naming a username implies it.
    if params.authenticate.unwrap_or(false) || params.auth_username.is_some() {
        insert_migrate_credential(&mut body, &service, params.auth_username, migrate_token)?;
    }
    Ok(body)
}

/// Splices the server's source credential into a migration body, in the fields `service`'s
/// importer reads it from.
fn insert_migrate_credential(
    body: &mut serde_json::Map<String, Value>,
    service: &str,
    auth_username: Option<String>,
    migrate_token: Option<&str>,
) -> Result<(), McpError> {
    let by_password = MIGRATE_PASSWORD_SERVICES.contains(&service);
    // A token service given a username stops using the token: Forgejo's Gitea importer then
    // authenticates with the username and an empty password, and Codeberg answers an empty 500
    // (27 October 2026). Refuse before anything is created rather than leave a stub behind.
    if !by_password && auth_username.is_some() {
        return Err(McpError::invalid_params(
            format!(
                "migrate refused: service \"{service}\" authenticates by token alone, and a \
                 username makes the instance ignore the token — drop auth_username and set \
                 authenticate=true"
            ),
            None,
        ));
    }
    if service != "git" && by_password && auth_username.is_none() {
        return Err(McpError::invalid_params(
            format!(
                "migrate refused: service \"{service}\" authenticates with a username and the \
                 token as its password — set auth_username to the account on the source"
            ),
            None,
        ));
    }
    let token = migrate_token.ok_or_else(|| {
        McpError::invalid_params(
            "no source credential configured: set FORGEJO_MIGRATE_TOKEN on the server to a token \
             for the *source* instance, or drop auth_username/authenticate if the source \
             repository is public"
                .to_owned(),
            None,
        )
    })?;
    if by_password {
        let username = auth_username.unwrap_or_else(|| MIGRATE_DEFAULT_CLONE_USER.to_owned());
        body.insert("auth_username".to_owned(), Value::String(username));
        body.insert("auth_password".to_owned(), Value::String(token.to_owned()));
    } else {
        body.insert("auth_token".to_owned(), Value::String(token.to_owned()));
    }
    Ok(())
}

/// Migrates (copies) a repository from another forge into this instance, optionally bringing
/// issues, PRs, labels, milestones, releases and the wiki with it.
///
/// The source credential is supplied by the server via `FORGEJO_MIGRATE_TOKEN`
/// (`migrate_token`), never as a tool argument, so it stays out of the conversation.
///
/// The import runs inside the request: Codeberg answers once it has finished (27 October 2026).
/// A failed import, a 500 or a 422 alike, can leave an empty repository behind under the target
/// name, which blocks a retry. So the name is checked to be free first, and looked up again
/// after a failure to say whether something was left.
pub async fn migrate_repo(
    forge: &Forge,
    migrate_token: Option<&str>,
    params: MigrateRepoParams,
) -> Result<CallToolResult, McpError> {
    let owner = params.repo_owner.clone();
    let name = params.repo_name.trim().to_owned();
    let body = migrate_repo_body(params, migrate_token)?;
    let authenticated = body.contains_key("auth_token") || body.contains_key("auth_password");
    let owner = if let Some(owner) = owner {
        owner
    } else {
        let me = forge.user_get_current().await.map_err(to_mcp)?;
        me["login"].as_str().unwrap_or_default().to_owned()
    };
    match forge.get_repo(&owner, &name).await {
        Ok(_) => {
            return Err(McpError::invalid_params(
                format!("migrate refused: {owner}/{name} already exists"),
                None,
            ));
        }
        Err(ForgeError::Status {
            code: StatusCode::NOT_FOUND,
            ..
        }) => {}
        Err(e) => return Err(to_mcp(e)),
    }
    match forge.migrate_repo(&Value::Object(body)).await {
        Ok(repo) => repo_detail_result(repo),
        Err(e) => {
            let left_behind = forge.get_repo(&owner, &name).await.is_ok();
            Err(migrate_failure(
                &e,
                &owner,
                &name,
                authenticated,
                left_behind,
            ))
        }
    }
}

/// The error for a failed migration: the forge's own, plus what it does not say. Codeberg
/// reports a source that refused the credential as an empty 500, and both a 500 and a 422 can
/// leave the half-made repository in place.
fn migrate_failure(
    err: &ForgeError,
    owner: &str,
    name: &str,
    authenticated: bool,
    left_behind: bool,
) -> McpError {
    // The forge's body ends in a newline, which would split the notes below onto lines of
    // their own.
    let mut message = err.to_string().trim_end().to_owned();
    if authenticated && matches!(err, ForgeError::Status { code, .. } if code.is_server_error()) {
        message.push_str(
            ". A server error here usually means the source refused the request: check that the \
             source repository exists, and that FORGEJO_MIGRATE_TOKEN is still valid, can read \
             it, and covers private repositories if this one is",
        );
    }
    if left_behind {
        message = format!(
            "{message}. The failed import left an empty {owner}/{name} behind; delete it with \
             delete_repo before retrying under that name"
        );
    }
    if err.is_caller_error() {
        McpError::invalid_params(message, None)
    } else {
        McpError::internal_error(message, None)
    }
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
    json_result(&issue_detail(&issue))
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
    // The forge answers with the branch and its whole head commit (author, committer,
    // verification); `list_branches`' shape is name, head SHA and protection.
    json_result(&slim_branches(vec![branch]).pop())
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
    json_result(&pull_detail(&pull))
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

/// Parameters for the `edit_issue` tool.
///
/// Everything but the issue's address is optional, and only what the caller gives is sent, so
/// closing an issue is `state: "closed"` and nothing else.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct EditIssueParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Issue or pull-request number.
    pub index: i64,
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New body (Markdown). Replaces the existing body wholesale.
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    #[serde(default)]
    pub state: Option<String>,
    /// Logins to assign, replacing the current assignees. An empty list unassigns everyone.
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
    /// Milestone id to set; `0` removes the milestone.
    #[serde(default)]
    pub milestone: Option<i64>,
    /// Due date as RFC 3339 (e.g. `2026-11-30T00:00:00Z`); an empty string removes it.
    #[serde(default)]
    pub due_date: Option<String>,
    /// Label names, replacing the current labels. An empty list removes them all.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

/// Builds the `EditIssueOption` body: only the fields the caller gave. Labels are not part of it;
/// they go through their own endpoint.
fn edit_issue_body(params: &EditIssueParams) -> Result<serde_json::Map<String, Value>, McpError> {
    let mut body = serde_json::Map::new();
    if let Some(title) = &params.title {
        body.insert("title".to_owned(), Value::String(title.clone()));
    }
    if let Some(text) = &params.body {
        body.insert("body".to_owned(), Value::String(text.clone()));
    }
    if let Some(state) = &params.state {
        if !matches!(state.as_str(), "open" | "closed") {
            return Err(McpError::invalid_params(
                format!("state must be `open` or `closed`, not {state:?}"),
                None,
            ));
        }
        body.insert("state".to_owned(), Value::String(state.clone()));
    }
    if let Some(assignees) = &params.assignees {
        body.insert("assignees".to_owned(), serde_json::json!(assignees));
    }
    if let Some(milestone) = params.milestone {
        body.insert("milestone".to_owned(), Value::from(milestone));
    }
    match params.due_date.as_deref() {
        Some("") => {
            body.insert("unset_due_date".to_owned(), Value::Bool(true));
        }
        Some(due) => {
            body.insert("due_date".to_owned(), Value::String(due.to_owned()));
        }
        None => {}
    }
    Ok(body)
}

/// The requested label names the forge did not apply — the ones missing from the label list its
/// labels endpoint answers with.
///
/// Codeberg drops a label name the repository has no label for, without an error, and answers
/// `200` with the labels it did apply (25 October 2026). Left there, a caller setting `bug` on a
/// repository whose label is `Kind/Bug` would be told it worked.
fn labels_not_applied(requested: &[String], applied: &Value) -> Vec<String> {
    let applied: Vec<&str> = applied
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    requested
        .iter()
        .filter(|name| !applied.contains(&name.as_str()))
        .cloned()
        .collect()
}

/// `edit_issue`'s answer: the issue in `get_issue`'s shape, plus any label names the forge did
/// not apply.
#[derive(Debug, Serialize)]
struct EditedIssue {
    #[serde(flatten)]
    issue: IssueDetail,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    labels_not_found: Vec<String>,
}

/// Edits an issue, or the issue half of a pull request, in place. Labels, when given, are
/// replaced through their own endpoint after the edit, and the issue is then read back so the
/// answer shows them; any the forge did not apply are named in `labels_not_found`.
pub async fn edit_issue(
    forge: &Forge,
    params: EditIssueParams,
) -> Result<CallToolResult, McpError> {
    let body = edit_issue_body(&params)?;
    if body.is_empty() && params.labels.is_none() {
        return Err(McpError::invalid_params(
            "nothing to change: pass at least one of title, body, state, assignees, milestone, \
             due_date or labels"
                .to_owned(),
            None,
        ));
    }
    let mut issue = Value::Null;
    if !body.is_empty() {
        issue = forge
            .edit_issue(
                &params.owner,
                &params.repo,
                params.index,
                &Value::Object(body),
            )
            .await
            .map_err(to_mcp)?;
    }
    let mut labels_not_found = Vec::new();
    if let Some(labels) = &params.labels {
        let applied = forge
            .replace_issue_labels(&params.owner, &params.repo, params.index, labels)
            .await
            .map_err(to_mcp)?;
        labels_not_found = labels_not_applied(labels, &applied);
        issue = forge
            .get_issue(&params.owner, &params.repo, params.index)
            .await
            .map_err(to_mcp)?;
    }
    json_result(&EditedIssue {
        issue: issue_detail(&issue),
        labels_not_found,
    })
}

/// The merge styles both forges accept. `manually-merged`, which marks a pull request merged
/// without merging anything, is left out on purpose.
const MERGE_STYLES: [&str; 5] = [
    "merge",
    "rebase",
    "rebase-merge",
    "squash",
    "fast-forward-only",
];

/// Parameters for the `merge_pull_request` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct MergePullRequestParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Pull-request number.
    pub index: i64,
    /// `merge` (the default), `rebase`, `rebase-merge`, `squash` or `fast-forward-only`. The
    /// repository's settings may allow only some of them.
    #[serde(default)]
    pub style: Option<String>,
    /// Merge commit title. The forge composes one when absent.
    #[serde(default)]
    pub title: Option<String>,
    /// Merge commit message body.
    #[serde(default)]
    pub message: Option<String>,
    /// Delete the head branch after merging. Defaults to the repository's own setting.
    #[serde(default)]
    pub delete_branch: Option<bool>,
    /// Merge only if the pull request's head is still this commit (the full SHA, from
    /// `get_pull_request`'s `head_sha`). Guards against merging something pushed after review.
    #[serde(default)]
    pub head_sha: Option<String>,
}

/// Merges a pull request, then reads it back: both forges answer the merge itself with an empty
/// `200`, so the pull request as it now stands is the useful answer.
pub async fn merge_pull_request(
    forge: &Forge,
    params: MergePullRequestParams,
) -> Result<CallToolResult, McpError> {
    let style = params.style.as_deref().unwrap_or("merge");
    if !MERGE_STYLES.contains(&style) {
        return Err(McpError::invalid_params(
            format!(
                "style must be one of {}, not {style:?}",
                MERGE_STYLES.join(", ")
            ),
            None,
        ));
    }
    let head_sha = params.head_sha.as_deref().map(full_sha).transpose()?;
    let merge = PullMerge {
        style,
        title: params.title.as_deref(),
        message: params.message.as_deref(),
        delete_branch: params.delete_branch,
        head_sha: head_sha.as_deref(),
    };
    forge
        .merge_pull_request(&params.owner, &params.repo, params.index, &merge)
        .await
        .map_err(to_mcp)?;
    let pull = forge
        .get_pull_request(&params.owner, &params.repo, params.index)
        .await
        .map_err(to_mcp)?;
    json_result(&pull_detail(&pull))
}

/// Parameters for the `create_pull_request_review` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CreatePullRequestReviewParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Pull-request number.
    pub index: i64,
    /// `approve`, `request_changes` or `comment`.
    pub verdict: String,
    /// The review's summary text (Markdown). The forge requires it for `request_changes` and
    /// `comment`.
    #[serde(default)]
    pub body: Option<String>,
    /// The commit the review is of (full SHA). Defaults to the pull request's current head.
    #[serde(default)]
    pub commit_id: Option<String>,
}

/// Submits a review verdict on a pull request.
pub async fn create_pull_request_review(
    forge: &Forge,
    params: CreatePullRequestReviewParams,
) -> Result<CallToolResult, McpError> {
    let event = match params.verdict.as_str() {
        "approve" => "APPROVED",
        "request_changes" => "REQUEST_CHANGES",
        "comment" => "COMMENT",
        other => {
            return Err(McpError::invalid_params(
                format!("verdict must be `approve`, `request_changes` or `comment`, not {other:?}"),
                None,
            ));
        }
    };
    let mut body = serde_json::Map::new();
    body.insert("event".to_owned(), Value::String(event.to_owned()));
    if let Some(text) = params.body {
        body.insert("body".to_owned(), Value::String(text));
    }
    if let Some(commit) = params.commit_id.as_deref() {
        body.insert("commit_id".to_owned(), Value::String(full_sha(commit)?));
    }
    let raw = forge
        .create_pull_request_review(
            &params.owner,
            &params.repo,
            params.index,
            &Value::Object(body),
        )
        .await
        .map_err(to_mcp)?;
    let review: RawReview = decode(raw)?;
    json_result(&summarize_review(review))
}

/// Encodes a caller's repository path for a request URL, refusing one that could leave the
/// endpoint (see [`encode_repo_path`]).
fn repo_path_param(name: &str, path: &str) -> Result<String, McpError> {
    encode_repo_path(path).map_err(|e| McpError::invalid_params(format!("{name}: {e}"), None))
}

/// Parameters for the `delete_branch` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct DeleteBranchParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Branch to delete, e.g. `feature/login`.
    pub branch: String,
}

/// Deletes a branch. The forges refuse the default branch and protected ones.
///
/// Codeberg answers the delete of a branch that does not exist with an empty `500`, not a
/// `404` — for a name that never existed and for one already deleted alike (25 October 2026).
/// A server error would tell the caller something broke and might be worth retrying, so after
/// one the branch is looked up, and a missing one is reported as what it is. Gitea answers a
/// plain `404 not found`, which reads the same for a missing repository, so that gets the
/// same lookup and the same message.
pub async fn delete_branch(
    forge: &Forge,
    params: DeleteBranchParams,
) -> Result<CallToolResult, McpError> {
    let branch = repo_path_param("branch", &params.branch)?;
    match forge
        .delete_branch(&params.owner, &params.repo, &branch)
        .await
    {
        Ok(()) => {}
        Err(ForgeError::Status { code, .. })
            if (code.is_server_error() || code == StatusCode::NOT_FOUND)
                && matches!(
                    forge.get_branch(&params.owner, &params.repo, &branch).await,
                    Err(ForgeError::Status {
                        code: StatusCode::NOT_FOUND,
                        ..
                    })
                ) =>
        {
            return Err(McpError::invalid_params(
                format!(
                    "no branch `{}` in {}/{}, so nothing was deleted",
                    params.branch, params.owner, params.repo
                ),
                None,
            ));
        }
        Err(e) => return Err(to_mcp(e)),
    }
    json_result(&serde_json::json!({ "deleted": true, "branch": params.branch }))
}

/// The commit a file change made, and the file as it now stands — from a `FileResponse` or
/// `FileDeleteResponse`, which otherwise carry the file's base64 body, its three URLs and the
/// commit's full author, committer, tree and parents.
fn file_change_result(raw: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (key, pointer) in [
        ("path", "/content/path"),
        ("sha", "/content/sha"),
        ("html_url", "/content/html_url"),
    ] {
        if let Some(v) = text_at(raw, pointer) {
            out.insert(key.to_owned(), Value::String(v));
        }
    }
    let mut commit = serde_json::Map::new();
    for (key, pointer) in [
        ("sha", "/commit/sha"),
        ("message", "/commit/message"),
        ("html_url", "/commit/html_url"),
    ] {
        if let Some(v) = text_at(raw, pointer) {
            commit.insert(key.to_owned(), Value::String(v));
        }
    }
    out.insert("commit".to_owned(), Value::Object(commit));
    Value::Object(out)
}

/// The commit placement every file change shares: message, branch, and a new branch to commit
/// to instead.
fn file_commit_fields(
    body: &mut serde_json::Map<String, Value>,
    message: Option<String>,
    branch: Option<String>,
    new_branch: Option<String>,
) {
    for (key, value) in [
        ("message", message),
        ("branch", branch),
        ("new_branch", new_branch),
    ] {
        if let Some(value) = value {
            body.insert(key.to_owned(), Value::String(value));
        }
    }
}

/// Parameters for the `create_or_update_file` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct CreateOrUpdateFileParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// File path from the repository root, e.g. `docs/setup.md`.
    pub path: String,
    /// The whole new content, as UTF-8 text.
    pub content: String,
    /// To update an existing file: its current blob SHA, from `get_file_contents`. Omit it to
    /// create a new file. A stale SHA is refused, so a change made meanwhile is not overwritten.
    #[serde(default)]
    pub sha: Option<String>,
    /// Commit message. The forge composes one when absent.
    #[serde(default)]
    pub message: Option<String>,
    /// Branch to commit to. Defaults to the repository's default branch.
    #[serde(default)]
    pub branch: Option<String>,
    /// Create this branch from `branch` and commit there instead — the way to propose a change
    /// as a pull request.
    #[serde(default)]
    pub new_branch: Option<String>,
}

/// Creates a file, or replaces one's content, as a single commit.
pub async fn create_or_update_file(
    forge: &Forge,
    params: CreateOrUpdateFileParams,
) -> Result<CallToolResult, McpError> {
    let path = repo_path_param("path", &params.path)?;
    let mut body = serde_json::Map::new();
    body.insert(
        "content".to_owned(),
        Value::String(BASE64.encode(params.content.as_bytes())),
    );
    file_commit_fields(&mut body, params.message, params.branch, params.new_branch);
    let created = params.sha.is_none();
    let raw = match params.sha {
        Some(sha) => {
            body.insert("sha".to_owned(), Value::String(sha));
            forge
                .update_file(&params.owner, &params.repo, &path, &Value::Object(body))
                .await
        }
        None => {
            forge
                .create_file(&params.owner, &params.repo, &path, &Value::Object(body))
                .await
        }
    }
    .map_err(to_mcp)?;
    let mut result = file_change_result(&raw);
    if let Value::Object(out) = &mut result {
        out.insert("created".to_owned(), Value::Bool(created));
    }
    json_result(&result)
}

/// Parameters for the `delete_file` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct DeleteFileParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// File path from the repository root.
    pub path: String,
    /// The file's current blob SHA, from `get_file_contents`.
    pub sha: String,
    /// Commit message. The forge composes one when absent.
    #[serde(default)]
    pub message: Option<String>,
    /// Branch to commit to. Defaults to the repository's default branch.
    #[serde(default)]
    pub branch: Option<String>,
    /// Create this branch from `branch` and commit the deletion there instead.
    #[serde(default)]
    pub new_branch: Option<String>,
}

/// Deletes a file, as a single commit.
pub async fn delete_file(
    forge: &Forge,
    params: DeleteFileParams,
) -> Result<CallToolResult, McpError> {
    let path = repo_path_param("path", &params.path)?;
    let mut body = serde_json::Map::new();
    body.insert("sha".to_owned(), Value::String(params.sha));
    file_commit_fields(&mut body, params.message, params.branch, params.new_branch);
    let raw = forge
        .delete_file(&params.owner, &params.repo, &path, &Value::Object(body))
        .await
        .map_err(to_mcp)?;
    let mut result = file_change_result(&raw);
    if let Value::Object(out) = &mut result {
        out.insert("deleted".to_owned(), Value::Bool(true));
        out.insert("path".to_owned(), Value::String(params.path));
    }
    json_result(&result)
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

/// Parameters for the `edit_release` tool.
///
/// Everything but the release's address is optional, and only what the caller gives is sent:
/// this edits a release in place, so omitting a field has to leave it alone.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct EditReleaseParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Release id, from `get_release` or `list_releases`.
    pub release_id: i64,
    /// New release title.
    #[serde(default)]
    pub name: Option<String>,
    /// New release notes (markdown). Replaces the existing notes wholesale.
    #[serde(default)]
    pub body: Option<String>,
    /// Move the release onto a different git tag.
    #[serde(default)]
    pub tag_name: Option<String>,
    /// Commit, branch or ref the tag points at.
    #[serde(default)]
    pub target_commitish: Option<String>,
    /// Switch draft state.
    #[serde(default)]
    pub draft: Option<bool>,
    /// Switch pre-release state.
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

/// A release reduced to what identifies it, and to the names of its files.
///
/// The raw object also carries the full uploader `User`, the tarball/zipball URLs and every
/// asset in full; those are dropped. Assets are the bulk: each `forgejo/forgejo` release carries
/// 21 (a binary, `.xz`, `.asc` and `.sha256` per platform), and listing 15 releases with each
/// asset's id, size, download count and URL came to 66 KB. Their names alone are 11 KB, and are
/// enough to say what a release ships; `list_release_assets` has the rest, including the id
/// `delete_release_asset` takes.
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
    /// How many files are attached. `0` for a release that has none.
    asset_count: usize,
    /// Their file names, in the forge's order. Empty for a release that has none.
    asset_names: Vec<String>,
}

/// One release as `get_release`, `create_release` and `edit_release` report it: the summary, plus
/// what only matters when looking at a single release.
#[derive(Debug, Serialize)]
struct ReleaseDetail {
    #[serde(flatten)]
    summary: ReleaseSummary,
    /// The branch or commit the tag was created from, when the forge reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_commitish: Option<String>,
    /// The release notes, as Markdown. Absent when there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
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

/// Projects one raw release onto [`ReleaseSummary`], flattening the uploader to a login and the
/// assets to their names.
fn slim_release(value: &Value) -> ReleaseSummary {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    let assets = value
        .get("assets")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    ReleaseSummary {
        id: value.get("id").and_then(Value::as_i64),
        tag_name: text("tag_name"),
        name: text("name"),
        draft: value.get("draft").and_then(Value::as_bool),
        prerelease: value.get("prerelease").and_then(Value::as_bool),
        published_at: text("published_at"),
        html_url: text("html_url"),
        author: value
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        asset_count: assets.len(),
        asset_names: assets
            .iter()
            .filter_map(|a| a.get("name").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect(),
    }
}

/// Projects raw release objects down to [`ReleaseSummary`].
fn slim_releases(items: &[Value]) -> Vec<ReleaseSummary> {
    items.iter().map(slim_release).collect()
}

/// Projects one raw release onto [`ReleaseDetail`]. Empty notes count as none, since both forges
/// send `""` for a release published without any.
fn release_detail(value: &Value) -> ReleaseDetail {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    };
    ReleaseDetail {
        summary: slim_release(value),
        target_commitish: text("target_commitish"),
        body: text("body"),
    }
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
        return gathered_result(&slim_releases(&all.items), all.total, all.truncated);
    }
    let (releases, total) = forge
        .list_releases(&params.owner, &params.repo, params.page, params.limit)
        .await
        .map_err(to_mcp)?;
    paged_result(
        params.page,
        params.limit,
        total,
        &slim_releases(&into_items(releases)),
    )
}

/// Gets one release by its git tag — the lookup that makes a release script idempotent, since
/// the tag is known before the release exists and the numeric id only after.
pub async fn get_release(forge: &Forge, params: ReleaseTagRef) -> Result<CallToolResult, McpError> {
    let release = forge
        .get_release_by_tag(&params.owner, &params.repo, &params.tag)
        .await
        .map_err(|e| release_404_hint(e, &params.owner, &params.repo))?;
    json_result(&release_detail(&release))
}

/// Maps a release-endpoint error, disambiguating the one 404 that reads as something it is not.
///
/// Forgejo returns `404` for a release the repository does not have *and* for a repository with
/// the Releases unit switched off — the whole endpoint family disappears, anonymously included.
/// Untreated, the first reading ("no such release yet") sends a caller straight into
/// `create_release`, which 404s in turn for a reason nothing has named. So the hint says both.
fn release_404_hint(err: crate::mcp_core::ApiError, owner: &str, repo: &str) -> McpError {
    use crate::mcp_core::ApiError;
    if let ApiError::Status { code, .. } = &err
        && code.as_u16() == 404
    {
        return McpError::invalid_params(
            format!(
                "{err} — either {owner}/{repo} has no release for that tag, or the repository \
                 has Releases disabled, which 404s the whole endpoint family. Check \
                 `has_releases` via get_repo: if it is false, turn the unit on (edit_repo \
                 has_releases=true) before creating a release."
            ),
            None,
        );
    }
    to_mcp(err)
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
    json_result(&release_detail(&release))
}

/// Edits an existing release in place — its notes, title, tag, or draft/pre-release state.
///
/// Only the fields the caller supplied go into the request. A PATCH that filled in defaults for
/// the rest would overwrite whatever the release already said, which for a call meaning to fix
/// the title would mean losing the release notes.
///
/// This is the tool for correcting published notes. The alternative — delete the release and
/// recreate it — takes its assets with it.
pub async fn edit_release(
    forge: &Forge,
    params: EditReleaseParams,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::Map::new();
    if let Some(name) = params.name {
        body.insert("name".to_owned(), Value::String(name));
    }
    if let Some(notes) = params.body {
        body.insert("body".to_owned(), Value::String(notes));
    }
    if let Some(tag) = params.tag_name {
        body.insert("tag_name".to_owned(), Value::String(tag));
    }
    if let Some(target) = params.target_commitish {
        body.insert("target_commitish".to_owned(), Value::String(target));
    }
    if let Some(draft) = params.draft {
        body.insert("draft".to_owned(), Value::Bool(draft));
    }
    if let Some(prerelease) = params.prerelease {
        body.insert("prerelease".to_owned(), Value::Bool(prerelease));
    }
    if body.is_empty() {
        return Err(McpError::invalid_params(
            "nothing to change: pass at least one of name, body, tag_name, target_commitish, \
             draft or prerelease"
                .to_owned(),
            None,
        ));
    }

    let release = forge
        .edit_release(
            &params.owner,
            &params.repo,
            params.release_id,
            &Value::Object(body),
        )
        .await
        .map_err(|e| release_404_hint(e, &params.owner, &params.repo))?;
    json_result(&release_detail(&release))
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
    /// Filter by head commit SHA — the most reliable way to find the run for a given push. Must
    /// be the full 40-character SHA (64 for SHA-256): the match is exact, so an abbreviated one
    /// would find nothing, and is refused instead.
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
    /// 1-based page number. Defaults to 1, the most recent runs.
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page. Defaults to 30; Codeberg caps it at 50.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Page size for `list_workflow_runs` when the caller gives none — what Forgejo itself uses once
/// it is paging at all. Always sent, with a page, because Forgejo pages this listing only when
/// asked to; see `run_list_request` in the client.
const RUNS_DEFAULT_LIMIT: u32 = 30;

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
    /// Forgejo's `trigger_event`, else `event` — Gitea has only the latter.
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

/// Reads the first of `keys` that holds a real timestamp.
///
/// "Never" is not always `""`. Forgejo writes a zero Unix timestamp for a run that has not
/// started — `1970-01-01T01:00:00+01:00` on Codeberg, rendered in the server's zone — and Go's
/// zero `time.Time` is `0001-01-01T00:00:00Z`. Either reported as a date would claim a queued
/// run started in 1970. The epoch can fall on 31 December 1969 west of UTC, so both days count;
/// no CI run is from either.
fn first_time(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .find(|s| {
            !s.is_empty()
                && !s.starts_with("1970-01-01")
                && !s.starts_with("1969-12-31")
                && !s.starts_with("0001-01-01")
        })
        .map(ToOwned::to_owned)
}

/// Checks a `head_sha` filter is a whole commit id, lowercased.
///
/// Both forges match the filter exactly, so an abbreviated or uppercase SHA does not fail — it
/// returns an empty list, indistinguishable from "this commit has no runs". Confirmed live on
/// Codeberg and gitea.com. Refusing it is the only way to make that mistake visible.
fn full_sha(sha: &str) -> Result<String, McpError> {
    let is_full = matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if is_full {
        Ok(sha.to_ascii_lowercase())
    } else {
        Err(McpError::invalid_params(
            format!(
                "head_sha must be a full commit SHA (40 hex characters, or 64 for a SHA-256 \
                 repository), not {sha:?}: the forge compares it exactly, so an abbreviated \
                 SHA never matches. Expand it with `git rev-parse` or get_commit."
            ),
            None,
        ))
    }
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
        // Forgejo leaves `event` empty for a dispatched or scheduled run and names the trigger
        // only in `trigger_event`, which is also what its `event` filter matches.
        event: first_str(run, &["trigger_event", "event"]),
        workflow: first_str(run, &["workflow_id"]).or(path_workflow),
        commit_sha: first_str(run, &["commit_sha", "head_sha"]),
        // `head_branch` is populated only for branch runs — it is null for tags and pull
        // requests — so the ref embedded in `path` is the reliable fallback.
        git_ref: first_str(run, &["prettyref", "head_branch"]).or(path_ref),
        html_url: first_str(run, &["html_url"]),
        created: first_time(run, &["created", "created_at"]),
        started: first_time(run, &["started", "started_at"]),
        stopped: first_time(run, &["stopped", "completed_at"]),
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
        head_sha: params.head_sha.as_deref().map(full_sha).transpose()?,
        git_ref: params.git_ref,
        status: params.status,
        event: params.event,
        workflow: params.workflow_id,
    };
    let page = params.page.unwrap_or(1);
    let limit = params.limit.unwrap_or(RUNS_DEFAULT_LIMIT);
    let body = forge
        .list_workflow_runs(&params.owner, &params.repo, &filters, page, limit)
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
    paged_result(Some(page), Some(limit), total, &slim_runs(&items))
}

/// Gets one workflow run by id, in `list_workflow_runs`' shape.
///
/// The raw run was about 20 KB on Codeberg (27 October 2026): the whole push `event_payload`
/// (as an escaped string), the repository twice, and `trigger_user` with its email address.
pub async fn get_workflow_run(forge: &Forge, params: RunRef) -> Result<CallToolResult, McpError> {
    let run = forge
        .get_workflow_run(&params.owner, &params.repo, params.run_id)
        .await
        .map_err(to_mcp)?;
    json_result(&normalize_run(&run))
}

/// One job of a workflow run, in one shape on both forges — a translation for the same reason
/// [`RunSummary`] is one. Forgejo's per-run listing is sparse (no timestamps, no steps); Gitea's
/// carries both, so most fields here are present on Gitea only.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct JobSummary {
    /// What `get_job_log` takes as `job_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// The outcome where there is one, else the phase — Gitea's `conclusion` promoted, exactly
    /// as for runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    /// Gitea's raw `conclusion`; never present on Forgejo.
    #[serde(skip_serializing_if = "Option::is_none")]
    conclusion: Option<String>,
    /// Forgejo's `attempt`, Gitea's `run_attempt` — above 1 once the job has been re-run.
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<i64>,
    /// Runner labels — Forgejo's `runs_on`, Gitea's `labels`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    runs_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stopped: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html_url: Option<String>,
    /// The job's steps, where the listing includes them (Gitea does; Forgejo does not).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    steps: Vec<StepSummary>,
}

/// One step of a job: enough to name the step that failed.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct StepSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
}

/// Reads an array of strings from the first of `keys` that holds one.
fn first_str_list(v: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|k| v.get(*k)?.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The outcome-or-phase status shared by runs, jobs and steps: Gitea's `conclusion` once it has
/// one, else `status`. On Forgejo, which has no `conclusion`, this is simply `status`.
fn outcome(v: &Value) -> Option<String> {
    first_str(v, &["conclusion"]).or_else(|| first_str(v, &["status"]))
}

/// Projects one raw job — Forgejo's `ActionRunJob` or Gitea's `ActionWorkflowJob` — onto
/// [`JobSummary`]. Allow-listed, which drops Forgejo's `handle`, `task_id` and owner/repo ids and
/// Gitea's runner identity and API URLs.
fn normalize_job(job: &Value) -> JobSummary {
    let steps = job
        .get("steps")
        .and_then(Value::as_array)
        .map(|steps| {
            steps
                .iter()
                .map(|s| StepSummary {
                    number: first_i64(s, &["number"]),
                    name: first_str(s, &["name"]),
                    status: outcome(s),
                })
                .collect()
        })
        .unwrap_or_default();
    JobSummary {
        id: first_i64(job, &["id"]),
        name: first_str(job, &["name"]),
        status: outcome(job),
        conclusion: first_str(job, &["conclusion"]),
        attempt: first_i64(job, &["attempt", "run_attempt"]),
        runs_on: first_str_list(job, &["runs_on", "labels"]),
        started: first_time(job, &["started", "started_at"]),
        stopped: first_time(job, &["stopped", "completed_at"]),
        html_url: first_str(job, &["html_url"]),
        steps,
    }
}

/// Lists the jobs of one workflow run — the step between "this run failed" and reading the log
/// of the job that failed it.
///
/// Forgejo answers with a bare array; Gitea wraps it as `{ jobs, total_count }`. Both are
/// unwrapped here, and `truncated` is set only if Gitea reports more jobs than it returned.
pub async fn list_workflow_jobs(forge: &Forge, params: RunRef) -> Result<CallToolResult, McpError> {
    let body = forge
        .list_workflow_jobs(&params.owner, &params.repo, params.run_id)
        .await
        .map_err(to_mcp)?;
    let (items, total) = match body {
        Value::Object(mut map) => (
            into_items(map.remove("jobs").unwrap_or(Value::Null)),
            map.get("total_count")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok()),
        ),
        other => (into_items(other), None),
    };
    let jobs: Vec<JobSummary> = items
        .iter()
        .filter(|v| v.is_object())
        .map(normalize_job)
        .collect();
    let truncated = total.is_some_and(|t| t > jobs.len());
    gathered_result(&jobs, total, truncated)
}

/// Default window on a job log, in raw bytes. A failed job's error is at its tail, and 32 KiB
/// is a few hundred lines — more once timestamps and colour codes are stripped.
const LOG_DEFAULT_BYTES: u64 = 32 * 1024;

/// Ceiling on one window. A whole log is unbounded — a long build runs to megabytes — so a larger
/// request is refused rather than quietly clamped; page through with `offset` instead.
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Parameters for reading part of a job's log.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct JobLogParams {
    /// Repository owner — user or organization.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Job id — the `id` field from `list_workflow_jobs`, not the run id.
    pub job_id: i64,
    /// Read forward from this byte offset instead of returning the tail. Take it from the
    /// `earlier` or `later` field of a previous call.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Window size in raw log bytes (default 32768, at most 1048576), trimmed to whole lines.
    /// Counted before timestamps and colour codes are stripped, so the text returned is shorter.
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// Keep each line's timestamp (default false — they are about a third of a log's bytes).
    #[serde(default)]
    pub timestamps: bool,
}

/// Which part of a log to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogWindow {
    /// The last `n` bytes.
    Tail(u64),
    /// `len` bytes from `offset`.
    From { offset: u64, len: u64 },
}

impl LogWindow {
    /// The `Range` header for this window. Each asks for one byte more, *before* the window, so
    /// [`cut_lines`] can tell whether the window opens on a line boundary or mid-line.
    fn range_header(self) -> String {
        match self {
            Self::Tail(n) => format!("bytes=-{}", n + 1),
            Self::From { offset, len } => {
                format!("bytes={}-{}", offset.saturating_sub(1), offset + len - 1)
            }
        }
    }
}

/// Where a window of a log landed: `start..end` in offsets of the whole log, and `span`, the same
/// bytes as indices into what the server sent.
#[derive(Debug, PartialEq, Eq)]
struct LogCut {
    start: u64,
    end: u64,
    span: std::ops::Range<usize>,
}

/// A byte offset as an index. Log windows are capped at [`LOG_MAX_BYTES`], so this saturates
/// only on a corrupt `Content-Range`, which then fails the bounds check as an empty window.
fn as_index(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Cuts `window` out of what the server sent, trimmed to whole lines.
///
/// The server may have honoured the range or ignored it and sent the whole log; `served` says
/// which, so the window is found the same way either way. A partial first line is dropped unless
/// the byte before it — requested for exactly this — is a newline, and a partial last line is
/// dropped unless the window reaches the end of the log. A window with no newline in it at all is
/// one over-long line, and is returned as it is rather than emptied.
fn cut_lines(served: &ByteRange, window: LogWindow) -> LogCut {
    let bytes = &served.bytes;
    let base = served.start;
    let have_end = base + bytes.len() as u64;
    let total = served.total.unwrap_or(have_end);
    let (lo, hi) = match window {
        LogWindow::Tail(n) => (total.saturating_sub(n), total),
        LogWindow::From { offset, len } => (offset, offset.saturating_add(len)),
    };
    let lo = lo.clamp(base, have_end);
    let hi = hi.clamp(lo, have_end);
    let (mut s, mut e) = (as_index(lo - base), as_index(hi - base));

    let opens_on_line = lo == 0 || (s > 0 && bytes[s - 1] == b'\n');
    if !opens_on_line && let Some(i) = bytes[s..e].iter().position(|&b| b == b'\n') {
        s += i + 1;
    }
    if hi < total
        && let Some(i) = bytes[s..e].iter().rposition(|&b| b == b'\n')
    {
        e = s + i + 1;
    }
    LogCut {
        start: base + s as u64,
        end: base + e as u64,
        span: s..e,
    }
}

/// Strips ANSI escape sequences: CSI (`ESC [` … final byte — colours, cursor moves), OSC
/// (`ESC ]` … `BEL` or `ESC \` — the hyperlinks Forgejo 17 renders), and any other two-character
/// `ESC x`. Pure presentation, and on a cargo build they are on most lines.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                // Stops at a newline too, so an unterminated OSC costs one line, not the log.
                while let Some(c) = chars.next() {
                    match c {
                        '\x07' => break,
                        // The string terminator is `ESC \`; take the backslash with it.
                        '\x1b' => {
                            chars.next();
                            break;
                        }
                        '\n' => {
                            out.push('\n');
                            break;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Drops the runner's timestamp from the front of a log line — `2026-09-27T12:25:29.0387087Z `.
/// Only a prefix that looks like one is removed; any other line passes through whole.
fn strip_timestamp(line: &str) -> &str {
    let Some((head, rest)) = line.split_once(' ') else {
        return line;
    };
    let b = head.as_bytes();
    let is_stamp = b.len() >= 20
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[10] == b'T'
        && head.ends_with('Z');
    if is_stamp { rest } else { line }
}

/// Makes a raw log window readable: escape sequences out, and timestamps too unless kept.
fn clean_log(raw: &str, timestamps: bool) -> String {
    let text = strip_ansi(raw);
    if timestamps {
        return text;
    }
    text.split_inclusive('\n').map(strip_timestamp).collect()
}

/// Reads part of one job's log — the tail by default, since that is where a failure is.
///
/// The log is fetched by HTTP `Range`, never whole, unless the server ignores the range. The
/// answer gives the window's byte offsets in the whole log and, where there is more, the
/// `earlier` / `later` windows to ask for next, so a caller walks back from the tail without
/// arithmetic of its own.
pub async fn get_job_log(forge: &Forge, params: JobLogParams) -> Result<CallToolResult, McpError> {
    let len = params.max_bytes.unwrap_or(LOG_DEFAULT_BYTES);
    if len == 0 || len > LOG_MAX_BYTES {
        return Err(McpError::invalid_params(
            format!(
                "max_bytes must be between 1 and {LOG_MAX_BYTES} — page through a longer log \
                 with offset"
            ),
            None,
        ));
    }
    let window = params
        .offset
        .map_or(LogWindow::Tail(len), |offset| LogWindow::From {
            offset,
            len,
        });
    let past_end = |offset: u64| {
        McpError::invalid_params(
            format!(
                "offset {offset} is past the end of job {}'s log",
                params.job_id
            ),
            None,
        )
    };

    let served = match forge
        .get_job_log(
            &params.owner,
            &params.repo,
            params.job_id,
            &window.range_header(),
        )
        .await
    {
        Ok(served) => served,
        // A range wholly past the end. For the tail that can only be an empty log — a job that
        // has not written anything yet — which is an answer, not an error.
        Err(ForgeError::Status { code, .. }) if code == StatusCode::RANGE_NOT_SATISFIABLE => {
            match window {
                LogWindow::From { offset, .. } => return Err(past_end(offset)),
                LogWindow::Tail(_) => ByteRange {
                    bytes: Vec::new(),
                    start: 0,
                    total: Some(0),
                },
            }
        }
        Err(e) => return Err(to_mcp(e)),
    };
    // A server that ignored the range sent the whole log, so an offset past its end arrives as
    // a 200 rather than a 416; it is the same mistake either way.
    if let (LogWindow::From { offset, .. }, Some(total)) = (window, served.total)
        && offset > total
    {
        return Err(past_end(offset));
    }

    let cut = cut_lines(&served, window);
    let raw = String::from_utf8_lossy(&served.bytes[cut.span.clone()]);
    let mut out = serde_json::json!({
        "job_id": params.job_id,
        "start": cut.start,
        "end": cut.end,
        "total_bytes": served.total,
        "log": clean_log(&raw, params.timestamps),
    });
    if let Some(obj) = out.as_object_mut() {
        if cut.start > 0 {
            let offset = cut.start.saturating_sub(len);
            obj.insert(
                "earlier".to_owned(),
                serde_json::json!({ "offset": offset, "max_bytes": cut.start - offset }),
            );
        }
        if served.total.is_some_and(|total| cut.end < total) {
            obj.insert("later".to_owned(), serde_json::json!({ "offset": cut.end }));
        }
    }
    json_result(&out)
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

/// Parameters for the `rerun_workflow` tool.
#[derive(Debug, serde::Deserialize, JsonSchema)]
pub struct RerunWorkflowParams {
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Workflow run id (the `id` field from `list_workflow_runs`). Required even with
    /// `job_id`: Gitea addresses a job under its run, and the answer reads the run back.
    pub run_id: i64,
    /// Rerun only this job, and the jobs that depend on it, instead of the whole run — the
    /// job `id` from `list_workflow_jobs`.
    #[serde(default)]
    pub job_id: Option<i64>,
}

/// Whether an error says the instance has no such endpoint at all, rather than no such run.
///
/// Both forges answer a path their router does not know with Go's plain-text
/// `404 page not found`, while a handler's own 404 — `run with id …: resource does not exist` —
/// is JSON. Seen on Codeberg and gitea.com on 24 October 2026, for `GET` and `POST` alike. A
/// `405` would mean the path exists for another method only, which is as good as absent.
fn is_missing_endpoint(err: &ForgeError) -> bool {
    match err {
        ForgeError::Status { code, body } => {
            *code == StatusCode::METHOD_NOT_ALLOWED
                || (*code == StatusCode::NOT_FOUND && serde_json::from_str::<Value>(body).is_err())
        }
        _ => false,
    }
}

/// Picks the run a cancel or rerun answered with, if the answer is one — Gitea's is, Forgejo's
/// empty `204` is not. A job (Gitea's answer to a job rerun) is not a run, so the caller says
/// whether a run is to be expected at all.
fn returned_run(body: Value, expect_run: bool) -> Option<Value> {
    (expect_run && body.get("id").is_some()).then_some(body)
}

/// Answers a successful cancel or rerun with the run as it now stands, in the shape
/// `list_workflow_runs` uses: the run the forge answered with, or else the run read back.
///
/// Forgejo answers both actions with an empty `204`, and its cancel is a no-op on a finished
/// run, so there only the run itself says what happened. The action has already succeeded if
/// the read fails, so that is a note, not an error.
async fn run_after(
    forge: &Forge,
    params: &RunRef,
    returned: Option<Value>,
    mut out: serde_json::Map<String, Value>,
) -> Result<CallToolResult, McpError> {
    let run = match returned {
        Some(run) => Ok(run),
        None => {
            forge
                .get_workflow_run(&params.owner, &params.repo, params.run_id)
                .await
        }
    };
    match run {
        Ok(run) => {
            let run = serde_json::to_value(normalize_run(&run))
                .map_err(|e| McpError::internal_error(format!("serializing the run: {e}"), None))?;
            out.insert("run".to_owned(), run);
        }
        Err(e) => {
            out.insert(
                "note".to_owned(),
                Value::String(format!(
                    "The request succeeded, but reading run {} back failed: {e}",
                    params.run_id
                )),
            );
        }
    }
    json_result(&Value::Object(out))
}

/// The error for an instance without an action's endpoint: what it lacks, which Forgejo
/// release added it, and the version the instance reports — fetched only now, on the way out.
async fn missing_endpoint(forge: &Forge, err: &ForgeError, action: &str, since: &str) -> McpError {
    let version = forge
        .server_version()
        .await
        .ok()
        .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    // The router's plain-text body ends in a newline, which would land inside the parentheses.
    let err = err.to_string();
    let err = err.trim_end();
    McpError::invalid_params(
        format!(
            "this instance has no API for {action} ({err}) — Forgejo added it in {since}; this \
             instance reports version {version}. Nothing was changed."
        ),
        None,
    )
}

/// Cancels a pending or running workflow run, on Forgejo 16+ or Gitea.
pub async fn cancel_workflow_run(
    forge: &Forge,
    params: RunRef,
) -> Result<CallToolResult, McpError> {
    let body = match forge
        .cancel_workflow_run(&params.owner, &params.repo, params.run_id)
        .await
    {
        Ok(body) => body,
        Err(e) if is_missing_endpoint(&e) => {
            return Err(missing_endpoint(forge, &e, "cancelling a run", "v16").await);
        }
        Err(e) => return Err(to_mcp(e)),
    };
    let mut out = serde_json::Map::new();
    out.insert("cancel_requested".to_owned(), Value::Bool(true));
    run_after(forge, &params, returned_run(body, true), out).await
}

/// Reruns a finished workflow run, or one job of it and the jobs that depend on it — on
/// Forgejo 17+ or Gitea.
///
/// No version check up front: Codeberg's version string names a development build, which says
/// nothing reliable about which routes it has. The request is made, and a missing route —
/// harmless, since nothing ran — is reported as such, with the version the instance gives.
pub async fn rerun_workflow(
    forge: &Forge,
    params: RerunWorkflowParams,
) -> Result<CallToolResult, McpError> {
    let body = match forge
        .rerun_workflow(&params.owner, &params.repo, params.run_id, params.job_id)
        .await
    {
        Ok(body) => body,
        Err(e) if is_missing_endpoint(&e) => {
            let action = if params.job_id.is_some() {
                "rerunning a job"
            } else {
                "rerunning a run"
            };
            return Err(missing_endpoint(forge, &e, action, "v17").await);
        }
        Err(e) => return Err(to_mcp(e)),
    };
    let mut out = serde_json::Map::new();
    out.insert("rerun_requested".to_owned(), Value::Bool(true));
    if let Some(job_id) = params.job_id {
        out.insert("job_id".to_owned(), Value::from(job_id));
    }
    let returned = returned_run(body, params.job_id.is_none());
    let run = RunRef {
        owner: params.owner,
        repo: params.repo,
        run_id: params.run_id,
    };
    run_after(forge, &run, returned, out).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tool parameters addressing item 1 of `o/r`, plus the `extra` fields given.
    fn item_params<T: serde::de::DeserializeOwned>(extra: Value) -> T {
        let mut base = serde_json::json!({ "owner": "o", "repo": "r", "index": 1 });
        if let (Value::Object(base), Value::Object(extra)) = (&mut base, extra) {
            base.extend(extra);
        }
        serde_json::from_value(base).unwrap()
    }

    fn edit_params(extra: Value) -> EditIssueParams {
        item_params(extra)
    }

    #[test]
    fn edit_issue_sends_only_the_fields_given() {
        let body = edit_issue_body(&edit_params(serde_json::json!({ "state": "closed" }))).unwrap();
        assert_eq!(
            Value::Object(body),
            serde_json::json!({ "state": "closed" })
        );

        let body = edit_issue_body(&edit_params(serde_json::json!({
            "title": "t", "assignees": [], "milestone": 0, "due_date": "2026-11-30T00:00:00Z",
        })))
        .unwrap();
        assert_eq!(
            Value::Object(body),
            serde_json::json!({
                "title": "t", "assignees": [], "milestone": 0,
                "due_date": "2026-11-30T00:00:00Z",
            })
        );
    }

    #[test]
    fn edit_issue_clears_a_due_date_with_its_own_flag() {
        let body = edit_issue_body(&edit_params(serde_json::json!({ "due_date": "" }))).unwrap();
        assert_eq!(
            Value::Object(body),
            serde_json::json!({ "unset_due_date": true })
        );
    }

    #[test]
    fn labels_the_forge_did_not_apply_are_named() {
        let requested = vec!["bug".to_owned(), "Kind/Docs".to_owned()];
        let applied = serde_json::json!([{ "id": 7, "name": "Kind/Docs", "color": "aaaaaa" }]);
        assert_eq!(labels_not_applied(&requested, &applied), ["bug"]);
        // Codeberg's answer when none matched.
        assert_eq!(
            labels_not_applied(&requested, &serde_json::json!([])),
            ["bug", "Kind/Docs"]
        );
        assert_eq!(
            labels_not_applied(&[], &serde_json::json!([])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn edit_issue_refuses_an_unknown_state() {
        assert!(edit_issue_body(&edit_params(serde_json::json!({ "state": "merged" }))).is_err());
    }

    /// Labels travel separately, so a labels-only edit has an empty PATCH body — which must not
    /// be mistaken for "nothing to change".
    #[test]
    fn a_labels_only_edit_has_no_patch_body() {
        let params = edit_params(serde_json::json!({ "labels": ["bug"] }));
        assert!(edit_issue_body(&params).unwrap().is_empty());
        assert!(params.labels.is_some());
    }

    #[tokio::test]
    async fn merge_refuses_an_unknown_style_and_a_short_head_sha_before_any_request() {
        // An unreachable instance: reaching the network would fail differently.
        let url = url::Url::parse("https://example.invalid").unwrap();
        let forge = Forge::new(&url, "t").unwrap();
        let params = item_params::<MergePullRequestParams>;
        let err = merge_pull_request(&forge, params(serde_json::json!({ "style": "octopus" })))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("style must be one of"),
            "{}",
            err.message
        );
        let err = merge_pull_request(&forge, params(serde_json::json!({ "head_sha": "1948f50" })))
            .await
            .unwrap_err();
        assert!(err.message.contains("full commit SHA"), "{}", err.message);
    }

    #[tokio::test]
    async fn file_writes_refuse_a_path_that_leaves_the_repository() {
        let url = url::Url::parse("https://example.invalid").unwrap();
        let forge = Forge::new(&url, "t").unwrap();
        let params: CreateOrUpdateFileParams = serde_json::from_value(serde_json::json!({
            "owner": "o", "repo": "r", "path": "../../../user", "content": "x",
        }))
        .unwrap();
        let err = create_or_update_file(&forge, params).await.unwrap_err();
        assert!(err.message.starts_with("path:"), "{}", err.message);
    }

    /// A `FileResponse` as Codeberg shapes it, cut down: the file's base64 body and the commit's
    /// people and tree are what the slimming drops.
    #[test]
    fn a_file_change_keeps_the_new_sha_and_the_commit() {
        let raw = serde_json::json!({
            "content": {
                "path": "docs/a.md", "sha": "b10b", "html_url": "https://x/docs/a.md",
                "content": "aGVsbG8=", "encoding": "base64", "size": 5,
            },
            "commit": {
                "sha": "c0mm17", "message": "Add docs/a.md\n", "html_url": "https://x/commit/c0mm17",
                "author": { "name": "n", "email": "e" }, "tree": { "sha": "7ree" }, "parents": [],
            },
            "verification": { "verified": false },
        });
        assert_eq!(
            file_change_result(&raw),
            serde_json::json!({
                "path": "docs/a.md", "sha": "b10b", "html_url": "https://x/docs/a.md",
                "commit": {
                    "sha": "c0mm17", "message": "Add docs/a.md\n",
                    "html_url": "https://x/commit/c0mm17",
                },
            })
        );
    }

    /// A deletion's `content` is null, so only the commit remains.
    #[test]
    fn a_file_deletion_reports_only_the_commit() {
        let raw = serde_json::json!({
            "content": null,
            "commit": { "sha": "c0mm17", "message": "Delete a.md\n" },
        });
        assert_eq!(
            file_change_result(&raw),
            serde_json::json!({ "commit": { "sha": "c0mm17", "message": "Delete a.md\n" } })
        );
    }

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
    fn releases_are_slimmed_to_their_identity_and_asset_names() {
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

        let slimmed = slim_releases(&into_items(raw));
        assert_eq!(slimmed.len(), 1);
        let release = &slimmed[0];
        assert_eq!(release.id, Some(42));
        assert_eq!(release.tag_name.as_deref(), Some("v3.3.1"));
        assert_eq!(
            release.author.as_deref(),
            Some("brechanbech"),
            "the uploader should flatten to a login, dropping the rest of the User object"
        );
        assert_eq!(release.asset_count, 1);
        assert_eq!(
            release.asset_names,
            vec!["xmcp-3.3.1-aarch64-apple-darwin.tar.gz".to_owned()]
        );

        // The serialized form is what reaches the model: no email, no tarball boilerplate.
        let json = serde_json::to_string(&slimmed).unwrap();
        assert!(!json.contains("hidden@example.com"), "author email leaked");
        assert!(!json.contains("tarball_url"), "tarball boilerplate leaked");
        // Per-asset detail is list_release_assets' job.
        assert!(
            !json.contains("browser_download_url"),
            "asset detail leaked"
        );
        assert!(!json.contains("532480"), "asset size leaked");
    }

    #[test]
    fn a_release_without_assets_serializes_an_empty_list() {
        let raw = serde_json::json!([{ "id": 1, "tag_name": "v0.1.0" }]);
        let slimmed = slim_releases(&into_items(raw));
        assert_eq!(slimmed[0].asset_count, 0);
        assert_eq!(slimmed[0].asset_names, Vec::<String>::new());
    }

    #[test]
    fn one_release_adds_its_notes_and_target_to_the_summary() {
        let raw = serde_json::json!({
            "id": 42, "tag_name": "v0.20.6", "target_commitish": "main",
            "body": "Refuse a short head_sha.",
            "upload_url": "https://codeberg.org/…/assets",
            "assets": [{ "id": 7, "name": "a.tar.gz" }, { "id": 8, "name": "a.tar.gz.sha256" }]
        });
        let json = serde_json::to_value(release_detail(&raw)).unwrap();
        assert_eq!(json["body"], "Refuse a short head_sha.");
        assert_eq!(json["target_commitish"], "main");
        // The summary is flattened in, not nested.
        assert_eq!(json["tag_name"], "v0.20.6");
        assert_eq!(json["asset_count"], 2);
        assert!(json.get("upload_url").is_none());
        assert!(json.get("summary").is_none());

        // Notes published as "" are reported as none.
        let bare = serde_json::json!({ "id": 1, "body": "" });
        assert!(
            serde_json::to_value(release_detail(&bare))
                .unwrap()
                .get("body")
                .is_none()
        );
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
            has_releases: None,
            has_actions: None,
            has_packages: None,
            has_projects: None,
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
            has_releases: None,
            has_actions: None,
            has_packages: None,
            has_projects: None,
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
    fn a_sort_is_checked_and_latest_is_not_sent() {
        assert_eq!(sort_param(None, &ISSUE_SORTS).unwrap(), None);
        assert_eq!(sort_param(Some("latest"), &PULL_SORTS).unwrap(), None);
        assert_eq!(
            sort_param(Some("oldest"), &ISSUE_SORTS).unwrap(),
            Some("oldest")
        );
        // Each listing takes its own: the issue listing has no recentclose, the pull listing
        // no due dates.
        assert!(sort_param(Some("recentclose"), &ISSUE_SORTS).is_err());
        assert!(sort_param(Some("nearduedate"), &PULL_SORTS).is_err());
        // The forges would ignore it and list newest first.
        let err = sort_param(Some("newest"), &ISSUE_SORTS).unwrap_err();
        assert!(err.message.contains("unknown sort"), "{}", err.message);
    }

    #[test]
    fn an_issue_matches_any_label_and_milestone_asked_for() {
        let issue = serde_json::json!({
            "labels": [{ "name": "Kind/Bug" }, { "name": "priority" }],
            "milestone": { "id": 7, "title": "v0.26" }
        });
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(issue_matches(&issue, &[], &[]));
        assert!(issue_matches(&issue, &names(&["kind/bug", "x"]), &[]));
        assert!(issue_matches(&issue, &[], &names(&["V0.26"])));
        assert!(issue_matches(&issue, &[], &names(&["7"])));
        assert!(!issue_matches(&issue, &names(&["docs"]), &[]));
        assert!(!issue_matches(&issue, &[], &names(&["v1.0"])));
        assert!(!issue_matches(
            &serde_json::json!({ "milestone": null }),
            &[],
            &names(&["7"])
        ));
    }

    #[test]
    fn a_dropped_label_filter_is_refused_not_returned() {
        // What both forges answered labels=no-such-label with: every issue, filter dropped.
        let items = vec![
            serde_json::json!({ "number": 1, "labels": [{ "name": "bug" }] }),
            serde_json::json!({ "number": 2, "labels": [] }),
        ];
        let labels = vec!["no-such-label".to_owned()];
        let err = check_issue_filters(&items, &labels, &[]).unwrap_err();
        assert!(
            err.message.contains("no labels named no-such-label"),
            "{}",
            err.message
        );

        // A milestone dropped the same way is named as a milestone.
        let milestones = vec!["v9".to_owned()];
        let err = check_issue_filters(&items, &[], &milestones).unwrap_err();
        assert!(
            err.message.contains("no milestones named v9"),
            "{}",
            err.message
        );

        // An answer that honoured the filter passes, and so does an empty one.
        let bug = vec!["bug".to_owned()];
        assert!(check_issue_filters(&items[..1], &bug, &[]).is_ok());
        assert!(check_issue_filters(&[], &labels, &[]).is_ok());
    }

    #[test]
    fn a_token_service_gets_the_token_alone() {
        let mut params = migrate_params("https://gitea.com/o/r.git");
        params.service = Some("gitea".to_owned());
        params.authenticate = Some(true);
        let body = migrate_repo_body(params, Some("tok")).unwrap();
        assert_eq!(body["auth_token"], "tok");
        assert!(body.get("auth_username").is_none());
        assert!(body.get("auth_password").is_none());
    }

    #[test]
    fn a_token_service_refuses_a_username() {
        // Forgejo's Gitea importer ignores the token once a username is given, and Codeberg
        // answered that with an empty 500 and a stub repository (27 October 2026).
        let mut params = migrate_params("https://gitea.com/o/r.git");
        params.service = Some("gitea".to_owned());
        params.auth_username = Some("brechanbech".to_owned());
        let err = migrate_repo_body(params, Some("tok")).unwrap_err();
        assert!(err.message.contains("authenticate=true"), "{}", err.message);
    }

    #[test]
    fn a_plain_clone_sends_the_token_as_its_password() {
        // No service means `git`, whose clone URL is built from username and password only.
        let mut params = migrate_params("https://example.com/o/r.git");
        params.authenticate = Some(true);
        let body = migrate_repo_body(params, Some("tok")).unwrap();
        assert_eq!(body["auth_username"], "oauth2");
        assert_eq!(body["auth_password"], "tok");
        assert!(body.get("auth_token").is_none());

        let mut params = migrate_params("https://example.com/o/r.git");
        params.auth_username = Some("brechanbech".to_owned());
        let body = migrate_repo_body(params, Some("tok")).unwrap();
        assert_eq!(body["auth_username"], "brechanbech");
        assert_eq!(body["auth_password"], "tok");
    }

    #[test]
    fn a_password_service_other_than_git_needs_a_username() {
        let mut params = migrate_params("https://onedev.example/o/r");
        params.service = Some("onedev".to_owned());
        params.authenticate = Some(true);
        assert!(migrate_repo_body(params, Some("tok")).is_err());

        let mut params = migrate_params("https://onedev.example/o/r");
        params.service = Some("onedev".to_owned());
        params.auth_username = Some("me".to_owned());
        let body = migrate_repo_body(params, Some("tok")).unwrap();
        assert_eq!(body["auth_username"], "me");
        assert_eq!(body["auth_password"], "tok");
    }

    #[test]
    fn a_failed_migration_names_the_repository_it_left_behind() {
        let err = ForgeError::Status {
            code: StatusCode::INTERNAL_SERVER_ERROR,
            body: "{\"message\":\"\"}\n".to_owned(),
        };
        let mcp = migrate_failure(&err, "o", "r", true, true);
        assert!(
            mcp.message.contains("source refused the request"),
            "{}",
            mcp.message
        );
        assert!(mcp.message.contains("empty o/r behind"), "{}", mcp.message);
        assert!(!mcp.message.contains('\n'), "{}", mcp.message);

        // A 422 says what went wrong itself; only the leftover is added.
        let err = ForgeError::Status {
            code: StatusCode::UNPROCESSABLE_ENTITY,
            body: "Authentication failed".to_owned(),
        };
        let mcp = migrate_failure(&err, "o", "r", true, false);
        assert!(
            !mcp.message.contains("refused the request"),
            "{}",
            mcp.message
        );
        assert!(!mcp.message.contains("behind"), "{}", mcp.message);
    }

    #[test]
    fn migrate_repo_body_fails_when_auth_is_requested_without_a_configured_token() {
        let mut params = migrate_params("https://example.com/o/r.git");
        params.auth_username = Some("brechanbech".to_owned());
        assert!(migrate_repo_body(params, None).is_err());
    }

    /// Shaped like what codeberg.org and gitea.com actually return — the two
    /// were probed and their commit objects agree field for field, so one
    /// fixture stands for both.
    fn commit_fixture(verification: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "sha": "117d025f357da221db0423cba321fc51605a4ea7",
            "html_url": "https://codeberg.org/brechanbech/xojo-mcp/commit/117d025f35",
            "commit": {
                "message": "lookup_class: return a shaped page, not the raw source\n\nA body that runs on for paragraphs and should not reach the caller.\n",
                "author": { "name": "Stefan Ractliffe", "email": "stefan@example.ch", "date": "2026-09-20T18:03:11Z" },
                "committer": { "name": "Stefan Ractliffe", "email": "stefan@example.ch", "date": "2026-09-20T18:03:11Z" },
                "tree": { "sha": "5e1", "url": "…" },
                "verification": verification
            },
            // Bulk that must not survive the slim.
            "files": [{ "filename": "src/lookup.rs" }],
            "stats": { "additions": 412, "deletions": 90, "total": 502 },
            "parents": [{ "sha": "e8e1b9f6b8" }]
        })
    }

    #[test]
    fn slim_combined_status_keeps_one_verdict_and_the_checks_behind_it() {
        // The shape both forges returned on 27 September 2026 for a repository
        // whose CI is Actions — the case an earlier ci_status tool was dropped
        // over, when it answered state:"" with total_count 0.
        let raw = serde_json::json!({
            "state": "success",
            "sha": "117d025f357da221db0423cba321fc51605a4ea7",
            "total_count": 2,
            "statuses": [
                { "context": "CI / check (push)", "status": "success", "description": "Successful in 1m2s",
                  "target_url": "/brechanbech/forgejo-mcp-rs/actions/runs/32/jobs/0" },
                { "context": "CI / deny (push)", "status": "success", "description": "Successful in 48s",
                  "target_url": "/brechanbech/forgejo-mcp-rs/actions/runs/32/jobs/1" }
            ],
            "commit_url": "https://codeberg.org/api/v1/…",
            "repository": { "full_name": "brechanbech/forgejo-mcp-rs", "owner": { "email": "x@y.z" } },
            "url": "https://codeberg.org/api/v1/…"
        });
        let v = serde_json::to_value(slim_combined_status(&raw)).unwrap();
        assert_eq!(v["state"], "success");
        assert_eq!(v["total_count"], 2);
        assert_eq!(v["statuses"][0]["context"], "CI / check (push)");
        // The repository object (with its owner's email) and the api urls go.
        assert!(v.get("repository").is_none());
        assert!(v.get("commit_url").is_none());
        assert!(v.get("url").is_none());
    }

    #[test]
    fn an_empty_combined_state_is_dropped_rather_than_reported_blank() {
        // What the endpoint used to answer for Actions repositories, and may
        // still answer for a ref nothing has reported on.
        let raw = serde_json::json!({ "state": "", "total_count": 0, "statuses": [] });
        let v = serde_json::to_value(slim_combined_status(&raw)).unwrap();
        assert!(v.get("state").is_none());
        assert_eq!(v["total_count"], 0);
        assert_eq!(v["statuses"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn slim_commit_statuses_keeps_the_check_and_its_verdict() {
        let raw = vec![serde_json::json!({
            "id": 1,
            "context": "build / test (pull_request)",
            "status": "failure",
            "description": "1 of 77 tests failed",
            "target_url": "https://codeberg.org/brechanbech/xojo-mcp/actions/runs/12",
            "updated_at": "2026-09-27T10:00:00Z",
            "created_at": "2026-09-27T09:58:00Z",
            "creator": { "login": "forgejo-actions", "email": "bot@example.com" },
            "url": "https://codeberg.org/api/v1/repos/…/statuses/1"
        })];
        let v = serde_json::to_value(&slim_commit_statuses(&raw)[0]).unwrap();
        assert_eq!(v["context"], "build / test (pull_request)");
        assert_eq!(v["status"], "failure");
        // The only route to a failing run's detail, there being no logs API.
        assert!(v["target_url"].as_str().unwrap().contains("/actions/runs/"));
        // The reporter's identity and the api url are not the question.
        assert!(v.get("creator").is_none());
        assert!(v.get("url").is_none());
        assert!(v.get("created_at").is_none());
    }

    #[test]
    fn an_empty_status_field_is_dropped_rather_than_reported_blank() {
        let raw = vec![serde_json::json!({
            "context": "", "status": "pending", "description": "", "target_url": ""
        })];
        let v = serde_json::to_value(&slim_commit_statuses(&raw)[0]).unwrap();
        assert_eq!(v["status"], "pending");
        assert!(v.get("context").is_none());
        assert!(v.get("description").is_none());
        assert!(v.get("target_url").is_none());
    }

    #[test]
    fn slim_tree_carries_the_truncated_flag() {
        // Truncation is the difference between "no such file" and "I stopped
        // looking", so it survives even when false.
        let raw = serde_json::json!({
            "sha": "abc",
            "url": "https://…",
            "tree": [
                { "path": "src/main.rs", "type": "blob", "size": 4096, "sha": "d1", "mode": "100644", "url": "https://…" },
                { "path": "src", "type": "tree", "sha": "d2", "mode": "040000", "url": "https://…" }
            ],
            "total_count": 2,
            "truncated": true,
            "page": 1
        });
        let v = serde_json::to_value(slim_tree(&raw)).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["total_count"], 2);
        assert_eq!(v["entries"][0]["path"], "src/main.rs");
        assert_eq!(v["entries"][0]["type"], "blob");
        assert_eq!(v["entries"][0]["size"], 4096);
        // A url per entry repeats the sha across what may be thousands of lines.
        assert!(v["entries"][0].get("url").is_none());
        assert!(v["entries"][0].get("mode").is_none());
        // A directory has no size and says nothing rather than zero.
        assert!(v["entries"][1].get("size").is_none());
    }

    #[test]
    fn a_tree_with_no_entries_is_empty_not_missing() {
        let v = serde_json::to_value(slim_tree(&serde_json::json!({ "sha": "abc" }))).unwrap();
        assert_eq!(v["entries"].as_array().unwrap().len(), 0);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn slim_branch_protections_keeps_the_gates_and_drops_the_allowlists() {
        let raw = vec![serde_json::json!({
            "rule_name": "main",
            "branch_name": "main",
            "require_signed_commits": true,
            "enable_push": false,
            "enable_push_whitelist": true,
            "required_approvals": 1,
            "enable_status_check": true,
            "block_on_outdated_branch": false,
            "block_on_rejected_reviews": true,
            "protected_file_patterns": "",
            // Who may bypass each gate — omitted, the question is what is enforced.
            "push_whitelist_usernames": ["brechanbech", "someone"],
            "push_whitelist_teams": ["owners"],
            "approvals_whitelist_username": ["reviewer"],
            "merge_whitelist_teams": ["maintainers"],
            "created_at": "2026-09-01T00:00:00Z",
            "updated_at": "2026-09-20T00:00:00Z"
        })];
        let v = serde_json::to_value(&slim_branch_protections(&raw)[0]).unwrap();
        assert_eq!(v["rule_name"], "main");
        assert_eq!(v["require_signed_commits"], true);
        assert_eq!(v["enable_push"], false);
        assert_eq!(v["required_approvals"], 1);
        // Every allowlist, and the timestamps, are gone.
        assert!(v.get("push_whitelist_usernames").is_none());
        assert!(v.get("push_whitelist_teams").is_none());
        assert!(v.get("approvals_whitelist_username").is_none());
        assert!(v.get("created_at").is_none());
        // An empty pattern string says nothing and is dropped rather than
        // reported as a rule protecting "".
        assert!(v.get("protected_file_patterns").is_none());
    }

    #[test]
    fn branch_protection_flags_one_forge_lacks_stay_absent() {
        // Forgejo reports apply_to_admins and not enable_force_push; Gitea the
        // reverse. Absent must not read as false on either.
        let forgejo = vec![serde_json::json!({
            "rule_name": "main", "require_signed_commits": true, "apply_to_admins": true
        })];
        let v = serde_json::to_value(&slim_branch_protections(&forgejo)[0]).unwrap();
        assert_eq!(v["apply_to_admins"], true);
        assert!(v.get("enable_force_push").is_none());

        let gitea = vec![serde_json::json!({
            "rule_name": "main", "require_signed_commits": true, "enable_force_push": false
        })];
        let v = serde_json::to_value(&slim_branch_protections(&gitea)[0]).unwrap();
        assert_eq!(v["enable_force_push"], false);
        assert!(v.get("apply_to_admins").is_none());
    }

    #[test]
    fn a_rule_named_only_by_the_legacy_field_still_reports_its_name() {
        // Older payloads carry branch_name and no rule_name.
        let raw = vec![
            serde_json::json!({ "branch_name": "release/*", "require_signed_commits": false }),
        ];
        let v = serde_json::to_value(&slim_branch_protections(&raw)[0]).unwrap();
        assert_eq!(v["rule_name"], "release/*");
    }

    #[test]
    fn slim_ssh_keys_keeps_the_verified_flag_and_drops_the_key_material() {
        // The shape /user/keys returns. `verified` is the field that decides
        // whether commits signed with this key can verify at all.
        let raw = vec![serde_json::json!({
            "id": 12345,
            "title": "MacBook Pro M1 Max",
            "fingerprint": "SHA256:j3AI4r6eQLd1dm0hTch3H0Y6Iua/1Em7un58XAoIocg",
            "key_type": "ssh",
            "verified": false,
            "read_only": false,
            "key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILongBlobNobodyReadsByEye codeberg",
            "user": { "login": "brechanbech", "email": "person@example.com" }
        })];
        let v = serde_json::to_value(&slim_ssh_keys(&raw)[0]).unwrap();
        assert_eq!(
            v["fingerprint"],
            "SHA256:j3AI4r6eQLd1dm0hTch3H0Y6Iua/1Em7un58XAoIocg"
        );
        assert_eq!(v["verified"], false);
        // A present-but-unverified key is the case this tool exists to expose.
        assert_eq!(v["title"], "MacBook Pro M1 Max");
        // The blob and the nested user (with their email) are dropped.
        assert!(v.get("key").is_none());
        assert!(v.get("user").is_none());
    }

    #[test]
    fn slim_gpg_keys_keeps_the_addresses_the_key_covers() {
        let raw = vec![serde_json::json!({
            "id": 99,
            "key_id": "ABCDEF0123456789",
            "verified": true,
            "can_sign": true,
            "expires_at": "2027-01-01T00:00:00Z",
            "emails": [
                { "email": "stefan.ractliffe@metrakol.ch", "verified": true },
                { "email": "old@example.com", "verified": false }
            ],
            "public_key": "-----BEGIN PGP PUBLIC KEY BLOCK-----\nmQINBF…\n",
            "subkeys": [{ "id": 100, "key_id": "FEDCBA9876543210" }]
        })];
        let v = serde_json::to_value(&slim_gpg_keys(&raw)[0]).unwrap();
        assert_eq!(v["key_id"], "ABCDEF0123456789");
        assert_eq!(v["can_sign"], true);
        // Which addresses are activated is half the answer to an unverified
        // commit, so the list survives.
        assert_eq!(v["emails"][0]["email"], "stefan.ractliffe@metrakol.ch");
        assert_eq!(v["emails"][0]["verified"], true);
        assert_eq!(v["emails"][1]["verified"], false);
        // The armored blob and the nested subkey copies do not.
        assert!(v.get("public_key").is_none());
        assert!(v.get("subkeys").is_none());
    }

    #[test]
    fn a_key_list_with_no_verified_flag_says_nothing_rather_than_false() {
        // The shape gitea.com returns: its PublicKey has no `verified` field
        // at all, so absent must not read as "not verified" — the key this
        // fixture is modelled on is verified on that instance.
        let raw = vec![serde_json::json!({
            "id": 1,
            "title": "someone else",
            "fingerprint": "SHA256:abc",
            "key_type": "ssh"
        })];
        let v = serde_json::to_value(&slim_ssh_keys(&raw)[0]).unwrap();
        assert!(v.get("verified").is_none());
    }

    #[test]
    fn slim_commits_drops_the_signature_blobs_and_keeps_the_verdict() {
        let raw = vec![commit_fixture(&serde_json::json!({
            "verified": true,
            "reason": "brechanbech / SHA256:j3AI4r6eQLd1dm0hTch3H0Y6Iua",
            "signature": "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH…\n-----END SSH SIGNATURE-----\n",
            "signer": { "name": "brechanbech", "email": "brechanbech@noreply.codeberg.org", "username": "" },
            "payload": "tree 5e1\nparent e8e…\n"
        }))];
        let slim = slim_commits(&raw);
        assert_eq!(slim.len(), 1);

        let v = serde_json::to_value(&slim[0]).unwrap();
        assert_eq!(v["sha"], "117d025f357da221db0423cba321fc51605a4ea7");
        // The subject only: a body can run to paragraphs.
        assert_eq!(
            v["summary"],
            "lookup_class: return a shaped page, not the raw source"
        );
        assert_eq!(v["author"], "Stefan Ractliffe");
        assert_eq!(v["signature"], "verified");
        assert_eq!(v["signer"], "brechanbech");
        // The blobs and the per-commit bulk are gone — they are the whole
        // reason this slim exists.
        assert!(v.get("payload").is_none());
        assert!(v.get("files").is_none());
        assert!(v.get("stats").is_none());
        assert!(v.get("parents").is_none());
        // A verified commit has nothing to explain.
        assert!(v.get("reason").is_none());
    }

    #[test]
    fn an_unsigned_commit_is_not_an_unverified_one() {
        // Forgejo and Gitea both answer this way for a commit nobody signed:
        // verified false, but with an empty signature.
        let raw = vec![commit_fixture(&serde_json::json!({
            "verified": false,
            "reason": "gpg.error.not_signed_commit",
            "signature": "",
            "signer": serde_json::Value::Null,
            "payload": ""
        }))];
        let v = serde_json::to_value(&slim_commits(&raw)[0]).unwrap();
        assert_eq!(v["signature"], "unsigned");
        // Neither field says anything about a commit that was never signed.
        assert!(v.get("signer").is_none());
        assert!(v.get("reason").is_none());
    }

    #[test]
    fn an_unverified_commit_keeps_the_reason_but_names_no_signer() {
        let raw = vec![commit_fixture(&serde_json::json!({
            "verified": false,
            "reason": "gpg.error.no_gpg_keys_found",
            "signature": "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH…\n-----END SSH SIGNATURE-----\n",
            "signer": serde_json::Value::Null,
            "payload": "tree 5e1\n"
        }))];
        let v = serde_json::to_value(&slim_commits(&raw)[0]).unwrap();
        assert_eq!(v["signature"], "unverified");
        assert_eq!(v["reason"], "gpg.error.no_gpg_keys_found");
        assert!(v.get("signer").is_none());
    }

    #[test]
    fn a_commit_with_no_verification_block_claims_nothing() {
        // An older instance may omit it entirely.
        let mut raw = commit_fixture(&serde_json::Value::Null);
        raw["commit"]
            .as_object_mut()
            .unwrap()
            .remove("verification");
        let v = serde_json::to_value(&slim_commits(&[raw])[0]).unwrap();
        assert!(v.get("signature").is_none());
        assert_eq!(v["sha"], "117d025f357da221db0423cba321fc51605a4ea7");
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
    fn a_repo_detail_names_its_origin_only_when_migrated() {
        // Trimmed from what Codeberg answered migrate_repo with on 27 October 2026.
        let migrated = serde_json::json!({
            "full_name": "brechanbech/migrate-check-3-git",
            "empty": false,
            "mirror": false,
            "original_url": "https://gitea.com/brechanbech/mcp-scratch.git",
            "owner": { "login": "brechanbech", "email": "brechanbech@noreply.codeberg.org" },
            "allow_squash_merge": true,
            "wiki_clone_url": "https://codeberg.org/brechanbech/migrate-check-3-git.wiki.git"
        });
        let v = serde_json::to_value(repo_detail(migrated).unwrap()).unwrap();
        assert_eq!(v["empty"], false);
        assert_eq!(
            v["original_url"],
            "https://gitea.com/brechanbech/mcp-scratch.git"
        );
        assert!(v.get("owner").is_none());
        assert!(v.get("allow_squash_merge").is_none());
        assert!(v.get("wiki_clone_url").is_none());

        // Every repository carries the field; a created one has it empty.
        let created = serde_json::json!({ "full_name": "o/r", "empty": true, "original_url": "" });
        let v = serde_json::to_value(repo_detail(created).unwrap()).unwrap();
        assert_eq!(v["empty"], true);
        assert!(v.get("original_url").is_none());
    }

    #[test]
    fn get_repo_detail_carries_the_unit_toggles() {
        // The toggles are what release_404_hint tells a caller to go and check, so a repo read
        // that dropped them would make that hint unfollowable.
        let raw = serde_json::json!({
            "full_name": "brechanbech/forgejo-mcp-rs",
            "has_issues": true,
            "has_releases": false,
            "has_actions": true,
            "owner": { "login": "brechanbech" }
        });
        let detail: RepoDetail = serde_json::from_value(raw).unwrap();
        let v = serde_json::to_value(&detail).unwrap();

        // Flattened summary fields still come through.
        assert_eq!(v["full_name"], "brechanbech/forgejo-mcp-rs");
        assert!(v.get("owner").is_none());
        // And the toggles, including the false one that is the whole point.
        assert_eq!(v["has_issues"], true);
        assert_eq!(v["has_releases"], false);
        assert_eq!(v["has_actions"], true);
        // Toggles the API did not send stay omitted rather than defaulting to false, which
        // would assert a unit is off when we simply were not told.
        assert!(v.get("has_wiki").is_none());
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
        assert_eq!(text, "");
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

    // --- workflow jobs and job logs ---

    #[test]
    fn a_forgejo_job_normalizes_without_inventing_what_it_lacks() {
        // Verbatim from Codeberg (brechanbech/forgejo-mcp-rs run 7311558): Forgejo's per-run
        // listing has no timestamps, steps or URL, and those must stay absent, not empty.
        let job = serde_json::json!({
            "id": 13_391_796, "run_id": 7_311_558, "attempt": 1,
            "handle": "3bae7b1d-18ff-43c1-8cad-53fc12498dfb", "repo_id": 2_123_516,
            "owner_id": 728_880, "name": "check", "needs": null,
            "runs_on": ["codeberg-medium"], "task_id": 9_885_636, "status": "success"
        });
        let v = serde_json::to_value(normalize_job(&job)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "id": 13_391_796, "name": "check", "status": "success", "attempt": 1,
                "runs_on": ["codeberg-medium"]
            })
        );
    }

    #[test]
    fn a_gitea_job_normalizes_onto_the_same_shape() {
        // Shaped from Gitea's published `ActionWorkflowJob` schema, and matching what gitea.com
        // returned for gitea/tea run 1118217: a `status`/`conclusion` split on steps as well.
        let job = serde_json::json!({
            "id": 42, "run_id": 7, "name": "lint", "status": "completed",
            "conclusion": "failure", "run_attempt": 2, "labels": ["ubuntu-latest"],
            "started_at": "2026-10-01T10:00:00Z", "completed_at": "2026-10-01T10:03:00Z",
            "html_url": "https://gitea.com/o/r/actions/runs/7/jobs/0",
            "runner_name": "runner-1", "url": "https://gitea.com/api/v1/…",
            "steps": [
                { "number": 1, "name": "checkout", "status": "completed", "conclusion": "success" },
                { "number": 2, "name": "make lint", "status": "completed", "conclusion": "failure" }
            ]
        });
        let s = normalize_job(&job);
        assert_eq!(
            s.status.as_deref(),
            Some("failure"),
            "conclusion is promoted"
        );
        assert_eq!(s.conclusion.as_deref(), Some("failure"));
        assert_eq!(s.attempt, Some(2));
        assert_eq!(s.runs_on, ["ubuntu-latest"]);
        assert_eq!(s.stopped.as_deref(), Some("2026-10-01T10:03:00Z"));
        assert_eq!(s.steps[1].name.as_deref(), Some("make lint"));
        assert_eq!(s.steps[1].status.as_deref(), Some("failure"));
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("runner_name").is_none() && v.get("url").is_none());
    }

    /// A stand-in for a server that honours `Range`, answering as Go's `http.ServeContent` does.
    fn serve(log: &[u8], range: &str) -> ByteRange {
        let total = log.len() as u64;
        let spec = range.strip_prefix("bytes=").unwrap();
        let (a, b) = if let Some(n) = spec.strip_prefix('-') {
            (total.saturating_sub(n.parse().unwrap()), total)
        } else {
            let (a, b) = spec.split_once('-').unwrap();
            (
                a.parse().unwrap(),
                (b.parse::<u64>().unwrap() + 1).min(total),
            )
        };
        ByteRange {
            bytes: log[as_index(a)..as_index(b)].to_vec(),
            start: a,
            total: Some(total),
        }
    }

    /// Fetches `window` from `log` through [`serve`] and returns the cut and its text.
    fn fetch(log: &[u8], window: LogWindow) -> (LogCut, String) {
        let served = serve(log, &window.range_header());
        let cut = cut_lines(&served, window);
        let text = String::from_utf8(served.bytes[cut.span.clone()].to_vec()).unwrap();
        (cut, text)
    }

    /// Ten numbered lines of nine bytes each (`line 000\n` …), so offsets are easy to reason
    /// about: line `k` occupies `9k..9k+9`.
    fn ten_lines() -> Vec<u8> {
        (0..10)
            .flat_map(|k| format!("line {k:03}\n").into_bytes())
            .collect()
    }

    #[test]
    fn the_tail_drops_a_partial_first_line() {
        let log = ten_lines();
        // 20 bytes reach back into the middle of line 7.
        let (cut, text) = fetch(&log, LogWindow::Tail(20));
        assert_eq!(text, "line 008\nline 009\n");
        assert_eq!((cut.start, cut.end), (72, 90));
    }

    #[test]
    fn the_tail_keeps_a_first_line_that_starts_exactly_on_the_window() {
        let log = ten_lines();
        // The byte before the window is a newline, so line 8 is whole and must be kept.
        let (cut, text) = fetch(&log, LogWindow::Tail(18));
        assert_eq!(text, "line 008\nline 009\n");
        assert_eq!(cut.start, 72);
    }

    #[test]
    fn a_forward_window_ends_on_a_whole_line() {
        let log = ten_lines();
        let (cut, text) = fetch(&log, LogWindow::From { offset: 9, len: 22 });
        assert_eq!(text, "line 001\nline 002\n");
        assert_eq!((cut.start, cut.end), (9, 27));
    }

    #[test]
    fn walking_back_from_the_tail_is_seamless() {
        // The paging contract: the `earlier` window a call reports ends exactly where that call
        // began, so walking back loses and repeats nothing.
        let log = ten_lines();
        let len = 25;
        let (tail, mut text) = fetch(&log, LogWindow::Tail(len));
        let mut start = tail.start;
        while start > 0 {
            let offset = start.saturating_sub(len);
            let (cut, earlier) = fetch(
                &log,
                LogWindow::From {
                    offset,
                    len: start - offset,
                },
            );
            assert_eq!(
                cut.end, start,
                "earlier window must end where the last began"
            );
            text.insert_str(0, &earlier);
            start = cut.start;
        }
        assert_eq!(text.as_bytes(), log.as_slice());
    }

    #[test]
    fn a_server_that_ignores_the_range_is_cut_locally() {
        let log = ten_lines();
        let whole = ByteRange {
            bytes: log.clone(),
            start: 0,
            total: Some(log.len() as u64),
        };
        let cut = cut_lines(&whole, LogWindow::Tail(20));
        assert_eq!(&log[cut.span], b"line 008\nline 009\n");
    }

    #[test]
    fn an_empty_log_is_an_empty_window() {
        let empty = ByteRange {
            bytes: Vec::new(),
            start: 0,
            total: Some(0),
        };
        let cut = cut_lines(&empty, LogWindow::Tail(1024));
        assert_eq!((cut.start, cut.end), (0, 0));
    }

    #[test]
    fn an_over_long_line_is_returned_rather_than_emptied() {
        let log = vec![b'x'; 100];
        let (cut, text) = fetch(&log, LogWindow::Tail(10));
        assert_eq!(text.len(), 10);
        assert_eq!(cut.end, 100);
    }

    #[test]
    fn colour_codes_are_stripped() {
        // Verbatim from a Codeberg cargo log: bold, bright green, reset.
        assert_eq!(
            strip_ansi("\x1b[1m\x1b[92m   Doc-tests\x1b[0m forgejo_mcp_rs"),
            "   Doc-tests forgejo_mcp_rs"
        );
    }

    #[test]
    fn hyperlinks_keep_their_text() {
        // OSC 8, with both terminators.
        let link = "see \x1b]8;;https://x.test/\x1b\\the docs\x1b]8;;\x07 now";
        assert_eq!(strip_ansi(link), "see the docs now");
    }

    #[test]
    fn an_unterminated_osc_costs_one_line() {
        assert_eq!(strip_ansi("a\x1b]8;;broken\nnext\n"), "a\nnext\n");
    }

    #[test]
    fn timestamps_are_stripped_only_where_they_are() {
        assert_eq!(
            strip_timestamp("2026-09-27T12:30:18.7383845Z 🏁  Job succeeded\n"),
            "🏁  Job succeeded\n"
        );
        assert_eq!(strip_timestamp("2026-09-27T12:30:14.3964960Z \n"), "\n");
        assert_eq!(strip_timestamp("plain line\n"), "plain line\n");
        assert_eq!(strip_timestamp("2026 was a year\n"), "2026 was a year\n");
    }

    #[test]
    fn clean_log_strips_both_or_keeps_timestamps_on_request() {
        let raw = "2026-09-27T12:30:14.3978135Z \x1b[1mok\x1b[0m\n2026-09-27T12:30:15.0Z done\n";
        assert_eq!(clean_log(raw, false), "ok\ndone\n");
        assert_eq!(
            clean_log(raw, true),
            "2026-09-27T12:30:14.3978135Z ok\n2026-09-27T12:30:15.0Z done\n"
        );
    }

    #[test]
    fn a_queued_forgejo_run_reports_no_epoch_timestamps() {
        // From Codeberg (this repository's run 38, seconds after the push): "never started"
        // arrives as the Unix epoch in the server's zone, not as an empty string.
        let run = serde_json::json!({
            "id": 7_959_932, "index_in_repo": 38, "status": "waiting", "workflow_id": "ci.yml",
            "created": "2026-10-23T14:43:54+02:00",
            "started": "1970-01-01T01:00:00+01:00",
            "stopped": "1970-01-01T01:00:00+01:00"
        });
        let s = normalize_run(&run);
        assert_eq!(s.created.as_deref(), Some("2026-10-23T14:43:54+02:00"));
        assert_eq!((s.started, s.stopped), (None, None));
    }

    #[test]
    fn a_dispatched_forgejo_run_reports_its_trigger_event() {
        // From Codeberg (this repository's run 40, dispatched): `event` is empty and only
        // `trigger_event` names the trigger. A push run has both.
        let run = serde_json::json!({ "id": 7_968_548, "event": "", "trigger_event": "workflow_dispatch" });
        assert_eq!(
            normalize_run(&run).event.as_deref(),
            Some("workflow_dispatch")
        );
        let gitea = serde_json::json!({ "id": 1, "event": "push" });
        assert_eq!(normalize_run(&gitea).event.as_deref(), Some("push"));
    }

    #[test]
    fn zero_times_in_any_zone_count_as_unset() {
        for never in [
            "1970-01-01T00:00:00Z",
            "1969-12-31T19:00:00-05:00",
            "0001-01-01T00:00:00Z",
            "",
        ] {
            let v = serde_json::json!({ "started_at": never });
            assert_eq!(first_time(&v, &["started_at"]), None, "{never:?}");
        }
        let v = serde_json::json!({ "started_at": "2026-10-23T00:11:04Z" });
        assert_eq!(
            first_time(&v, &["started_at"]).as_deref(),
            Some("2026-10-23T00:11:04Z")
        );
    }

    #[test]
    fn head_sha_must_be_a_whole_commit_id() {
        let sha1 = "126a915c1551d436f96071a9ba94e8a36d2f2101";
        assert_eq!(full_sha(sha1).unwrap(), sha1);
        // Uppercase would also silently miss, so it is normalized rather than refused.
        assert_eq!(full_sha(&sha1.to_ascii_uppercase()).unwrap(), sha1);
        assert!(full_sha(&"a".repeat(64)).is_ok(), "SHA-256 repositories");
        // The abbreviation that returned an empty list on Codeberg.
        let err = full_sha("126a915").unwrap_err();
        assert!(err.message.contains("full commit SHA"), "{}", err.message);
        assert!(full_sha("main").is_err());
        assert!(full_sha(&"g".repeat(40)).is_err(), "hex only");
    }

    fn status(code: u16, body: &str) -> ForgeError {
        ForgeError::Status {
            code: StatusCode::from_u16(code).unwrap(),
            body: body.to_owned(),
        }
    }

    #[test]
    fn a_plain_text_404_is_a_missing_endpoint_and_a_json_404_a_missing_run() {
        // Both bodies as Codeberg and gitea.com sent them on 24 October 2026: the router's own
        // answer for an unknown path, and the handler's for an unknown run.
        assert!(is_missing_endpoint(&status(404, "404 page not found\n")));
        assert!(!is_missing_endpoint(&status(
            404,
            r#"{"message":"run with id 999999999: resource does not exist","url":"https://codeberg.org/api/swagger"}"#
        )));
        assert!(is_missing_endpoint(&status(405, "")));
    }

    #[test]
    fn other_failures_are_not_mistaken_for_a_missing_endpoint() {
        assert!(!is_missing_endpoint(&status(
            401,
            r#"{"message":"token is required"}"#
        )));
        assert!(!is_missing_endpoint(&status(409, "")));
        assert!(!is_missing_endpoint(&status(500, "internal error")));
    }

    #[test]
    fn only_a_run_in_the_answer_stands_in_for_reading_it_back() {
        let run = serde_json::json!({ "id": 7, "status": "cancelled" });
        assert_eq!(returned_run(run.clone(), true), Some(run.clone()));
        // Forgejo's empty 204.
        assert_eq!(returned_run(Value::Null, true), None);
        // Gitea answers a job rerun with the job, whose `id` is not the run's.
        assert_eq!(returned_run(run, false), None);
    }

    #[test]
    fn a_head_filter_is_a_branch_or_owner_colon_branch() {
        assert_eq!(
            HeadFilter::parse("fix/login").unwrap(),
            HeadFilter {
                owner: None,
                branch: "fix/login"
            }
        );
        assert_eq!(
            HeadFilter::parse("khnorgaard:feat/x").unwrap(),
            HeadFilter {
                owner: Some("khnorgaard"),
                branch: "feat/x"
            }
        );
        for bad in ["", "owner:", ":branch"] {
            assert!(HeadFilter::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// Head and base of real pull requests on Codeberg, 24 October 2026: an ordinary one from a
    /// fork (#14981), and an agit-flow one (#14984) whose `ref` is not its branch.
    fn codeberg_pulls() -> (Value, Value) {
        let fork = serde_json::json!({
            "number": 14981,
            "base": { "ref": "forgejo", "label": "forgejo" },
            "head": {
                "ref": "feat/7162-non-managed-certificates",
                "label": "feat/7162-non-managed-certificates",
                "repo": { "full_name": "khnorgaard/forgejo", "owner": { "login": "khnorgaard" } }
            }
        });
        let agit = serde_json::json!({
            "number": 14984,
            "base": { "ref": "forgejo", "label": "forgejo" },
            "head": {
                "ref": "refs/pull/14984/head",
                "label": "mahlzahn/fix_package_view_templates",
                "repo": { "full_name": "forgejo/forgejo", "owner": { "login": "forgejo" } }
            }
        });
        (fork, agit)
    }

    #[test]
    fn the_head_filter_matches_a_branch_by_ref_or_label() {
        let (fork, agit) = codeberg_pulls();
        let head = |s| HeadFilter::parse(s).unwrap();
        let branch = head("feat/7162-non-managed-certificates");
        assert!(pull_matches(&fork, None, Some(&branch)));
        assert!(!pull_matches(&agit, None, Some(&branch)));
        // The agit-flow one is found by its label, as Forgejo's own filter finds it.
        let topic = head("mahlzahn/fix_package_view_templates");
        assert!(pull_matches(&agit, None, Some(&topic)));
        assert!(!pull_matches(
            &agit,
            None,
            Some(&head("fix_package_view_templates"))
        ));
    }

    #[test]
    fn owner_colon_branch_also_requires_the_fork() {
        let (fork, _) = codeberg_pulls();
        let head = |s| HeadFilter::parse(s).unwrap();
        assert!(pull_matches(
            &fork,
            None,
            Some(&head("KHNorgaard:feat/7162-non-managed-certificates"))
        ));
        assert!(!pull_matches(
            &fork,
            None,
            Some(&head("someone-else:feat/7162-non-managed-certificates"))
        ));
    }

    #[test]
    fn the_base_filter_matches_the_target_branch() {
        let (fork, _) = codeberg_pulls();
        assert!(pull_matches(&fork, Some("forgejo"), None));
        assert!(!pull_matches(&fork, Some("v16.0/forgejo"), None));
        // No filters: everything matches, so an unfiltered page is passed on untouched.
        assert!(pull_matches(&fork, None, None));
    }

    #[test]
    fn a_fork_pull_request_keeps_its_branches_and_drops_the_embedded_objects() {
        // Codeberg #14981's shape, trimmed: an ordinary pull request from a fork.
        let raw = serde_json::json!({
            "number": 14981, "title": "feat: non-managed certificates", "state": "open",
            "merged": false, "draft": false, "mergeable": true,
            "user": { "login": "khnorgaard", "email": "k@example.com" },
            "labels": [{ "name": "forgejo/ui", "color": "eb6420", "description": "User interface" }],
            "base": { "ref": "forgejo", "repo": { "full_name": "forgejo/forgejo", "stars_count": 5702 } },
            "head": {
                "ref": "feat/7162-non-managed-certificates",
                "label": "feat/7162-non-managed-certificates",
                "sha": "6df19ae47ff50b506f529b808e4c0992afe0963d",
                "repo": { "full_name": "khnorgaard/forgejo", "owner": { "login": "khnorgaard" } }
            },
            "created_at": "2026-10-23T12:33:08+02:00",
            "closed_at": null, "merged_at": null,
            "html_url": "https://codeberg.org/forgejo/forgejo/pulls/14981"
        });
        let s = slim_pull(&raw);
        assert_eq!(s.base.as_deref(), Some("forgejo"));
        assert_eq!(
            s.head.as_deref(),
            Some("feat/7162-non-managed-certificates")
        );
        assert_eq!(s.head_repo.as_deref(), Some("khnorgaard/forgejo"));
        assert_eq!(
            s.head_sha.as_deref(),
            Some("6df19ae47ff50b506f529b808e4c0992afe0963d")
        );
        assert_eq!(s.author.as_deref(), Some("khnorgaard"));
        assert_eq!(s.labels, vec!["forgejo/ui".to_owned()]);
        assert_eq!((s.closed_at.clone(), s.merged_at.clone()), (None, None));

        let json = serde_json::to_string(&s).unwrap();
        for leaked in ["k@example.com", "stars_count", "eb6420"] {
            assert!(!json.contains(leaked), "{leaked} leaked");
        }
    }

    #[test]
    fn an_agit_pull_request_reports_its_label_as_the_head_and_no_fork() {
        // Codeberg #14984: `ref` is the pull ref, the branch is in `label`, and the head
        // repository is the base repository.
        let raw = serde_json::json!({
            "number": 14984,
            "base": { "ref": "forgejo", "repo": { "full_name": "forgejo/forgejo" } },
            "head": {
                "ref": "refs/pull/14984/head",
                "label": "mahlzahn/fix_package_view_templates",
                "repo": { "full_name": "forgejo/forgejo" }
            }
        });
        let s = slim_pull(&raw);
        assert_eq!(
            s.head.as_deref(),
            Some("mahlzahn/fix_package_view_templates")
        );
        assert_eq!(s.head_repo, None);
    }

    #[test]
    fn one_pull_request_adds_its_body_reviewers_and_merge_details() {
        let raw = serde_json::json!({
            "number": 7, "state": "closed", "merged": true,
            "body": "Fixes #6.",
            "merged_by": { "login": "brechanbech", "email": "x@example.com" },
            "merge_commit_sha": "8a65a14a7150319759b5dfad587f584f30e4b2e2",
            "requested_reviewers": [{ "login": "Gusted", "email": "g@example.com" }],
            "assignees": null,
            "milestone": { "title": "v1.0", "description": "first" },
            "comments": 1, "review_comments": 0
        });
        let d = pull_detail(&raw);
        assert_eq!(d.summary.merged, Some(true));
        assert_eq!(d.body.as_deref(), Some("Fixes #6."));
        assert_eq!(d.merged_by.as_deref(), Some("brechanbech"));
        assert_eq!(d.requested_reviewers, vec!["Gusted".to_owned()]);
        assert_eq!(d.assignees, Vec::<String>::new());
        assert_eq!(d.milestone.as_deref(), Some("v1.0"));

        let json = serde_json::to_value(&d).unwrap();
        // Flattened, and the empty assignee list is left out rather than sent as [].
        assert_eq!(json["number"], 7);
        assert!(json.get("summary").is_none());
        assert!(json.get("assignees").is_none());
        assert!(
            !json.to_string().contains("@example.com"),
            "an email leaked"
        );
    }

    #[test]
    fn a_listing_without_a_state_asks_for_open_explicitly() {
        // Both forges default `…/pulls` to every state, so "open by default" has to be sent.
        assert_eq!(state_or_open(None).unwrap(), "open");
        assert_eq!(state_or_open(Some("ALL")).unwrap(), "all");
        assert!(state_or_open(Some("merged")).is_err());
    }

    /// Codeberg `forgejo/forgejo` #14991 on 24 October 2026, trimmed: an open issue with labels,
    /// no assignee or milestone, and the empty strings and nulls both forges send for "none".
    fn codeberg_issue() -> Value {
        serde_json::json!({
            "number": 14991,
            "title": "problem: I have no way of distinguishing review requests made by CODEOWNERS",
            "state": "open",
            "user": { "login": "rdfm", "email": "rdfm@noreply.codeberg.org" },
            "original_author": "",
            "labels": [
                { "name": "impact/unknown", "color": "545454" },
                { "name": "problem", "color": "fbca04" }
            ],
            "assignees": null, "milestone": null, "comments": 1,
            "body": "When a review is requested…",
            "created_at": "2026-10-23T19:22:07+02:00", "closed_at": null, "due_date": null,
            "ref": "", "is_locked": false, "pull_request": null,
            "repository": { "id": 73144, "full_name": "forgejo/forgejo" }
        })
    }

    #[test]
    fn an_issue_in_a_listing_keeps_its_standing_and_drops_the_body() {
        let s = slim_issue(&codeberg_issue());
        assert_eq!(s.number, Some(14991));
        assert_eq!(s.author.as_deref(), Some("rdfm"));
        assert_eq!(
            s.original_author, None,
            "an empty original_author is no author"
        );
        assert_eq!(
            s.labels,
            vec!["impact/unknown".to_owned(), "problem".to_owned()]
        );
        assert_eq!(s.comments, Some(1));

        let json = serde_json::to_string(&s).unwrap();
        for leaked in [
            "noreply.codeberg.org",
            "fbca04",
            "When a review",
            "repository",
        ] {
            assert!(!json.contains(leaked), "{leaked} leaked");
        }
    }

    #[test]
    fn one_issue_adds_its_body_and_says_nothing_it_does_not_have() {
        let json = serde_json::to_value(issue_detail(&codeberg_issue())).unwrap();
        assert_eq!(json["body"], "When a review is requested…");
        assert_eq!(json["number"], 14991);
        assert_eq!(json["locked"], false);
        // `ref: ""`, `due_date: null` and `pull_request: null` are all "none", and left out.
        for absent in ["ref", "due_date", "pull_request", "assignees", "milestone"] {
            assert!(json.get(absent).is_none(), "{absent} should be absent");
        }
    }

    #[test]
    fn a_pull_request_fetched_as_an_issue_says_so() {
        // Codeberg answers `…/issues/14984` for a pull request, with a `pull_request` object.
        let mut raw = codeberg_issue();
        raw["pull_request"] = serde_json::json!({
            "merged": false, "draft": false,
            "html_url": "https://codeberg.org/forgejo/forgejo/pulls/14984"
        });
        assert_eq!(issue_detail(&raw).pull_request, Some(true));
    }

    #[test]
    fn a_migrated_issue_names_its_original_author() {
        let mut raw = codeberg_issue();
        raw["original_author"] = serde_json::json!("octocat");
        assert_eq!(slim_issue(&raw).original_author.as_deref(), Some("octocat"));
    }
}
