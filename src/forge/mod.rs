//! A small, in-house Forgejo / Codeberg REST client.
//!
//! `forgejo-mcp-rs` only touches ~14 endpoints, so rather than depend on a third-party SDK we
//! speak the documented Forgejo REST API directly over [`reqwest`]. Every response is returned
//! as raw JSON ([`serde_json::Value`]); the tool layer ([`crate::tools`]) reshapes it. The
//! token is held in a [`Zeroizing`] string and wiped on drop, and the `Authorization` header
//! is marked sensitive so it never lands in logs.
//!
//! Errors funnel through [`ForgeError`], which carries enough to tell a caller-side 4xx from
//! an internal transport/5xx/decode failure.

mod error;

pub use error::ForgeError;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use reqwest::{Client, Method};
use serde_json::Value;
use url::Url;
use zeroize::Zeroizing;

/// Forgejo REST API path prefix; joined onto the instance base URL.
const API_PREFIX: &str = "api/v1/";
/// Header Forgejo sets on list endpoints with the full (unpaginated) item count.
const TOTAL_COUNT: &str = "x-total-count";

/// A thin Forgejo REST client bound to one instance and one token.
pub struct Forge {
    http: Client,
    /// `{base}/api/v1/` — every request path is joined onto this.
    api_root: Url,
    /// API token, zeroized on drop. Sent as `Authorization: token <token>`.
    token: Zeroizing<String>,
}

impl std::fmt::Debug for Forge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the token.
        f.debug_struct("Forge")
            .field("api_root", &self.api_root.as_str())
            .finish_non_exhaustive()
    }
}

/// Builds the `page` / `limit` query pairs, skipping any that are unset.
fn paging(page: Option<u32>, limit: Option<u32>) -> Vec<(&'static str, String)> {
    let mut query = Vec::new();
    if let Some(page) = page {
        query.push(("page", page.to_string()));
    }
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    query
}

impl Forge {
    /// Builds a client for `base_url` (e.g. `https://codeberg.org`) authenticating with `token`.
    ///
    /// # Errors
    /// Fails if the base URL can't be extended with the API path, or the HTTP client can't be
    /// constructed.
    pub fn new(base_url: &Url, token: &str) -> Result<Self, ForgeError> {
        // Ensure a trailing slash so `join` appends `api/v1/` rather than replacing the last
        // path segment (matters for instances hosted under a sub-path).
        let mut base = base_url.clone();
        if !base.path().ends_with('/') {
            let with_slash = format!("{}/", base.path());
            base.set_path(&with_slash);
        }
        let api_root = base
            .join(API_PREFIX)
            .map_err(|e| ForgeError::Config(format!("invalid base URL {base_url}: {e}")))?;
        let http = Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| ForgeError::Config(format!("building the HTTP client: {e}")))?;
        Ok(Self {
            http,
            api_root,
            token: Zeroizing::new(token.to_owned()),
        })
    }

    /// Performs one request and returns `(parsed body, total-count header)`. The body is
    /// `None` only when the response is empty (e.g. a `204` from a delete).
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(Option<Value>, Option<usize>), ForgeError> {
        let url = self
            .api_root
            .join(path)
            .map_err(|e| ForgeError::Config(format!("invalid request path {path}: {e}")))?;

        // Build the auth header per request and mark it sensitive so reqwest keeps it out of
        // any debug output. The persistent copy lives in `self.token` and is zeroized on drop.
        let mut auth = HeaderValue::from_str(&format!("token {}", self.token.as_str()))
            .map_err(|e| ForgeError::Config(format!("invalid token: {e}")))?;
        auth.set_sensitive(true);

        let mut req = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, auth)
            .header(ACCEPT, "application/json");
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(body) = body {
            req = req.json(body);
        }

        let resp = req.send().await.map_err(ForgeError::Transport)?;
        let status = resp.status();
        let total = resp
            .headers()
            .get(TOTAL_COUNT)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok());
        let bytes = resp.bytes().await.map_err(ForgeError::Transport)?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            return Err(ForgeError::Status { code: status, body });
        }
        let value = if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&bytes).map_err(ForgeError::Decode)?)
        };
        Ok((value, total))
    }

    /// `GET` returning a single JSON value (object or array). Empty bodies become `Null`.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ForgeError> {
        Ok(self
            .request(Method::GET, path, query, None)
            .await?
            .0
            .unwrap_or(Value::Null))
    }

    /// `GET` of a list endpoint: the JSON array plus its `X-Total-Count`, when reported.
    async fn get_list(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<(Value, Option<usize>), ForgeError> {
        let (value, total) = self.request(Method::GET, path, query, None).await?;
        Ok((value.unwrap_or_else(|| Value::Array(Vec::new())), total))
    }

    /// `POST` of a JSON body returning the created resource.
    async fn post(&self, path: &str, body: &Value) -> Result<Value, ForgeError> {
        Ok(self
            .request(Method::POST, path, &[], Some(body))
            .await?
            .0
            .unwrap_or(Value::Null))
    }

    /// `DELETE`, discarding any (typically empty) body.
    async fn delete(&self, path: &str) -> Result<(), ForgeError> {
        self.request(Method::DELETE, path, &[], None).await?;
        Ok(())
    }

    // --- read endpoints ---

    /// The configured instance base URL (the API root with the `api/v1/` suffix trimmed),
    /// for display — e.g. `https://codeberg.org/`.
    pub fn base_url(&self) -> String {
        self.api_root
            .as_str()
            .strip_suffix(API_PREFIX)
            .unwrap_or_else(|| self.api_root.as_str())
            .to_owned()
    }

    /// `GET /version` — the Forgejo/Gitea instance software version.
    pub async fn server_version(&self) -> Result<Value, ForgeError> {
        self.get("version", &[]).await
    }

    /// `GET /user` — the authenticated user.
    pub async fn user_get_current(&self) -> Result<Value, ForgeError> {
        self.get("user", &[]).await
    }

    /// `GET /user/repos` — the authenticated user's repositories.
    pub async fn list_my_repos(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.get_list("user/repos", &paging(page, limit)).await
    }

    /// `GET /repos/{owner}/{repo}/issues` — issues, optionally filtered by `state`.
    pub async fn list_issues(
        &self,
        owner: &str,
        repo: &str,
        state: Option<&str>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        let mut query = paging(page, limit);
        if let Some(state) = state {
            query.push(("state", state.to_owned()));
        }
        self.get_list(&format!("repos/{owner}/{repo}/issues"), &query)
            .await
    }

    /// `GET /repos/{owner}/{repo}/issues/{index}` — one issue.
    pub async fn get_issue(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
    ) -> Result<Value, ForgeError> {
        self.get(&format!("repos/{owner}/{repo}/issues/{index}"), &[])
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls` — pull requests, optionally filtered by `state`.
    pub async fn list_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        state: Option<&str>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        let mut query = paging(page, limit);
        if let Some(state) = state {
            query.push(("state", state.to_owned()));
        }
        self.get_list(&format!("repos/{owner}/{repo}/pulls"), &query)
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{index}` — one pull request.
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
    ) -> Result<Value, ForgeError> {
        self.get(&format!("repos/{owner}/{repo}/pulls/{index}"), &[])
            .await
    }

    /// `GET /repos/search` — repository search. Returns `{ ok, data }`.
    pub async fn search_repos(
        &self,
        query: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Value, ForgeError> {
        let mut q = paging(page, limit);
        q.push(("q", query.to_owned()));
        self.get("repos/search", &q).await
    }

    /// `GET /user/orgs` — organizations the authenticated user belongs to.
    pub async fn list_orgs(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Value, ForgeError> {
        self.get("user/orgs", &paging(page, limit)).await
    }

    /// `GET /notifications` — the user's notification threads (unread unless `all`).
    pub async fn list_notifications(
        &self,
        all: Option<bool>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Value, ForgeError> {
        let mut query = paging(page, limit);
        if let Some(all) = all {
            query.push(("all", all.to_string()));
        }
        self.get("notifications", &query).await
    }

    /// `GET /repos/{owner}/{repo}/issues/{index}/comments` — comments on an issue or PR.
    pub async fn list_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.get_list(
            &format!("repos/{owner}/{repo}/issues/{index}/comments"),
            &paging(page, limit),
        )
        .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{index}/reviews` — reviews on a pull request.
    pub async fn list_pull_request_reviews(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.get_list(
            &format!("repos/{owner}/{repo}/pulls/{index}/reviews"),
            &paging(page, limit),
        )
        .await
    }

    // --- write endpoints ---

    /// `POST /user/repos` — create a repository for the authenticated user.
    pub async fn create_repo(&self, body: &Value) -> Result<Value, ForgeError> {
        self.post("user/repos", body).await
    }

    /// `POST /repos/{owner}/{repo}/issues` — create an issue.
    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.post(&format!("repos/{owner}/{repo}/issues"), body)
            .await
    }

    /// `POST /repos/{owner}/{repo}/pulls` — open a pull request.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.post(&format!("repos/{owner}/{repo}/pulls"), body)
            .await
    }

    /// `POST /repos/{owner}/{repo}/issues/{index}/comments` — comment on an issue or PR.
    pub async fn comment_on_issue(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.post(
            &format!("repos/{owner}/{repo}/issues/{index}/comments"),
            body,
        )
        .await
    }

    /// `DELETE /repos/{owner}/{repo}` — delete a repository.
    pub async fn delete_repo(&self, owner: &str, repo: &str) -> Result<(), ForgeError> {
        self.delete(&format!("repos/{owner}/{repo}")).await
    }
}
