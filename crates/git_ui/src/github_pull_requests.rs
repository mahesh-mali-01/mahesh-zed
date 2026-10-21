//! Authenticated GitHub pull request API. Call these blocking methods only
//! from a background executor. Authentication stays in `gh`; response bodies and
//! subprocess stderr are deliberately excluded from errors/logs.
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

mod transport;
pub(crate) use transport::GitHubHttpError;
use transport::{included_http_response, run_gh};

const HOST: &str = "github.com";
const TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryName {
    pub owner: String,
    pub name: String,
}
impl RepositoryName {
    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split('/').collect();
        ensure!(
            parts.len() == 2
                && parts.iter().all(|part| !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
            "Expected an explicit owner/repository"
        );
        Ok(Self {
            owner: parts[0].into(),
            name: parts[1].into(),
        })
    }
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        Self::parse(&self.full_name()).map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReference {
    pub repository: RepositoryName,
    pub number: u64,
}
impl PullRequestReference {
    /// Bare numbers and #numbers require a repository supplied by the caller.
    pub fn parse(value: &str, repository: Option<&RepositoryName>) -> Result<Self> {
        let value = value.trim();
        let (repository, number) = if let Some(path) = value.strip_prefix("https://github.com/") {
            let path = path.split(['?', '#']).next().unwrap_or_default();
            let parts: Vec<_> = path.trim_end_matches('/').split('/').collect();
            ensure!(
                parts.len() >= 4
                    && parts[2] == "pull"
                    && (parts.len() == 4
                        || (parts.len() == 5
                            && matches!(parts[4], "files" | "changes" | "commits"))),
                "Expected a github.com pull request URL"
            );
            (
                RepositoryName::parse(&format!("{}/{}", parts[0], parts[1]))?,
                parts[3],
            )
        } else {
            let repository = repository
                .context("A PR number requires an explicit repository")?
                .clone();
            repository.validate()?;
            (repository, value.strip_prefix('#').unwrap_or(value))
        };
        ensure!(
            !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit()),
            "Invalid pull request number"
        );
        let number: u64 = number.parse().context("Invalid pull request number")?;
        ensure!(
            number > 0 && number <= i32::MAX as u64,
            "Invalid pull request number"
        );
        Ok(Self { repository, number })
    }
    pub fn url(&self) -> String {
        format!(
            "https://{}/{}/pull/{}",
            HOST,
            self.repository.full_name(),
            self.number
        )
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.repository.validate()?;
        ensure!(
            self.number > 0 && self.number <= i32::MAX as u64,
            "Invalid pull request number"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubUser {
    #[serde(default, alias = "node_id")]
    pub node_id: Option<String>,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestSummary {
    pub id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub is_draft: bool,
    pub author: Option<GitHubUser>,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub updated_at: String,
    #[serde(default, skip_deserializing)]
    pub head_repository_name: Option<String>,
    #[serde(default, skip_deserializing)]
    pub requested_reviewers: Vec<String>,
    #[serde(default, skip_deserializing)]
    pub requested_teams: Vec<String>,
    #[serde(default, skip_deserializing)]
    pub review_requested_by_viewer: bool,
    #[serde(default, skip_deserializing)]
    pub review_request_error: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestCommitSummary {
    pub oid: String,
    pub message_headline: String,
    #[serde(default)]
    pub author_name: Option<String>,
    pub committed_date: String,
}
#[derive(Clone, Debug, Default)]
pub struct PullRequestFilter {
    /// OPEN, CLOSED or MERGED. None means every state.
    pub state: Option<String>,
    pub author: Option<String>,
    pub base: Option<String>,
    pub source: Option<String>,
    pub head_repository: Option<String>,
    pub labels: Vec<String>,
    pub review_requested: Option<String>,
    pub text: String,
}
impl PullRequestFilter {
    fn query(&self, repository: &RepositoryName) -> Result<String> {
        repository.validate()?;
        let mut query = format!("repo:{} is:pr sort:updated-desc", repository.full_name());
        if let Some(state) = &self.state {
            let qualifier = match state.as_str() {
                "OPEN" => "is:open",
                "CLOSED" => "is:closed is:unmerged",
                "MERGED" => "is:merged",
                _ => bail!("Unknown PR state"),
            };
            query.push(' ');
            query.push_str(qualifier);
        }
        // All user input is quoted as one search value. It can never introduce
        // another repository qualifier or escape the explicitly selected repo.
        for (key, values) in [
            ("author", self.author.iter().collect::<Vec<_>>()),
            ("base", self.base.iter().collect()),
            ("head", self.source.iter().collect()),
            ("label", self.labels.iter().collect()),
            ("review-requested", self.review_requested.iter().collect()),
        ] {
            for value in values {
                query.push_str(&format!(" {key}:{}", search_literal(value)));
            }
        }
        if !self.text.trim().is_empty() {
            query.push_str(&format!(" {} in:title", search_literal(self.text.trim())));
        }
        Ok(query)
    }
}
fn search_literal(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\n', '\r'], " ")
    )
}
#[derive(Clone, Debug)]
pub struct PullRequestPage {
    pub items: Vec<PullRequestSummary>,
    pub total_count: usize,
    pub next_cursor: Option<String>,
    /// GitHub search exposes at most 1000 results; narrow filters when true.
    pub search_limit_reached: bool,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDetails {
    #[serde(flatten)]
    pub summary: PullRequestSummary,
    pub body: String,
    /// PR-associated base object, retained even after branch deletion. This is
    /// not necessarily the current branch target; never use it for branch fetch
    /// expectations. See GitHub's PullRequest.baseRefOid schema description.
    pub base_ref_oid: String,
    pub base_ref: Option<GitHubRef>,
    pub head_ref_oid: String,
    pub changed_files: usize,
    pub additions: usize,
    pub deletions: usize,
    pub mergeable: String,
    pub review_decision: Option<String>,
    pub repository: RepositoryIdentity,
    pub head_repository: Option<RepositoryIdentity>,
    pub commits: TotalCount,
    pub labels: GitHubConnection<GitHubLabel>,
    pub review_requests: GitHubConnection<GitHubReviewRequest>,
    pub recent_commits: GitHubRecentCommits,
}
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubRef {
    pub name: String,
    pub target: GitHubRefTarget,
}
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubRefTarget {
    pub oid: String,
}
impl PullRequestDetails {
    /// Live branch target for immutable fetch verification and pinned PR
    /// membership. A deleted base cannot safely fall back to baseRefOid.
    pub fn base_tip_oid(&self) -> Result<&str> {
        let reference=self.base_ref.as_ref().context("PR base branch is deleted or unavailable; reviewing this PR requires a live base branch")?;
        ensure!(
            reference.name == self.summary.base_ref_name,
            "GitHub base branch identity changed; refresh the PR"
        );
        let oid = reference.target.oid.as_str();
        ensure!(
            matches!(oid.len(), 40 | 64)
                && oid
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                && oid.bytes().any(|c| c != b'0'),
            "GitHub returned an invalid live base branch object ID"
        );
        Ok(oid)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubConnection<T> {
    pub total_count: usize,
    pub nodes: Vec<T>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubLabel {
    pub name: String,
    pub color: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubReviewRequest {
    pub requested_reviewer: Option<GitHubReviewer>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubReviewer {
    pub login: Option<String>,
    pub name: Option<String>,
    #[serde(rename = "teamName")]
    pub team_name: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubRecentCommits {
    pub nodes: Vec<GitHubRecentCommit>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubRecentCommit {
    pub commit: GitHubCommitStatus,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubCommitStatus {
    pub status_check_rollup: Option<GitHubCheckRollup>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct GitHubCheckRollup {
    pub state: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryIdentity {
    pub id: String,
    pub name_with_owner: String,
    pub url: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TotalCount {
    pub total_count: usize,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ConversationComment {
    pub id: u64,
    pub node_id: String,
    pub body: String,
    pub html_url: String,
    pub user: Option<GitHubUser>,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct PullRequestReview {
    pub id: u64,
    pub node_id: String,
    pub body: Option<String>,
    pub html_url: String,
    pub user: Option<GitHubUser>,
    pub state: String,
    pub submitted_at: Option<String>,
    pub commit_id: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ConversationPage {
    comments: Vec<ConversationComment>,
    reviews: Vec<PullRequestReview>,
    threads: Vec<crate::github_review::RemoteThread>,
}
impl ConversationPage {
    pub(crate) fn append(
        &mut self,
        page: Self,
        context: &crate::github_review::ReviewContext,
    ) -> Vec<ConversationActivity> {
        self.comments.extend(page.comments);
        self.reviews.extend(page.reviews);
        self.threads.extend(page.threads);
        // Independent connections advance at different speeds; assemble the whole loaded prefix so later threads find earlier reviews.
        assemble_activity(
            self.comments.clone(),
            self.reviews.clone(),
            self.threads.clone(),
            Vec::new(),
            context,
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConversationCursor {
    comments: Option<u64>,
    reviews: Option<u64>,
    threads: Option<String>,
    threads_complete: bool,
    seen_comments: HashSet<String>,
    seen_reviews: HashSet<String>,
    seen_thread_cursors: HashSet<String>,
}
impl Default for ConversationCursor {
    fn default() -> Self {
        Self {
            comments: Some(1),
            reviews: Some(1),
            threads: None,
            threads_complete: false,
            seen_comments: HashSet::new(),
            seen_reviews: HashSet::new(),
            seen_thread_cursors: HashSet::new(),
        }
    }
}
impl ConversationCursor {
    pub(crate) fn has_more(&self) -> bool {
        self.comments.is_some() || self.reviews.is_some() || !self.threads_complete
    }
}

/// GitHub coordinates remain typed and separate from their display label.
/// Original/outdated lines are never silently substituted for a current line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationAnchor {
    pub identity: crate::review::PullRequestIdentity,
    pub thread_id: String,
    pub head_oid: String,
    pub path: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub side: crate::github_review::DiffSide,
    pub start_side: Option<crate::github_review::DiffSide>,
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub is_outdated: bool,
    pub subject_type: crate::github_review::CommentSubject,
}
impl ConversationAnchor {
    pub fn from_thread(
        thread: &crate::github_review::RemoteThread,
        context: &crate::github_review::ReviewContext,
    ) -> Self {
        Self {
            identity: context.identity.clone(),
            thread_id: thread.id.clone(),
            head_oid: context.head_oid.clone(),
            path: thread.path.clone(),
            line: thread.line,
            start_line: thread.start_line,
            side: thread.diff_side,
            start_side: thread.start_diff_side,
            original_line: thread.original_line,
            original_start_line: thread.original_start_line,
            is_outdated: thread.is_outdated,
            subject_type: thread.subject_type,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct ConversationActivity {
    pub id: String,
    pub created_at: String,
    pub author_login: String,
    pub action_label: String,
    pub body: String,
    pub location: Option<String>,
    pub anchor: Option<ConversationAnchor>,
    pub url: Option<String>,
    pub children: Vec<ConversationActivity>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PullRequestFile {
    pub filename: String,
    pub previous_filename: Option<String>,
    pub status: String,
    /// GitHub can omit patches for binary or oversized changes. Such files
    /// remain readable locally but cannot establish line-comment coordinates.
    pub patch: Option<String>,
}
#[derive(Clone, Debug)]
pub struct PullRequestFiles {
    pub files: Vec<PullRequestFile>,
    /// REST exposes at most 3000 files. Reconcile the returned count with PR
    /// changedFiles; never interpret a capped response as a complete manifest.
    pub limit_reached: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GitHubClient {
    cancelled: Arc<AtomicBool>,
}
/// Keep in the UI alongside its Task. Replacing/dropping the guard cancels the
/// background subprocess and prevents further pagination requests.
#[derive(Debug)]
pub struct GitHubRequestGuard {
    cancelled: Arc<AtomicBool>,
}
impl GitHubRequestGuard {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
impl Drop for GitHubRequestGuard {
    fn drop(&mut self) {
        self.cancel();
    }
}
impl GitHubClient {
    pub fn cancellable() -> (Self, GitHubRequestGuard) {
        let client = Self::default();
        let guard = GitHubRequestGuard {
            cancelled: client.cancelled.clone(),
        };
        (client, guard)
    }
    fn ensure_active(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(Ordering::Acquire),
            "GitHub request cancelled"
        );
        Ok(())
    }

    pub fn viewer(&self) -> Result<GitHubUser> {
        let data = self.graphql("query { viewer { nodeId:id login name url } }", json!({}))?;
        decode(
            data.get("viewer")
                .cloned()
                .context("Missing GitHub viewer")?,
        )
    }
    /// One cursor page, suitable for a virtualized list. The caller appends pages
    /// and must surface `search_limit_reached`, not imply all PRs were retrieved.
    pub fn list_pull_requests(
        &self,
        repository: &RepositoryName,
        filter: &PullRequestFilter,
        cursor: Option<&str>,
    ) -> Result<PullRequestPage> {
        let query = format!(
            "query($query:String!,$cursor:String) {{ search(query:$query,type:ISSUE,first:50,after:$cursor) {{ issueCount pageInfo {{ hasNextPage endCursor }} nodes {{ ... on PullRequest {{ {SUMMARY_FIELDS} }} }} }} }}"
        );
        let data = self.graphql(
            &query,
            json!({"query":filter.query(repository)?,"cursor":cursor}),
        )?;
        parse_search_page(
            data.get("search")
                .context("Missing GitHub search response")?,
            cursor,
        )
    }
    /// Repository connections avoid the search API's 1,000-result ceiling.
    /// Run on a cancellable background task and publish only a complete catalog.
    pub fn pull_request_catalog(
        &self,
        repository: &RepositoryName,
    ) -> Result<Vec<PullRequestSummary>> {
        self.pull_request_catalog_with_progress(repository, |_| {})
    }

    pub fn pull_request_catalog_with_progress(
        &self,
        repository: &RepositoryName,
        mut on_page: impl FnMut(Vec<PullRequestSummary>),
    ) -> Result<Vec<PullRequestSummary>> {
        repository.validate()?;
        let viewer = self.viewer()?;
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut seen_ids = HashSet::new();
        let mut result = Vec::new();
        loop {
            self.ensure_active()?;
            let query = format!(
                "query($owner:String!,$name:String!,$cursor:String) {{ repository(owner:$owner,name:$name) {{ pullRequests(first:100,after:$cursor,orderBy:{{field:UPDATED_AT,direction:DESC}}) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ {SUMMARY_FIELDS} headRepository {{ nameWithOwner }} reviewRequests(first:100) {{ totalCount pageInfo {{ hasNextPage endCursor }} nodes {{ requestedReviewer {{ ... on User {{ login }} ... on Team {{ id }} }} }} }} }} }} }} }}"
            );
            let data = self.graphql(
                &query,
                json!({"owner":repository.owner,"name":repository.name,"cursor":cursor}),
            )?;
            let page = data
                .pointer("/repository/pullRequests")
                .context("Repository unavailable; verify access")?;
            let page_start = result.len();
            for value in page
                .get("nodes")
                .and_then(Value::as_array)
                .context("Missing PR catalog items")?
            {
                let mut item: PullRequestSummary = decode(value.clone())?;
                ensure!(
                    seen_ids.insert(item.id.clone()),
                    "PR catalog changed while loading; refresh to retry"
                );
                item.head_repository_name = value
                    .pointer("/headRepository/nameWithOwner")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut requests = value
                    .get("reviewRequests")
                    .cloned()
                    .context("Missing review requests")?;
                let mut request_cursors = HashSet::new();
                loop {
                    for request in requests
                        .get("nodes")
                        .and_then(Value::as_array)
                        .context("Missing reviewer nodes")?
                    {
                        if let Some(login) = request
                            .pointer("/requestedReviewer/login")
                            .and_then(Value::as_str)
                        {
                            item.requested_reviewers.push(login.to_owned());
                        }
                        if let Some(id) = request
                            .pointer("/requestedReviewer/id")
                            .and_then(Value::as_str)
                        {
                            item.requested_teams.push(id.to_owned());
                        }
                    }
                    let Some(next) = collection_cursor(&requests, &mut request_cursors)? else {
                        break;
                    };
                    let data = self.graphql("query($id:ID!,$cursor:String) { node(id:$id) { ... on PullRequest { reviewRequests(first:100,after:$cursor) { pageInfo { hasNextPage endCursor } nodes { requestedReviewer { ... on User { login } ... on Team { id } } } } } } }",json!({"id":item.id,"cursor":next}))?;
                    requests = data
                        .pointer("/node/reviewRequests")
                        .cloned()
                        .context("PR review requests unavailable")?;
                }
                item.review_requested_by_viewer = item
                    .requested_reviewers
                    .iter()
                    .any(|login| login.eq_ignore_ascii_case(&viewer.login));
                result.push(item);
            }
            on_page(result[page_start..].to_vec());
            cursor = collection_cursor(page, &mut seen_cursors)?;
            if cursor.is_none() {
                let count = page
                    .get("totalCount")
                    .and_then(Value::as_u64)
                    .context("Missing PR catalog count")? as usize;
                ensure!(
                    result.len() == count,
                    "PR catalog changed while loading; refresh to retry"
                );
                break;
            }
        }
        if result.iter().any(|item| !item.requested_teams.is_empty()) {
            match self.rest_pages::<Value>("user/teams") {
                Ok(teams) => {
                    let team_ids = teams
                        .iter()
                        .filter_map(|team| team.get("node_id").and_then(Value::as_str))
                        .collect::<HashSet<_>>();
                    for item in &mut result {
                        item.review_requested_by_viewer |= item
                            .requested_teams
                            .iter()
                            .any(|team| team_ids.contains(team.as_str()));
                    }
                }
                Err(error) => {
                    self.ensure_active()?;
                    for item in &mut result {
                        if !item.requested_teams.is_empty() && !item.review_requested_by_viewer {
                            item.review_request_error =
                                Some(format!("Could not verify team review requests: {error}"));
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    /// PR commit connections can stop at 250 despite a larger totalCount.
    /// Explicitly paginated immutable comparisons return the full head-only set.
    pub fn pull_request_commits(
        &self,
        reference: &PullRequestReference,
    ) -> Result<Vec<PullRequestCommitSummary>> {
        reference.validate()?;
        let before = self.pull_request(reference)?;
        let base = before.base_tip_oid()?.to_owned();
        let head = before.head_ref_oid.clone();
        ensure!(
            [&base, &head]
                .iter()
                .all(|oid| oid.len() == 40 && oid.bytes().all(|byte| byte.is_ascii_hexdigit())),
            "Invalid immutable PR revision"
        );
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for page in 1u64.. {
            self.ensure_active()?;
            let value = self
                .rest_json(
                    "GET",
                    &format!(
                        "repos/{}/compare/{base}...{head}?per_page=100&page={page}",
                        reference.repository.full_name()
                    ),
                    None,
                )?
                .context("Missing commit comparison")?;
            let complete = append_comparison_commits(
                &value,
                before.commits.total_count,
                &mut seen,
                &mut result,
            )?;
            if complete {
                break;
            }
        }
        let after = self.pull_request(reference)?;
        ensure!(
            after.head_ref_oid == head
                && after.base_tip_oid()? == base
                && after.commits.total_count == before.commits.total_count,
            "PR changed while loading commits; refresh to retry"
        );
        Ok(result)
    }
    pub fn repository_identity(&self, repository: &RepositoryName) -> Result<RepositoryIdentity> {
        repository.validate()?;
        let data=self.graphql("query($owner:String!,$name:String!) { repository(owner:$owner,name:$name) { id nameWithOwner url } }",json!({"owner":repository.owner,"name":repository.name}))?;
        decode(
            data.get("repository")
                .filter(|value| !value.is_null())
                .cloned()
                .context("Repository unavailable; verify access")?,
        )
    }
    pub fn pull_request(&self, reference: &PullRequestReference) -> Result<PullRequestDetails> {
        reference.validate()?;
        let query = format!(
            "query($owner:String!,$name:String!,$number:Int!) {{ repository(owner:$owner,name:$name) {{ pullRequest(number:$number) {{ {SUMMARY_FIELDS} body baseRefOid baseRef {{ name target {{ oid }} }} headRefOid changedFiles additions deletions mergeable reviewDecision commits {{ totalCount }} labels(first:100) {{ totalCount nodes {{ name color }} }} reviewRequests(first:100) {{ totalCount nodes {{ requestedReviewer {{ ... on User {{ login name }} ... on Team {{ teamName:name }} }} }} }} recentCommits:commits(last:1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }} repository {{ id nameWithOwner url }} headRepository {{ id nameWithOwner url }} }} }} }}"
        );
        let data = self.graphql(&query, json!({"owner":reference.repository.owner,"name":reference.repository.name,"number":reference.number}))?;
        let value = data
            .pointer("/repository/pullRequest")
            .filter(|v| !v.is_null())
            .cloned()
            .context("PR unavailable: verify repository access and number")?;
        let details: PullRequestDetails = decode(value)?;
        ensure!(
            details.summary.number == reference.number
                && details
                    .repository
                    .name_with_owner
                    .eq_ignore_ascii_case(&reference.repository.full_name()),
            "GitHub returned a different PR identity"
        );
        Ok(details)
    }
    pub fn conversation(
        &self,
        reference: &PullRequestReference,
    ) -> Result<Vec<ConversationComment>> {
        reference.validate()?;
        self.rest_pages(&format!(
            "repos/{}/issues/{}/comments",
            reference.repository.full_name(),
            reference.number
        ))
    }
    pub fn reviews(&self, reference: &PullRequestReference) -> Result<Vec<PullRequestReview>> {
        reference.validate()?;
        self.rest_pages(&format!(
            "repos/{}/pulls/{}/reviews",
            reference.repository.full_name(),
            reference.number
        ))
    }
    pub(crate) fn activity_page(
        &self,
        context: &crate::github_review::ReviewContext,
        cursor: &ConversationCursor,
    ) -> Result<(ConversationPage, ConversationCursor)> {
        context.validate()?;
        if cursor.threads_complete {
            crate::github_review::ReviewService::new(self).verify_context(context)?;
        }
        let reference = &context.reference;
        let mut next = cursor.clone();
        let mut comments: Vec<ConversationComment> = Vec::new();
        let mut reviews: Vec<PullRequestReview> = Vec::new();
        if let Some(page) = cursor.comments {
            comments = self.conversation_rest_page(
                &format!(
                    "repos/{}/issues/{}/comments",
                    reference.repository.full_name(),
                    reference.number
                ),
                page,
            )?;
            for comment in &comments {
                ensure!(
                    next.seen_comments.insert(comment.node_id.clone()),
                    "Conversation changed during pagination; refresh"
                );
            }
            next.comments = (comments.len() == 20).then_some(page + 1);
        }
        if let Some(page) = cursor.reviews {
            reviews = self.conversation_rest_page(
                &format!(
                    "repos/{}/pulls/{}/reviews",
                    reference.repository.full_name(),
                    reference.number
                ),
                page,
            )?;
            for review in &reviews {
                ensure!(
                    next.seen_reviews.insert(review.node_id.clone()),
                    "Reviews changed during pagination; refresh"
                );
            }
            next.reviews = (reviews.len() == 20).then_some(page + 1);
        }
        let threads = if !cursor.threads_complete {
            let (threads, continuation) = crate::github_review::ReviewService::new(self)
                .load_threads_page(context, cursor.threads.as_deref())?;
            if let Some(continuation) = continuation.as_ref() {
                ensure!(
                    next.seen_thread_cursors.insert(continuation.clone()),
                    "Repeated conversation cursor; refresh"
                );
            }
            next.threads_complete = continuation.is_none();
            next.threads = continuation;
            threads
        } else {
            Vec::new()
        };
        Ok((
            ConversationPage {
                comments,
                reviews,
                threads,
            },
            next,
        ))
    }

    fn conversation_rest_page<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        page: u64,
    ) -> Result<Vec<T>> {
        self.ensure_active()?;
        let body = run_gh(
            &[
                "--method",
                "GET",
                &format!("{endpoint}?per_page=20&page={page}"),
            ],
            None,
            &self.cancelled,
            Instant::now() + TIMEOUT,
        )?;
        serde_json::from_slice(&body).context("Invalid conversation response")
    }

    pub fn files(&self, reference: &PullRequestReference) -> Result<PullRequestFiles> {
        reference.validate()?;
        let deadline = Instant::now() + TIMEOUT;
        let mut files = Vec::new();
        let mut seen = HashSet::new();
        for page in 1..=30 {
            self.ensure_active()?;
            let body = run_gh(
                &[
                    "--method",
                    "GET",
                    &format!(
                        "repos/{}/pulls/{}/files?per_page=100&page={page}",
                        reference.repository.full_name(),
                        reference.number
                    ),
                ],
                None,
                &self.cancelled,
                deadline,
            )?;
            let values: Vec<PullRequestFile> =
                serde_json::from_slice(&body).context("Invalid GitHub PR file response")?;
            let count = values.len();
            for file in values {
                ensure!(
                    seen.insert(file.filename.clone()),
                    "GitHub PR file pagination changed while loading; refresh the PR"
                );
                files.push(file);
            }
            if count < 100 {
                return Ok(PullRequestFiles {
                    files,
                    limit_reached: false,
                });
            }
        }
        Ok(PullRequestFiles {
            files,
            limit_reached: true,
        })
    }

    fn rest_pages<T: DeserializeOwned>(&self, endpoint: &str) -> Result<Vec<T>> {
        let deadline = Instant::now() + TIMEOUT;
        let mut result = Vec::new();
        // Explicit pages keep each subprocess bounded. Empty final page handles
        // an exact multiple of 100 without relying on a guessed total/cap.
        let mut seen = HashSet::new();
        for page in 1u64.. {
            self.ensure_active()?;
            ensure!(
                Instant::now() < deadline,
                "GitHub collection request timed out"
            );
            let body = run_gh(
                &[
                    "--method",
                    "GET",
                    &format!("{endpoint}?per_page=100&page={page}"),
                ],
                None,
                &self.cancelled,
                deadline,
            )?;
            let values: Vec<Value> =
                serde_json::from_slice(&body).context("Invalid GitHub collection response")?;
            let length = values.len();
            for value in values {
                let id = value
                    .get("id")
                    .and_then(Value::as_u64)
                    .context("GitHub collection item missing identity")?;
                ensure!(
                    seen.insert(id),
                    "GitHub pagination changed while loading; refresh the PR"
                );
                result.push(decode(value)?);
            }
            if length < 100 {
                return Ok(result);
            }
        }
        unreachable!()
    }
    /// Shared structured transport. For mutations, any returned error must be
    /// treated as delivery-unknown unless a typed authoritative HTTP rejection
    /// proves non-delivery. CLI/process failures alone cannot prove rejection.
    pub(crate) fn rest_json(
        &self,
        method: &str,
        endpoint: &str,
        input: Option<Value>,
    ) -> Result<Option<Value>> {
        self.ensure_active()?;
        validate_rest_endpoint(method, endpoint)?;
        ensure!(
            method != "GET" || input.is_none(),
            "GET GitHub requests cannot include a body"
        );
        let deadline = Instant::now() + TIMEOUT;
        let bytes = input.map(|input| serde_json::to_vec(&input)).transpose()?;
        let mut args = vec!["--method", method, endpoint];
        let include_status = method != "GET";
        if include_status {
            args.push("--include");
        }
        if bytes.is_some() {
            args.extend(["--input", "-"]);
        }
        let response = run_gh(&args, bytes.as_deref(), &self.cancelled, deadline)?;
        let body = if include_status {
            let (status, body) = included_http_response(&response)?;
            if !(200..=299).contains(&status) {
                return Err(GitHubHttpError::from_response(status, body).into());
            }
            body
        } else {
            response.as_slice()
        };
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        Ok(Some(
            serde_json::from_slice(&body).context("Invalid GitHub JSON response")?,
        ))
    }

    pub(crate) fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.ensure_active()?;
        let deadline = Instant::now() + TIMEOUT;
        let input = serde_json::to_vec(&json!({"query":query,"variables":variables}))?;
        let bytes = run_gh(
            &["graphql", "--input", "-"],
            Some(&input),
            &self.cancelled,
            deadline,
        )?;
        let envelope: Value =
            serde_json::from_slice(&bytes).context("Invalid GitHub GraphQL response")?;
        // Partial data plus errors must not become a successful empty UI.
        if !envelope
            .get("errors")
            .is_none_or(|e| e.as_array().is_some_and(Vec::is_empty))
        {
            return Err(GitHubHttpError::from_response(200, &bytes).into());
        }
        envelope
            .get("data")
            .filter(|v| !v.is_null())
            .cloned()
            .context("Missing GitHub GraphQL data")
    }
}
fn timeline_identity(value: &Value) -> Result<String> {
    let event = value
        .get("event")
        .and_then(Value::as_str)
        .context("Timeline event missing type")?;
    if let Some(id) = value.get("node_id").and_then(Value::as_str) {
        return Ok(format!("{event}:{id}"));
    }
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        return Ok(format!("{event}:{id}"));
    }
    if let Some(sha) = value.get("sha").and_then(Value::as_str) {
        return Ok(format!("{event}:{sha}"));
    }
    // Cross references have no event id. Source issue identity + actor + creation
    // time form a semantic key; a timestamp alone must never merge activities.
    if event == "cross-referenced" {
        let source = value
            .pointer("/source/issue/node_id")
            .and_then(Value::as_str)
            .context("Timeline cross reference missing source identity")?;
        let actor = value
            .pointer("/actor/login")
            .and_then(Value::as_str)
            .unwrap_or("deleted user");
        let date = value
            .get("created_at")
            .and_then(Value::as_str)
            .context("Timeline cross reference missing date")?;
        return Ok(format!("{event}:{source}:{actor}:{date}"));
    }
    bail!("Timeline event missing semantic identity")
}

fn assemble_activity(
    comments: Vec<ConversationComment>,
    reviews: Vec<PullRequestReview>,
    threads: Vec<crate::github_review::RemoteThread>,
    events: Vec<Value>,
    context: &crate::github_review::ReviewContext,
) -> Vec<ConversationActivity> {
    let viewer_id = context.identity.viewer_node_id.as_str();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for comment in comments {
        if !seen.insert(comment.node_id.clone()) {
            continue;
        }
        items.push(ConversationActivity {
            id: comment.node_id,
            created_at: comment.created_at,
            author_login: comment
                .user
                .map(|u| u.login)
                .unwrap_or_else(|| "Deleted user".into()),
            action_label: "commented".into(),
            body: comment.body,
            url: Some(comment.html_url),
            ..Default::default()
        });
    }
    for review in reviews {
        let pending = review.state.eq_ignore_ascii_case("PENDING");
        if pending && review.user.as_ref().and_then(|u| u.node_id.as_deref()) != Some(viewer_id) {
            continue;
        }
        if !seen.insert(review.node_id.clone()) {
            continue;
        }
        items.push(ConversationActivity {
            id: review.node_id,
            created_at: review.submitted_at.unwrap_or_default(),
            author_login: review
                .user
                .map(|u| u.login)
                .unwrap_or_else(|| "Deleted user".into()),
            action_label: if pending {
                "Pending review · only you".into()
            } else {
                review.state
            },
            body: review.body.unwrap_or_default(),
            url: Some(review.html_url),
            ..Default::default()
        });
    }
    for thread in threads {
        if !seen.insert(thread.id.clone()) {
            continue;
        }
        let review_id = thread
            .comments
            .first()
            .and_then(|c| c.pull_request_review.as_ref())
            .map(|r| r.id.clone());
        let anchor = ConversationAnchor::from_thread(&thread, context);
        let children: Vec<_> = thread
            .comments
            .into_iter()
            .filter(|c| {
                !c.state.eq_ignore_ascii_case("PENDING")
                    || c.author.as_ref().and_then(|a| a.node_id.as_deref()) == Some(viewer_id)
            })
            .map(|c| ConversationActivity {
                id: c.id,
                created_at: c.created_at,
                author_login: c
                    .author
                    .map(|a| a.login)
                    .unwrap_or_else(|| "Deleted user".into()),
                action_label: if c.state.eq_ignore_ascii_case("PENDING") {
                    "Pending comment · only you".into()
                } else {
                    "commented".into()
                },
                body: c.body,
                anchor: Some(anchor.clone()),
                url: Some(c.url),
                ..Default::default()
            })
            .collect();
        if children.is_empty() {
            continue;
        }
        let activity = ConversationActivity {
            id: thread.id,
            created_at: children[0].created_at.clone(),
            action_label: format!(
                "Discussion{}{}",
                if thread.is_resolved {
                    " · Resolved"
                } else {
                    ""
                },
                if thread.is_outdated {
                    " · Outdated"
                } else {
                    ""
                }
            ),
            anchor: Some(anchor),
            location: Some(match thread.line.or(thread.original_line) {
                Some(line) => format!("{}:{line}", thread.path),
                None => thread.path,
            }),
            children,
            ..Default::default()
        };
        if let Some(parent) = review_id.and_then(|id| items.iter_mut().find(|i| i.id == id)) {
            parent.children.push(activity);
        } else {
            items.push(activity);
        }
    }
    for value in events {
        let event = value
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // These have richer dedicated entities above. Notifications are not
        // conversation entries and are excluded by semantic type, not timestamp.
        if matches!(
            event,
            "commented" | "reviewed" | "mentioned" | "subscribed" | "unsubscribed"
        ) {
            continue;
        }
        let Ok(id) = timeline_identity(&value) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let date = value
            .get("created_at")
            .or_else(|| value.pointer("/committer/date"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let action = match event {
            "committed" => format!(
                "pushed commit {}",
                value
                    .get("sha")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .chars()
                    .take(7)
                    .collect::<String>()
            ),
            "review_requested" => format!(
                "requested review from {}",
                value
                    .pointer("/requested_reviewer/login")
                    .or_else(|| value.pointer("/requested_team/name"))
                    .and_then(Value::as_str)
                    .unwrap_or("reviewer")
            ),
            "labeled" | "unlabeled" => format!(
                "{} {}",
                event,
                value
                    .pointer("/label/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
            _ => event.replace(['_', '-'], " "),
        };
        items.push(ConversationActivity {
            id,
            created_at: date.into(),
            author_login: value
                .pointer("/actor/login")
                .or_else(|| value.pointer("/author/name"))
                .and_then(Value::as_str)
                .unwrap_or("Deleted user")
                .into(),
            action_label: action,
            body: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            url: value
                .get("html_url")
                .or_else(|| value.pointer("/source/issue/html_url"))
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Default::default()
        });
    }
    items.sort_by(|a, b| {
        (a.created_at.is_empty(), &a.created_at, &a.id).cmp(&(
            b.created_at.is_empty(),
            &b.created_at,
            &b.id,
        ))
    });
    items
}

fn validate_rest_endpoint(method: &str, endpoint: &str) -> Result<()> {
    ensure!(
        matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE"),
        "Unsupported GitHub HTTP method"
    );
    ensure!(
        !endpoint
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
            && !endpoint.contains(['\\', '#']),
        "Invalid GitHub API endpoint"
    );
    let path = endpoint
        .split('?')
        .next()
        .context("Missing GitHub API path")?;
    let parts = path.split('/').collect::<Vec<_>>();
    ensure!(
        parts.len() >= 4
            && parts[0] == "repos"
            && parts.iter().all(|part| !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
        "GitHub API endpoint must be a relative repository path"
    );
    RepositoryName::parse(&format!("{}/{}", parts[1], parts[2]))?;
    Ok(())
}

const SUMMARY_FIELDS: &str =
    "id number title url state isDraft author { login url } baseRefName headRefName updatedAt";
fn append_comparison_commits(
    value: &Value,
    expected: usize,
    seen: &mut HashSet<String>,
    result: &mut Vec<PullRequestCommitSummary>,
) -> Result<bool> {
    let total = value
        .get("total_commits")
        .and_then(Value::as_u64)
        .context("Missing comparison commit count")? as usize;
    ensure!(
        total == expected,
        "PR history count differs from its branch comparison; refresh to retry"
    );
    let commits = value
        .get("commits")
        .and_then(Value::as_array)
        .context("Missing comparison commits")?;
    for commit in commits {
        let oid = commit
            .get("sha")
            .and_then(Value::as_str)
            .context("Missing commit identity")?;
        ensure!(
            seen.insert(oid.to_owned()),
            "Repeated comparison commit; refresh to retry"
        );
        result.push(PullRequestCommitSummary {
            oid: oid.to_owned(),
            message_headline: commit
                .pointer("/commit/message")
                .and_then(Value::as_str)
                .context("Missing commit message")?
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned(),
            author_name: commit
                .pointer("/commit/author/name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            committed_date: commit
                .pointer("/commit/committer/date")
                .and_then(Value::as_str)
                .context("Missing commit date")?
                .to_owned(),
        });
    }
    ensure!(result.len() <= expected, "Too many comparison commits");
    ensure!(
        !commits.is_empty() || result.len() == expected,
        "Incomplete comparison history; refresh to retry"
    );
    Ok(result.len() == expected)
}

fn collection_cursor(value: &Value, seen: &mut HashSet<String>) -> Result<Option<String>> {
    let info = value
        .get("pageInfo")
        .context("Missing collection pagination")?;
    let has_next = info
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .context("Missing next-page flag")?;
    if !has_next {
        return Ok(None);
    }
    let cursor = info
        .get("endCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .context("Missing collection cursor")?;
    ensure!(
        seen.insert(cursor.to_owned()),
        "Repeated collection cursor; refresh to retry"
    );
    Ok(Some(cursor.to_owned()))
}
fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("Unsupported GitHub response shape")
}
fn parse_search_page(value: &Value, previous_cursor: Option<&str>) -> Result<PullRequestPage> {
    let total_count = value
        .get("issueCount")
        .and_then(Value::as_u64)
        .context("Missing PR search total")? as usize;
    let page_info = value
        .get("pageInfo")
        .context("Missing PR search pagination")?;
    let has_next = page_info
        .get("hasNextPage")
        .and_then(Value::as_bool)
        .context("Missing PR pagination status")?;
    let next_cursor = if has_next {
        let next = page_info
            .get("endCursor")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .context("GitHub omitted next PR cursor")?;
        ensure!(
            Some(next) != previous_cursor,
            "GitHub repeated a PR pagination cursor"
        );
        Some(next.into())
    } else {
        None
    };
    let nodes = value
        .get("nodes")
        .and_then(Value::as_array)
        .context("Missing PR search results")?;
    let items = nodes
        .iter()
        .cloned()
        .map(decode)
        .collect::<Result<Vec<_>>>()?;
    Ok(PullRequestPage {
        items,
        total_count,
        next_cursor,
        search_limit_reached: total_count > 1000,
    })
}

#[cfg(test)]
mod tests;
