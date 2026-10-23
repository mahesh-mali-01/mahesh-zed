use super::*;
use crate::review::*;
use db::sqlez::thread_safe_connection::ThreadSafeConnection;
use futures::executor::block_on;
use serde_json::json;

fn outbox_db(path: &std::path::Path) -> Result<ReviewDb> {
    let connection = block_on(
        ThreadSafeConnection::builder::<ReviewDb>(path.to_str().context("database path")?, true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=1000;")
            .build(),
    )?;
    ReviewDb::from_connection(connection)
}
fn context(viewer: &str) -> ReviewOperationContext {
    ReviewOperationContext {
        selector: ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_example".into(),
                pull_request_node_id: "PR_example".into(),
                viewer_node_id: viewer.into(),
            },
        },
        metadata: PullRequestMetadata {
            owner: "owner".into(),
            repository: "repository".into(),
            number: 7,
            title: "Example review".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(3),
        },
    }
}
fn create(db: &ReviewDb) -> Result<ReviewOperation> {
    block_on(db.create_operation(
        context("U_viewer"),
        ReviewOperationKind::PostInlineComment,
        json!({"body":"Local review draft","path":"example.rs","line":3}),
        ReviewLocationState::Ready,
    ))
}
fn sending(db: &ReviewDb) -> Result<ReviewOperation> {
    let draft = create(db)?;
    let queued = block_on(db.queue_operation(draft.operation_id.clone(), draft.revision))?;
    block_on(db.claim_operation(
        queued.operation_id,
        queued.revision,
        vec!["comment:old".into()],
    ))
}
#[test]
fn review_outbox_persists_before_dispatch_and_only_one_claim_wins() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("outbox.sqlite"))?;
    let draft = create(&db)?;
    assert_eq!(draft.state, ReviewOperationState::LocalDraft);
    let queued = block_on(db.queue_operation(draft.operation_id.clone(), draft.revision))?;
    let claimed = block_on(db.claim_operation(
        queued.operation_id.clone(),
        queued.revision,
        vec!["comment:old".into()],
    ))?;
    assert_eq!(
        db.operation(&claimed.operation_id)?.state,
        ReviewOperationState::Sending
    );
    assert_eq!(
        db.operation(&claimed.operation_id)?.before_server_ids,
        vec!["comment:old"]
    );
    assert_eq!(db.operation(&claimed.operation_id)?.payload, draft.payload);
    let second = block_on(db.claim_operation(
        queued.operation_id,
        queued.revision,
        vec!["wrong:baseline".into()],
    ))
    .unwrap_err();
    assert!(second.downcast_ref::<StaleReview>().is_some());
    let sent = block_on(db.finish_operation(
        claimed.operation_id.clone(),
        claimed.revision,
        ReviewOperationOutcome::Sent {
            server_ids: vec!["comment:new".into()],
        },
    ))?;
    assert_eq!(sent.state, ReviewOperationState::Sent);
    assert_eq!(sent.operation_id, draft.operation_id);
    assert!(block_on(db.queue_operation(sent.operation_id.clone(), sent.revision)).is_err());
    assert!(block_on(db.delete_operation_draft(sent.operation_id, sent.revision)).is_err());
    Ok(())
}
#[test]
fn review_outbox_restart_recovers_sending_as_unknown_and_never_resends() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("outbox.sqlite");
    let db = outbox_db(&path)?;
    let claimed = sending(&db)?;
    drop(db);
    let db = outbox_db(&path)?;
    let recovered = block_on(db.recover_sending_operations())?;
    assert_eq!(recovered.len(), 1);
    let unknown = &recovered[0];
    assert_eq!(unknown.operation_id, claimed.operation_id);
    assert_eq!(unknown.state, ReviewOperationState::DeliveryUnknown);
    assert_eq!(unknown.before_server_ids, vec!["comment:old"]);
    assert!(block_on(db.queue_operation(unknown.operation_id.clone(), unknown.revision)).is_err());
    assert!(
        block_on(db.claim_operation(unknown.operation_id.clone(), unknown.revision, vec![]))
            .is_err()
    );
    assert!(
        block_on(db.delete_operation_draft(unknown.operation_id.clone(), unknown.revision))
            .is_err()
    );
    assert!(block_on(db.recover_sending_operations())?.is_empty());
    assert!(db.operations(context("U_other").identity()?)?.is_empty());
    let wrong = ReviewReconciliation {
        viewer_node_id: "U_other".into(),
        head_oid: "2".repeat(40),
        observed_server_ids: vec!["comment:new".into()],
        outcome: ReviewReconciliationOutcome::Delivered {
            server_ids: vec!["comment:new".into()],
        },
    };
    assert!(
        block_on(db.reconcile_operation(unknown.operation_id.clone(), unknown.revision, wrong))
            .is_err()
    );
    let proof = ReviewReconciliation {
        viewer_node_id: "U_viewer".into(),
        head_oid: "2".repeat(40),
        observed_server_ids: vec!["comment:new".into()],
        outcome: ReviewReconciliationOutcome::Delivered {
            server_ids: vec!["comment:new".into()],
        },
    };
    let sent =
        block_on(db.reconcile_operation(unknown.operation_id.clone(), unknown.revision, proof))?;
    assert_eq!(sent.state, ReviewOperationState::Sent);
    Ok(())
}
#[test]
fn review_outbox_unknown_absence_requires_explicit_reconciliation_and_new_location() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("outbox.sqlite"))?;
    let claimed = sending(&db)?;
    let unknown = block_on(db.finish_operation(
        claimed.operation_id.clone(),
        claimed.revision,
        ReviewOperationOutcome::DeliveryUnknown {
            reason: "Connection ended before confirmation".into(),
        },
    ))?;
    assert!(block_on(db.queue_operation(unknown.operation_id.clone(), unknown.revision)).is_err());
    let proof = ReviewReconciliation {
        viewer_node_id: "U_viewer".into(),
        head_oid: "3".repeat(40),
        observed_server_ids: vec!["comment:old".into()],
        outcome: ReviewReconciliationOutcome::NotDelivered,
    };
    let draft = block_on(db.reconcile_operation(unknown.operation_id, unknown.revision, proof))?;
    assert_eq!(draft.state, ReviewOperationState::LocalDraft);
    assert_eq!(draft.location, ReviewLocationState::NeedsLocation);
    assert!(block_on(db.queue_operation(draft.operation_id.clone(), draft.revision)).is_err());
    let mut refreshed = context("U_viewer");
    refreshed.metadata.head_ref_oid = "3".repeat(40);
    let corrected = block_on(db.update_operation_draft(
        draft.operation_id,
        draft.revision,
        refreshed,
        json!({"body":"Preserved and remapped","path":"example.rs","line":4}),
        ReviewLocationState::Ready,
    ))?;
    assert_eq!(corrected.state, ReviewOperationState::LocalDraft);
    assert_eq!(
        block_on(db.queue_operation(corrected.operation_id, corrected.revision))?.state,
        ReviewOperationState::Queued
    );
    Ok(())
}
#[test]
fn review_outbox_refresh_preserves_text_and_account_isolation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("outbox.sqlite"))?;
    let draft = create(&db)?;
    let queued = block_on(db.queue_operation(draft.operation_id.clone(), draft.revision))?;
    let changed = block_on(
        db.invalidate_operation_locations(context("U_viewer").identity()?.clone(), "3".repeat(40)),
    )?;
    assert_eq!(changed.len(), 1);
    let operation = &changed[0];
    assert_eq!(operation.state, ReviewOperationState::LocalDraft);
    assert_eq!(operation.location, ReviewLocationState::NeedsLocation);
    assert_eq!(operation.payload, draft.payload);
    assert!(block_on(db.claim_operation(queued.operation_id, queued.revision, vec![])).is_err());
    assert!(
        block_on(db.update_operation_draft(
            operation.operation_id.clone(),
            operation.revision,
            context("U_other"),
            json!({"body":"wrong account"}),
            ReviewLocationState::Ready
        ))
        .is_err()
    );
    assert!(db.operations(context("U_other").identity()?)?.is_empty());
    assert_eq!(db.operations(context("U_viewer").identity()?)?.len(), 1);
    Ok(())
}
#[test]
fn review_outbox_subset_cannot_queue_github_viewed_and_local_drafts_are_not_server_pending_reviews()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("outbox.sqlite"))?;
    let mut subset = context("U_viewer");
    let identity = subset.identity()?.clone();
    let base = ObjectId {
        format: ObjectFormat::Sha1,
        hex: subset.metadata.base_ref_oid.clone(),
    };
    let head = ObjectId {
        format: ObjectFormat::Sha1,
        hex: subset.metadata.head_ref_oid.clone(),
    };
    subset.selector = ComparisonSelector::PullRequestScope {
        identity,
        base_tip: base.clone(),
        head_tip: head.clone(),
        scope: PullRequestCommitScope::Commits {
            selections: vec![CommitSelection {
                commit: head,
                parent: Some(base),
            }],
        },
    };
    assert!(
        block_on(db.create_operation(
            subset,
            ReviewOperationKind::SetFileViewed,
            json!({"path":"example.rs","viewed":true}),
            ReviewLocationState::NotRequired
        ))
        .is_err()
    );
    let local = create(&db)?;
    assert_eq!(local.state, ReviewOperationState::LocalDraft);
    assert!(local.server_ids.is_empty());
    assert!(local.before_server_ids.is_empty());
    assert_eq!(db.operations(local.context.identity()?)?.len(), 1);
    block_on(db.delete_operation_draft(local.operation_id, local.revision))?;
    assert!(db.operations(context("U_viewer").identity()?)?.is_empty());
    Ok(())
}

#[test]
fn review_outbox_queued_recovery_is_explicit_cas_and_never_releases_sending() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("outbox.sqlite"))?;
    let draft = create(&db)?;
    let queued = block_on(db.queue_operation(draft.operation_id.clone(), draft.revision))?;
    let restored =
        block_on(db.reset_queued_operation(queued.operation_id.clone(), queued.revision))?;
    assert_eq!(restored.state, ReviewOperationState::LocalDraft);
    assert_eq!(restored.payload, draft.payload);
    assert_eq!(restored.operation_id, draft.operation_id);
    assert!(
        block_on(db.claim_operation(queued.operation_id, queued.revision, vec![]))
            .unwrap_err()
            .downcast_ref::<StaleReview>()
            .is_some()
    );
    assert!(
        block_on(db.reset_queued_operation(restored.operation_id.clone(), restored.revision))
            .is_err()
    );
    let queued = block_on(db.queue_operation(restored.operation_id, restored.revision))?;
    let sending = block_on(db.claim_operation(queued.operation_id, queued.revision, vec![]))?;
    assert!(
        block_on(db.reset_queued_operation(sending.operation_id.clone(), sending.revision))
            .is_err()
    );
    assert_eq!(
        db.operation(&sending.operation_id)?.state,
        ReviewOperationState::Sending
    );
    Ok(())
}

#[test]
fn review_outbox_interruption_classifies_only_unclaimed_as_unsent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("outbox.sqlite"))?;
    let draft = create(&db)?;
    let queued = block_on(db.queue_operation(draft.operation_id, draft.revision))?;
    let unsent =
        block_on(db.record_delivery_interruption(queued.operation_id, "Offline preflight".into()))?;
    assert_eq!(unsent.state, ReviewOperationState::Failed);
    let claimed = sending(&db)?;
    let unknown = block_on(
        db.record_delivery_interruption(claimed.operation_id.clone(), "Lost response".into()),
    )?;
    assert_eq!(unknown.state, ReviewOperationState::DeliveryUnknown);
    assert!(block_on(db.queue_operation(unknown.operation_id.clone(), unknown.revision)).is_err());
    assert_eq!(
        db.operation(&claimed.operation_id)?.last_error.as_deref(),
        Some("Lost response")
    );
    Ok(())
}

#[test]
fn review_outbox_routes_failed_comment_into_existing_review_without_changing_identity_or_body()
-> Result<()> {
    use crate::github_pull_requests::{PullRequestReference, RepositoryName};
    use crate::github_review::{
        CommentAnchor, CommentIntent, CommentPublication, CommentSubject, DiffSide, ReviewContext,
    };
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("outbox.sqlite"))?;
    let context = context("U_viewer");
    let intent = CommentIntent {
        context: ReviewContext {
            reference: PullRequestReference {
                repository: RepositoryName {
                    owner: "owner".into(),
                    name: "repository".into(),
                },
                number: 7,
            },
            identity: context.identity()?.clone(),
            head_oid: "2".repeat(40),
            base_tip_oid: "1".repeat(40),
        },
        anchor: CommentAnchor {
            path: "src/example.rs".into(),
            commit_oid: "2".repeat(40),
            side: DiffSide::Right,
            line: Some(3),
            start_line: None,
            start_side: None,
            subject_type: CommentSubject::Line,
        },
        body: "Preserved recovery comment".into(),
        publication: CommentPublication::Immediate,
    };
    let draft = block_on(db.create_operation(
        context.clone(),
        ReviewOperationKind::PostInlineComment,
        serde_json::json!({"action":"comment","intent":intent}),
        ReviewLocationState::Ready,
    ))?;
    let queued = block_on(db.queue_operation(draft.operation_id, draft.revision))?;
    let claimed = block_on(db.claim_operation(queued.operation_id, queued.revision, Vec::new()))?;
    let failed = block_on(db.finish_operation(
        claimed.operation_id,
        claimed.revision,
        ReviewOperationOutcome::Failed {
            reason: "Existing pending review prevents immediate publication".into(),
        },
    ))?;
    let mut pending = intent.clone();
    pending.publication = CommentPublication::Pending {
        review_id: "PRR_existing".into(),
    };
    let mut changed = pending.clone();
    changed.body = "Unexpected changed text".into();
    assert!(
        block_on(db.queue_comment_in_pending_review(
            failed.operation_id.clone(),
            failed.revision,
            context.clone(),
            changed
        ))
        .is_err()
    );
    let recovered = block_on(db.queue_comment_in_pending_review(
        failed.operation_id.clone(),
        failed.revision,
        context.clone(),
        pending.clone(),
    ))?;
    assert_eq!(recovered.operation_id, failed.operation_id);
    assert_eq!(recovered.kind, ReviewOperationKind::AddPendingThread);
    assert_eq!(recovered.state, ReviewOperationState::Queued);
    let payload: CommentIntent = serde_json::from_value(recovered.payload["intent"].clone())?;
    assert_eq!(payload.body, intent.body);
    assert_eq!(payload.anchor, intent.anchor);
    assert_eq!(payload.publication, pending.publication);
    assert!(
        block_on(db.queue_comment_in_pending_review(
            recovered.operation_id.clone(),
            recovered.revision,
            context.clone(),
            pending.clone()
        ))
        .is_err(),
        "a live queued delivery cannot be retargeted"
    );
    let claimed =
        block_on(db.claim_operation(recovered.operation_id, recovered.revision, Vec::new()))?;
    let unknown = block_on(db.finish_operation(
        claimed.operation_id,
        claimed.revision,
        ReviewOperationOutcome::DeliveryUnknown {
            reason: "Response lost".into(),
        },
    ))?;
    assert!(
        block_on(db.queue_comment_in_pending_review(
            unknown.operation_id,
            unknown.revision,
            context,
            pending
        ))
        .is_err(),
        "unknown publication must never be replayed as recovery"
    );
    Ok(())
}

#[test]
fn archived_queued_operation_cannot_be_claimed_and_audit_survives_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("archive.sqlite");
    let db = outbox_db(&path)?;
    let draft = create(&db)?;
    let queued = block_on(db.queue_operation(draft.operation_id.clone(), draft.revision))?;
    block_on(db.archive_operation(queued.operation_id.clone(), queued.revision))?;
    assert!(db.operations(context("U_viewer").identity()?)?.is_empty());
    assert!(
        block_on(db.claim_operation(queued.operation_id.clone(), queued.revision, vec![])).is_err()
    );
    let archived = db.operation(&queued.operation_id)?;
    assert_eq!(archived.payload, draft.payload);
    assert!(
        block_on(db.claim_operation(archived.operation_id.clone(), archived.revision, vec![]))
            .is_err()
    );
    drop(db);
    let restored = outbox_db(&path)?;
    assert!(
        restored
            .operations(context("U_viewer").identity()?)?
            .is_empty()
    );
    assert_eq!(
        restored.operation(&queued.operation_id)?.payload,
        draft.payload
    );
    Ok(())
}

#[test]
fn archive_never_cancels_sending_or_replays_unknown_delivery() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db = outbox_db(&directory.path().join("archive.sqlite"))?;
    let sending = sending(&db)?;
    assert!(
        block_on(db.archive_operation(sending.operation_id.clone(), sending.revision)).is_err()
    );
    let unknown = block_on(db.finish_operation(
        sending.operation_id,
        sending.revision,
        ReviewOperationOutcome::DeliveryUnknown {
            reason: "Response lost".into(),
        },
    ))?;
    assert!(
        block_on(db.archive_operation(unknown.operation_id.clone(), unknown.revision + 1)).is_err()
    );
    block_on(db.archive_operation(unknown.operation_id.clone(), unknown.revision))?;
    let archived = db.operation(&unknown.operation_id)?;
    assert_eq!(archived.state, ReviewOperationState::DeliveryUnknown);
    assert!(db.operations(context("U_viewer").identity()?)?.is_empty());
    assert!(block_on(db.queue_operation(archived.operation_id, archived.revision)).is_err());
    Ok(())
}
