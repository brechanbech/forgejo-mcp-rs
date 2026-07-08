//! Result and pagination helpers shared by the tool layers.
//!
//! These convert raw API JSON and [`ApiError`]s into [`CallToolResult`]s, and drive
//! offset-pagination endpoints to completion. They are forge-agnostic: anything that returns
//! `serde_json::Value` pages and an optional `X-Total-Count` can reuse them.

use std::pin::Pin;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::error::ApiError;

/// Maps an [`ApiError`] to an MCP error. Caller-side rejections (bad token, missing resource,
/// validation — any 4xx) are `invalid_params`; transport, 5xx, and decode failures are ours
/// (`internal_error`).
// By value so it reads as a point-free `.map_err(to_mcp)`; the body only needs a borrow.
#[allow(clippy::needless_pass_by_value)]
#[must_use]
pub fn to_mcp(err: ApiError) -> McpError {
    if err.is_caller_error() {
        McpError::invalid_params(err.to_string(), None)
    } else {
        McpError::internal_error(err.to_string(), None)
    }
}

/// Deserializes a raw API [`Value`] into a local shape, mapping a mismatch to an internal error
/// (an unexpected response shape is our problem, not the caller's).
///
/// # Errors
/// `internal_error` if `value` does not match `T`.
pub fn decode<T: DeserializeOwned>(value: Value) -> Result<T, McpError> {
    serde_json::from_value(value)
        .map_err(|e| McpError::internal_error(format!("unexpected response shape: {e}"), None))
}

/// Unwraps a JSON array into its elements (anything else becomes empty).
#[must_use]
pub fn into_items(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// Serializes a value as pretty JSON in a successful tool result.
///
/// # Errors
/// `internal_error` if `value` cannot be serialized.
pub fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

/// Wraps a page of list results with pagination metadata, so the caller can tell where it is and
/// whether more remain. `total` is the full count when the endpoint reports it.
///
/// KNOWN LIMITATION (offset pagination + server-side caps). Forgejo-style APIs paginate by offset
/// (`offset = (page-1) * limit`) and clamp page size to the instance maximum — on Codeberg that
/// is 50, and when `limit` is omitted the server falls back to its default of 30. Two
/// consequences a caller must know about:
///   1. The `limit` echoed here is the *requested* value, not the *effective* page size the
///      server used. Asking for `limit=100` returns at most 50, but the response still says
///      `limit: 100`. Trust `returned` + `total` to tell whether more pages remain.
///   2. Because pages are position-based, walking them only yields a complete, duplicate-free set
///      if EVERY page uses the *same* `limit` (≤ the server max). Mixing limits across calls
///      overlaps and silently skips rows. This is an API quirk, not something we can fix here.
///
/// To sidestep this, the list tools auto-paginate by default via [`gather_all`], returning the
/// full aggregated set (with a `truncated` flag instead of a `page`/`limit` echo). Passing an
/// explicit `page` or `limit` opts back into the single-page behavior this function formats.
///
/// # Errors
/// `internal_error` if the response cannot be serialized.
pub fn paged_result<T: Serialize>(
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

/// Formats an auto-paginated set. No `page`/`limit` (the whole list is here); `truncated` is
/// `true` only if the safety cap stopped collection before the end.
///
/// # Errors
/// `internal_error` if the response cannot be serialized.
pub fn gathered_result<T: Serialize>(
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

/// Per-request page size used while auto-paginating. The server clamps to its own maximum (50 on
/// Codeberg); that is fine — [`gather_all`] discovers the *effective* size from the first page
/// rather than assuming this value was honored.
const AUTO_PAGE_SIZE: u32 = 50;

/// Safety cap: stop auto-paginating after this many items even if more remain, so a huge account
/// can't produce an unbounded response. Surfaced as `truncated` when hit.
const AUTO_PAGE_MAX_ITEMS: usize = 1000;

/// One boxed page fetch, so [`gather_all`] can call it in a loop while the future borrows the
/// client (and any request params). Yields the raw array plus `X-Total-Count`, when present.
pub type PageFetch<'a> =
    Pin<Box<dyn Future<Output = Result<(Value, Option<usize>), ApiError>> + Send + 'a>>;

/// The full result of walking every page of a list endpoint.
#[derive(Debug)]
pub struct Gathered {
    pub items: Vec<Value>,
    pub total: Option<usize>,
    pub truncated: bool,
}

/// Walks a list endpoint to completion, sidestepping the offset-pagination footgun documented on
/// [`paged_result`]: it drives every page itself with one fixed page size, so successive pages
/// can neither overlap nor skip rows. It stops when the endpoint is exhausted — via
/// `X-Total-Count` when reported, otherwise a short final page (shorter than the first page's
/// length, the effective server page size) — or when [`AUTO_PAGE_MAX_ITEMS`] is reached.
///
/// # Errors
/// Propagates any [`ApiError`] from a page fetch.
pub async fn gather_all<'a, F>(mut fetch: F) -> Result<Gathered, ApiError>
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

#[cfg(test)]
mod tests {
    use super::{AUTO_PAGE_MAX_ITEMS, ApiError, Value, gather_all};

    /// A page of `n` placeholder objects — enough for the gather loop to count.
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
            Box::pin(
                async move { Ok::<_, ApiError>((body.unwrap_or_else(|| page(0)), Some(125usize))) },
            )
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
            Box::pin(async move { Ok::<_, ApiError>((body.unwrap_or_else(|| page(0)), None)) })
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
            Box::pin(async move { Ok::<_, ApiError>((page(limit as usize), None)) })
        })
        .await
        .unwrap();
        assert!(g.truncated);
        assert!(g.items.len() >= AUTO_PAGE_MAX_ITEMS);
    }

    #[tokio::test]
    async fn gather_all_handles_empty_first_page() {
        let g =
            gather_all(|_p, _limit| Box::pin(async { Ok::<_, ApiError>((page(0), Some(0usize))) }))
                .await
                .unwrap();
        assert_eq!(g.items.len(), 0);
        assert!(!g.truncated);
    }
}
