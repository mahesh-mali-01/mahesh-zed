use super::{
    CommentAnchor, CommentSubject, RemoteActor, RemoteComment, RemoteReview, RemoteThread,
    ReviewContext, ReviewWriteError, WriteReceipt,
};
use crate::github_pull_requests::GitHubHttpError;
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::HashSet;

pub(super) const GUARD_FIELDS: &str = "id number headRefOid baseRef { target { oid } } repository { id nameWithOwner } author { ... on User { nodeId:id } }";
pub(super) const COMMENT_FIELDS: &str = "id fullDatabaseId state replyTo { id } pullRequestReview { id state } body url author { login ... on User { nodeId:id } ... on Bot { nodeId:id } } createdAt updatedAt path line startLine diffHunk commit { oid } originalCommit { oid } viewerCanUpdate viewerCanDelete viewerDidAuthor";
pub(super) const REVIEW_FIELDS: &str = "id state body url author { login ... on User { nodeId:id } ... on Bot { nodeId:id } } commit { oid } viewerCanUpdate viewerCanDelete viewerDidAuthor";
pub(super) const THREAD_FIELDS: &str = "id path line startLine originalLine originalStartLine diffSide startDiffSide subjectType isResolved isOutdated viewerCanReply viewerCanResolve viewerCanUnresolve";
pub(super) const PAGE_INFO: &str = "totalCount pageInfo { hasNextPage endCursor }";
pub(super) fn check_guard(data: &Value, pr: Option<&Value>, context: &ReviewContext) -> Result<()> {
    ensure!(
        data.pointer("/viewer/id").and_then(Value::as_str)
            == Some(context.identity.viewer_node_id.as_str()),
        "GitHub account changed; reconnect before continuing"
    );
    let pr = pr
        .filter(|value| !value.is_null())
        .context("PR is unavailable")?;
    ensure!(
        pr.get("id").and_then(Value::as_str)
            == Some(context.identity.pull_request_node_id.as_str())
            && pr.pointer("/repository/id").and_then(Value::as_str)
                == Some(context.identity.repository_node_id.as_str())
            && pr.get("number").and_then(Value::as_u64) == Some(context.reference.number),
        "GitHub PR identity changed"
    );
    ensure!(
        pr.pointer("/repository/nameWithOwner")
            .and_then(Value::as_str)
            .is_some_and(
                |name| name.eq_ignore_ascii_case(&context.reference.repository.full_name())
            ),
        "GitHub repository identity changed"
    );
    ensure!(
        pr.get("headRefOid").and_then(Value::as_str) == Some(context.head_oid.as_str())
            && pr.pointer("/baseRef/target/oid").and_then(Value::as_str)
                == Some(context.base_tip_oid.as_str()),
        "PR branches changed; refresh before continuing"
    );
    Ok(())
}
pub(super) fn check_target(value: &Value, context: &ReviewContext, id: &str) -> Result<()> {
    ensure!(
        value.get("id").and_then(Value::as_str) == Some(id)
            && value.pointer("/pullRequest/id").and_then(Value::as_str)
                == Some(context.identity.pull_request_node_id.as_str()),
        "GitHub resource belongs to a different PR"
    );
    Ok(())
}
pub(super) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 512
            && !id.chars().any(|ch| ch.is_whitespace() || ch.is_control()),
        "Invalid operation or resource identity"
    );
    Ok(())
}
pub(super) fn ensure_body(body: &str) -> Result<()> {
    ensure!(
        !body.trim().is_empty() && body.len() <= 65536,
        "Comment body must contain text and be at most 64 KiB"
    );
    Ok(())
}
pub(super) fn not_sent(error: impl std::fmt::Display) -> ReviewWriteError {
    ReviewWriteError::NotSent(error.to_string())
}
pub(super) fn rest_write_error(operation_id: &str, error: anyhow::Error) -> ReviewWriteError {
    if let Some(rejection) = error.downcast_ref::<GitHubHttpError>()
        && rejection.authoritative_rejection()
    {
        ReviewWriteError::NotSent(rejection.to_string())
    } else {
        unknown(operation_id, None)
    }
}
pub(super) fn unknown(operation_id: &str, id: Option<&str>) -> ReviewWriteError {
    ReviewWriteError::DeliveryUnknown {
        operation_id: operation_id.into(),
        remote_id: id.map(str::to_string),
    }
}
pub(super) fn receipt<T>(operation_id: &str, remote: T) -> WriteReceipt<T> {
    WriteReceipt {
        operation_id: operation_id.into(),
        remote,
    }
}
pub(super) fn rest_comment_identity(
    value: &Value,
    operation_id: &str,
) -> std::result::Result<(String, String), ReviewWriteError> {
    let node_id = value
        .get("node_id")
        .and_then(Value::as_str)
        .filter(|id| validate_id(id).is_ok())
        .ok_or_else(|| unknown(operation_id, None))?;
    let database_id = value
        .get("id")
        .and_then(|id| id.as_u64().or_else(|| id.as_str()?.parse::<u64>().ok()))
        .filter(|id| *id > 0)
        .ok_or_else(|| unknown(operation_id, Some(node_id)))?;
    Ok((node_id.into(), database_id.to_string()))
}
pub(super) fn result_id(
    payload: &Value,
    field: &str,
    operation_id: &str,
) -> std::result::Result<String, ReviewWriteError> {
    payload
        .get(field)
        .and_then(|value| value.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| unknown(operation_id, None))
}
pub(super) fn deserialize_database_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if id.parse::<u64>().is_ok() => Ok(Some(id)),
        Some(Value::Number(id)) => id
            .as_u64()
            .map(|id| Some(id.to_string()))
            .ok_or_else(|| serde::de::Error::custom("Invalid comment database identity")),
        _ => Err(serde::de::Error::custom(
            "Invalid comment database identity",
        )),
    }
}
pub(super) fn collection_total(connection: &Value) -> Result<u64> {
    connection
        .get("totalCount")
        .and_then(Value::as_u64)
        .context("Missing collection total")
}
pub(super) fn decode_thread(value: &Value) -> Result<RemoteThread> {
    let mut value = value.clone();
    value
        .as_object_mut()
        .context("Invalid thread")?
        .remove("comments");
    decode(value)
}
pub(super) fn normalized_range_start(line: Option<u32>, start: Option<u32>) -> Option<u32> {
    // GitHub can represent a single-line thread as startLine == line while its
    // comment has startLine null. Both denote the same one-line location.
    start.filter(|start| Some(*start) != line)
}
pub(super) fn thread_matches_anchor(thread: &RemoteThread, anchor: &CommentAnchor) -> bool {
    let range_matches = if anchor.subject_type == CommentSubject::Line
        && anchor.line.is_some()
        && normalized_range_start(anchor.line, anchor.start_line).is_none()
    {
        normalized_range_start(thread.line, thread.start_line).is_none()
            && anchor.start_side.is_none_or(|side| side == anchor.side)
            && thread
                .start_diff_side
                .is_none_or(|side| side == anchor.side)
    } else {
        // Real ranges retain their exact start line and start side.
        thread.start_line == anchor.start_line && thread.start_diff_side == anchor.start_side
    };
    thread.path == anchor.path
        && thread.diff_side == anchor.side
        && thread.line == anchor.line
        && range_matches
        && thread.subject_type == anchor.subject_type
}
pub(super) fn decode<D: DeserializeOwned>(value: Value) -> Result<D> {
    serde_json::from_value(value).context("Unsupported GitHub review response")
}
pub(super) fn nodes(connection: &Value) -> Result<&Vec<Value>> {
    connection
        .get("nodes")
        .and_then(Value::as_array)
        .context("Missing GitHub review collection")
}
pub(super) fn decode_nodes<D: DeserializeOwned>(connection: &Value) -> Result<Vec<D>> {
    nodes(connection)?.iter().cloned().map(decode).collect()
}
pub(super) fn actor_id(actor: &Option<RemoteActor>) -> Option<&str> {
    actor.as_ref().and_then(|author| author.node_id.as_deref())
}
pub(super) fn owns_pending(review: &RemoteReview, context: &ReviewContext) -> bool {
    review.state == "PENDING"
        && actor_id(&review.author) == Some(context.identity.viewer_node_id.as_str())
        && review
            .commit
            .as_ref()
            .is_some_and(|commit| commit.oid == context.head_oid)
}
pub(super) fn ensure_no_pending(
    pending: Option<RemoteReview>,
) -> std::result::Result<(), ReviewWriteError> {
    if pending.is_some() {
        Err(ReviewWriteError::NotSent(
            "A pending review already exists; resume it explicitly".into(),
        ))
    } else {
        Ok(())
    }
}
pub(super) fn comment_matches(
    comment: &RemoteComment,
    context: &ReviewContext,
    anchor: &CommentAnchor,
    body: &str,
) -> bool {
    comment.body == body
        && actor_id(&comment.author) == Some(context.identity.viewer_node_id.as_str())
        && comment.path == anchor.path
        && comment.line == anchor.line
        && normalized_range_start(comment.line, comment.start_line)
            == normalized_range_start(anchor.line, anchor.start_line)
        && comment
            .original_commit
            .as_ref()
            .or(comment.commit.as_ref())
            .is_some_and(|commit| commit.oid == anchor.commit_oid)
}
#[derive(Default)]
pub(super) struct Pager {
    pub(super) cursor: Option<String>,
    seen: HashSet<String>,
}
impl Pager {
    pub(super) fn advance(&mut self, connection: &Value) -> Result<bool> {
        let info = connection
            .get("pageInfo")
            .context("Missing review pagination")?;
        if !info
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .context("Missing review pagination status")?
        {
            return Ok(false);
        };
        let next = info
            .get("endCursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .context("Missing review cursor")?;
        ensure!(
            self.seen.insert(next.into()),
            "Repeated review cursor; refresh"
        );
        self.cursor = Some(next.into());
        Ok(true)
    }
}
