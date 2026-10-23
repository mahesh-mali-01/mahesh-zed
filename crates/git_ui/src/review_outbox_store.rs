use super::{ReviewDb, StaleReview, ensure_durable};
use crate::review::*;
use anyhow::{Context as _, Result, ensure};
use db::sqlez::connection::Connection;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
pub(super) fn read_operation(connection: &Connection, id: &str) -> Result<ReviewOperation> {
    let (identity, revision, state, record) = connection
        .select_row_bound::<String, (String, i64, String, String)>(
            "SELECT identity, revision, state, record FROM review_operations WHERE id = ?",
        )?(id.to_owned())?
    .context("Review operation was not found")?;
    let operation: ReviewOperation = serde_json::from_str(&record)?;
    db::uuid::Uuid::parse_str(&operation.operation_id)
        .context("Invalid review operation identity")?;
    operation.validate()?;
    ensure!(
        operation.operation_id == id
            && revision >= 0
            && operation.revision == revision as u64
            && operation.state.as_str() == state
            && serde_json::to_string(operation.context.identity()?)? == identity,
        "Review operation identity or state is corrupt"
    );
    Ok(operation)
}
pub(super) fn ensure_active(connection: &Connection, id: &str) -> Result<()> {
    ensure!(
        connection.select_row_bound::<String, i64>(
            "SELECT 1 FROM review_operation_archives WHERE operation_id = ?"
        )?(id.to_owned())?
        .is_none(),
        "This local action was archived and cannot be sent or replayed"
    );
    Ok(())
}

pub(super) fn store_changed(
    connection: &Connection,
    mut operation: ReviewOperation,
    old_revision: u64,
) -> Result<ReviewOperation> {
    ensure_active(connection, &operation.operation_id)?;
    operation.revision = old_revision
        .checked_add(1)
        .filter(|revision| *revision <= i64::MAX as u64)
        .context("Review operation revision exhausted")?;
    operation.updated_at_unix_seconds = now()?;
    operation.validate()?;
    connection.exec_bound::<(i64,String,String,String,i64)>("UPDATE review_operations SET revision = ?, state = ?, record = ? WHERE id = ? AND revision = ?")?((operation.revision as i64,operation.state.as_str().into(),serde_json::to_string(&operation)?,operation.operation_id.clone(),old_revision as i64))?;
    if connection.select_row::<i64>("SELECT changes()")?()? != Some(1) {
        anyhow::bail!(StaleReview);
    }
    Ok(operation)
}
impl ReviewDb {
    pub fn operation(&self, id: &str) -> Result<ReviewOperation> {
        ensure_durable(self)?;
        read_operation(self, id)
    }
    /// Account-scoped local intent only. Existing pending server reviews are
    /// loaded through GitHub and must not be inserted as LocalDraft rows.
    pub fn operations(&self, identity: &PullRequestIdentity) -> Result<Vec<ReviewOperation>> {
        ensure_durable(self)?;
        identity.validate()?;
        let ids = self.select_bound::<String, String>(
            "SELECT id FROM review_operations WHERE identity = ? AND NOT EXISTS (SELECT 1 FROM review_operation_archives a WHERE a.operation_id = review_operations.id) ORDER BY id",
        )?(serde_json::to_string(identity)?)?;
        ids.into_iter()
            .map(|id| read_operation(self, &id))
            .collect()
    }
    pub(crate) fn archived_operation_ids(
        &self,
        identity: &PullRequestIdentity,
    ) -> Result<Vec<String>> {
        ensure_durable(self)?;
        identity.validate()?;
        self.select_bound::<String, String>(
            "SELECT a.operation_id FROM review_operation_archives a JOIN review_operations o ON o.id = a.operation_id WHERE o.identity = ?",
        )?(serde_json::to_string(identity)?)
    }

    /// Archive local tracking atomically. A sending request must settle first;
    /// unknown delivery is never converted to a definitely-unsent state.
    pub async fn archive_operation(&self, id: String, expected_revision: u64) -> Result<()> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_operation_archive", || {
                ensure_active(connection, &id)?;
                let operation = read_operation(connection, &id)?;
                ensure!(operation.revision == expected_revision, "Local action changed; refresh Pending actions");
                ensure!(matches!(operation.state, ReviewOperationState::LocalDraft | ReviewOperationState::Queued | ReviewOperationState::Failed | ReviewOperationState::DeliveryUnknown),
                    "Wait for an in-flight action to settle before removing it");
                let reason = if operation.state == ReviewOperationState::DeliveryUnknown {
                    "Stopped local tracking; GitHub delivery remains unconfirmed"
                } else { "Cancelled local action before delivery" };
                store_changed(connection, operation, expected_revision)?;
                connection.exec_bound::<(String, i64, String)>(
                    "INSERT INTO review_operation_archives(operation_id, archived_at, reason) VALUES (?, ?, ?)"
                )?((id, now()? as i64, reason.to_owned()))?;
                Ok(())
            })
        }).await
    }

    pub async fn create_operation(
        &self,
        context: ReviewOperationContext,
        kind: ReviewOperationKind,
        payload: Value,
        location: ReviewLocationState,
    ) -> Result<ReviewOperation> {
        self.write(move|connection| {
            ensure_durable(connection)?;
            let timestamp=now()?;
            let operation=ReviewOperation{operation_id:db::uuid::Uuid::now_v7().to_string(),revision:0,context,kind,payload,location,state:ReviewOperationState::LocalDraft,before_server_ids:Vec::new(),server_ids:Vec::new(),last_error:None,created_at_unix_seconds:timestamp,updated_at_unix_seconds:timestamp};
            operation.validate()?;
            connection.exec_bound::<(String,String,i64,String,String)>("INSERT INTO review_operations(id,identity,revision,state,record) VALUES (?,?,?,?,?)")?((operation.operation_id.clone(),serde_json::to_string(operation.context.identity()?)?,0,operation.state.as_str().into(),serde_json::to_string(&operation)?))?;
            Ok(operation)
        }).await
    }
    async fn change_operation<F>(
        &self,
        id: String,
        expected_revision: u64,
        change: F,
    ) -> Result<ReviewOperation>
    where
        F: FnOnce(&mut ReviewOperation) -> Result<()> + Send + 'static,
    {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_operation_transition", || {
                let mut operation = read_operation(connection, &id)?;
                if operation.revision != expected_revision {
                    anyhow::bail!(StaleReview);
                }
                change(&mut operation)?;
                store_changed(connection, operation, expected_revision)
            })
        })
        .await
    }
    pub async fn update_operation_draft(
        &self,
        id: String,
        expected_revision: u64,
        context: ReviewOperationContext,
        payload: Value,
        location: ReviewLocationState,
    ) -> Result<ReviewOperation> {
        self.change_operation(id, expected_revision, move |operation| {
            ensure!(
                matches!(
                    operation.state,
                    ReviewOperationState::LocalDraft | ReviewOperationState::Failed
                ),
                "Only an unsent local draft can be edited"
            );
            ensure!(
                operation.context.identity()? == context.identity()?,
                "A draft cannot move to another PR or account"
            );
            operation.context = context;
            operation.payload = payload;
            operation.location = location;
            operation.state = ReviewOperationState::LocalDraft;
            operation.last_error = None;
            Ok(())
        })
        .await
    }
    pub async fn queue_operation(
        &self,
        id: String,
        expected_revision: u64,
    ) -> Result<ReviewOperation> {
        self.change_operation(id, expected_revision, |operation| {
            ensure!(
                matches!(
                    operation.state,
                    ReviewOperationState::LocalDraft | ReviewOperationState::Failed
                ),
                "Only a confirmed unsent draft can be queued; reconcile unknown delivery first"
            );
            operation.ensure_dispatchable()?;
            operation.state = ReviewOperationState::Queued;
            operation.last_error = None;
            Ok(())
        })
        .await
    }
    /// Explicitly route a definitely unsent inline comment into the owner's
    /// existing pending review without changing its body, anchor or operation ID.
    /// This never submits a review and cannot replay uncertain delivery.
    pub async fn queue_comment_in_pending_review(
        &self,
        id: String,
        expected_revision: u64,
        context: ReviewOperationContext,
        intent: crate::github_review::CommentIntent,
    ) -> Result<ReviewOperation> {
        self.change_operation(id, expected_revision, move |operation| {
            ensure!(matches!(operation.state, ReviewOperationState::LocalDraft | ReviewOperationState::Failed), "Only a definitely unsent comment can be added to a pending review");
            ensure!(matches!(operation.kind, ReviewOperationKind::PostInlineComment | ReviewOperationKind::AddPendingThread) && operation.context == context, "Comment belongs to another review context");
            let original: crate::github_review::CommentIntent = serde_json::from_value(operation.payload.get("intent").context("Inline comment payload is missing")?.clone())?;
            ensure!(original.context.identity == intent.context.identity && original.context.head_oid == intent.context.head_oid && original.context.base_tip_oid == intent.context.base_tip_oid && original.context.reference.number == intent.context.reference.number && original.context.reference.repository.full_name() == intent.context.reference.repository.full_name() && original.anchor == intent.anchor && original.body == intent.body, "Pending-review recovery cannot change the saved comment or its location");
            ensure!(matches!(&intent.publication, crate::github_review::CommentPublication::Pending { review_id } if !review_id.is_empty()), "An explicit pending review is required");
            operation.kind = ReviewOperationKind::AddPendingThread;
            operation.payload = serde_json::json!({"action":"comment","intent":intent});
            operation.state = ReviewOperationState::Queued;
            operation.last_error = None;
            operation.ensure_dispatchable()?;
            Ok(())
        }).await
    }

    /// Explicit user recovery for an operation that was queued but never
    /// claimed. A Sending/Unknown operation must instead be reconciled.
    pub async fn reset_queued_operation(
        &self,
        id: String,
        expected_revision: u64,
    ) -> Result<ReviewOperation> {
        self.change_operation(id, expected_revision, |operation| {
            ensure!(
                operation.state == ReviewOperationState::Queued,
                "Only an undispatched queued operation can return to a draft"
            );
            operation.state = ReviewOperationState::LocalDraft;
            operation.last_error = None;
            Ok(())
        })
        .await
    }
    /// Await this committed transition before invoking any GitHub write. An
    /// empty before list is a captured empty baseline, not an omitted read.
    pub async fn claim_operation(
        &self,
        id: String,
        expected_revision: u64,
        before_server_ids: Vec<String>,
    ) -> Result<ReviewOperation> {
        self.change_operation(id, expected_revision, move |operation| {
            ensure!(
                operation.state == ReviewOperationState::Queued,
                "Only a queued operation can be dispatched"
            );
            operation.ensure_dispatchable()?;
            operation.before_server_ids = before_server_ids;
            operation.server_ids.clear();
            operation.state = ReviewOperationState::Sending;
            Ok(())
        })
        .await
    }
    pub async fn finish_operation(
        &self,
        id: String,
        expected_revision: u64,
        outcome: ReviewOperationOutcome,
    ) -> Result<ReviewOperation> {
        self.change_operation(id, expected_revision, move |operation| {
            ensure!(
                operation.state == ReviewOperationState::Sending,
                "Operation is no longer owned by this dispatch; reconcile its receipt"
            );
            match outcome {
                ReviewOperationOutcome::Sent { server_ids } => {
                    operation.state = ReviewOperationState::Sent;
                    operation.server_ids = server_ids;
                    operation.last_error = None;
                }
                ReviewOperationOutcome::Failed { reason } => {
                    operation.state = ReviewOperationState::Failed;
                    operation.last_error = Some(reason);
                }
                ReviewOperationOutcome::DeliveryUnknown { reason } => {
                    operation.state = ReviewOperationState::DeliveryUnknown;
                    operation.last_error = Some(reason);
                }
            }
            Ok(())
        })
        .await
    }
    /// Only explicit, successful provider reconciliation can release an unknown
    /// operation. NotDelivered returns a draft and never automatically resends.
    pub async fn reconcile_operation(
        &self,
        id: String,
        expected_revision: u64,
        evidence: ReviewReconciliation,
    ) -> Result<ReviewOperation> {
        self.change_operation(id, expected_revision, move |operation| {
            ensure!(
                operation.state == ReviewOperationState::DeliveryUnknown,
                "Only uncertain delivery needs reconciliation"
            );
            ensure!(
                operation.context.identity()?.viewer_node_id == evidence.viewer_node_id,
                "Reconciliation belongs to another GitHub account"
            );
            validate_server_ids(&evidence.observed_server_ids)?;
            ObjectId {
                format: operation.context.object_format()?,
                hex: evidence.head_oid.clone(),
            }
            .validate()?;
            match evidence.outcome {
                ReviewReconciliationOutcome::Delivered { server_ids } => {
                    ensure!(
                        server_ids
                            .iter()
                            .all(|id| evidence.observed_server_ids.contains(id)),
                        "Delivered IDs were not observed during reconciliation"
                    );
                    operation.state = ReviewOperationState::Sent;
                    operation.server_ids = server_ids;
                }
                ReviewReconciliationOutcome::NotDelivered => {
                    operation.state = ReviewOperationState::LocalDraft;
                    if evidence.head_oid != operation.context.metadata.head_ref_oid
                        && operation.kind.needs_location()
                    {
                        operation.location = ReviewLocationState::NeedsLocation;
                    }
                }
            }
            operation.last_error = None;
            Ok(())
        })
        .await
    }
    /// Persist an interrupted dispatch without guessing whether a claimed
    /// request reached GitHub. Unclaimed queues are definitely unsent; claimed
    /// writes require read-back before any retry.
    pub async fn record_delivery_interruption(
        &self,
        id: String,
        reason: String,
    ) -> Result<ReviewOperation> {
        let operation = self.operation(&id)?;
        self.change_operation(id, operation.revision, move |operation| {
            match operation.state {
                ReviewOperationState::Queued => operation.state = ReviewOperationState::Failed,
                ReviewOperationState::Sending => {
                    operation.state = ReviewOperationState::DeliveryUnknown
                }
                _ => return Ok(()),
            }
            operation.last_error = Some(reason);
            Ok(())
        })
        .await
    }

    /// Startup recovery only, before dispatch starts for this profile. A process
    /// crash cannot establish delivery failure; no recovered operation is sent.
    pub async fn recover_sending_operations(&self) -> Result<Vec<ReviewOperation>> {
        self.write(move|connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_operation_recover",|| {
                let ids=connection.select_bound::<String,String>("SELECT id FROM review_operations WHERE state = ? ORDER BY id")?(ReviewOperationState::Sending.as_str().into())?;
                let mut recovered=Vec::new();
                for id in ids {
                    let mut operation=read_operation(connection,&id)?;let revision=operation.revision;
                    operation.state=ReviewOperationState::DeliveryUnknown;
                    operation.last_error=Some("Application stopped before delivery was confirmed; reconcile before retrying".into());
                    recovered.push(store_changed(connection,operation,revision)?);
                }
                Ok(recovered)
            })
        }).await
    }
    /// Refresh preserves local text while preventing a stale line anchor from
    /// being sent. An already dispatched or uncertain write is never requeued.
    pub async fn invalidate_operation_locations(
        &self,
        identity: PullRequestIdentity,
        head_oid: String,
    ) -> Result<Vec<ReviewOperation>> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            identity.validate()?;
            connection.with_savepoint("review_operation_locations", || {
                let ids = connection.select_bound::<String, String>(
                    "SELECT id FROM review_operations WHERE identity = ? AND NOT EXISTS (SELECT 1 FROM review_operation_archives a WHERE a.operation_id = review_operations.id) ORDER BY id",
                )?(serde_json::to_string(&identity)?)?;
                let mut changed = Vec::new();
                for id in ids {
                    let mut operation = read_operation(connection, &id)?;
                    if matches!(
                        operation.state,
                        ReviewOperationState::LocalDraft
                            | ReviewOperationState::Queued
                            | ReviewOperationState::Failed
                    ) && operation.kind.needs_location()
                        && operation.context.metadata.head_ref_oid != head_oid
                    {
                        let revision = operation.revision;
                        operation.location = ReviewLocationState::NeedsLocation;
                        operation.state = ReviewOperationState::LocalDraft;
                        changed.push(store_changed(connection, operation, revision)?);
                    }
                }
                Ok(changed)
            })
        })
        .await
    }
    pub async fn delete_operation_draft(&self, id: String, expected_revision: u64) -> Result<()> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_operation_delete", || {
                let operation = read_operation(connection, &id)?;
                if operation.revision != expected_revision {
                    anyhow::bail!(StaleReview);
                }
                ensure!(
                    matches!(
                        operation.state,
                        ReviewOperationState::LocalDraft | ReviewOperationState::Failed
                    ),
                    "Only a confirmed unsent local draft can be deleted"
                );
                connection.exec_bound::<(String, i64)>(
                    "DELETE FROM review_operations WHERE id = ? AND revision = ?",
                )?((id, expected_revision as i64))?;
                Ok(())
            })
        })
        .await
    }
}
