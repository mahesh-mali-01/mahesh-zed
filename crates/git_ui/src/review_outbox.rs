//! Durable local review intent. Server pending reviews are fetched separately;
//! they are never imported as unsent local operations.
use crate::review::{ComparisonSelector, ObjectFormat, PullRequestIdentity, PullRequestMetadata};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewOperationContext {
    pub selector: ComparisonSelector,
    pub metadata: PullRequestMetadata,
}
impl ReviewOperationContext {
    pub fn identity(&self) -> Result<&PullRequestIdentity> {
        self.selector
            .pull_request_identity()
            .context("A review operation requires a PR and account identity")
    }
    pub fn object_format(&self) -> Result<ObjectFormat> {
        match self.metadata.head_ref_oid.len() {
            40 => Ok(ObjectFormat::Sha1),
            64 => Ok(ObjectFormat::Sha256),
            _ => anyhow::bail!("Review operation requires a pinned head"),
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.identity()?.validate()?;
        let format = self.object_format()?;
        self.selector.validate(format)?;
        self.metadata.validate_pinned_tips(format)?;
        if let ComparisonSelector::PullRequestScope {
            base_tip, head_tip, ..
        } = &self.selector
        {
            ensure!(
                base_tip.hex == self.metadata.base_ref_oid
                    && head_tip.hex == self.metadata.head_ref_oid,
                "Draft scope and pinned PR metadata differ"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOperationKind {
    PostInlineComment,
    Reply,
    EditComment,
    DeleteComment,
    CreatePendingReview,
    DeletePendingReview,
    AddPendingThread,
    SubmitReview,
    SetThreadResolved,
    SetFileViewed,
}
impl ReviewOperationKind {
    pub fn needs_location(self) -> bool {
        matches!(
            self,
            Self::PostInlineComment | Self::CreatePendingReview | Self::AddPendingThread
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLocationState {
    NotRequired,
    Ready,
    NeedsLocation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOperationState {
    LocalDraft,
    Queued,
    Sending,
    Sent,
    Failed,
    DeliveryUnknown,
}
impl ReviewOperationState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LocalDraft => "local_draft",
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::DeliveryUnknown => "delivery_unknown",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewOperation {
    pub operation_id: String,
    pub revision: u64,
    pub context: ReviewOperationContext,
    pub kind: ReviewOperationKind,
    /// Serialized service DTO; authentication credentials never belong here.
    pub payload: Value,
    pub location: ReviewLocationState,
    pub state: ReviewOperationState,
    pub before_server_ids: Vec<String>,
    pub server_ids: Vec<String>,
    /// Sanitized failure reason supplied by the transport/UI, never raw stderr.
    pub last_error: Option<String>,
    pub created_at_unix_seconds: u64,
    pub updated_at_unix_seconds: u64,
}
impl ReviewOperation {
    pub fn validate(&self) -> Result<()> {
        self.context.validate()?;
        ensure!(
            self.payload.is_object(),
            "Review operation payload must be a JSON object"
        );
        ensure!(
            self.revision <= i64::MAX as u64,
            "Review operation revision exhausted"
        );
        if self.kind == ReviewOperationKind::SetFileViewed {
            ensure!(
                self.context.selector.supports_github_viewed(),
                "A subset review cannot change GitHub's full-file Viewed state"
            );
        }
        validate_server_ids(&self.before_server_ids)?;
        validate_server_ids(&self.server_ids)?;
        Ok(())
    }
    pub fn ensure_dispatchable(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            self.location != ReviewLocationState::NeedsLocation
                && (!self.kind.needs_location() || self.location == ReviewLocationState::Ready),
            "Choose a current PR location before posting this draft"
        );
        Ok(())
    }
}
pub(crate) fn validate_server_ids(ids: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for id in ids {
        ensure!(
            !id.is_empty()
                && !id.chars().any(|c| c.is_control() || c.is_whitespace())
                && seen.insert(id),
            "Invalid or duplicate server identity in review operation"
        );
    }
    Ok(())
}
#[derive(Clone, Debug)]
pub enum ReviewOperationOutcome {
    Sent {
        server_ids: Vec<String>,
    },
    /// Only use when the service established that no request was dispatched.
    Failed {
        reason: String,
    },
    DeliveryUnknown {
        reason: String,
    },
}
#[derive(Clone, Debug)]
pub enum ReviewReconciliationOutcome {
    Delivered {
        server_ids: Vec<String>,
    },
    /// Explicit provider verification is required; an empty cached list or a
    /// failed read cannot establish absence. This never automatically retries.
    NotDelivered,
}
#[derive(Clone, Debug)]
pub struct ReviewReconciliation {
    pub viewer_node_id: String,
    pub head_oid: String,
    pub observed_server_ids: Vec<String>,
    pub outcome: ReviewReconciliationOutcome,
}
