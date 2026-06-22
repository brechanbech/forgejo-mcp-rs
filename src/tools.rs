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

use std::pin::Pin;

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
///
/// KNOWN LIMITATION (offset pagination + server-side caps). Forgejo paginates by offset
/// (`offset = (page-1) * limit`) and clamps page size to the instance maximum — on Codeberg
/// that is 50, and when `limit` is omitted the server falls back to its default of 30. Two
/// consequences a caller must know about:
///   1. The `limit` echoed here is the *requested* value, not the *effective* page size the
///      server used. Asking for `limit=100` returns at most 50, but the response still says
///      `limit: 100`. Trust `returned` + `total` to tell whether more pages remain.
///   2. Because pages are position-based, walking them only yields a complete, duplicate-free
///      set if EVERY page uses the *same* `limit` (≤ the server max). Mixing limits across
///      calls — e.g. `limit=100` on page 1 (→50 rows) then an omitted limit on page 2 (→30
///      rows at offset 30) — overlaps rows 30–49 and silently skips everything past the
///      shorter walk. This is a Forgejo API quirk, not something we can fix in the request.
///
/// To sidestep this, the list tools auto-paginate by default: called with no `page` and no
/// `limit`, they walk every page via [`gather_all`] and return the full aggregated set (with a
/// `truncated` flag instead of a `page`/`limit` echo). Passing an explicit `page` or `limit`
/// opts back into the single-page behavior this function formats, offsets and all.
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

/// Per-request page size used while auto-paginating. The server clamps to its own maximum
/// (50 on Codeberg); that is fine — [`gather_all`] discovers the *effective* size from the
/// first page rather than assuming this value was honored.
const AUTO_PAGE_SIZE: u32 = 50;

/// Safety cap: stop auto-paginating after this many items even if more remain, so a huge
/// account can't produce an unbounded response. Surfaced as `truncated` when hit.
const AUTO_PAGE_MAX_ITEMS: usize = 1000;

/// One boxed page fetch, so [`gather_all`] can call it in a loop while the future borrows the
/// [`Forge`] (and any request params). Yields the raw array plus `X-Total-Count`, when present.
type PageFetch<'a> =
    Pin<Box<dyn Future<Output = Result<(Value, Option<usize>), ForgeError>> + Send + 'a>>;

/// The full result of walking every page of a list endpoint.
struct Gathered {
    items: Vec<Value>,
    total: Option<usize>,
    truncated: bool,
}

/// Walks a list endpoint to completion, sidestepping the offset-pagination footgun documented
/// on [`paged_result`]: it drives every page itself with one fixed page size, so successive
/// pages can neither overlap nor skip rows. It stops when the endpoint is exhausted — via
/// `X-Total-Count` when reported, otherwise a short final page (shorter than the first page's
/// length, the effective server page size) — or when [`AUTO_PAGE_MAX_ITEMS`] is reached.
async fn gather_all<'a, F>(mut fetch: F) -> Result<Gathered, ForgeError>
where
    F: FnMut(u32, u32) -> PageFetch<'a>,
{
    let mut items: Vec<Value> = Vec::new();
    let mut total: Option<usize> = None;
    let mut effective: Option<usize> = None;
    let mut truncated = false;
    let mut page: u32 = 1;

    loop {
        let (value, page_total) = fetch(page, AUTO_PAGE_SIZE).await?;
        if page_total.is_some() {
            total = page_total;
        }
        let batch = into_items(value);
        let got = batch.len();
        // The first page reveals the server's effective page size (it may clamp below our
        // request); later short pages are how we detect the end when there's no total.
        let eff = *effective.get_or_insert(got);
        items.extend(batch);

        if got == 0 {
            break; // server has no more rows
        }
        if let Some(t) = total
            && items.len() >= t
        {
            break; // collected everything the count promised
        }
        if got < eff {
            break; // a short page — this was the last one
        }
        if items.len() >= AUTO_PAGE_MAX_ITEMS {
            truncated = true;
            break;
        }
        page = page.saturating_add(1);
    }

    Ok(Gathered {
        items,
        total,
        truncated,
    })
}

/// Formats an auto-paginated set. No `page`/`limit` (the whole list is here); `truncated` is
/// `true` only if the safety cap stopped collection before the end.
fn gathered_result<T: Serialize>(
    items: &[T],
    total: Option<usize>,
    truncated: bool,
) -> Result<CallToolResult, McpError> {
    json_result(&serde_json::json!({
        "returned": items.len(),
        "total": total,
        "truncated": truncated,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of `n` placeholder repo objects — enough for the gather loop to count.
    fn page(n: usize) -> Value {
        Value::Array(
            (0..n)
                .map(|i| serde_json::json!({ "full_name": format!("o/r{i}") }))
                .collect(),
        )
    }

    #[tokio::test]
    async fn gather_all_stops_at_reported_total() {
        // 50 + 50 + 25 = 125, with the total reported on every page.
        let pages = [page(50), page(50), page(25)];
        let g = gather_all(|p, _limit| {
            let body = pages.get(p as usize - 1).cloned();
            Box::pin(async move {
                Ok::<_, ForgeError>((body.unwrap_or_else(|| page(0)), Some(125usize)))
            })
        })
        .await
        .unwrap();
        assert_eq!(g.items.len(), 125);
        assert_eq!(g.total, Some(125));
        assert!(!g.truncated);
    }

    #[tokio::test]
    async fn gather_all_survives_server_clamp_without_total() {
        // No total reported, and the server clamps to 30/page (below our 50 request). The loop
        // must NOT stop after page 1 just because 30 < the requested 50 — it compares against
        // the effective first-page size and ends only on the genuinely short final page.
        let pages = [page(30), page(30), page(10)];
        let g = gather_all(|p, _limit| {
            let body = pages.get(p as usize - 1).cloned();
            Box::pin(async move { Ok::<_, ForgeError>((body.unwrap_or_else(|| page(0)), None)) })
        })
        .await
        .unwrap();
        assert_eq!(g.items.len(), 70);
        assert_eq!(g.total, None);
        assert!(!g.truncated);
    }

    #[tokio::test]
    async fn gather_all_truncates_at_safety_cap() {
        // Always a full page, never a total: only AUTO_PAGE_MAX_ITEMS stops the walk.
        let g = gather_all(|_p, limit| {
            Box::pin(async move { Ok::<_, ForgeError>((page(limit as usize), None)) })
        })
        .await
        .unwrap();
        assert!(g.truncated);
        assert!(g.items.len() >= AUTO_PAGE_MAX_ITEMS);
    }

    #[tokio::test]
    async fn gather_all_handles_empty_first_page() {
        let g = gather_all(|_p, _limit| {
            Box::pin(async { Ok::<_, ForgeError>((page(0), Some(0usize))) })
        })
        .await
        .unwrap();
        assert_eq!(g.items.len(), 0);
        assert!(!g.truncated);
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
        // Nested objects (and the owner's email) are dropped.
        assert!(v.get("owner").is_none());
        assert!(v.get("internal_tracker").is_none());
        assert!(v.get("permissions").is_none());
        // Absent optional fields are omitted, not serialized as null.
        assert!(v.get("private").is_none());
    }
}
