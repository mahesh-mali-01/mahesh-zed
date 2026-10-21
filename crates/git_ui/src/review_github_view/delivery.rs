use super::*;
use crate::github_review::{CommentReconciliation, ReviewWriteError, reconcile_comment};

pub(super) fn pending_operation(operation: &ReviewOperation) -> bool {
    matches!(
        operation.state,
        ReviewOperationState::Queued
            | ReviewOperationState::Sending
            | ReviewOperationState::Failed
            | ReviewOperationState::DeliveryUnknown
    )
}

pub(super) fn operation_matches_thread(operation: &ReviewOperation, thread: &RemoteThread) -> bool {
    operation.server_ids.contains(&thread.id)
        || thread
            .comments
            .iter()
            .any(|comment| operation.server_ids.contains(&comment.id))
}

pub(super) fn all_ids(threads: &[RemoteThread], reviews: &[RemoteReview]) -> Vec<String> {
    threads
        .iter()
        .flat_map(|thread| {
            std::iter::once(thread.id.clone())
                .chain(thread.comments.iter().map(|comment| comment.id.clone()))
        })
        .chain(reviews.iter().map(|review| review.id.clone()))
        .collect()
}
pub(super) fn enqueue_comment_delivery(
    db: ReviewDb,
    remote: ReviewContext,
    operation: ReviewOperation,
    intent: ReviewIntent,
    pause: Option<crate::review_outbox_runtime::Recovery>,
    cx: &mut App,
) -> crate::review_outbox_runtime::Delivery {
    let id = operation.operation_id.clone();
    let lane = format!(
        "{}:{}:{}:{}",
        remote.identity.host,
        remote.identity.repository_node_id,
        remote.identity.pull_request_node_id,
        remote.identity.viewer_node_id
    );
    crate::review_outbox_runtime::deliver(
        lane,
        id.clone(),
        async move {
            if let Some(pause) = pause {
                if let Err(error) = pause.await {
                    return db.record_delivery_interruption(id, error.to_string()).await;
                }
            }
            match dispatch(&db, &remote, operation, intent).await {
                Ok(operation) => Ok(operation),
                Err(error) => {
                    db.record_delivery_interruption(id, format!("Delivery paused: {error:#}"))
                        .await
                }
            }
        },
        cx,
    )
}

async fn dispatch(
    db: &ReviewDb,
    context: &ReviewContext,
    operation: ReviewOperation,
    intent: ReviewIntent,
) -> Result<ReviewOperation> {
    let client = GitHubClient::default();
    let service = ReviewService::new(&client);
    if let ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } = &intent {
        let files = client.files(&context.reference)?;
        let details = client.pull_request(&context.reference)?;
        anyhow::ensure!(
            !files.limit_reached && files.files.len() == details.changed_files,
            "Canonical PR file list is incomplete"
        );
        let file = files
            .files
            .iter()
            .find(|file| file.filename == intent.anchor.path)
            .ok_or_else(|| anyhow::anyhow!("Draft path is absent from the current PR"))?;
        crate::review_comment_anchor::validate_github_anchor(
            &intent.anchor,
            file.patch.as_deref(),
        )?;
        intent.anchor.validate(context)?;
        anyhow::ensure!(
            intent.context.identity == context.identity
                && intent.context.head_oid == context.head_oid
                && intent.context.base_tip_oid == context.base_tip_oid,
            "Draft belongs to an older PR revision; select its location again"
        );
        service.verify_context(context)?;
    }
    let threads = service.load_threads(context)?;
    let reviews = service.load_reviews(context)?;
    let queued = if operation.state == ReviewOperationState::Queued {
        operation
    } else {
        db.queue_operation(operation.operation_id, operation.revision)
            .await?
    };
    let claimed = db
        .claim_operation(
            queued.operation_id,
            queued.revision,
            all_ids(&threads, &reviews),
        )
        .await?;
    let id = &claimed.operation_id;
    let result = match &intent {
        ReviewIntent::AddToReview { intent } => {
            let pending = reviews
                .iter()
                .find(|review| review.viewer_did_author && review.state == "PENDING");
            let review = match pending {
                Some(review) => Ok(review.clone()),
                None => service
                    .create_pending_review(context, id)
                    .map(|receipt| receipt.remote),
            };
            review.and_then(|review| {
                service
                    .add_pending_thread(context, id, &review.id, &intent.anchor, &intent.body)
                    .map(|receipt| vec![receipt.remote.id])
            })
        }
        ReviewIntent::Comment { intent } => match &intent.publication {
            CommentPublication::Immediate => service
                .post_comment(context, id, &intent.anchor, &intent.body)
                .map(|receipt| vec![receipt.remote.id]),
            CommentPublication::Pending { review_id } => service
                .add_pending_thread(context, id, review_id, &intent.anchor, &intent.body)
                .map(|receipt| vec![receipt.remote.id]),
        },
        ReviewIntent::Reply {
            thread_id,
            pending_review_id,
            body,
        } => service
            .reply(context, id, thread_id, pending_review_id.as_deref(), body)
            .map(|receipt| vec![receipt.remote.id]),
        ReviewIntent::Edit { comment_id, body } => service
            .edit_comment(context, id, comment_id, body)
            .map(|receipt| vec![receipt.remote.id]),
        ReviewIntent::Delete { comment_id } => service
            .delete_comment(context, id, comment_id)
            .map(|_| vec![comment_id.clone()]),
        ReviewIntent::StartReview => service
            .create_pending_review(context, id)
            .map(|receipt| vec![receipt.remote.id]),
        ReviewIntent::DeleteReview { review_id } => service
            .delete_pending_review(context, id, review_id)
            .map(|_| vec![review_id.clone()]),
        ReviewIntent::Submit {
            review_id,
            event,
            body,
        } => service
            .submit_review(context, id, review_id, *event, body)
            .map(|receipt| vec![receipt.remote.id]),
        ReviewIntent::SubmitNew { event, body } => service
            .post_review(context, id, *event, body)
            .map(|receipt| vec![receipt.remote.id]),
        ReviewIntent::Resolve {
            thread_id,
            resolved,
        } => service
            .set_resolved(context, id, thread_id, *resolved)
            .map(|receipt| vec![receipt.remote.id]),
    };
    let outcome = match result {
        Ok(server_ids) => ReviewOperationOutcome::Sent { server_ids },
        Err(ReviewWriteError::NotSent(reason)) => ReviewOperationOutcome::Failed { reason },
        Err(error @ ReviewWriteError::DeliveryUnknown { .. }) => {
            ReviewOperationOutcome::DeliveryUnknown {
                reason: error.to_string(),
            }
        }
    };
    db.finish_operation(claimed.operation_id, claimed.revision, outcome)
        .await
}

pub(super) fn reconciled_ids<T: crate::github_review::ReviewTransport>(
    service: &ReviewService<'_, T>,
    context: &ReviewContext,
    operation: &ReviewOperation,
    intent: &ReviewIntent,
    threads: &[RemoteThread],
    reviews: &[RemoteReview],
) -> Result<Option<Vec<String>>> {
    let baseline = operation
        .before_server_ids
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    Ok(match intent {
        ReviewIntent::AddToReview { intent } => {
            let mut candidates = std::collections::BTreeSet::new();
            for review in reviews.iter().filter(|review| review.viewer_did_author) {
                let mut expected = intent.clone();
                expected.publication = CommentPublication::Pending {
                    review_id: review.id.clone(),
                };
                match reconcile_comment(&expected, &baseline, threads) {
                    CommentReconciliation::Confirmed(comment)
                    | CommentReconciliation::SubmittedPendingReview(comment) => {
                        candidates.insert(comment.id);
                    }
                    CommentReconciliation::Ambiguous => return Ok(None),
                    CommentReconciliation::NotObserved => {}
                }
            }
            if candidates.len() == 1 {
                Some(candidates.into_iter().collect())
            } else {
                None
            }
        }
        ReviewIntent::Comment { intent } => match reconcile_comment(intent, &baseline, threads) {
            CommentReconciliation::Confirmed(comment)
            | CommentReconciliation::SubmittedPendingReview(comment) => Some(vec![comment.id]),
            CommentReconciliation::NotObserved | CommentReconciliation::Ambiguous => None,
        },
        ReviewIntent::Edit { comment_id, body } => service
            .load_comment(context, comment_id)?
            .filter(|comment| comment.viewer_did_author && &comment.body == body)
            .map(|comment| vec![comment.id]),
        ReviewIntent::Delete { comment_id } => {
            if service.comment_is_absent(context, comment_id)? {
                Some(Vec::new())
            } else {
                None
            }
        }
        ReviewIntent::Resolve {
            thread_id,
            resolved,
        } => service
            .load_thread(context, thread_id)?
            .filter(|thread| thread.is_resolved == *resolved)
            .map(|thread| vec![thread.id]),
        ReviewIntent::Submit {
            review_id,
            event,
            body,
        } => {
            let state = match event {
                ReviewEvent::Comment => "COMMENTED",
                ReviewEvent::Approve => "APPROVED",
                ReviewEvent::RequestChanges => "CHANGES_REQUESTED",
            };
            service
                .load_review(context, review_id)?
                .filter(|review| {
                    review.viewer_did_author && review.state == state && &review.body == body
                })
                .map(|review| vec![review.id])
        }
        ReviewIntent::SubmitNew { event, body } => {
            let state = match event {
                ReviewEvent::Comment => "COMMENTED",
                ReviewEvent::Approve => "APPROVED",
                ReviewEvent::RequestChanges => "CHANGES_REQUESTED",
            };
            let candidates = reviews
                .iter()
                .filter(|review| {
                    !baseline.contains(&review.id)
                        && review.viewer_did_author
                        && review.state == state
                        && &review.body == body
                        && review
                            .commit
                            .as_ref()
                            .is_some_and(|commit| commit.oid == context.head_oid)
                })
                .collect::<Vec<_>>();
            if candidates.len() == 1 {
                Some(vec![candidates[0].id.clone()])
            } else {
                None
            }
        }
        ReviewIntent::DeleteReview { review_id } => {
            // `reviews` is a complete, guarded read by the reconciliation caller.
            // Querying the deleted node produces NOT_FOUND rather than authoritative absence.
            if !reviews.iter().any(|review| &review.id == review_id) {
                Some(Vec::new())
            } else {
                None
            }
        }
        ReviewIntent::StartReview => {
            let candidates = reviews
                .iter()
                .filter(|review| {
                    !baseline.contains(&review.id)
                        && review.viewer_did_author
                        && review.state == "PENDING"
                        && review
                            .commit
                            .as_ref()
                            .is_some_and(|commit| commit.oid == context.head_oid)
                })
                .collect::<Vec<_>>();
            if candidates.len() == 1 {
                Some(vec![candidates[0].id.clone()])
            } else {
                None
            }
        }
        ReviewIntent::Reply {
            thread_id,
            pending_review_id,
            body,
        } => {
            let candidates = threads
                .iter()
                .filter(|thread| &thread.id == thread_id)
                .flat_map(|thread| &thread.comments)
                .filter(|comment| {
                    !baseline.contains(&comment.id)
                        && comment.viewer_did_author
                        && &comment.body == body
                        && match pending_review_id {
                            Some(id) => comment
                                .pull_request_review
                                .as_ref()
                                .is_some_and(|review| &review.id == id),
                            None => comment.state != "PENDING",
                        }
                })
                .collect::<Vec<_>>();
            if candidates.len() == 1 {
                Some(vec![candidates[0].id.clone()])
            } else {
                None
            }
        }
    })
}
