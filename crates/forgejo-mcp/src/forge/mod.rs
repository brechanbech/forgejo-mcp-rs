//! The Forgejo / Codeberg endpoint set.
//!
//! `forgejo-mcp-rs` only touches ~20 endpoints, so rather than depend on a third-party SDK we
//! speak the documented Forgejo REST API directly. The transport — auth, JSON decoding,
//! `X-Total-Count` — lives in [`mcp_core::RestClient`]; [`Forge`] is a thin newtype over it that
//! adds just the endpoint methods. Every response is returned as raw JSON
//! ([`serde_json::Value`]); the tool layer ([`crate::tools`]) reshapes it.
//!
//! Errors funnel through [`mcp_core::ApiError`], re-exported here as [`ForgeError`] for the
//! existing call sites.

use mcp_core::{Auth, RestClient, RestConfig, paging};
use serde_json::Value;
use url::Url;

pub use mcp_core::ApiError as ForgeError;

/// Forgejo REST API path prefix; joined onto the instance base URL.
const API_PREFIX: &str = "api/v1/";

/// A thin Forgejo REST client bound to one instance and one token.
#[derive(Debug)]
pub struct Forge(RestClient);

impl Forge {
    /// Builds a client for `base_url` (e.g. `https://codeberg.org`) authenticating with `token`.
    ///
    /// # Errors
    /// Fails if the base URL can't be extended with the API path, or the HTTP client can't be
    /// constructed.
    pub fn new(base_url: &Url, token: &str) -> Result<Self, ForgeError> {
        let client = RestClient::new(&RestConfig {
            base_url,
            token,
            api_prefix: API_PREFIX,
            // Forgejo/Gitea authenticate with `Authorization: token <token>`.
            auth: Auth::Token,
            user_agent: concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        })?;
        Ok(Self(client))
    }

    // --- read endpoints ---

    /// The configured instance base URL, for display — e.g. `https://codeberg.org/`.
    #[must_use]
    pub fn base_url(&self) -> String {
        self.0.base_url()
    }

    /// `GET /version` — the Forgejo/Gitea instance software version.
    pub async fn server_version(&self) -> Result<Value, ForgeError> {
        self.0.get("version", &[]).await
    }

    /// `GET /user` — the authenticated user.
    pub async fn user_get_current(&self) -> Result<Value, ForgeError> {
        self.0.get("user", &[]).await
    }

    /// `GET /user/repos` — the authenticated user's repositories.
    pub async fn list_my_repos(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.0.get_list("user/repos", &paging(page, limit)).await
    }

    /// `GET /repos/{owner}/{repo}` — one repository's details.
    pub async fn get_repo(&self, owner: &str, repo: &str) -> Result<Value, ForgeError> {
        self.0.get(&format!("repos/{owner}/{repo}"), &[]).await
    }

    /// `GET /repos/{owner}/{repo}/branches` — branches (paged).
    pub async fn list_branches(
        &self,
        owner: &str,
        repo: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.0
            .get_list(
                &format!("repos/{owner}/{repo}/branches"),
                &paging(page, limit),
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/contents/{path}` — file or directory metadata. For a file the
    /// object carries the body as base64 in `content`; `git_ref` selects a branch/tag/commit.
    pub async fn get_contents(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: Option<&str>,
    ) -> Result<Value, ForgeError> {
        let query: Vec<(&str, String)> = git_ref
            .map(|r| vec![("ref", r.to_owned())])
            .unwrap_or_default();
        self.0
            .get(&format!("repos/{owner}/{repo}/contents/{path}"), &query)
            .await
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
        self.0
            .get_list(&format!("repos/{owner}/{repo}/issues"), &query)
            .await
    }

    /// `GET /repos/{owner}/{repo}/issues/{index}` — one issue.
    pub async fn get_issue(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
    ) -> Result<Value, ForgeError> {
        self.0
            .get(&format!("repos/{owner}/{repo}/issues/{index}"), &[])
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
        self.0
            .get_list(&format!("repos/{owner}/{repo}/pulls"), &query)
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{index}` — one pull request.
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
    ) -> Result<Value, ForgeError> {
        self.0
            .get(&format!("repos/{owner}/{repo}/pulls/{index}"), &[])
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
        self.0.get("repos/search", &q).await
    }

    /// `GET /user/orgs` — organizations the authenticated user belongs to.
    pub async fn list_orgs(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Value, ForgeError> {
        self.0.get("user/orgs", &paging(page, limit)).await
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
        self.0.get("notifications", &query).await
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
        self.0
            .get_list(
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
        self.0
            .get_list(
                &format!("repos/{owner}/{repo}/pulls/{index}/reviews"),
                &paging(page, limit),
            )
            .await
    }

    // --- write endpoints ---

    /// `POST /user/repos` — create a repository for the authenticated user.
    pub async fn create_repo(&self, body: &Value) -> Result<Value, ForgeError> {
        self.0.post("user/repos", body).await
    }

    /// `POST /repos/{owner}/{repo}/issues` — create an issue.
    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.0
            .post(&format!("repos/{owner}/{repo}/issues"), body)
            .await
    }

    /// `POST /repos/{owner}/{repo}/branches` — create a branch.
    pub async fn create_branch(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.0
            .post(&format!("repos/{owner}/{repo}/branches"), body)
            .await
    }

    /// `POST /repos/{owner}/{repo}/pulls` — open a pull request.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.0
            .post(&format!("repos/{owner}/{repo}/pulls"), body)
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
        self.0
            .post(
                &format!("repos/{owner}/{repo}/issues/{index}/comments"),
                body,
            )
            .await
    }

    /// `DELETE /repos/{owner}/{repo}` — delete a repository.
    pub async fn delete_repo(&self, owner: &str, repo: &str) -> Result<(), ForgeError> {
        self.0.delete(&format!("repos/{owner}/{repo}")).await
    }

    // --- push mirrors (repo-admin; auto-push this repo to an external remote) ---

    /// `POST /repos/{owner}/{repo}/push_mirrors` — add a push mirror. The body is a
    /// `CreatePushMirrorOption`; the response `PushMirror` never echoes the password.
    pub async fn add_push_mirror(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.0
            .post(&format!("repos/{owner}/{repo}/push_mirrors"), body)
            .await
    }

    /// `GET /repos/{owner}/{repo}/push_mirrors` — list configured push mirrors (paged).
    pub async fn list_push_mirrors(
        &self,
        owner: &str,
        repo: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.0
            .get_list(
                &format!("repos/{owner}/{repo}/push_mirrors"),
                &paging(page, limit),
            )
            .await
    }

    /// `DELETE /repos/{owner}/{repo}/push_mirrors/{name}` — remove a push mirror by remote name.
    pub async fn delete_push_mirror(
        &self,
        owner: &str,
        repo: &str,
        remote_name: &str,
    ) -> Result<(), ForgeError> {
        self.0
            .delete(&format!("repos/{owner}/{repo}/push_mirrors/{remote_name}"))
            .await
    }

    /// `POST /repos/{owner}/{repo}/push_mirrors-sync` — trigger an immediate sync of all mirrors.
    pub async fn sync_push_mirrors(&self, owner: &str, repo: &str) -> Result<(), ForgeError> {
        self.0
            .post_empty(&format!("repos/{owner}/{repo}/push_mirrors-sync"))
            .await
    }

    // --- actions (CI) ---

    /// `GET /repos/{owner}/{repo}/actions/runs` — workflow runs. Returns the raw
    /// `{ workflow_runs, total_count }` wrapper (confirmed live: no `X-Total-Count` header), so
    /// the tool layer unwraps it like search rather than auto-paginating. `filters` carries any of
    /// `head_sha`/`ref`/`status`/`event`/`workflow_id` already stringified; paging is merged in.
    pub async fn list_workflow_runs(
        &self,
        owner: &str,
        repo: &str,
        page: Option<u32>,
        limit: Option<u32>,
        filters: &[(&'static str, String)],
    ) -> Result<Value, ForgeError> {
        let mut query = paging(page, limit);
        query.extend(filters.iter().cloned());
        self.0
            .get(&format!("repos/{owner}/{repo}/actions/runs"), &query)
            .await
    }

    /// `GET /repos/{owner}/{repo}/actions/runs/{run_id}` — one workflow run.
    pub async fn get_workflow_run(
        &self,
        owner: &str,
        repo: &str,
        run_id: i64,
    ) -> Result<Value, ForgeError> {
        self.0
            .get(&format!("repos/{owner}/{repo}/actions/runs/{run_id}"), &[])
            .await
    }

    /// `POST /repos/{owner}/{repo}/actions/workflows/{filename}/dispatches` — trigger a
    /// `workflow_dispatch` run. The body is a `DispatchWorkflowOption` (`ref` required).
    pub async fn dispatch_workflow(
        &self,
        owner: &str,
        repo: &str,
        filename: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.0
            .post(
                &format!("repos/{owner}/{repo}/actions/workflows/{filename}/dispatches"),
                body,
            )
            .await
    }
}
