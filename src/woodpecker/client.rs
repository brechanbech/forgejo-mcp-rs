//! The Woodpecker CI endpoint set.
//!
//! The transport — Bearer auth, JSON decoding, pagination plumbing — lives in
//! [`crate::mcp_core::RestClient`]; [`Woodpecker`] is a thin newtype over it that adds just the
//! endpoint methods. Every response is returned as raw JSON ([`serde_json::Value`]); the tool
//! layer ([`crate::woodpecker::tools`]) reshapes it.
//!
//! Woodpecker addresses repositories by their numeric `repo_id` (not `owner/name`); the
//! `lookup/{owner}/{name}` endpoint resolves a full name to that id. List endpoints return bare
//! JSON arrays with no `X-Total-Count`, and paginate with `page` / `perPage` (default 50), so
//! [`crate::mcp_core::gather_all`] detects the end by a short final page.
//!
//! Errors funnel through [`crate::mcp_core::ApiError`], re-exported here as [`WoodpeckerError`].

use crate::mcp_core::{Auth, RestClient, RestConfig};
use serde_json::Value;
use url::Url;

pub use crate::mcp_core::ApiError as WoodpeckerError;

/// Woodpecker REST API path prefix; joined onto the instance base URL.
const API_PREFIX: &str = "api/";

/// Builds the `page` / `perPage` query pairs, skipping any that are unset. (Woodpecker names the
/// page-size parameter `perPage`, unlike the `limit` that [`crate::mcp_core::paging`] emits.)
fn paging(page: Option<u32>, per_page: Option<u32>) -> Vec<(&'static str, String)> {
    let mut query = Vec::new();
    if let Some(page) = page {
        query.push(("page", page.to_string()));
    }
    if let Some(per_page) = per_page {
        query.push(("perPage", per_page.to_string()));
    }
    query
}

/// A thin Woodpecker CI REST client bound to one instance and one token.
#[derive(Debug)]
pub struct Woodpecker(RestClient);

impl Woodpecker {
    /// Builds a client for `base_url` (e.g. `https://ci.example.org`) authenticating with `token`.
    ///
    /// # Errors
    /// Fails if the base URL can't be extended with the API path, or the HTTP client can't be
    /// constructed.
    pub fn new(base_url: &Url, token: &str) -> Result<Self, WoodpeckerError> {
        let client = RestClient::new(&RestConfig {
            base_url,
            token,
            api_prefix: API_PREFIX,
            // Woodpecker authenticates with `Authorization: Bearer <token>`.
            auth: Auth::Bearer,
            user_agent: concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        })?;
        Ok(Self(client))
    }

    // --- read endpoints ---

    /// `GET /user` — the authenticated user.
    pub async fn self_user(&self) -> Result<Value, WoodpeckerError> {
        self.0.get("user", &[]).await
    }

    /// `GET /user/repos` — repositories the authenticated user has access to (paged).
    pub async fn list_repos(
        &self,
        page: Option<u32>,
        per_page: Option<u32>,
    ) -> Result<(Value, Option<usize>), WoodpeckerError> {
        self.0.get_list("user/repos", &paging(page, per_page)).await
    }

    /// `GET /repos/lookup/{owner}/{name}` — resolve a repo's full name to its record (which
    /// carries the numeric `id` the other endpoints need).
    pub async fn lookup_repo(&self, owner: &str, name: &str) -> Result<Value, WoodpeckerError> {
        self.0
            .get(&format!("repos/lookup/{owner}/{name}"), &[])
            .await
    }

    /// `GET /repos/{repo_id}` — one repository's details.
    pub async fn get_repo(&self, repo_id: i64) -> Result<Value, WoodpeckerError> {
        self.0.get(&format!("repos/{repo_id}"), &[]).await
    }

    /// `GET /repos/{repo_id}/pipelines` — pipeline (CI) runs, newest first (paged).
    pub async fn list_pipelines(
        &self,
        repo_id: i64,
        page: Option<u32>,
        per_page: Option<u32>,
    ) -> Result<(Value, Option<usize>), WoodpeckerError> {
        self.0
            .get_list(
                &format!("repos/{repo_id}/pipelines"),
                &paging(page, per_page),
            )
            .await
    }

    /// `GET /repos/{repo_id}/pipelines/{number}` — one pipeline by its per-repo number.
    pub async fn get_pipeline(&self, repo_id: i64, number: i64) -> Result<Value, WoodpeckerError> {
        self.0
            .get(&format!("repos/{repo_id}/pipelines/{number}"), &[])
            .await
    }

    // --- write endpoints (require push permission on the token) ---

    /// `POST /repos/{repo_id}/pipelines` — trigger a new pipeline. The body is a
    /// `PipelineOptions` (`{ branch?, variables? }`); returns the created pipeline.
    pub async fn create_pipeline(
        &self,
        repo_id: i64,
        body: &Value,
    ) -> Result<Value, WoodpeckerError> {
        self.0
            .post(&format!("repos/{repo_id}/pipelines"), body)
            .await
    }

    /// `POST /repos/{repo_id}/pipelines/{number}/cancel` — cancel a running pipeline.
    pub async fn cancel_pipeline(&self, repo_id: i64, number: i64) -> Result<(), WoodpeckerError> {
        self.0
            .post_empty(&format!("repos/{repo_id}/pipelines/{number}/cancel"))
            .await
    }

    /// `POST /repos/{repo_id}/pipelines/{number}` — restart (re-run) a pipeline. Takes no body
    /// and returns the newly created pipeline.
    pub async fn restart_pipeline(
        &self,
        repo_id: i64,
        number: i64,
    ) -> Result<Value, WoodpeckerError> {
        self.0
            .post_none(&format!("repos/{repo_id}/pipelines/{number}"))
            .await
    }
}
