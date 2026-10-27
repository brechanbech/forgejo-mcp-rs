//! The Forgejo / Codeberg endpoint set.
//!
//! `forgejo-mcp-rs` only touches ~20 endpoints, so rather than depend on a third-party SDK we
//! speak the documented Forgejo REST API directly. The transport — auth, JSON decoding,
//! `X-Total-Count` — lives in [`crate::mcp_core::RestClient`]; [`Forge`] is a thin newtype over it that
//! adds just the endpoint methods. Every response is returned as raw JSON
//! ([`serde_json::Value`]); the tool layer ([`crate::forgejo::tools`]) reshapes it.
//!
//! Errors funnel through [`crate::mcp_core::ApiError`], re-exported here as [`ForgeError`] for the
//! existing call sites.

use crate::mcp_core::{ByteRange, RestClient, RestConfig, paging};
use serde_json::Value;
use std::sync::OnceLock;
use url::Url;

pub use crate::mcp_core::ApiError as ForgeError;

/// Forgejo REST API path prefix; joined onto the instance base URL. Gitea serves the same
/// prefix — the two APIs are the same surface except where [`Flavor`] says otherwise.
const API_PREFIX: &str = "api/v1/";

/// Which of the two compatible forges we're talking to.
///
/// Forgejo began as a Gitea fork and the REST surface is still almost entirely shared: of the
/// ~25 endpoints this server calls, only the Actions (CI) ones differ. Everything else —
/// issues, pull requests, diffs, branches, contents, search, orgs, notifications, push
/// mirrors, migration — is identical in path, method and response shape on both. So the flavor
/// is consulted only by the Actions calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// Forgejo (and therefore Codeberg).
    Forgejo,
    /// Gitea.
    Gitea,
}

impl Flavor {
    /// Parses the `FORGEJO_FLAVOR` override. `auto` (or anything empty) means "detect".
    ///
    /// # Errors
    /// Returns the offending value if it is neither `forgejo`, `gitea` nor `auto`.
    pub fn parse_override(raw: &str) -> Result<Option<Self>, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(None),
            "forgejo" | "codeberg" => Ok(Some(Self::Forgejo)),
            "gitea" => Ok(Some(Self::Gitea)),
            other => Err(other.to_owned()),
        }
    }

    /// Classifies an instance from the `version` string `GET /version` returns.
    ///
    /// One signal: Forgejo advertises its Gitea API compatibility in its own version string —
    /// Codeberg reports `16.0.0-dev-785-465b60ef+gitea-1.22.0`, and older releases
    /// `7.0.4+0-gitea-1.22.0`. Gitea never names itself in its version
    /// (`29.0.0+dev-160-g55186ca39b`), so the `gitea-` marker means Forgejo, counter-intuitive
    /// as that reads. Anything without it is Gitea.
    ///
    /// The major version is no signal any more. Until v0.20.6 a major of 2 or more also meant
    /// Forgejo, because Gitea was still on 1.x; gitea.com then started reporting 29.0.0 and was
    /// taken for Forgejo. A Forgejo that dropped the marker would now be taken for Gitea —
    /// `FORGEJO_FLAVOR` exists for that.
    #[must_use]
    pub fn classify(version: &str) -> Self {
        if version.to_ascii_lowercase().contains("gitea-") {
            Self::Forgejo
        } else {
            Self::Gitea
        }
    }

    /// The lowercase name, for logs and the `version` tool.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forgejo => "forgejo",
            Self::Gitea => "gitea",
        }
    }
}

/// The workflow-run filters a caller can ask for, before they're translated to whichever query
/// parameters this instance's flavor actually understands.
///
/// Kept as typed options rather than pre-stringified pairs precisely because the two flavors
/// spell them differently — see [`run_list_request`].
#[derive(Debug, Default, Clone)]
pub struct RunFilters {
    /// Head commit SHA.
    pub head_sha: Option<String>,
    /// Git ref, ideally fully qualified (`refs/heads/main`).
    pub git_ref: Option<String>,
    /// Run status or conclusion, e.g. `success`, `failure`, `in_progress`.
    pub status: Option<String>,
    /// Triggering event, e.g. `push`, `workflow_dispatch`.
    pub event: Option<String>,
    /// Workflow file name, e.g. `ci.yml`.
    pub workflow: Option<String>,
}

/// A thin Forgejo/Gitea REST client bound to one instance and one token.
#[derive(Debug)]
pub struct Forge {
    rest: RestClient,
    /// Set from `FORGEJO_FLAVOR`; skips detection entirely when present.
    forced_flavor: Option<Flavor>,
    /// Detection result, cached after the first Actions call. A lost race just means two
    /// `GET /version` requests, so a plain `OnceLock` beats an async-aware cell here.
    detected_flavor: OnceLock<Flavor>,
}

impl Forge {
    /// Builds a client for `base_url` (e.g. `https://codeberg.org`) authenticating with `token`.
    ///
    /// The flavor is detected lazily, on the first call that needs it; use
    /// [`Forge::with_forced_flavor`] to pin it instead.
    ///
    /// # Errors
    /// Fails if the base URL can't be extended with the API path, or the HTTP client can't be
    /// constructed.
    pub fn new(base_url: &Url, token: &str) -> Result<Self, ForgeError> {
        let client = RestClient::new(&RestConfig {
            base_url,
            token,
            api_prefix: API_PREFIX,
            user_agent: concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        })?;
        Ok(Self {
            rest: client,
            forced_flavor: None,
            detected_flavor: OnceLock::new(),
        })
    }

    /// Pins the flavor, bypassing detection. `None` restores automatic detection.
    #[must_use]
    pub fn with_forced_flavor(mut self, flavor: Option<Flavor>) -> Self {
        self.forced_flavor = flavor;
        self
    }

    /// The flavor if it is already settled — pinned via `FORGEJO_FLAVOR`, or cached from an
    /// earlier detection — without making a request.
    ///
    /// For callers that must stay local. `write_status` is the one that matters: it reports
    /// elevation state and is exactly the tool you reach for when an instance is misbehaving,
    /// so it should not be the tool that blocks on a `GET /version` to that instance.
    #[must_use]
    pub fn flavor_if_known(&self) -> Option<Flavor> {
        self.forced_flavor
            .or_else(|| self.detected_flavor.get().copied())
    }

    /// The instance flavor: the pinned value, the cached detection, or a fresh
    /// `GET /version`.
    ///
    /// Never fails: an unreachable or unparseable instance falls back to
    /// [`Flavor::Forgejo`], this server's historical assumption, and is *not* cached — so a
    /// later call retries the detection rather than living with a guess made during an
    /// outage.
    pub async fn flavor(&self) -> Flavor {
        if let Some(flavor) = self.forced_flavor {
            return flavor;
        }
        if let Some(flavor) = self.detected_flavor.get() {
            return *flavor;
        }
        let Ok(value) = self.server_version().await else {
            return Flavor::Forgejo;
        };
        let Some(flavor) = value
            .get("version")
            .and_then(Value::as_str)
            .map(Flavor::classify)
        else {
            return Flavor::Forgejo;
        };
        let _ = self.detected_flavor.set(flavor);
        tracing::debug!(target: "forgejo_mcp.flavor", flavor = flavor.as_str(), "detected instance flavor");
        flavor
    }

    // --- read endpoints ---

    /// The configured instance base URL, for display — e.g. `https://codeberg.org/`.
    #[must_use]
    pub fn base_url(&self) -> String {
        self.rest.base_url()
    }

    /// `GET /version` — the Forgejo/Gitea instance software version.
    pub async fn server_version(&self) -> Result<Value, ForgeError> {
        self.rest.get("version", &[]).await
    }

    /// `GET /user` — the authenticated user.
    pub async fn user_get_current(&self) -> Result<Value, ForgeError> {
        self.rest.get("user", &[]).await
    }

    /// `GET /user/repos` — the authenticated user's repositories.
    pub async fn list_my_repos(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.rest.get_list("user/repos", &paging(page, limit)).await
    }

    /// `GET /repos/{owner}/{repo}` — one repository's details.
    pub async fn get_repo(&self, owner: &str, repo: &str) -> Result<Value, ForgeError> {
        self.rest.get(&format!("repos/{owner}/{repo}"), &[]).await
    }

    /// `GET /repos/{owner}/{repo}/commits/{ref}/status` — the rolled-up
    /// verdict for a ref.
    ///
    /// An early `ci_status` tool on this endpoint was removed in v0.12 because
    /// it answered `state: ""` with `total_count: 0` for Actions repositories.
    /// That is no longer so on either forge — probed 27 September 2026,
    /// Codeberg answering `pending`/2 and gitea.com `success`/4 — so the
    /// endpoint is useful again.
    pub async fn get_combined_status(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Value, ForgeError> {
        self.rest
            .get(
                &format!("repos/{owner}/{repo}/commits/{git_ref}/status"),
                &[],
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/commits/{ref}/statuses` — every status
    /// reported against a commit.
    ///
    /// Distinct from the *combined* status: this is the individual checks with
    /// their contexts, where the combined endpoint is one rolled-up verdict.
    pub async fn list_commit_statuses(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.rest
            .get_list(
                &format!("repos/{owner}/{repo}/commits/{git_ref}/statuses"),
                &paging(page, limit),
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/git/trees/{sha}` — a tree, optionally whole.
    ///
    /// `recursive` walks the entire tree in one request, which is what makes
    /// "where is every file of this kind" a single call rather than a descent.
    /// Large trees are truncated by the forge, which says so in the response.
    pub async fn get_repo_tree(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
        recursive: bool,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Value, ForgeError> {
        let mut params = paging(page, limit);
        if recursive {
            params.push(("recursive", "true".to_owned()));
        }
        self.rest
            .get(&format!("repos/{owner}/{repo}/git/trees/{sha}"), &params)
            .await
    }

    /// `GET /repos/{owner}/{repo}/branch_protections` — protection rules.
    ///
    /// A branch matching no rule is unprotected, so an empty list is the
    /// answer "nothing is protected here" rather than a missing feature.
    pub async fn list_branch_protections(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.rest
            .get_list(&format!("repos/{owner}/{repo}/branch_protections"), &[])
            .await
    }

    /// `GET /user/keys` or `GET /users/{username}/keys` — SSH keys.
    ///
    /// Without a username this is the authenticated user's own list, which is
    /// the one that carries `verified`. Another user's list is public
    /// information and may omit it.
    pub async fn list_ssh_keys(
        &self,
        username: Option<&str>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        let path = match username {
            Some(u) => format!("users/{u}/keys"),
            None => "user/keys".to_owned(),
        };
        self.rest.get_list(&path, &paging(page, limit)).await
    }

    /// `GET /user/gpg_keys` or `GET /users/{username}/gpg_keys` — GPG keys.
    pub async fn list_gpg_keys(
        &self,
        username: Option<&str>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        let path = match username {
            Some(u) => format!("users/{u}/gpg_keys"),
            None => "user/gpg_keys".to_owned(),
        };
        self.rest.get_list(&path, &paging(page, limit)).await
    }

    /// `GET /repos/{owner}/{repo}/commits` — commits on a ref (paged).
    ///
    /// `sha` names the ref to start from — a branch, a tag or a commit id —
    /// and defaults to the repository's default branch. Asking for one the
    /// forge cannot resolve answers 404, not an empty list.
    pub async fn list_commits(
        &self,
        owner: &str,
        repo: &str,
        sha: Option<&str>,
        path: Option<&str>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        let mut params = paging(page, limit);
        if let Some(sha) = sha {
            params.push(("sha", sha.to_owned()));
        }
        if let Some(path) = path {
            params.push(("path", path.to_owned()));
        }
        self.rest
            .get_list(&format!("repos/{owner}/{repo}/commits"), &params)
            .await
    }

    /// `GET /repos/{owner}/{repo}/git/commits/{sha}` — one commit.
    ///
    /// Forgejo and Gitea answer this with the same shape as a listing entry,
    /// verification block included, so both go through one summary.
    pub async fn get_commit(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Value, ForgeError> {
        self.rest
            .get(&format!("repos/{owner}/{repo}/git/commits/{sha}"), &[])
            .await
    }

    /// `GET /repos/{owner}/{repo}/branches` — branches (paged).
    pub async fn list_branches(
        &self,
        owner: &str,
        repo: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.rest
            .get_list(
                &format!("repos/{owner}/{repo}/branches"),
                &paging(page, limit),
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/contents/{path}` — file or directory metadata. For a file the
    /// object carries the body as base64 in `content`; `git_ref` selects a branch/tag/commit.
    /// `path` must come from [`encode_repo_path`], or be empty for the repository root.
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
        let endpoint = if path.is_empty() {
            format!("repos/{owner}/{repo}/contents")
        } else {
            format!("repos/{owner}/{repo}/contents/{path}")
        };
        self.rest.get(&endpoint, &query).await
    }

    /// `GET /repos/{owner}/{repo}/branches/{branch}` — one branch. `branch` must come from
    /// [`encode_repo_path`].
    pub async fn get_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Value, ForgeError> {
        self.rest
            .get(&format!("repos/{owner}/{repo}/branches/{branch}"), &[])
            .await
    }

    /// `GET /repos/{owner}/{repo}/issues?type=issues` — issues, optionally filtered by `state`.
    ///
    /// Always asks for `type=issues`: without it both forges list pull requests here too, since
    /// a pull request is an issue to them. Codeberg's `forgejo/forgejo` had 1,728 open "issues"
    /// on 24 October 2026, 154 of them pull requests; `list_pull_requests` is the tool for those.
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
        query.push(("type", "issues".to_owned()));
        self.rest
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
        self.rest
            .get(&format!("repos/{owner}/{repo}/issues/{index}"), &[])
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls` — pull requests, optionally filtered by state and by base
    /// and head branch. The query depends on the flavor; see [`pull_list_query`].
    pub async fn list_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        filters: &PullFilters<'_>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        let query = pull_list_query(self.flavor().await, filters, page, limit);
        self.rest
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
        self.rest
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
        self.rest.get("repos/search", &q).await
    }

    /// `GET /user/orgs` — organizations the authenticated user belongs to.
    pub async fn list_orgs(
        &self,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Value, ForgeError> {
        self.rest.get("user/orgs", &paging(page, limit)).await
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
        self.rest.get("notifications", &query).await
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
        self.rest
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
        self.rest
            .get_list(
                &format!("repos/{owner}/{repo}/pulls/{index}/reviews"),
                &paging(page, limit),
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{index}/files` — files changed by a pull request.
    ///
    /// Entries carry per-file counts (`additions` / `deletions` / `changes`) and, on a rename,
    /// `previous_filename` — but **not** the hunks themselves; Forgejo omits the `patch` field
    /// that GitHub's equivalent returns. Use [`Forge::get_pull_request_diff`] for the content.
    pub async fn list_pull_request_files(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.rest
            .get_list(
                &format!("repos/{owner}/{repo}/pulls/{index}/files"),
                &paging(page, limit),
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{index}.diff` — the pull request's unified diff.
    ///
    /// Serves `text/plain`, not JSON, so it goes through [`RestClient::get_text`]. The sibling
    /// `.patch` view (same diff wrapped in a mail-formatted commit series) is not exposed: it
    /// carries author/date headers that add tokens without adding reviewable content.
    pub async fn get_pull_request_diff(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
    ) -> Result<String, ForgeError> {
        self.rest
            .get_text(&format!("repos/{owner}/{repo}/pulls/{index}.diff"), &[])
            .await
    }

    // --- write endpoints ---

    /// `POST /user/repos` — create a repository for the authenticated user.
    pub async fn create_repo(&self, body: &Value) -> Result<Value, ForgeError> {
        self.rest.post("user/repos", body).await
    }

    /// `POST /repos/migrate` — copy a repository from another forge into this instance. The
    /// body is a `MigrateRepoOptions`; the response is the new `Repository`.
    ///
    /// The import runs asynchronously in Forgejo's task queue, so the returned repo is a
    /// placeholder that fills in over the following seconds or minutes — poll `get_repo` to
    /// watch it land. The source repository is never modified: this is a copy, not a move.
    pub async fn migrate_repo(&self, body: &Value) -> Result<Value, ForgeError> {
        self.rest.post("repos/migrate", body).await
    }

    /// `POST /repos/{owner}/{repo}/issues` — create an issue.
    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
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
        self.rest
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
        self.rest
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
        self.rest
            .post(
                &format!("repos/{owner}/{repo}/issues/{index}/comments"),
                body,
            )
            .await
    }

    /// `PATCH /repos/{owner}/{repo}` — edit repository settings. The body is a partial
    /// `EditRepoOption`; only the fields present change.
    pub async fn edit_repo(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .patch(&format!("repos/{owner}/{repo}"), body)
            .await
    }

    /// `DELETE /repos/{owner}/{repo}` — delete a repository.
    pub async fn delete_repo(&self, owner: &str, repo: &str) -> Result<(), ForgeError> {
        self.rest.delete(&format!("repos/{owner}/{repo}")).await
    }

    /// `PATCH /repos/{owner}/{repo}/issues/{index}` — edit an issue (or a pull request's issue
    /// half: title, body, state, assignees, milestone). The body is a partial `EditIssueOption`;
    /// only the fields present change. Labels are not part of it — see
    /// [`Forge::replace_issue_labels`].
    pub async fn edit_issue(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .patch(&format!("repos/{owner}/{repo}/issues/{index}"), body)
            .await
    }

    /// `PUT /repos/{owner}/{repo}/issues/{index}/labels` — replace an issue's labels. Both forges
    /// take label names as well as ids here; an empty list clears them.
    pub async fn replace_issue_labels(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        labels: &[String],
    ) -> Result<Value, ForgeError> {
        self.rest
            .put(
                &format!("repos/{owner}/{repo}/issues/{index}/labels"),
                &serde_json::json!({ "labels": labels }),
            )
            .await
    }

    /// `POST /repos/{owner}/{repo}/pulls/{index}/merge` — merge a pull request. Both forges
    /// answer an empty `200`, so the caller reads the pull request back. The body's keys differ
    /// by flavor; see [`merge_body`].
    pub async fn merge_pull_request(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        merge: &PullMerge<'_>,
    ) -> Result<(), ForgeError> {
        let body = merge_body(self.flavor().await, merge);
        self.rest
            .post(&format!("repos/{owner}/{repo}/pulls/{index}/merge"), &body)
            .await?;
        Ok(())
    }

    /// `POST /repos/{owner}/{repo}/pulls/{index}/reviews` — submit a review. The body is a
    /// `CreatePullReviewOptions`; the response is the new `PullReview`.
    pub async fn create_pull_request_review(
        &self,
        owner: &str,
        repo: &str,
        index: i64,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .post(&format!("repos/{owner}/{repo}/pulls/{index}/reviews"), body)
            .await
    }

    /// `DELETE /repos/{owner}/{repo}/branches/{branch}` — delete a branch. `branch` must come
    /// from [`encode_repo_path`]: a branch name may hold `/` and `#`.
    pub async fn delete_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<(), ForgeError> {
        self.rest
            .delete(&format!("repos/{owner}/{repo}/branches/{branch}"))
            .await
    }

    /// `POST /repos/{owner}/{repo}/contents/{path}` — create a file, as a commit. The body is a
    /// `CreateFileOptions` with base64 `content`; the response is a `FileResponse`. `path` must
    /// come from [`encode_repo_path`].
    pub async fn create_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .post(&format!("repos/{owner}/{repo}/contents/{path}"), body)
            .await
    }

    /// `PUT /repos/{owner}/{repo}/contents/{path}` — replace a file's content, as a commit. The
    /// body is an `UpdateFileOptions`, whose `sha` must be the blob being replaced. `path` must
    /// come from [`encode_repo_path`].
    pub async fn update_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .put(&format!("repos/{owner}/{repo}/contents/{path}"), body)
            .await
    }

    /// `DELETE /repos/{owner}/{repo}/contents/{path}` — remove a file, as a commit. The body is
    /// a `DeleteFileOptions` carrying the blob `sha`. `path` must come from
    /// [`encode_repo_path`].
    pub async fn delete_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .delete_with_body(&format!("repos/{owner}/{repo}/contents/{path}"), body)
            .await
    }

    // --- releases (and their downloadable assets) ---

    /// `GET /repos/{owner}/{repo}/releases` — releases, newest first (paged).
    pub async fn list_releases(
        &self,
        owner: &str,
        repo: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<(Value, Option<usize>), ForgeError> {
        self.rest
            .get_list(
                &format!("repos/{owner}/{repo}/releases"),
                &paging(page, limit),
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/releases/tags/{tag}` — one release by its git tag.
    ///
    /// Addressing by tag rather than by numeric id is what makes a release script idempotent:
    /// the tag is known before the release exists, the id only afterwards.
    pub async fn get_release_by_tag(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Value, ForgeError> {
        self.rest
            .get(&format!("repos/{owner}/{repo}/releases/tags/{tag}"), &[])
            .await
    }

    /// `POST /repos/{owner}/{repo}/releases` — create a release. The body is a
    /// `CreateReleaseOption`; the response is the new `Release`, whose `id` addresses its assets.
    ///
    /// The tag must already exist on the instance unless `target_commitish` is given, in which
    /// case Forgejo creates the tag at that commit.
    pub async fn create_release(
        &self,
        owner: &str,
        repo: &str,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .post(&format!("repos/{owner}/{repo}/releases"), body)
            .await
    }

    /// `PATCH /repos/{owner}/{repo}/releases/{id}` — edit a release in place. The body is an
    /// `EditReleaseOption`; the response is the updated `Release`.
    ///
    /// Forgejo and Gitea share this endpoint verbatim, request and response alike, so unlike
    /// the workflow-run endpoints it needs no per-flavor translation.
    pub async fn edit_release(
        &self,
        owner: &str,
        repo: &str,
        id: i64,
        body: &Value,
    ) -> Result<Value, ForgeError> {
        self.rest
            .patch(&format!("repos/{owner}/{repo}/releases/{id}"), body)
            .await
    }

    /// `GET /repos/{owner}/{repo}/releases/{id}/assets` — the files attached to one release.
    pub async fn list_release_assets(
        &self,
        owner: &str,
        repo: &str,
        id: i64,
    ) -> Result<Value, ForgeError> {
        self.rest
            .get(&format!("repos/{owner}/{repo}/releases/{id}/assets"), &[])
            .await
    }

    /// `POST /repos/{owner}/{repo}/releases/{id}/assets?name=…` — attach a file to a release.
    ///
    /// The one multipart endpoint this server speaks: the file goes in an `attachment` form
    /// part and the published filename in the `name` query parameter. Forgejo keeps same-named
    /// assets side by side rather than replacing, so a caller re-running a release should delete
    /// the old one first via [`Forge::delete_release_asset`].
    pub async fn upload_release_asset(
        &self,
        owner: &str,
        repo: &str,
        id: i64,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<Value, ForgeError> {
        self.rest
            .post_multipart(
                &format!("repos/{owner}/{repo}/releases/{id}/assets"),
                &[("name", name.to_owned())],
                "attachment",
                name.to_owned(),
                bytes,
            )
            .await
    }

    /// `DELETE /repos/{owner}/{repo}/releases/{id}/assets/{attachment_id}` — detach one file.
    pub async fn delete_release_asset(
        &self,
        owner: &str,
        repo: &str,
        id: i64,
        attachment_id: i64,
    ) -> Result<(), ForgeError> {
        self.rest
            .delete(&format!(
                "repos/{owner}/{repo}/releases/{id}/assets/{attachment_id}"
            ))
            .await
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
        self.rest
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
        self.rest
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
        self.rest
            .delete(&format!("repos/{owner}/{repo}/push_mirrors/{remote_name}"))
            .await
    }

    /// `POST /repos/{owner}/{repo}/push_mirrors-sync` — trigger an immediate sync of all mirrors.
    pub async fn sync_push_mirrors(&self, owner: &str, repo: &str) -> Result<(), ForgeError> {
        self.rest
            .post_empty(&format!("repos/{owner}/{repo}/push_mirrors-sync"))
            .await
    }

    // --- actions (CI) ---

    /// `GET …/actions/runs` — workflow runs. Returns the raw `{ workflow_runs, total_count }`
    /// wrapper, which both flavors use (confirmed live on Forgejo: no `X-Total-Count` header),
    /// so the tool layer unwraps it like search rather than auto-paginating.
    ///
    /// The path and query depend on the flavor; [`run_list_request`] does that translation.
    /// `page` and `limit` are not optional here, for the reason given there.
    pub async fn list_workflow_runs(
        &self,
        owner: &str,
        repo: &str,
        filters: &RunFilters,
        page: u32,
        limit: u32,
    ) -> Result<Value, ForgeError> {
        let (path, query) =
            run_list_request(self.flavor().await, owner, repo, filters, page, limit);
        self.rest.get(&path, &query).await
    }

    /// `GET /repos/{owner}/{repo}/actions/runs/{run_id}` — one workflow run. Same path on both
    /// flavors; the response shape differs, which the tool layer handles.
    pub async fn get_workflow_run(
        &self,
        owner: &str,
        repo: &str,
        run_id: i64,
    ) -> Result<Value, ForgeError> {
        self.rest
            .get(&format!("repos/{owner}/{repo}/actions/runs/{run_id}"), &[])
            .await
    }

    /// `GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs` — the jobs of one run. Forgejo 16+.
    ///
    /// Same path on both flavors, different bodies: Forgejo answers with a bare array and no
    /// paging, Gitea with a `{ jobs, total_count }` wrapper paged at its default size. Gitea is
    /// asked for its usual 50-item ceiling so that a matrix run arrives in one answer; the tool
    /// layer reports `total_count` when even that falls short.
    pub async fn list_workflow_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: i64,
    ) -> Result<Value, ForgeError> {
        let query = match self.flavor().await {
            Flavor::Forgejo => Vec::new(),
            Flavor::Gitea => paging(None, Some(50)),
        };
        self.rest
            .get(
                &format!("repos/{owner}/{repo}/actions/runs/{run_id}/jobs"),
                &query,
            )
            .await
    }

    /// `GET /repos/{owner}/{repo}/actions/jobs/{job_id}/logs` — part of one job's log, by HTTP
    /// `Range`. Same path on both flavors; plain text, one timestamped line per log line.
    ///
    /// Codeberg honours the range (`206` with `Content-Range`); a server that ignores it sends
    /// the whole log, which [`ByteRange`] reports so the caller can cut the window itself.
    pub async fn get_job_log(
        &self,
        owner: &str,
        repo: &str,
        job_id: i64,
        range: &str,
    ) -> Result<ByteRange, ForgeError> {
        self.rest
            .get_text_range(
                &format!("repos/{owner}/{repo}/actions/jobs/{job_id}/logs"),
                &[],
                range,
            )
            .await
    }

    /// `POST /repos/{owner}/{repo}/actions/workflows/{filename}/dispatches` — trigger a
    /// `workflow_dispatch` run.
    ///
    /// Same path on both flavors, but only Forgejo accepts `return_run_info` and so answers
    /// with the created run; Gitea replies `204 No Content`, which surfaces here as
    /// [`Value::Null`]. See [`dispatch_body`].
    pub async fn dispatch_workflow(
        &self,
        owner: &str,
        repo: &str,
        filename: &str,
        git_ref: &str,
        inputs: Option<serde_json::Map<String, Value>>,
    ) -> Result<Value, ForgeError> {
        let body = dispatch_body(self.flavor().await, git_ref, inputs);
        self.rest
            .post(
                &format!("repos/{owner}/{repo}/actions/workflows/{filename}/dispatches"),
                &body,
            )
            .await
    }

    /// `POST /repos/{owner}/{repo}/actions/runs/{run_id}/cancel` — cancel a pending or running
    /// run. Forgejo 16+ and Gitea, same path.
    ///
    /// Forgejo answers `204` — also for a run that has already finished, which it leaves alone —
    /// and Gitea answers with the run. The body is returned as is: [`Value::Null`] from
    /// Forgejo.
    pub async fn cancel_workflow_run(
        &self,
        owner: &str,
        repo: &str,
        run_id: i64,
    ) -> Result<Value, ForgeError> {
        self.rest
            .post_bare(&format!(
                "repos/{owner}/{repo}/actions/runs/{run_id}/cancel"
            ))
            .await
    }

    /// Reruns a finished run, or one job of it and the jobs that depend on it. Forgejo 17+ and
    /// Gitea; the path depends on the flavor, see [`rerun_path`].
    ///
    /// Forgejo answers `204`, so [`Value::Null`]; Gitea answers with the run it restarted, or
    /// for a job rerun with the job.
    pub async fn rerun_workflow(
        &self,
        owner: &str,
        repo: &str,
        run_id: i64,
        job_id: Option<i64>,
    ) -> Result<Value, ForgeError> {
        let path = rerun_path(self.flavor().await, owner, repo, run_id, job_id);
        self.rest.post_bare(&path).await
    }
}

/// Builds the rerun path, per flavor. The whole-run path is the same on both; a single job is
/// addressed on its own by Forgejo but under its run by Gitea, so a Forgejo job rerun never uses
/// `run_id`.
fn rerun_path(flavor: Flavor, owner: &str, repo: &str, run_id: i64, job_id: Option<i64>) -> String {
    match (flavor, job_id) {
        (_, None) => format!("repos/{owner}/{repo}/actions/runs/{run_id}/rerun"),
        (Flavor::Forgejo, Some(job)) => format!("repos/{owner}/{repo}/actions/jobs/{job}/rerun"),
        (Flavor::Gitea, Some(job)) => {
            format!("repos/{owner}/{repo}/actions/runs/{run_id}/jobs/{job}/rerun")
        }
    }
}

/// The pull-request filters a caller can ask for, before they are translated to the query
/// parameters this instance's flavor understands.
#[derive(Debug, Default, Clone, Copy)]
pub struct PullFilters<'a> {
    /// `open`, `closed` or `all`.
    pub state: Option<&'a str>,
    /// Base (target) branch name.
    pub base: Option<&'a str>,
    /// Head (source) branch name, bare. Forgejo matches it against the head's `label`, which is
    /// the branch name for an ordinary pull request and `user/topic` for an agit-flow one.
    pub head: Option<&'a str>,
}

/// How to merge a pull request, before it is spelled the way this instance's flavor expects.
#[derive(Debug, Clone, Copy)]
pub struct PullMerge<'a> {
    /// `merge`, `rebase`, `rebase-merge`, `squash` or `fast-forward-only`.
    pub style: &'a str,
    /// Merge commit title; the forge composes one when absent.
    pub title: Option<&'a str>,
    /// Merge commit message body.
    pub message: Option<&'a str>,
    /// Delete the head branch once merged.
    pub delete_branch: Option<bool>,
    /// Refuse the merge unless the pull request's head is still this commit.
    pub head_sha: Option<&'a str>,
}

/// Builds a merge request's body, per flavor.
///
/// | field | Forgejo | Gitea |
/// |---|---|---|
/// | style | `Do` | `do` |
/// | title | `MergeTitleField` | `merge_title_field` |
/// | message | `MergeMessageField` | `merge_message_field` |
/// | `delete_branch_after_merge`, `head_commit_id` | same | same |
///
/// From the two published `OpenAPI` specs, 25 October 2026. Forgejo kept the Go field names
/// that both forges once shared, and Gitea has since given them JSON names. Go's decoder matches
/// keys case-insensitively, which would forgive `Do`/`do`, but not `MergeTitleField` against
/// `merge_title_field`: a title sent in the wrong spelling would be dropped without an error.
fn merge_body(flavor: Flavor, merge: &PullMerge<'_>) -> Value {
    let (style, title, message) = match flavor {
        Flavor::Forgejo => ("Do", "MergeTitleField", "MergeMessageField"),
        Flavor::Gitea => ("do", "merge_title_field", "merge_message_field"),
    };
    let mut body = serde_json::Map::new();
    body.insert(style.to_owned(), Value::String(merge.style.to_owned()));
    if let Some(t) = merge.title {
        body.insert(title.to_owned(), Value::String(t.to_owned()));
    }
    if let Some(m) = merge.message {
        body.insert(message.to_owned(), Value::String(m.to_owned()));
    }
    if let Some(d) = merge.delete_branch {
        body.insert("delete_branch_after_merge".to_owned(), Value::Bool(d));
    }
    if let Some(sha) = merge.head_sha {
        body.insert("head_commit_id".to_owned(), Value::String(sha.to_owned()));
    }
    Value::Object(body)
}

/// Percent-encodes a repository path (a file path, or a branch name) for use in a request URL,
/// one `/`-separated segment at a time, and refuses one that could leave the endpoint.
///
/// Request URLs are built with `Url::join`, which resolves `.` and `..` segments — so a file
/// path of `../../../user` would address a different API endpoint entirely, with the write
/// token. `?` and `#` would likewise cut the path short into a query or fragment. Every byte but
/// the unreserved set is escaped, and empty, `.` and `..` segments are refused outright.
///
/// # Errors
/// A message naming what is wrong with `path`, for the caller.
pub fn encode_repo_path(path: &str) -> Result<String, String> {
    if path.is_empty() {
        return Err("the path is empty".to_owned());
    }
    let mut encoded = String::with_capacity(path.len());
    for (i, segment) in path.split('/').enumerate() {
        if matches!(segment, "" | "." | "..") {
            return Err(format!(
                "`{path}` has an empty, `.` or `..` segment; give a path relative to the \
                 repository root, without a leading or trailing `/`"
            ));
        }
        if i > 0 {
            encoded.push('/');
        }
        for byte in segment.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                encoded.push(char::from(byte));
            } else {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
            }
        }
    }
    Ok(encoded)
}

/// Builds the query for a pull-request listing, per flavor.
///
/// | filter | Forgejo 16+ | Gitea |
/// |---|---|---|
/// | base branch | `base` | `base_branch` |
/// | head branch | `head` | none |
///
/// Probed on Codeberg and gitea.com on 24 October 2026, and each forge silently ignores the
/// other's spelling: Codeberg returned all 10,350 of `forgejo/forgejo`'s pull requests for
/// `base_branch`, and gitea.com all 748 of `gitea/tea`'s for `base` or `head`. So only the
/// flavor's own spelling is sent, and a head filter is not sent to Gitea at all — the tool layer
/// applies it there, and checks the answer on every forge, since Forgejo before 16 has neither.
fn pull_list_query(
    flavor: Flavor,
    filters: &PullFilters<'_>,
    page: Option<u32>,
    limit: Option<u32>,
) -> Vec<(&'static str, String)> {
    let mut query = paging(page, limit);
    if let Some(state) = filters.state {
        query.push(("state", state.to_owned()));
    }
    if let Some(base) = filters.base {
        let key = match flavor {
            Flavor::Forgejo => "base",
            Flavor::Gitea => "base_branch",
        };
        query.push((key, base.to_owned()));
    }
    if let (Flavor::Forgejo, Some(head)) = (flavor, filters.head) {
        query.push(("head", head.to_owned()));
    }
    query
}

/// Builds the path and query for a workflow-run listing, per flavor.
///
/// The differences, all confirmed against the two published `OpenAPI` specs:
///
/// | filter | Forgejo | Gitea |
/// |---|---|---|
/// | git ref | `ref`, fully qualified | `branch`, bare name |
/// | workflow file | `workflow_id` query | a separate `…/actions/workflows/{file}/runs` path |
/// | `head_sha`, `status`, `event` | same | same |
///
/// Sending Forgejo's spelling to Gitea would not error — unknown query parameters are simply
/// ignored — it would silently return *unfiltered* results, which is why this translates
/// rather than sending both spellings and hoping.
///
/// `page` and `limit` are always sent. Forgejo ignores `limit` on this endpoint unless `page` is
/// given too, and without `page` it returns *every* run: all 41 of this repository's on Codeberg
/// for `limit=3`, and for `forgejo/forgejo`, with 210,515 runs, a `504` from Codeberg's gateway
/// after 30 seconds. Probed on 24 October 2026; no other listing tried behaves this way.
fn run_list_request(
    flavor: Flavor,
    owner: &str,
    repo: &str,
    filters: &RunFilters,
    page: u32,
    limit: u32,
) -> (String, Vec<(&'static str, String)>) {
    let mut query = paging(Some(page), Some(limit));
    for (key, value) in [
        ("head_sha", &filters.head_sha),
        ("status", &filters.status),
        ("event", &filters.event),
    ] {
        if let Some(value) = value {
            query.push((key, value.clone()));
        }
    }

    let path = match flavor {
        Flavor::Forgejo => {
            if let Some(git_ref) = &filters.git_ref {
                query.push(("ref", git_ref.clone()));
            }
            if let Some(workflow) = &filters.workflow {
                query.push(("workflow_id", workflow.clone()));
            }
            format!("repos/{owner}/{repo}/actions/runs")
        }
        Flavor::Gitea => {
            if let Some(git_ref) = &filters.git_ref {
                query.push(("branch", strip_ref_prefix(git_ref).to_owned()));
            }
            match &filters.workflow {
                Some(workflow) => {
                    format!("repos/{owner}/{repo}/actions/workflows/{workflow}/runs")
                }
                None => format!("repos/{owner}/{repo}/actions/runs"),
            }
        }
    };
    (path, query)
}

/// Reduces a fully-qualified branch ref to the bare name Gitea's `branch` filter wants.
///
/// Tag refs are left as-is: Gitea has no tag filter on this endpoint, so `refs/tags/v1` would
/// match nothing either way, and mangling it into `v1` would only make the miss look like a
/// branch that doesn't exist.
fn strip_ref_prefix(git_ref: &str) -> &str {
    git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref)
}

/// Builds the `workflow_dispatch` request body, per flavor.
///
/// `return_run_info` is a Forgejo extension: it makes the endpoint answer with the run it just
/// created instead of an empty `204`. Gitea rejects unknown body fields on this endpoint, so
/// it must be omitted there.
fn dispatch_body(
    flavor: Flavor,
    git_ref: &str,
    inputs: Option<serde_json::Map<String, Value>>,
) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("ref".to_owned(), Value::String(git_ref.to_owned()));
    if flavor == Flavor::Forgejo {
        body.insert("return_run_info".to_owned(), Value::Bool(true));
    }
    if let Some(inputs) = inputs {
        body.insert("inputs".to_owned(), Value::Object(inputs));
    }
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_paths_keep_their_slashes_and_escape_the_rest() {
        assert_eq!(encode_repo_path("docs/setup.md").unwrap(), "docs/setup.md");
        assert_eq!(encode_repo_path("feature/login").unwrap(), "feature/login");
        assert_eq!(
            encode_repo_path("notes/a b#1?.md").unwrap(),
            "notes/a%20b%231%3F.md"
        );
        assert_eq!(encode_repo_path("Ä.txt").unwrap(), "%C3%84.txt");
        assert_eq!(encode_repo_path("100%.md").unwrap(), "100%25.md");
        assert_eq!(encode_repo_path("...md").unwrap(), "...md");
    }

    #[test]
    fn repo_paths_that_could_leave_the_endpoint_are_refused() {
        for path in [
            "",
            "/etc",
            "docs/",
            "a//b",
            ".",
            "..",
            "../../user",
            "docs/../../x",
            "./x",
        ] {
            assert!(encode_repo_path(path).is_err(), "{path:?} was accepted");
        }
    }

    /// The point of the encoding: whatever the caller's path, the joined URL stays under the
    /// contents endpoint of the repository it names.
    #[test]
    fn an_encoded_path_joins_under_the_endpoint() {
        let root = Url::parse("https://codeberg.org/api/v1/").unwrap();
        for path in [
            "a b/c#d?e.md",
            "..x/y",
            "%2e%2e/%2e%2e/user",
            "a\\..\\..\\b",
        ] {
            let encoded = encode_repo_path(path).unwrap();
            let url = root.join(&format!("repos/o/r/contents/{encoded}")).unwrap();
            assert!(
                url.path().starts_with("/api/v1/repos/o/r/contents/"),
                "{path:?} -> {url}"
            );
            assert_eq!(url.query(), None, "{path:?}");
            assert_eq!(url.fragment(), None, "{path:?}");
        }
    }

    #[test]
    fn merge_body_uses_each_flavors_own_spelling() {
        let merge = PullMerge {
            style: "squash",
            title: Some("t"),
            message: Some("m"),
            delete_branch: Some(true),
            head_sha: Some("abc"),
        };
        assert_eq!(
            merge_body(Flavor::Forgejo, &merge),
            serde_json::json!({
                "Do": "squash",
                "MergeTitleField": "t",
                "MergeMessageField": "m",
                "delete_branch_after_merge": true,
                "head_commit_id": "abc",
            })
        );
        assert_eq!(
            merge_body(Flavor::Gitea, &merge),
            serde_json::json!({
                "do": "squash",
                "merge_title_field": "t",
                "merge_message_field": "m",
                "delete_branch_after_merge": true,
                "head_commit_id": "abc",
            })
        );
    }

    #[test]
    fn merge_body_sends_only_what_was_asked() {
        let merge = PullMerge {
            style: "merge",
            title: None,
            message: None,
            delete_branch: None,
            head_sha: None,
        };
        assert_eq!(
            merge_body(Flavor::Forgejo, &merge),
            serde_json::json!({ "Do": "merge" })
        );
    }

    /// Real `GET /version` strings, read off both live instances on 24 October 2026.
    const CODEBERG: &str = "16.0.0-dev-785-465b60ef+gitea-1.22.0";
    const GITEA_COM: &str = "29.0.0+dev-160-g55186ca39b";
    /// What gitea.com reported on 15 September 2026, before it left the 1.x line.
    const GITEA_COM_1X: &str = "1.27.0+dev-954-g1f3981a301";

    #[test]
    fn live_version_strings_classify_correctly() {
        assert_eq!(Flavor::classify(CODEBERG), Flavor::Forgejo);
        assert_eq!(Flavor::classify(GITEA_COM), Flavor::Gitea);
        assert_eq!(Flavor::classify(GITEA_COM_1X), Flavor::Gitea);
    }

    #[test]
    fn the_gitea_compatibility_marker_means_forgejo_not_gitea() {
        // The counter-intuitive case the heuristic exists for: Forgejo names Gitea in its own
        // version string, and Gitea never does.
        assert_eq!(Flavor::classify("7.0.4+0-gitea-1.22.0"), Flavor::Forgejo);
        assert_eq!(Flavor::classify("1.21.11-0-gitea-1.21.11"), Flavor::Forgejo);
        // A Gitea dev build's git hash starts with `g` but never spells `gitea-`.
        assert_eq!(Flavor::classify("1.24.0+dev-77-g0f2a1b3c4d"), Flavor::Gitea);
    }

    #[test]
    fn without_the_marker_the_major_version_decides_nothing() {
        // Gitea left the 1.x line (gitea.com reports 29.0.0), so a high major is no longer
        // Forgejo: only the marker is.
        assert_eq!(Flavor::classify("29.0.0"), Flavor::Gitea);
        assert_eq!(Flavor::classify("16.0.0"), Flavor::Gitea);
        assert_eq!(Flavor::classify("1.22.3"), Flavor::Gitea);
        // Unparseable: treated as Gitea, which FORGEJO_FLAVOR exists to override.
        assert_eq!(Flavor::classify(""), Flavor::Gitea);
    }

    #[test]
    fn the_flavor_override_accepts_the_documented_values_only() {
        assert_eq!(Flavor::parse_override("auto"), Ok(None));
        assert_eq!(Flavor::parse_override(""), Ok(None));
        assert_eq!(Flavor::parse_override(" Gitea "), Ok(Some(Flavor::Gitea)));
        assert_eq!(Flavor::parse_override("FORGEJO"), Ok(Some(Flavor::Forgejo)));
        assert_eq!(
            Flavor::parse_override("codeberg"),
            Ok(Some(Flavor::Forgejo))
        );
        assert_eq!(Flavor::parse_override("github"), Err("github".to_owned()));
    }

    /// Filters exercising every field, so each flavor's translation is visible in full.
    fn all_filters() -> RunFilters {
        RunFilters {
            head_sha: Some("deadbeef".to_owned()),
            git_ref: Some("refs/heads/main".to_owned()),
            status: Some("success".to_owned()),
            event: Some("push".to_owned()),
            workflow: Some("ci.yml".to_owned()),
        }
    }

    #[test]
    fn forgejo_filters_go_on_the_query_of_the_plain_runs_path() {
        let (path, query) = run_list_request(Flavor::Forgejo, "o", "r", &all_filters(), 2, 10);
        assert_eq!(path, "repos/o/r/actions/runs");
        assert_eq!(
            query,
            vec![
                ("page", "2".to_owned()),
                ("limit", "10".to_owned()),
                ("head_sha", "deadbeef".to_owned()),
                ("status", "success".to_owned()),
                ("event", "push".to_owned()),
                ("ref", "refs/heads/main".to_owned()),
                ("workflow_id", "ci.yml".to_owned()),
            ]
        );
    }

    #[test]
    fn gitea_moves_the_workflow_into_the_path_and_renames_ref_to_branch() {
        let (path, query) = run_list_request(Flavor::Gitea, "o", "r", &all_filters(), 2, 10);
        assert_eq!(path, "repos/o/r/actions/workflows/ci.yml/runs");
        assert_eq!(
            query,
            vec![
                ("page", "2".to_owned()),
                ("limit", "10".to_owned()),
                ("head_sha", "deadbeef".to_owned()),
                ("status", "success".to_owned()),
                ("event", "push".to_owned()),
                // Fully-qualified on Forgejo, bare on Gitea.
                ("branch", "main".to_owned()),
            ]
        );
        // Nothing named `ref` or `workflow_id` survives — sending those to Gitea would be
        // ignored, silently returning unfiltered runs.
        assert!(
            !query
                .iter()
                .any(|(k, _)| *k == "ref" || *k == "workflow_id")
        );
    }

    #[test]
    fn gitea_without_a_workflow_filter_uses_the_plain_runs_path() {
        let filters = RunFilters {
            status: Some("failure".to_owned()),
            ..RunFilters::default()
        };
        let (path, query) = run_list_request(Flavor::Gitea, "o", "r", &filters, 1, 30);
        assert_eq!(path, "repos/o/r/actions/runs");
        assert_eq!(
            query,
            vec![
                ("page", "1".to_owned()),
                ("limit", "30".to_owned()),
                ("status", "failure".to_owned()),
            ]
        );
    }

    #[test]
    fn pull_filters_use_each_flavors_own_spelling() {
        let filters = PullFilters {
            state: Some("open"),
            base: Some("main"),
            head: Some("feature"),
        };
        assert_eq!(
            pull_list_query(Flavor::Forgejo, &filters, None, None),
            vec![
                ("state", "open".to_owned()),
                ("base", "main".to_owned()),
                ("head", "feature".to_owned()),
            ]
        );
        // Gitea has no head filter and spells base differently; `head` would be ignored, so it
        // is not sent.
        assert_eq!(
            pull_list_query(Flavor::Gitea, &filters, Some(1), Some(50)),
            vec![
                ("page", "1".to_owned()),
                ("limit", "50".to_owned()),
                ("state", "open".to_owned()),
                ("base_branch", "main".to_owned()),
            ]
        );
    }

    #[test]
    fn a_tag_ref_is_left_alone_for_gitea() {
        // Gitea's `branch` filter cannot match a tag either way; rewriting `refs/tags/v1` to
        // `v1` would only disguise the miss as a missing branch.
        assert_eq!(strip_ref_prefix("refs/tags/v1.0.0"), "refs/tags/v1.0.0");
        assert_eq!(strip_ref_prefix("refs/heads/feature/x"), "feature/x");
        assert_eq!(strip_ref_prefix("main"), "main");
    }

    #[test]
    fn only_forgejo_gets_the_return_run_info_extension() {
        let forgejo = dispatch_body(Flavor::Forgejo, "main", None);
        assert_eq!(
            forgejo,
            serde_json::json!({ "ref": "main", "return_run_info": true })
        );

        // Gitea rejects the unknown field, so the body is just the ref.
        let gitea = dispatch_body(Flavor::Gitea, "main", None);
        assert_eq!(gitea, serde_json::json!({ "ref": "main" }));
    }

    #[test]
    fn dispatch_inputs_are_carried_through_on_both_flavors() {
        let inputs = serde_json::json!({ "level": "debug" })
            .as_object()
            .cloned()
            .unwrap();
        for flavor in [Flavor::Forgejo, Flavor::Gitea] {
            let body = dispatch_body(flavor, "main", Some(inputs.clone()));
            assert_eq!(body["inputs"]["level"], "debug");
            assert_eq!(body["ref"], "main");
        }
    }

    #[test]
    fn a_job_rerun_is_addressed_per_flavor_and_a_run_rerun_is_not() {
        for flavor in [Flavor::Forgejo, Flavor::Gitea] {
            assert_eq!(
                rerun_path(flavor, "o", "r", 7, None),
                "repos/o/r/actions/runs/7/rerun"
            );
        }
        // Forgejo addresses a job on its own; Gitea nests it under its run.
        assert_eq!(
            rerun_path(Flavor::Forgejo, "o", "r", 7, Some(9)),
            "repos/o/r/actions/jobs/9/rerun"
        );
        assert_eq!(
            rerun_path(Flavor::Gitea, "o", "r", 7, Some(9)),
            "repos/o/r/actions/runs/7/jobs/9/rerun"
        );
    }

    #[tokio::test]
    async fn a_pinned_flavor_skips_detection_entirely() {
        let url = Url::parse("https://example.invalid").unwrap();
        let forge = Forge::new(&url, "t")
            .unwrap()
            .with_forced_flavor(Some(Flavor::Gitea));
        // No network: the pinned value is returned without a `GET /version`, which would
        // otherwise fail against this unresolvable host.
        assert_eq!(forge.flavor().await, Flavor::Gitea);
    }
}
