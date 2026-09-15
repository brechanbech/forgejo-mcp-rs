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

use crate::mcp_core::{RestClient, RestConfig, paging};
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
    /// Two signals, in order:
    ///
    /// 1. Forgejo advertises its Gitea API compatibility in its own version string —
    ///    Codeberg reports `16.0.0-dev-741-6f391573+gitea-1.22.0`, and older releases
    ///    `7.0.4+0-gitea-1.22.0`. Gitea never names itself in its version
    ///    (`1.27.0+dev-954-g1f3981a301`), so the `gitea-` marker means Forgejo, counter-
    ///    intuitive as that reads.
    /// 2. Failing that, the major version: Gitea is still on 1.x, Forgejo renumbered to 7
    ///    and beyond, so a major of 2 or more is Forgejo.
    ///
    /// Anything else is treated as Gitea, which only costs a wrong guess on an unreleased
    /// Forgejo that has dropped the compatibility marker and gone back to 1.x — and
    /// `FORGEJO_FLAVOR` exists for that.
    #[must_use]
    pub fn classify(version: &str) -> Self {
        if version.to_ascii_lowercase().contains("gitea-") {
            return Self::Forgejo;
        }
        let major = version
            .split(['.', '-', '+'])
            .next()
            .and_then(|s| s.parse::<u32>().ok());
        if major.is_some_and(|m| m >= 2) {
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
        self.rest
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
    pub async fn list_workflow_runs(
        &self,
        owner: &str,
        repo: &str,
        filters: &RunFilters,
        page: Option<u32>,
        limit: Option<u32>,
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
fn run_list_request(
    flavor: Flavor,
    owner: &str,
    repo: &str,
    filters: &RunFilters,
    page: Option<u32>,
    limit: Option<u32>,
) -> (String, Vec<(&'static str, String)>) {
    let mut query = paging(page, limit);
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

    /// Real `GET /version` strings, read off both live instances on 15 September 2026.
    const CODEBERG: &str = "16.0.0-dev-741-6f391573+gitea-1.22.0";
    const GITEA_COM: &str = "1.27.0+dev-954-g1f3981a301";

    #[test]
    fn live_version_strings_classify_correctly() {
        assert_eq!(Flavor::classify(CODEBERG), Flavor::Forgejo);
        assert_eq!(Flavor::classify(GITEA_COM), Flavor::Gitea);
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
    fn a_marker_free_major_version_falls_back_to_the_version_number() {
        // Forgejo renumbered past Gitea's 1.x line, so a major of 2+ is Forgejo even without
        // the compatibility marker.
        assert_eq!(Flavor::classify("16.0.0"), Flavor::Forgejo);
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
        let (path, query) =
            run_list_request(Flavor::Forgejo, "o", "r", &all_filters(), Some(2), Some(10));
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
        let (path, query) =
            run_list_request(Flavor::Gitea, "o", "r", &all_filters(), Some(2), Some(10));
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
        let (path, query) = run_list_request(Flavor::Gitea, "o", "r", &filters, None, None);
        assert_eq!(path, "repos/o/r/actions/runs");
        assert_eq!(query, vec![("status", "failure".to_owned())]);
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
