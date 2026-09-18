//! A small, in-house REST client for the Forgejo API.
//!
//! The server only touches a handful of endpoints, so rather than depend on a third-party SDK
//! we speak the documented REST API directly over [`reqwest`]. Every response is returned as raw
//! JSON ([`serde_json::Value`]); the tool layer reshapes it. The token is held in a
//! [`Zeroizing`] string and wiped on drop, and the `Authorization` header is marked sensitive so
//! it never lands in logs.

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use reqwest::{Client, Method, RequestBuilder, multipart};
use serde_json::Value;
use url::Url;
use zeroize::Zeroizing;

use super::error::ApiError;

/// Header the list endpoints set with the full (unpaginated) item count. Absent when an
/// instance doesn't report it — [`super::gather_all`] then detects the end by a short final
/// page instead.
const TOTAL_COUNT: &str = "x-total-count";

/// Everything needed to construct a [`RestClient`]. A plain config struct (all fields required)
/// rather than a four-argument `new`.
#[derive(Debug)]
pub struct RestConfig<'a> {
    /// Instance base URL, e.g. `https://codeberg.org`.
    pub base_url: &'a Url,
    /// API token for this client.
    pub token: &'a str,
    /// API path prefix joined onto the base URL, e.g. `api/v1/`. Include the trailing
    /// slash.
    pub api_prefix: &'a str,
    /// `User-Agent` sent on every request, e.g. `forgejo-mcp-rs/0.12.0`.
    pub user_agent: &'a str,
}

/// A thin REST client bound to one instance and one token.
pub struct RestClient {
    http: Client,
    /// `{base}/{api_prefix}` — every request path is joined onto this.
    api_root: Url,
    /// The prefix, kept so [`RestClient::base_url`] can strip it back off for display.
    api_prefix: String,
    /// API token, zeroized on drop.
    token: Zeroizing<String>,
}

impl std::fmt::Debug for RestClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the token.
        f.debug_struct("RestClient")
            .field("api_root", &self.api_root.as_str())
            .finish_non_exhaustive()
    }
}

/// Builds the `page` / `limit` query pairs, skipping any that are unset.
#[must_use]
pub fn paging(page: Option<u32>, limit: Option<u32>) -> Vec<(&'static str, String)> {
    let mut query = Vec::new();
    if let Some(page) = page {
        query.push(("page", page.to_string()));
    }
    if let Some(limit) = limit {
        query.push(("limit", limit.to_string()));
    }
    query
}

impl RestClient {
    /// Builds a client from a [`RestConfig`].
    ///
    /// # Errors
    /// Fails if the base URL can't be extended with the API path, or the HTTP client can't be
    /// constructed.
    pub fn new(cfg: &RestConfig<'_>) -> Result<Self, ApiError> {
        // Ensure a trailing slash so `join` appends the API prefix rather than replacing the last
        // path segment (matters for instances hosted under a sub-path).
        let mut base = cfg.base_url.clone();
        if !base.path().ends_with('/') {
            let with_slash = format!("{}/", base.path());
            base.set_path(&with_slash);
        }
        let api_root = base
            .join(cfg.api_prefix)
            .map_err(|e| ApiError::Config(format!("invalid base URL {}: {e}", cfg.base_url)))?;
        let http = Client::builder()
            .user_agent(cfg.user_agent.to_owned())
            .build()
            .map_err(|e| ApiError::Config(format!("building the HTTP client: {e}")))?;
        Ok(Self {
            http,
            api_root,
            api_prefix: cfg.api_prefix.to_owned(),
            token: Zeroizing::new(cfg.token.to_owned()),
        })
    }

    /// Builds a request carrying everything common to every call: the joined URL, the sensitive
    /// auth header, the `Accept` header, and any query string.
    ///
    /// Split out from [`RestClient::send`] so a multipart upload gets identical auth treatment
    /// without restating it — the token handling is the part that must not drift.
    fn begin(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        accept: &'static str,
    ) -> Result<RequestBuilder, ApiError> {
        let url = self
            .api_root
            .join(path)
            .map_err(|e| ApiError::Config(format!("invalid request path {path}: {e}")))?;

        // Build the auth header per request and mark it sensitive so reqwest keeps it out of any
        // debug output. The persistent copy lives in `self.token` and is zeroized on drop.
        let mut auth = HeaderValue::from_str(&format!("token {}", self.token.as_str()))
            .map_err(|e| ApiError::Config(format!("invalid token: {e}")))?;
        auth.set_sensitive(true);

        let mut req = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, auth)
            .header(ACCEPT, accept);
        if !query.is_empty() {
            req = req.query(query);
        }
        Ok(req)
    }

    /// Sends a prepared request and returns `(status-checked body bytes, total-count header)`.
    async fn finish(req: RequestBuilder) -> Result<(Vec<u8>, Option<usize>), ApiError> {
        let resp = req.send().await.map_err(ApiError::Transport)?;
        let status = resp.status();
        let total = resp
            .headers()
            .get(TOTAL_COUNT)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok());
        let bytes = resp.bytes().await.map_err(ApiError::Transport)?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            return Err(ApiError::Status { code: status, body });
        }
        Ok((bytes.to_vec(), total))
    }

    /// Performs one request and returns `(status-checked body bytes, total-count header)`.
    ///
    /// `accept` is the `Accept` header to send — JSON for the API proper, `text/plain` for the
    /// endpoints that serve a raw document (e.g. a pull request's `.diff`).
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        accept: &'static str,
    ) -> Result<(Vec<u8>, Option<usize>), ApiError> {
        let mut req = self.begin(method, path, query, accept)?;
        if let Some(body) = body {
            req = req.json(body);
        }
        Self::finish(req).await
    }

    /// Performs one JSON request and returns `(parsed body, total-count header)`. The body is
    /// `None` only when the response is empty (e.g. a `204` from a delete).
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(Option<Value>, Option<usize>), ApiError> {
        let (bytes, total) = self
            .send(method, path, query, body, "application/json")
            .await?;
        let value = if bytes.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&bytes).map_err(ApiError::Decode)?)
        };
        Ok((value, total))
    }
    /// `GET` returning a single JSON value (object or array). Empty bodies become `Null`.
    ///
    /// # Errors
    /// Propagates transport, non-2xx ([`ApiError::Status`]), and decode failures.
    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        Ok(self
            .request(Method::GET, path, query, None)
            .await?
            .0
            .unwrap_or(Value::Null))
    }

    /// `GET` of a list endpoint: the JSON array plus its `X-Total-Count`, when reported.
    ///
    /// # Errors
    /// Propagates transport, non-2xx ([`ApiError::Status`]), and decode failures.
    pub async fn get_list(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<(Value, Option<usize>), ApiError> {
        let (value, total) = self.request(Method::GET, path, query, None).await?;
        Ok((value.unwrap_or_else(|| Value::Array(Vec::new())), total))
    }

    /// `GET` of an endpoint that serves a raw text document rather than JSON — Forgejo's
    /// `.diff` / `.patch` pull-request views. Returns the body decoded as UTF-8.
    ///
    /// # Errors
    /// Propagates transport and non-2xx ([`ApiError::Status`]) failures, and reports a body that
    /// isn't valid UTF-8 as [`ApiError::Config`].
    pub async fn get_text(&self, path: &str, query: &[(&str, String)]) -> Result<String, ApiError> {
        let (bytes, _) = self
            .send(Method::GET, path, query, None, "text/plain")
            .await?;
        String::from_utf8(bytes)
            .map_err(|e| ApiError::Config(format!("response was not valid UTF-8: {e}")))
    }

    /// `POST` of a JSON body returning the created resource.
    ///
    /// # Errors
    /// Propagates transport, non-2xx ([`ApiError::Status`]), and decode failures.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        Ok(self
            .request(Method::POST, path, &[], Some(body))
            .await?
            .0
            .unwrap_or(Value::Null))
    }

    /// `PATCH` of a JSON body returning the updated resource. Only the fields present in
    /// `body` change server-side, so callers send exactly what they mean to edit.
    ///
    /// # Errors
    /// Propagates transport, non-2xx ([`ApiError::Status`]), and decode failures.
    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        Ok(self
            .request(Method::PATCH, path, &[], Some(body))
            .await?
            .0
            .unwrap_or(Value::Null))
    }

    /// `POST` of a `multipart/form-data` body carrying one file part, returning the created
    /// resource.
    ///
    /// The release-asset upload is the only endpoint on this surface that is not JSON going in,
    /// so this is deliberately narrow: exactly one part, named by `field`, sent as opaque bytes.
    /// The caller has already read and size-checked the file — nothing here touches the disk.
    ///
    /// # Errors
    /// Propagates transport, non-2xx ([`ApiError::Status`]), and decode failures, and reports an
    /// unusable filename as [`ApiError::Config`].
    pub async fn post_multipart(
        &self,
        path: &str,
        query: &[(&str, String)],
        field: &'static str,
        file_name: String,
        bytes: Vec<u8>,
    ) -> Result<Value, ApiError> {
        let part = multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str("application/octet-stream")
            .map_err(|e| ApiError::Config(format!("building the upload part: {e}")))?;
        let req = self
            .begin(Method::POST, path, query, "application/json")?
            .multipart(multipart::Form::new().part(field, part));

        let (bytes, _) = Self::finish(req).await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(ApiError::Decode)
    }

    /// `POST` with no body, discarding any (typically empty) response — for sync-style endpoints.
    ///
    /// # Errors
    /// Propagates transport and non-2xx ([`ApiError::Status`]) failures.
    pub async fn post_empty(&self, path: &str) -> Result<(), ApiError> {
        self.request(Method::POST, path, &[], None).await?;
        Ok(())
    }

    /// `DELETE`, discarding any (typically empty) body.
    ///
    /// # Errors
    /// Propagates transport and non-2xx ([`ApiError::Status`]) failures.
    pub async fn delete(&self, path: &str) -> Result<(), ApiError> {
        self.request(Method::DELETE, path, &[], None).await?;
        Ok(())
    }

    /// The configured instance base URL (the API root with the API prefix trimmed), for display
    /// — e.g. `https://codeberg.org/`.
    #[must_use]
    pub fn base_url(&self) -> String {
        self.api_root
            .as_str()
            .strip_suffix(&self.api_prefix)
            .unwrap_or_else(|| self.api_root.as_str())
            .to_owned()
    }
}
