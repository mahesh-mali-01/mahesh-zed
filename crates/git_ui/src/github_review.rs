//! GitHub owns remote threads and reviews. Local drafts/outbox belong to ReviewDb.
//! These blocking operations use the cancellable shared transport off the UI thread.
mod protocol;
#[cfg(test)]
mod tests;

use protocol::{
    COMMENT_FIELDS, GUARD_FIELDS, PAGE_INFO, Pager, REVIEW_FIELDS, THREAD_FIELDS, actor_id,
    check_guard, check_target, collection_total, comment_matches, decode, decode_nodes,
    decode_thread, deserialize_database_id, ensure_body, ensure_no_pending, nodes, not_sent,
    owns_pending, receipt, rest_comment_identity, rest_write_error, result_id,
    thread_matches_anchor, unknown, validate_id,
};

use crate::{
    github_pull_requests::{GitHubClient, PullRequestReference},
    review::PullRequestIdentity,
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

// Official API contracts: https://docs.github.com/en/graphql/reference/pulls
// https://docs.github.com/en/rest/pulls/comments
// https://docs.github.com/en/rest/pulls/reviews

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewContext {
    pub reference: PullRequestReference,
    pub identity: PullRequestIdentity,
    pub head_oid: String,
    /// Actual base branch tip, rather than GitHub's historical baseRefOid.
    pub base_tip_oid: String,
}
impl ReviewContext {
    pub fn validate(&self) -> Result<()> {
        self.reference.validate()?;
        self.identity.validate()?;
        for oid in [&self.head_oid, &self.base_tip_oid] {
            ensure!(
                (oid.len() == 40 || oid.len() == 64)
                    && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && oid.bytes().any(|byte| byte != b'0'),
                "Invalid pinned PR revision"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DiffSide {
    Left,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CommentSubject {
    Line,
    File,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentAnchor {
    pub path: String,
    pub commit_oid: String,
    pub side: DiffSide,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub start_side: Option<DiffSide>,
    pub subject_type: CommentSubject,
}
impl CommentAnchor {
    /// The caller must additionally map this anchor to the current canonical PR
    /// diff; visual rows and older/subset comparisons are not API coordinates.
    pub fn validate(&self, context: &ReviewContext) -> Result<()> {
        validate_path(&self.path)?;
        ensure!(
            self.commit_oid == context.head_oid,
            "Comment belongs to a different PR revision"
        );
        match self.subject_type {
            CommentSubject::File => ensure!(
                self.line.is_none() && self.start_line.is_none() && self.start_side.is_none(),
                "File comments cannot have line ranges"
            ),
            CommentSubject::Line => {
                let line = self.line.context("Line comments require a line")?;
                ensure!(line > 0 && line <= i32::MAX as u32, "Invalid comment line");
                ensure!(
                    self.start_line.is_some() == self.start_side.is_some(),
                    "Incomplete comment range"
                );
                if let Some(start) = self.start_line {
                    // Cross-side ranges need explicit mapping not currently available.
                    ensure!(
                        start > 0 && start <= line && self.start_side == Some(self.side),
                        "Invalid comment range"
                    );
                }
            }
        }
        Ok(())
    }
}
fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.starts_with('/')
            && !path.contains('\0')
            && !path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."),
        "Invalid repository path"
    );
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteActor {
    pub login: String,
    #[serde(default, rename = "nodeId")]
    pub node_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitRef {
    pub oid: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteNode {
    pub id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommentReview {
    pub id: String,
    pub state: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteComment {
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_database_id")]
    pub full_database_id: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub reply_to: Option<RemoteNode>,
    #[serde(default)]
    pub pull_request_review: Option<CommentReview>,
    pub body: String,
    pub url: String,
    pub author: Option<RemoteActor>,
    pub created_at: String,
    pub updated_at: String,
    pub path: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub diff_hunk: String,
    pub commit: Option<CommitRef>,
    pub original_commit: Option<CommitRef>,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
    pub viewer_did_author: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteReview {
    pub id: String,
    pub state: String,
    pub body: String,
    pub author: Option<RemoteActor>,
    pub commit: Option<CommitRef>,
    pub url: String,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
    pub viewer_did_author: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteThread {
    pub id: String,
    pub path: String,
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub diff_side: DiffSide,
    pub start_diff_side: Option<DiffSide>,
    pub subject_type: CommentSubject,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub viewer_can_reply: bool,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
    #[serde(default)]
    pub comments: Vec<RemoteComment>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ViewedState {
    Viewed,
    Unviewed,
    Dismissed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteViewedFile {
    pub path: String,
    pub viewer_viewed_state: ViewedState,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}
#[derive(Clone, Debug)]
pub struct WriteReceipt<T> {
    pub operation_id: String,
    pub remote: T,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewWriteError {
    /// Preflight or an authoritative HTTP response proves the write was not applied.
    NotSent(String),
    /// This operation must be reconciled before the UI offers a retry.
    DeliveryUnknown {
        operation_id: String,
        remote_id: Option<String>,
    },
}
impl std::fmt::Display for ReviewWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(message) => f.write_str(message),
            Self::DeliveryUnknown { .. } => f.write_str(
                "GitHub write outcome is unconfirmed. Refresh and reconcile before retrying.",
            ),
        }
    }
}
impl std::error::Error for ReviewWriteError {}
pub type WriteResult<T> = std::result::Result<WriteReceipt<T>, ReviewWriteError>;

/// Injectable transport keeps pagination/uncertain-write tests independent of GH
/// authentication and prevents production/private PR contents entering fixtures.
pub trait ReviewTransport {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value>;
    fn rest_json(
        &self,
        method: &str,
        endpoint: &str,
        input: Option<Value>,
    ) -> Result<Option<Value>>;
}
impl ReviewTransport for GitHubClient {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        GitHubClient::graphql(self, query, variables)
    }
    fn rest_json(
        &self,
        method: &str,
        endpoint: &str,
        input: Option<Value>,
    ) -> Result<Option<Value>> {
        GitHubClient::rest_json(self, method, endpoint, input)
    }
}
pub struct ReviewService<'a, T: ReviewTransport = GitHubClient> {
    transport: &'a T,
}
impl<'a, T: ReviewTransport> ReviewService<'a, T> {
    pub fn new(transport: &'a T) -> Self {
        Self { transport }
    }

    pub fn verify_context(&self, context: &ReviewContext) -> Result<()> {
        context.validate()?;
        let query = format!(
            "query($pr:ID!) {{ viewer {{ id }} node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} }} }} }}"
        );
        let data = self
            .transport
            .graphql(&query, json!({"pr":context.identity.pull_request_node_id}))?;
        check_guard(&data, data.get("node"), context)
    }

    pub fn load_threads(&self, context: &ReviewContext) -> Result<Vec<RemoteThread>> {
        context.validate()?;
        let query = format!(
            "query($pr:ID!,$cursor:String) {{ viewer {{ id }} node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} reviewThreads(first:50,after:$cursor) {{ {PAGE_INFO} nodes {{ {THREAD_FIELDS} comments(first:50) {{ {PAGE_INFO} nodes {{ {COMMENT_FIELDS} }} }} }} }} }} }} }}"
        );
        let mut pager = Pager::default();
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        let mut expected_total = None;
        loop {
            let data = self.transport.graphql(
                &query,
                json!({"pr":context.identity.pull_request_node_id,"cursor":pager.cursor}),
            )?;
            check_guard(&data, data.get("node"), context)?;
            let connection = data
                .pointer("/node/reviewThreads")
                .context("Missing review threads")?;
            let total = collection_total(connection)?;
            ensure!(
                expected_total.is_none_or(|expected| expected == total),
                "Threads changed during pagination; refresh"
            );
            expected_total = Some(total);
            for value in nodes(connection)? {
                let mut thread = decode_thread(value)?;
                ensure!(
                    seen.insert(thread.id.clone()),
                    "Review threads changed during pagination; refresh"
                );
                let comments = value.get("comments").context("Missing thread comments")?;
                thread.comments = decode_nodes(comments)?;
                self.finish_thread_comments(context, &mut thread, comments)?;
                result.push(thread);
            }
            if !pager.advance(connection)? {
                ensure!(
                    result.len() as u64 == total,
                    "Thread list is incomplete; refresh"
                );
                break;
            }
        }
        Ok(result)
    }

    pub(crate) fn load_threads_page(
        &self,
        context: &ReviewContext,
        cursor: Option<&str>,
    ) -> Result<(Vec<RemoteThread>, Option<String>)> {
        context.validate()?;
        let query = format!(
            "query($pr:ID!,$cursor:String) {{ viewer {{ id }} node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} reviewThreads(first:20,after:$cursor) {{ {PAGE_INFO} nodes {{ {THREAD_FIELDS} comments(first:50) {{ {PAGE_INFO} nodes {{ {COMMENT_FIELDS} }} }} }} }} }} }} }}"
        );
        let data = self.transport.graphql(
            &query,
            json!({"pr":context.identity.pull_request_node_id,"cursor":cursor}),
        )?;
        check_guard(&data, data.get("node"), context)?;
        let connection = data
            .pointer("/node/reviewThreads")
            .context("Missing review threads")?;
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for value in nodes(connection)? {
            let mut thread = decode_thread(value)?;
            ensure!(
                seen.insert(thread.id.clone()),
                "Repeated review thread; refresh"
            );
            let comments = value.get("comments").context("Missing thread comments")?;
            thread.comments = decode_nodes(comments)?;
            self.finish_thread_comments(context, &mut thread, comments)?;
            result.push(thread);
        }
        let mut pager = Pager::default();
        let next = if pager.advance(connection)? {
            pager.cursor
        } else {
            None
        };
        ensure!(
            next.as_deref().is_none_or(|next| Some(next) != cursor),
            "Repeated review cursor; refresh"
        );
        Ok((result, next))
    }

    fn finish_thread_comments(
        &self,
        context: &ReviewContext,
        thread: &mut RemoteThread,
        initial: &Value,
    ) -> Result<()> {
        let mut seen: HashSet<String> = thread
            .comments
            .iter()
            .map(|comment| comment.id.clone())
            .collect();
        ensure!(
            seen.len() == thread.comments.len(),
            "Repeated thread comment"
        );
        let total = collection_total(initial)?;
        let mut pager = Pager::default();
        if !pager.advance(initial)? {
            ensure!(
                thread.comments.len() as u64 == total,
                "Thread comments are incomplete; refresh"
            );
            return Ok(());
        }
        let query = format!(
            "query($pr:ID!,$id:ID!,$cursor:String) {{ viewer {{ id }} pullRequest:node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} }} }} node(id:$id) {{ ... on PullRequestReviewThread {{ id pullRequest {{ id }} comments(first:100,after:$cursor) {{ {PAGE_INFO} nodes {{ {COMMENT_FIELDS} }} }} }} }} }}"
        );
        loop {
            let data = self.transport.graphql(&query, json!({"pr":context.identity.pull_request_node_id,"id":thread.id,"cursor":pager.cursor}))?;
            check_guard(&data, data.get("pullRequest"), context)?;
            let target = data
                .get("node")
                .context("Thread disappeared while loading")?;
            check_target(target, context, &thread.id)?;
            let connection = target
                .get("comments")
                .context("Missing thread reply page")?;
            ensure!(
                collection_total(connection)? == total,
                "Replies changed during pagination; refresh"
            );
            for comment in decode_nodes::<RemoteComment>(connection)? {
                ensure!(
                    seen.insert(comment.id.clone()),
                    "Thread replies changed during pagination; refresh"
                );
                thread.comments.push(comment);
            }
            if !pager.advance(connection)? {
                ensure!(
                    thread.comments.len() as u64 == total,
                    "Thread comments are incomplete; refresh"
                );
                break;
            }
        }
        Ok(())
    }

    pub fn load_reviews(&self, context: &ReviewContext) -> Result<Vec<RemoteReview>> {
        self.load_connection(context, "reviews", REVIEW_FIELDS)
    }
    pub fn load_viewed(&self, context: &ReviewContext) -> Result<Vec<RemoteViewedFile>> {
        let files: Vec<RemoteViewedFile> =
            self.load_connection(context, "files", "path viewerViewedState")?;
        let mut seen = HashSet::new();
        ensure!(
            files.iter().all(|file| seen.insert(file.path.clone())),
            "PR file pagination changed; refresh"
        );
        Ok(files)
    }
    fn load_connection<D: DeserializeOwned>(
        &self,
        context: &ReviewContext,
        field: &str,
        fields: &str,
    ) -> Result<Vec<D>> {
        context.validate()?;
        let query = format!(
            "query($pr:ID!,$cursor:String) {{ viewer {{ id }} node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} {field}(first:100,after:$cursor) {{ {PAGE_INFO} nodes {{ {fields} }} }} }} }} }}"
        );
        let mut pager = Pager::default();
        let mut result = Vec::new();
        let mut expected_total = None;
        let mut seen = HashSet::new();
        loop {
            let data = self.transport.graphql(
                &query,
                json!({"pr":context.identity.pull_request_node_id,"cursor":pager.cursor}),
            )?;
            check_guard(&data, data.get("node"), context)?;
            let connection = data
                .get("node")
                .and_then(|node| node.get(field))
                .context("Missing GitHub collection")?;
            let total = connection
                .get("totalCount")
                .and_then(Value::as_u64)
                .context("Missing collection total")?;
            ensure!(
                expected_total.is_none_or(|expected| expected == total),
                "GitHub collection changed during pagination; refresh"
            );
            expected_total = Some(total);
            for value in nodes(connection)? {
                let id = value
                    .get("id")
                    .or_else(|| value.get("path"))
                    .and_then(Value::as_str)
                    .context("Collection item lacks identity")?;
                ensure!(
                    seen.insert(id.to_string()),
                    "Collection changed during pagination; refresh"
                );
                result.push(decode(value.clone())?);
            }
            if !pager.advance(connection)? {
                ensure!(
                    result.len() as u64 == total,
                    "GitHub collection is incomplete; refresh or narrow scope"
                );
                return Ok(result);
            }
        }
    }
    pub fn load_comment(&self, context: &ReviewContext, id: &str) -> Result<Option<RemoteComment>> {
        self.node_resource(context, id, "PullRequestReviewComment", COMMENT_FIELDS)
    }
    pub fn load_review(&self, context: &ReviewContext, id: &str) -> Result<Option<RemoteReview>> {
        self.node_resource(context, id, "PullRequestReview", REVIEW_FIELDS)
    }
    pub fn load_thread(&self, context: &ReviewContext, id: &str) -> Result<Option<RemoteThread>> {
        let fields = format!(
            "{THREAD_FIELDS} comments(first:50) {{ {PAGE_INFO} nodes {{ {COMMENT_FIELDS} }} }}"
        );
        let value: Option<Value> =
            self.node_resource(context, id, "PullRequestReviewThread", &fields)?;
        let Some(value) = value else { return Ok(None) };
        let mut thread = decode_thread(&value)?;
        let comments = value.get("comments").context("Missing thread comments")?;
        thread.comments = decode_nodes(comments)?;
        self.finish_thread_comments(context, &mut thread, comments)?;
        Ok(Some(thread))
    }
    fn node_resource<D: DeserializeOwned>(
        &self,
        context: &ReviewContext,
        id: &str,
        kind: &str,
        fields: &str,
    ) -> Result<Option<D>> {
        context.validate()?;
        validate_id(id)?;
        let query = format!(
            "query($pr:ID!,$id:ID!) {{ viewer {{ id }} pullRequest:node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} }} }} node(id:$id) {{ ... on {kind} {{ {fields} pullRequest {{ id }} }} }} }}"
        );
        let data = self.transport.graphql(
            &query,
            json!({"pr":context.identity.pull_request_node_id,"id":id}),
        )?;
        check_guard(&data, data.get("pullRequest"), context)?;
        let target = data.get("node").context("Missing GitHub node")?;
        if target.is_null() {
            return Ok(None);
        };
        check_target(target, context, id)?;
        Ok(Some(decode(target.clone())?))
    }

    /// A submitted COMMENT review with one draft thread publishes one inline
    /// comment in a single mutation. Existing pending reviews are never joined
    /// or submitted as a side effect of Post now.
    pub fn post_comment(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        anchor: &CommentAnchor,
        body: &str,
    ) -> WriteResult<RemoteComment> {
        self.preflight(context, operation_id, Some(body))?;
        anchor.validate(context).map_err(not_sent)?;
        if anchor.subject_type == CommentSubject::File {
            // DraftPullRequestReviewThread has no subjectType in the documented
            // and live schema. Pending add-thread does support FILE explicitly.
            return Err(ReviewWriteError::NotSent(
                "Use Add to review for file comments, then submit the review explicitly".into(),
            ));
        }
        let before = self.load_reviews(context).map_err(not_sent)?;
        if before.iter().any(|review| {
            review.state == "PENDING"
                && actor_id(&review.author) == Some(context.identity.viewer_node_id.as_str())
        }) {
            return Err(ReviewWriteError::NotSent("A pending review already exists; resume it explicitly instead of posting a separate review".into()));
        }
        let mut thread =
            json!({"path":anchor.path,"body":body,"line":anchor.line,"side":anchor.side});
        if let Some(start) = anchor.start_line {
            thread["startLine"] = json!(start);
            thread["startSide"] = json!(anchor.start_side);
        }
        let payload=self.mutate(operation_id,"addPullRequestReview","AddPullRequestReviewInput",json!({"pullRequestId":context.identity.pull_request_node_id,"commitOID":context.head_oid,"event":"COMMENT","threads":[thread]}),"pullRequestReview { id state comments(first:2) { totalCount nodes { id } } }")?;
        let review_id = result_id(&payload, "pullRequestReview", operation_id)?;
        if before.iter().any(|review| review.id == review_id)
            || payload
                .pointer("/pullRequestReview/state")
                .and_then(Value::as_str)
                != Some("COMMENTED")
            || payload
                .pointer("/pullRequestReview/comments/totalCount")
                .and_then(Value::as_u64)
                != Some(1)
        {
            return Err(unknown(operation_id, Some(&review_id)));
        }
        let comments = payload
            .pointer("/pullRequestReview/comments/nodes")
            .and_then(Value::as_array)
            .filter(|nodes| nodes.len() == 1)
            .ok_or_else(|| unknown(operation_id, Some(&review_id)))?;
        let id = comments[0]
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| unknown(operation_id, Some(&review_id)))?;
        let comment = self
            .load_comment(context, id)
            .map_err(|_| unknown(operation_id, Some(id)))?
            .ok_or_else(|| unknown(operation_id, Some(id)))?;
        let correct_review = comment
            .pull_request_review
            .as_ref()
            .is_some_and(|review| review.id == review_id && review.state == "COMMENTED");
        if comment.state != "SUBMITTED"
            || !correct_review
            || !comment_matches(&comment, context, anchor, body)
        {
            return Err(unknown(operation_id, Some(id)));
        }
        let threads = self
            .load_threads(context)
            .map_err(|_| unknown(operation_id, Some(id)))?;
        if !threads.iter().any(|thread| {
            thread_matches_anchor(thread, anchor)
                && thread.comments.iter().any(|item| item.id == id)
        }) {
            return Err(unknown(operation_id, Some(id)));
        }
        Ok(receipt(operation_id, comment))
    }
    pub fn pending_review(&self, context: &ReviewContext) -> Result<Option<RemoteReview>> {
        let mut pending = self.load_reviews(context)?.into_iter().filter(|review| {
            review.state == "PENDING"
                && actor_id(&review.author) == Some(context.identity.viewer_node_id.as_str())
        });
        let result = pending.next();
        ensure!(
            pending.next().is_none(),
            "Multiple pending reviews require reconciliation"
        );
        Ok(result)
    }
    pub fn create_pending_review(
        &self,
        context: &ReviewContext,
        operation_id: &str,
    ) -> WriteResult<RemoteReview> {
        self.preflight(context, operation_id, None)?;
        ensure_no_pending(self.pending_review(context).map_err(not_sent)?)?;
        let payload=self.mutate(operation_id,"addPullRequestReview","AddPullRequestReviewInput",json!({"pullRequestId":context.identity.pull_request_node_id,"commitOID":context.head_oid}),"pullRequestReview { id }")?;
        let id = result_id(&payload, "pullRequestReview", operation_id)?;
        let review = self
            .load_review(context, &id)
            .map_err(|_| unknown(operation_id, Some(&id)))?
            .ok_or_else(|| unknown(operation_id, Some(&id)))?;
        if !owns_pending(&review, context) {
            return Err(unknown(operation_id, Some(&id)));
        };
        Ok(receipt(operation_id, review))
    }
    pub fn add_pending_thread(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        review_id: &str,
        anchor: &CommentAnchor,
        body: &str,
    ) -> WriteResult<RemoteThread> {
        self.preflight(context, operation_id, Some(body))?;
        anchor.validate(context).map_err(not_sent)?;
        self.require_pending(context, review_id)?;
        let mut input = json!({"pullRequestReviewId":review_id,"body":body,"path":anchor.path,"side":anchor.side,"subjectType":anchor.subject_type});
        if let Some(line) = anchor.line {
            input["line"] = json!(line)
        };
        if let Some(line) = anchor.start_line {
            input["startLine"] = json!(line);
            input["startSide"] = json!(anchor.start_side)
        };
        let payload = self.mutate(
            operation_id,
            "addPullRequestReviewThread",
            "AddPullRequestReviewThreadInput",
            input,
            "thread { id }",
        )?;
        let id = result_id(&payload, "thread", operation_id)?;
        let thread = self
            .load_thread(context, &id)
            .map_err(|_| unknown(operation_id, Some(&id)))?
            .ok_or_else(|| unknown(operation_id, Some(&id)))?;
        if !thread_matches_anchor(&thread, anchor)
            || !thread
                .comments
                .iter()
                .any(|comment| comment_matches(comment, context, anchor, body))
        {
            return Err(unknown(operation_id, Some(&id)));
        };
        Ok(receipt(operation_id, thread))
    }
    pub fn reply(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        thread_id: &str,
        pending_review_id: Option<&str>,
        body: &str,
    ) -> WriteResult<RemoteComment> {
        self.preflight(context, operation_id, Some(body))?;
        let thread = self
            .load_thread(context, thread_id)
            .map_err(not_sent)?
            .ok_or_else(|| ReviewWriteError::NotSent("Thread is unavailable".into()))?;
        if !thread.viewer_can_reply {
            return Err(ReviewWriteError::NotSent(
                "You cannot reply to this thread".into(),
            ));
        };
        if let Some(id) = pending_review_id {
            self.require_pending(context, id)?;
        }
        let mut rest_database_id = None;
        let mut immediate_root_id = None;
        let id = if let Some(review_id) = pending_review_id {
            let input = json!({"pullRequestReviewThreadId":thread_id,"body":body,"pullRequestReviewId":review_id});
            let payload = self.mutate(
                operation_id,
                "addPullRequestReviewThreadReply",
                "AddPullRequestReviewThreadReplyInput",
                input,
                "comment { id }",
            )?;
            result_id(&payload, "comment", operation_id)?
        } else {
            let root = thread
                .comments
                .iter()
                .find(|comment| comment.reply_to.is_none())
                .ok_or_else(|| {
                    ReviewWriteError::NotSent("Thread root comment is unavailable".into())
                })?;
            immediate_root_id = Some(root.id.clone());
            let root_id = root
                .full_database_id
                .as_deref()
                .and_then(|id| id.parse::<u64>().ok())
                .filter(|id| *id > 0)
                .ok_or_else(|| {
                    ReviewWriteError::NotSent("Thread root identity is unavailable".into())
                })?;
            let endpoint = format!(
                "repos/{}/pulls/{}/comments/{root_id}/replies",
                context.reference.repository.full_name(),
                context.reference.number
            );
            let value = self
                .transport
                .rest_json("POST", &endpoint, Some(json!({"body":body})))
                .map_err(|error| rest_write_error(operation_id, error))?
                .ok_or_else(|| unknown(operation_id, None))?;
            let (id, database_id) = rest_comment_identity(&value, operation_id)?;
            rest_database_id = Some(database_id);
            id
        };
        let comment = self
            .load_comment(context, &id)
            .map_err(|_| unknown(operation_id, Some(&id)))?
            .ok_or_else(|| unknown(operation_id, Some(&id)))?;
        let after = self
            .load_thread(context, thread_id)
            .map_err(|_| unknown(operation_id, Some(&id)))?
            .ok_or_else(|| unknown(operation_id, Some(&id)))?;
        let expected_state = if pending_review_id.is_some() {
            "PENDING"
        } else {
            "SUBMITTED"
        };
        let correct_review = pending_review_id.is_none_or(|expected| {
            comment
                .pull_request_review
                .as_ref()
                .is_some_and(|review| review.id == expected)
        });
        if immediate_root_id.as_ref().is_some_and(|expected| {
            comment.reply_to.as_ref().map(|reply| &reply.id) != Some(expected)
        }) || rest_database_id
            .as_ref()
            .is_some_and(|expected| comment.full_database_id.as_ref() != Some(expected))
            || comment.state != expected_state
            || !correct_review
            || comment.body != body
            || actor_id(&comment.author) != Some(context.identity.viewer_node_id.as_str())
            || !after.comments.iter().any(|item| item.id == id)
        {
            return Err(unknown(operation_id, Some(&id)));
        };
        Ok(receipt(operation_id, comment))
    }
    pub fn edit_comment(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        id: &str,
        body: &str,
    ) -> WriteResult<RemoteComment> {
        self.preflight(context, operation_id, Some(body))?;
        let before = self
            .load_comment(context, id)
            .map_err(not_sent)?
            .ok_or_else(|| ReviewWriteError::NotSent("Comment is unavailable".into()))?;
        if !before.viewer_did_author || !before.viewer_can_update {
            return Err(ReviewWriteError::NotSent(
                "You cannot edit this comment".into(),
            ));
        };
        self.mutate(
            operation_id,
            "updatePullRequestReviewComment",
            "UpdatePullRequestReviewCommentInput",
            json!({"pullRequestReviewCommentId":id,"body":body}),
            "pullRequestReviewComment { id }",
        )?;
        let comment = self
            .load_comment(context, id)
            .map_err(|_| unknown(operation_id, Some(id)))?
            .ok_or_else(|| unknown(operation_id, Some(id)))?;
        if comment.body != body {
            return Err(unknown(operation_id, Some(id)));
        };
        Ok(receipt(operation_id, comment))
    }
    /// A deleted node may return null plus a GraphQL NOT_FOUND field error.
    /// Prove absence through a complete, freshly guarded PR collection instead
    /// of converting any node/permission/transport error into a successful read.
    pub fn comment_is_absent(&self, context: &ReviewContext, id: &str) -> Result<bool> {
        validate_id(id)?;
        Ok(!self
            .load_threads(context)?
            .iter()
            .any(|thread| thread.comments.iter().any(|comment| comment.id == id)))
    }
    pub fn delete_comment(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        id: &str,
    ) -> WriteResult<()> {
        self.preflight(context, operation_id, None)?;
        let before = self
            .load_comment(context, id)
            .map_err(not_sent)?
            .ok_or_else(|| ReviewWriteError::NotSent("Comment is unavailable".into()))?;
        if !before.viewer_did_author || !before.viewer_can_delete {
            return Err(ReviewWriteError::NotSent(
                "You cannot delete this comment".into(),
            ));
        };
        self.mutate(
            operation_id,
            "deletePullRequestReviewComment",
            "DeletePullRequestReviewCommentInput",
            json!({"id":id}),
            "clientMutationId",
        )?;
        if !self
            .comment_is_absent(context, id)
            .map_err(|_| unknown(operation_id, Some(id)))?
        {
            return Err(unknown(operation_id, Some(id)));
        };
        Ok(receipt(operation_id, ()))
    }
    pub fn post_review(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        event: ReviewEvent,
        body: &str,
    ) -> WriteResult<RemoteReview> {
        self.preflight(context, operation_id, None)?;
        if event != ReviewEvent::Approve {
            ensure_body(body).map_err(not_sent)?;
        }
        let before = self.load_reviews(context).map_err(not_sent)?;
        ensure_no_pending(
            before
                .iter()
                .find(|review| {
                    review.state == "PENDING"
                        && actor_id(&review.author)
                            == Some(context.identity.viewer_node_id.as_str())
                })
                .cloned(),
        )?;
        if event != ReviewEvent::Comment {
            let query = format!(
                "query($pr:ID!) {{ viewer {{ id }} node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} }} }} }}"
            );
            let data = self
                .transport
                .graphql(&query, json!({"pr":context.identity.pull_request_node_id}))
                .map_err(not_sent)?;
            check_guard(&data, data.get("node"), context).map_err(not_sent)?;
            if data.pointer("/node/author/nodeId").and_then(Value::as_str)
                == Some(context.identity.viewer_node_id.as_str())
            {
                return Err(ReviewWriteError::NotSent(
                    "You cannot approve or request changes on your own PR".into(),
                ));
            }
        }
        let payload = self.mutate(operation_id, "addPullRequestReview", "AddPullRequestReviewInput",
            json!({"pullRequestId":context.identity.pull_request_node_id,"commitOID":context.head_oid,"event":event,"body":body}), "pullRequestReview { id }")?;
        let id = result_id(&payload, "pullRequestReview", operation_id)?;
        let review = self
            .load_review(context, &id)
            .map_err(|_| unknown(operation_id, Some(&id)))?
            .ok_or_else(|| unknown(operation_id, Some(&id)))?;
        let expected_state = match event {
            ReviewEvent::Comment => "COMMENTED",
            ReviewEvent::Approve => "APPROVED",
            ReviewEvent::RequestChanges => "CHANGES_REQUESTED",
        };
        if before.iter().any(|old| old.id == id)
            || review.state != expected_state
            || review.body != body
            || actor_id(&review.author) != Some(context.identity.viewer_node_id.as_str())
            || !review
                .commit
                .as_ref()
                .is_some_and(|commit| commit.oid == context.head_oid)
        {
            return Err(unknown(operation_id, Some(&id)));
        }
        Ok(receipt(operation_id, review))
    }
    pub fn submit_review(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        review_id: &str,
        event: ReviewEvent,
        body: &str,
    ) -> WriteResult<RemoteReview> {
        self.preflight(context, operation_id, None)?;
        let pending = self.require_owned_pending(context, review_id)?;
        if event != ReviewEvent::Comment
            && !pending
                .commit
                .as_ref()
                .is_some_and(|commit| commit.oid == context.head_oid)
        {
            return Err(ReviewWriteError::NotSent("Pending review targets an older revision; preserve its drafts before reviewing the new head".into()));
        }
        if event != ReviewEvent::Comment {
            let query = format!(
                "query($pr:ID!) {{ viewer {{ id }} node(id:$pr) {{ ... on PullRequest {{ {GUARD_FIELDS} }} }} }}"
            );
            let data = self
                .transport
                .graphql(&query, json!({"pr":context.identity.pull_request_node_id}))
                .map_err(not_sent)?;
            check_guard(&data, data.get("node"), context).map_err(not_sent)?;
            if data.pointer("/node/author/nodeId").and_then(Value::as_str)
                == Some(context.identity.viewer_node_id.as_str())
            {
                return Err(ReviewWriteError::NotSent(
                    "You cannot approve or request changes on your own PR".into(),
                ));
            }
            if event == ReviewEvent::RequestChanges {
                ensure_body(body).map_err(not_sent)?;
            }
        }
        let payload = self.mutate(
            operation_id,
            "submitPullRequestReview",
            "SubmitPullRequestReviewInput",
            json!({"pullRequestReviewId":review_id,"event":event,"body":body}),
            "pullRequestReview { id }",
        )?;
        let id = result_id(&payload, "pullRequestReview", operation_id)?;
        let review = self
            .load_review(context, &id)
            .map_err(|_| unknown(operation_id, Some(&id)))?
            .ok_or_else(|| unknown(operation_id, Some(&id)))?;
        let state = match event {
            ReviewEvent::Comment => "COMMENTED",
            ReviewEvent::Approve => "APPROVED",
            ReviewEvent::RequestChanges => "CHANGES_REQUESTED",
        };
        if review.id != review_id || review.state != state {
            return Err(unknown(operation_id, Some(&id)));
        };
        Ok(receipt(operation_id, review))
    }
    pub fn delete_pending_review(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        id: &str,
    ) -> WriteResult<()> {
        self.preflight(context, operation_id, None)?;
        self.require_owned_pending(context, id)?;
        self.mutate(
            operation_id,
            "deletePullRequestReview",
            "DeletePullRequestReviewInput",
            json!({"pullRequestReviewId":id}),
            "clientMutationId",
        )?;
        if self
            .load_reviews(context)
            .map_err(|_| unknown(operation_id, Some(id)))?
            .iter()
            .any(|review| review.id == id)
        {
            return Err(unknown(operation_id, Some(id)));
        };
        Ok(receipt(operation_id, ()))
    }
    pub fn set_resolved(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        id: &str,
        resolved: bool,
    ) -> WriteResult<RemoteThread> {
        self.preflight(context, operation_id, None)?;
        let before = self
            .load_thread(context, id)
            .map_err(not_sent)?
            .ok_or_else(|| ReviewWriteError::NotSent("Thread is unavailable".into()))?;
        if (resolved && !before.viewer_can_resolve) || (!resolved && !before.viewer_can_unresolve) {
            return Err(ReviewWriteError::NotSent(
                "You cannot change this thread's resolution".into(),
            ));
        };
        let (field, input) = if resolved {
            ("resolveReviewThread", "ResolveReviewThreadInput")
        } else {
            ("unresolveReviewThread", "UnresolveReviewThreadInput")
        };
        self.mutate(
            operation_id,
            field,
            input,
            json!({"threadId":id}),
            "thread { id }",
        )?;
        let thread = self
            .load_thread(context, id)
            .map_err(|_| unknown(operation_id, Some(id)))?
            .ok_or_else(|| unknown(operation_id, Some(id)))?;
        if thread.is_resolved != resolved {
            return Err(unknown(operation_id, Some(id)));
        };
        Ok(receipt(operation_id, thread))
    }
    pub fn set_viewed(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        path: &str,
        viewed: bool,
    ) -> WriteResult<RemoteViewedFile> {
        self.preflight(context, operation_id, None)?;
        validate_path(path).map_err(not_sent)?;
        let before = self.load_viewed(context).map_err(not_sent)?;
        if !before.iter().any(|file| file.path == path) {
            return Err(ReviewWriteError::NotSent(
                "File is absent from this PR".into(),
            ));
        };
        let (field, input) = if viewed {
            ("markFileAsViewed", "MarkFileAsViewedInput")
        } else {
            ("unmarkFileAsViewed", "UnmarkFileAsViewedInput")
        };
        // This API has no expectedHeadOid. Read-back guard detects races; never
        // claim that remote Viewed provides the stronger local snapshot CAS.
        self.mutate(
            operation_id,
            field,
            input,
            json!({"pullRequestId":context.identity.pull_request_node_id,"path":path}),
            "pullRequest { id }",
        )?;
        let file = self
            .load_viewed(context)
            .map_err(|_| unknown(operation_id, None))?
            .into_iter()
            .find(|file| file.path == path)
            .ok_or_else(|| unknown(operation_id, None))?;
        if (file.viewer_viewed_state == ViewedState::Viewed) != viewed {
            return Err(unknown(operation_id, None));
        };
        Ok(receipt(operation_id, file))
    }
    /// Folder operations are intentionally non-atomic on GitHub. One pre-read
    /// and final read-back avoid a full file-list request for every descendant.
    pub fn set_viewed_batch(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        paths: &[String],
        viewed: bool,
    ) -> WriteResult<Vec<RemoteViewedFile>> {
        self.set_viewed_batch_with_progress(context, operation_id, paths, viewed, |_, _| {})
    }

    pub fn set_viewed_batch_with_progress(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        paths: &[String],
        viewed: bool,
        mut on_progress: impl FnMut(usize, usize),
    ) -> WriteResult<Vec<RemoteViewedFile>> {
        self.set_viewed_batch_observed(
            context,
            operation_id,
            paths,
            viewed,
            &mut on_progress,
            |_| {},
        )
    }

    pub fn set_viewed_batch_with_observation(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        paths: &[String],
        viewed: bool,
    ) -> std::result::Result<
        (WriteReceipt<Vec<RemoteViewedFile>>, Vec<RemoteViewedFile>),
        ReviewWriteError,
    > {
        let mut observed = Vec::new();
        let receipt = self.set_viewed_batch_observed(
            context,
            operation_id,
            paths,
            viewed,
            |_, _| {},
            |files| observed = files.to_vec(),
        )?;
        Ok((receipt, observed))
    }

    fn set_viewed_batch_observed(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        paths: &[String],
        viewed: bool,
        mut on_progress: impl FnMut(usize, usize),
        mut on_observed: impl FnMut(&[RemoteViewedFile]),
    ) -> WriteResult<Vec<RemoteViewedFile>> {
        self.preflight(context, operation_id, None)?;
        if paths.is_empty() {
            return Err(ReviewWriteError::NotSent("No files selected".into()));
        };
        let mut seen = HashSet::new();
        for path in paths {
            validate_path(path).map_err(not_sent)?;
            if !seen.insert(path.as_str()) {
                return Err(ReviewWriteError::NotSent("Duplicate folder file".into()));
            };
        }
        let before = self.load_viewed(context).map_err(not_sent)?;
        let known: HashMap<&str, ViewedState> = before
            .iter()
            .map(|file| (file.path.as_str(), file.viewer_viewed_state))
            .collect();
        if !paths.iter().all(|path| known.contains_key(path.as_str())) {
            return Err(ReviewWriteError::NotSent(
                "Folder contains files outside this PR".into(),
            ));
        };
        let todo: Vec<&String> = paths
            .iter()
            .filter(|path| {
                known
                    .get(path.as_str())
                    .is_some_and(|state| (*state == ViewedState::Viewed) != viewed)
            })
            .collect();
        let (field, input_type) = if viewed {
            ("markFileAsViewed", "MarkFileAsViewedInput")
        } else {
            ("unmarkFileAsViewed", "UnmarkFileAsViewedInput")
        };
        let mut completed = paths.len() - todo.len();
        on_progress(completed, paths.len());
        let mut dispatched = false;
        for (chunk_index, chunk) in todo.chunks(20).enumerate() {
            if chunk_index > 0 {
                // GitHub recommends spacing mutative requests by at least one
                // second. This service runs on a background delivery worker.
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            self.verify_context(context).map_err(|error| {
                if dispatched {
                    unknown(operation_id, None)
                } else {
                    not_sent(error)
                }
            })?;
            let mut definitions = Vec::new();
            let mut selections = Vec::new();
            let mut variables = serde_json::Map::new();
            for (index, path) in chunk.iter().enumerate() {
                definitions.push(format!("$file{index}:{input_type}!"));
                selections.push(format!(
                    "file{index}:{field}(input:$file{index}) {{ pullRequest {{ id }} }}"
                ));
                variables.insert(format!("file{index}"),json!({"pullRequestId":context.identity.pull_request_node_id,"path":path,"clientMutationId":operation_id}));
            }
            let query = format!(
                "mutation({}) {{ {} }}",
                definitions.join(","),
                selections.join(" ")
            );
            dispatched = true;
            let data = self
                .transport
                .graphql(&query, Value::Object(variables))
                .map_err(|_| unknown(operation_id, None))?;
            for index in 0..chunk.len() {
                if data
                    .get(format!("file{index}"))
                    .and_then(|value| value.pointer("/pullRequest/id"))
                    .and_then(Value::as_str)
                    != Some(context.identity.pull_request_node_id.as_str())
                {
                    return Err(unknown(operation_id, None));
                }
            }
            completed += chunk.len();
            on_progress(completed, paths.len());
        }
        let after = self.load_viewed(context).map_err(|error| {
            if dispatched {
                unknown(operation_id, None)
            } else {
                not_sent(error)
            }
        })?;
        on_observed(&after);
        let after: HashMap<&str, &RemoteViewedFile> = after
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        let mut confirmed = Vec::new();
        for path in paths {
            let file = after
                .get(path.as_str())
                .copied()
                .ok_or_else(|| unknown(operation_id, None))?;
            if (file.viewer_viewed_state == ViewedState::Viewed) != viewed {
                return Err(unknown(operation_id, None));
            };
            confirmed.push(file.clone());
        }
        Ok(receipt(operation_id, confirmed))
    }

    fn require_owned_pending(
        &self,
        context: &ReviewContext,
        id: &str,
    ) -> std::result::Result<RemoteReview, ReviewWriteError> {
        let review = self
            .load_review(context, id)
            .map_err(not_sent)?
            .ok_or_else(|| ReviewWriteError::NotSent("Pending review is unavailable".into()))?;
        if review.state != "PENDING"
            || actor_id(&review.author) != Some(context.identity.viewer_node_id.as_str())
        {
            return Err(ReviewWriteError::NotSent(
                "Pending review belongs to another account or is already submitted".into(),
            ));
        }
        Ok(review)
    }
    fn require_pending(
        &self,
        context: &ReviewContext,
        id: &str,
    ) -> std::result::Result<RemoteReview, ReviewWriteError> {
        let review = self.require_owned_pending(context, id)?;
        if !owns_pending(&review, context) {
            return Err(ReviewWriteError::NotSent("Pending review targets an older revision; preserve its drafts before adding new anchors".into()));
        };
        Ok(review)
    }
    fn preflight(
        &self,
        context: &ReviewContext,
        operation_id: &str,
        body: Option<&str>,
    ) -> std::result::Result<(), ReviewWriteError> {
        validate_id(operation_id).map_err(not_sent)?;
        if let Some(body) = body {
            ensure_body(body).map_err(not_sent)?;
        }
        self.verify_context(context).map_err(not_sent)
    }
    fn mutate(
        &self,
        operation_id: &str,
        field: &str,
        input_type: &str,
        mut input: Value,
        selection: &str,
    ) -> std::result::Result<Value, ReviewWriteError> {
        input["clientMutationId"] = json!(operation_id);
        let query =
            format!("mutation($input:{input_type}!) {{ {field}(input:$input) {{ {selection} }} }}");
        let data = self
            .transport
            .graphql(&query, json!({"input":input}))
            .map_err(|error| rest_write_error(operation_id, error))?;
        data.get(field)
            .filter(|value| !value.is_null())
            .cloned()
            .ok_or_else(|| unknown(operation_id, None))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CommentPublication {
    Immediate,
    Pending { review_id: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommentIntent {
    pub context: ReviewContext,
    pub anchor: CommentAnchor,
    pub body: String,
    /// Required in durable payloads: an older intent without this field cannot
    /// safely infer whether the user intended publication or a pending draft.
    pub publication: CommentPublication,
}
#[derive(Clone, Debug)]
pub enum CommentReconciliation {
    Confirmed(RemoteComment),
    /// The intended draft reached its exact review, but that review was submitted
    /// concurrently. Show this transition explicitly; never recreate the draft.
    SubmittedPendingReview(RemoteComment),
    NotObserved,
    Ambiguous,
}
/// NotObserved never authorizes automatic replay: the server may still finish an
/// earlier request. The outbox retains DeliveryUnknown until explicit resolution.
pub fn reconcile_comment(
    intent: &CommentIntent,
    before_ids: &HashSet<String>,
    threads: &[RemoteThread],
) -> CommentReconciliation {
    let mut candidates = threads
        .iter()
        .filter(|thread| thread_matches_anchor(thread, &intent.anchor))
        .flat_map(|thread| thread.comments.iter())
        .filter(|comment| {
            !before_ids.contains(&comment.id)
                && comment_matches(comment, &intent.context, &intent.anchor, &intent.body)
        })
        .filter_map(|comment| {
            match &intent.publication {
                CommentPublication::Immediate => (comment.state == "SUBMITTED"
                    && comment
                        .pull_request_review
                        .as_ref()
                        .is_none_or(|review| review.state != "PENDING"))
                .then_some((comment, false)),
                CommentPublication::Pending { review_id } => {
                    let review = comment.pull_request_review.as_ref()?;
                    if review.id != *review_id {
                        return None;
                    }
                    if comment.state == "PENDING" && review.state == "PENDING" {
                        Some((comment, false))
                    } else if comment.state == "SUBMITTED"
                        && matches!(
                            review.state.as_str(),
                            "COMMENTED" | "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
                        )
                    {
                        Some((comment, true))
                    } else {
                        // A mixed publication state is not a stable read-back.
                        None
                    }
                }
            }
        });
    let first = candidates.next();
    if candidates.next().is_some() {
        return CommentReconciliation::Ambiguous;
    }
    match first {
        Some((comment, true)) => CommentReconciliation::SubmittedPendingReview(comment.clone()),
        Some((comment, false)) => CommentReconciliation::Confirmed(comment.clone()),
        None => CommentReconciliation::NotObserved,
    }
}
