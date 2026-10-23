//! Explicit, PR-scoped recovery. Unknown writes are verified, never replayed.
use super::*;
use crate::review_store::GitHubViewedIntent;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    RefreshAndRecover,
    Retry,
    Verify,
    DiscardUnsent,
    StopTracking,
}

fn removable(action: RecoveryAction, state: ReviewOperationState) -> bool {
    match action {
        RecoveryAction::DiscardUnsent => matches!(
            state,
            ReviewOperationState::Queued | ReviewOperationState::Failed
        ),
        RecoveryAction::StopTracking => state == ReviewOperationState::DeliveryUnknown,
        _ => false,
    }
}

pub(super) fn viewed_retry_matches(
    operation: &ReviewOperation,
    context: &ReviewOperationContext,
    session: &ReviewSession,
) -> bool {
    operation.state == ReviewOperationState::Failed
        && operation.kind == ReviewOperationKind::SetFileViewed
        && operation
            .context
            .identity()
            .ok()
            .is_some_and(|identity| context.identity().ok() == Some(identity))
        && operation
            .context
            .metadata
            .owner
            .eq_ignore_ascii_case(&context.metadata.owner)
        && operation
            .context
            .metadata
            .repository
            .eq_ignore_ascii_case(&context.metadata.repository)
        && operation.context.metadata.number == context.metadata.number
        && serde_json::from_value::<GitHubViewedIntent>(operation.payload.clone()).is_ok_and(
            |intent| {
                intent
                    .origin
                    .as_ref()
                    .is_some_and(|origin| origin.session == session.id)
                    && intent.validate_for(session).is_ok()
            },
        )
}

/// Terminal successes, queued changes and unknown outcomes are ordering
/// barriers too. An explicit retry may reuse an older UUID, so acceptance
/// revision precedes UUID order. Cross-checkout revision counters never mix.
pub(super) fn latest_viewed_intentions(
    operations: &[ReviewOperation],
    context: &ReviewOperationContext,
    session: &ReviewSession,
) -> BTreeMap<String, (String, bool)> {
    let mut latest = BTreeMap::<String, ((u64, String), bool)>::new();
    for operation in operations
        .iter()
        .filter(|operation| operation.kind == ReviewOperationKind::SetFileViewed)
    {
        if operation
            .context
            .identity()
            .ok()
            .is_none_or(|identity| context.identity().ok() != Some(identity))
        {
            continue;
        }
        let Ok(intent) = serde_json::from_value::<GitHubViewedIntent>(operation.payload.clone())
        else {
            continue;
        };
        let Some(origin) = &intent.origin else {
            continue;
        };
        if origin.session != session.id {
            continue;
        }
        let order = (origin.expected_revision, operation.operation_id.clone());
        for path in &intent.paths {
            let matches = session
                .snapshot
                .groups
                .first()
                .and_then(|group| session.snapshot.file(&group.endpoints.group, path).ok())
                .and_then(|file| file.fingerprint().ok())
                .is_some_and(|fingerprint| intent.fingerprints.get(path) == Some(&fingerprint));
            if matches && latest.get(path).is_none_or(|(prior, _)| prior < &order) {
                latest.insert(path.clone(), (order.clone(), intent.viewed));
            }
        }
    }
    latest
        .into_iter()
        .map(|(path, ((_, id), viewed))| (path, (id, viewed)))
        .collect()
}

fn status(
    view: &WeakEntity<ReviewView>,
    text: impl Into<String>,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let text = text.into();
    view.update(cx, |view, cx| {
        if let Some(panel) = &view.github_panel {
            panel.update(cx, |panel, cx| panel.set_bulk_status(true, text, cx));
        }
        cx.notify();
    })?;
    Ok(())
}

async fn wait_idle(view: &WeakEntity<ReviewView>, cx: &mut gpui::AsyncWindowContext) -> Result<()> {
    let started = Instant::now();
    loop {
        let busy = view.read_with(cx, |view, cx| {
            view.refreshing
                || view.full_refresh_pending
                || view.github_viewed_busy
                || view.github_viewed_preparing
                || !view.github_viewed_pending_clicks.is_empty()
                || view
                    .github_panel
                    .as_ref()
                    .is_some_and(|panel| panel.read(cx).delivery_busy())
        })?;
        if !busy {
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(120),
            "Recovery is still waiting for an active operation. It remains tracked; no request was replayed"
        );
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

async fn wait_operation(
    db: ReviewDb,
    id: String,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<ReviewOperation> {
    let started = Instant::now();
    loop {
        let operation = cx
            .background_spawn({
                let db = db.clone();
                let id = id.clone();
                async move { db.operation(&id) }
            })
            .await?;
        if !matches!(
            operation.state,
            ReviewOperationState::Queued | ReviewOperationState::Sending
        ) {
            return Ok(operation);
        }
        ensure!(
            started.elapsed() < Duration::from_secs(120),
            "Action is still queued or sending; it remains tracked"
        );
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

impl ReviewView {
    pub(super) fn start_recovery(
        &mut self,
        action: RecoveryAction,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recovery_running {
            return;
        }
        if matches!(
            action,
            RecoveryAction::DiscardUnsent | RecoveryAction::StopTracking
        ) && !confirmed
        {
            let (title, detail, accept) = if action == RecoveryAction::StopTracking {
                (
                    "Stop tracking unconfirmed actions for this PR?",
                    "This only archives local tracking. A change may already exist on GitHub; nothing will be undone or resent. Saved comment text remains in the recovery record.",
                    "Stop tracking",
                )
            } else {
                (
                    "Cancel unsent actions for this PR?",
                    "Queued and definitely-unsent failed actions will be archived. Sending requests and unfinished local drafts remain untouched. No GitHub content is deleted.",
                    "Cancel unsent actions",
                )
            };
            let answer = window.prompt(
                gpui::PromptLevel::Warning,
                title,
                Some(detail),
                &[accept, "Keep actions"],
                cx,
            );
            cx.spawn_in(window, async move |view, cx| {
                if matches!(answer.await, Ok(0)) {
                    view.update_in(cx, |view, window, cx| {
                        view.start_recovery(action, true, window, cx)
                    })
                    .log_err();
                }
            })
            .detach();
            return;
        }
        if self.data.session.selector.pull_request_identity().is_none() {
            return;
        }
        self.recovery_running = true;
        if let Some(panel) = &self.github_panel {
            panel.update(cx, |panel, cx| {
                panel.set_bulk_status(true, "Preparing recovery for this PR…".into(), cx)
            });
        }
        self.recovery_task = Some(cx.spawn_in(window, async move |view, cx| {
            let result: Result<String> = async {
                if action == RecoveryAction::RefreshAndRecover {
                    status(&view, "Refreshing the PR comparison before recovery…", cx)?;
                    view.update_in(cx, |view, window, cx| view.refresh(window, cx))?;
                }
                if !matches!(action, RecoveryAction::DiscardUnsent | RecoveryAction::StopTracking) {
                    wait_idle(&view, cx).await?;
                }
                let (db, context, remote, session) = view.read_with(cx, |view, cx| {
                    let panel = view.github_panel.as_ref().context("Open this PR's Pending actions before recovery")?.read(cx);
                    Ok::<_, anyhow::Error>((view.db.clone(), panel.operation_context().clone(), panel.remote_context().clone(), view.data.session.clone()))
                })??;
                let identity = context.identity()?.clone();
                let operations = cx.background_spawn({ let db = db.clone(); let identity = identity.clone(); async move { db.operations(&identity) } }).await?;
                let mut removed = 0;
                let mut verified = 0;
                let mut retried = 0;
                let mut attention = 0;
                if matches!(action, RecoveryAction::DiscardUnsent | RecoveryAction::StopTracking) {
                    for operation in operations.into_iter().filter(|op| removable(action, op.state)) {
                        let id = operation.operation_id.clone();
                        let outcome = cx.background_spawn({ let db = db.clone(); async move { db.archive_operation(id, operation.revision).await } }).await;
                        if outcome.is_ok() { removed += 1; } else { attention += 1; }
                    }
                    return Ok(format!("{removed} actions archived locally · {attention} changed or in-flight actions left tracked. GitHub was not modified."));
                }
                // One explicit preflight, before admitting any recovery writes.
                cx.background_spawn({ let remote = remote.clone(); async move {
                    let client = GitHubClient::default();
                    crate::github_review::ReviewService::new(&client).verify_context(&remote)
                }}).await?;
                if matches!(action, RecoveryAction::Verify | RecoveryAction::RefreshAndRecover) {
                    let unknown = operations.iter().filter(|op| op.state == ReviewOperationState::DeliveryUnknown).cloned().collect::<Vec<_>>();
                    for (index, operation) in unknown.iter().enumerate() {
                        status(&view, format!("Verifying delivery {} of {}…", index + 1, unknown.len()), cx)?;
                        if operation.context != context { attention += 1; continue; }
                        if operation.kind == ReviewOperationKind::SetFileViewed {
                            view.update_in(cx, |view, window, cx| view.handle_github_viewed_operation(operation.clone(), true, window, cx))?;
                        } else {
                            view.update_in(cx, |view, window, cx| {
                                if let Some(panel) = &view.github_panel { panel.update(cx, |panel, cx| panel.reconcile(operation.clone(), window, cx)); }
                            })?;
                        }
                        wait_idle(&view, cx).await?;
                        let current = cx.background_spawn({ let db = db.clone(); let id = operation.operation_id.clone(); async move { db.operation(&id) } }).await?;
                        if current.state == ReviewOperationState::Sent { verified += 1; } else { attention += 1; }
                    }
                }
                if matches!(action, RecoveryAction::Retry | RecoveryAction::RefreshAndRecover) {
                    // Latest intent wins per file; only byte-identical comparisons
                    // can carry an unsent Viewed intention across a refreshed base.
                    let latest = latest_viewed_intentions(&operations, &context, &session);
                    let mut eligible = Vec::new();
                    let mut retry_paths = BTreeMap::new();
                    for operation in operations.iter().filter(|op| op.kind == ReviewOperationKind::SetFileViewed && op.state == ReviewOperationState::Failed) {
                        if !viewed_retry_matches(operation, &context, &session) { attention += 1; continue; }
                        let intent: GitHubViewedIntent = serde_json::from_value(operation.payload.clone())?;
                        for path in intent.paths {
                            if latest.get(&path).is_some_and(|(id, _)| id == &operation.operation_id) {
                                retry_paths.insert(path, intent.viewed);
                            }
                        }
                        eligible.push(operation.clone());
                    }
                    let mut durably_replaced = BTreeSet::new();
                    for desired in [false, true] {
                        // Read the session before the operations. The strict
                        // transaction target below rejects any concurrent write
                        // after this snapshot, including writes from other views.
                        let fresh_session = cx.background_spawn({ let db = db.clone(); let id = session.id.clone(); async move { db.load_session(&id) } }).await?;
                        let fresh = cx.background_spawn({ let db = db.clone(); let identity = context.identity()?.clone(); async move { db.operations(&identity) } }).await?;
                        let fresh_latest = latest_viewed_intentions(&fresh, &context, &fresh_session);
                        let paths = retry_paths.iter().filter(|(path, viewed)| **viewed == desired && latest.get(*path) == fresh_latest.get(*path)
                            && fresh_latest.get(*path).is_some_and(|(id, _)| fresh.iter().any(|operation| &operation.operation_id == id && operation.state == ReviewOperationState::Failed)))
                            .map(|(path, _)| path.clone()).collect::<Vec<_>>();
                        if paths.is_empty() { continue; }
                        status(&view, format!("Recovering {} unchanged file intentions in a batch…", paths.len()), cx)?;
                        let prior_revisions = fresh.iter().map(|op| (op.operation_id.clone(), op.revision)).collect::<BTreeMap<_, _>>();
                        view.update_in(cx, |view, window, cx| view.set_github_paths_viewed_at_revision(paths.clone(), desired, None, Some(fresh_session.revision), window, cx))?;
                        wait_idle(&view, cx).await?;
                        let accepted = cx.background_spawn({ let db = db.clone(); let identity = context.identity()?.clone(); async move { db.operations(&identity) } }).await?;
                        // Do not archive the old intent until a replacement was
                        // actually persisted, regardless of its network outcome.
                        for operation in accepted.iter().filter(|op| prior_revisions.get(&op.operation_id).is_none_or(|revision| *revision < op.revision) && op.context == context && op.kind == ReviewOperationKind::SetFileViewed) {
                            if let Ok(intent) = serde_json::from_value::<GitHubViewedIntent>(operation.payload.clone()) {
                                if intent.viewed == desired && intent.validate_for(&session).is_ok() {
                                    if operation.state == ReviewOperationState::Sent {
                                        retried += intent.paths.len();
                                    } else { attention += 1; }
                                    durably_replaced.extend(intent.paths);
                                }
                            }
                        }
                    }
                    let final_operations = cx.background_spawn({ let db = db.clone(); let identity = context.identity()?.clone(); async move { db.operations(&identity) } }).await?;
                    let final_latest = latest_viewed_intentions(&final_operations, &context, &session);
                    for operation in eligible {
                        let intent: GitHubViewedIntent = serde_json::from_value(operation.payload.clone())?;
                        if intent.paths.iter().all(|path| durably_replaced.contains(path)
                            || final_latest.get(path).is_some_and(|(id, _)| id != &operation.operation_id)) {
                            let outcome = cx.background_spawn({ let db = db.clone(); async move { db.archive_operation(operation.operation_id, operation.revision).await } }).await;
                            if outcome.is_err() { attention += 1; }
                        } else { attention += 1; }
                    }
                    for operation in operations.iter().filter(|op| op.state == ReviewOperationState::Failed && op.kind != ReviewOperationKind::SetFileViewed) {
                        if operation.context != context || operation.location == ReviewLocationState::NeedsLocation { attention += 1; continue; }
                        let admitted = view.update_in(cx, |view, window, cx| {
                            view.github_panel.as_ref().is_some_and(|panel| panel.update(cx, |panel, cx| {
                                if panel.has_composer() { return false; }
                                panel.retry_failed_operation(operation.clone(), window, cx);
                                true
                            }))
                        })?;
                        if !admitted { attention += 1; continue; }
                        wait_idle(&view, cx).await?;
                        let current = wait_operation(db.clone(), operation.operation_id.clone(), cx).await?;
                        if current.state == ReviewOperationState::Sent { retried += 1; } else { attention += 1; }
                    }
                }
                let outstanding = cx.background_spawn({ let db = db.clone(); async move {
                    Ok::<_, anyhow::Error>(db.operations(&identity)?.iter().filter(|operation| !matches!(operation.state, ReviewOperationState::LocalDraft | ReviewOperationState::Sent)).count())
                }}).await?;
                Ok(format!("{retried} retried changes confirmed · {verified} earlier deliveries verified · {outstanding} actions pending · {attention} need attention. Drafts remain local."))
            }.await;
            view.update(cx, |view, cx| {
                view.recovery_running = false;
                let message = match result { Ok(message) => message, Err(error) => format!("Recovery stopped: {error:#}. Pending actions remain tracked.") };
                if let Ok(session) = view.db.load_session(&view.data.session.id) {
                    if session.snapshot.generation == view.data.session.snapshot.generation {
                        if let Ok(mut data) = read_data(&view.db, session) {
                            data.session.presentation = view.data.session.presentation.clone();
                            view.observed_revision = view.observed_revision.max(data.session.revision);
                            view.github_viewed_intents = view.db.github_viewed_local_intents(&data.session).unwrap_or_default();
                            view.data = data;
                        }
                    }
                }
                if let Some(panel) = &view.github_panel { panel.update(cx, |panel, cx| {
                    panel.set_bulk_status(false, message, cx);
                    panel.reload_local_operations(cx);
                }); }
                cx.notify();
            }).log_err();
        }));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_removal_never_cancels_sending_or_publishes_drafts() {
        for state in [
            ReviewOperationState::LocalDraft,
            ReviewOperationState::Sending,
            ReviewOperationState::Sent,
            ReviewOperationState::DeliveryUnknown,
        ] {
            assert!(!removable(RecoveryAction::DiscardUnsent, state));
        }
        assert!(removable(
            RecoveryAction::DiscardUnsent,
            ReviewOperationState::Queued
        ));
        assert!(removable(
            RecoveryAction::DiscardUnsent,
            ReviewOperationState::Failed
        ));
        assert!(removable(
            RecoveryAction::StopTracking,
            ReviewOperationState::DeliveryUnknown
        ));
        assert!(!removable(
            RecoveryAction::StopTracking,
            ReviewOperationState::Sending
        ));
    }
}
