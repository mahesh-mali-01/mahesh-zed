use super::*;
use crate::{
    github_review::{ReviewContext, ReviewService, ReviewWriteError},
    review_github_viewed::{
        ViewedCheckpointAction, folder_viewed_batches, reconcile_folder_viewed,
    },
    review_store::{GitHubViewedIntent, GitHubViewedSyncTarget},
};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "review-test-support")]
#[path = "review_view_viewed_fixture.rs"]
pub(super) mod fixture;

struct ViewedTransport {
    client: GitHubClient,
    #[cfg(feature = "review-test-support")]
    fixture: Option<fixture::FixtureTransport>,
}
impl ViewedTransport {
    fn new(client: GitHubClient, context: &ReviewContext) -> Result<Self> {
        #[cfg(feature = "review-test-support")]
        let fixture = fixture::FixtureTransport::for_context(context)?;
        #[cfg(not(feature = "review-test-support"))]
        let _ = context;
        Ok(Self {
            client,
            #[cfg(feature = "review-test-support")]
            fixture,
        })
    }
}
impl crate::github_review::ReviewTransport for ViewedTransport {
    fn graphql(&self, query: &str, variables: serde_json::Value) -> Result<serde_json::Value> {
        #[cfg(feature = "review-test-support")]
        if let Some(fixture) = &self.fixture {
            return fixture.graphql(query, variables);
        }
        self.client.graphql(query, variables)
    }
    fn rest_json(
        &self,
        method: &str,
        endpoint: &str,
        input: Option<serde_json::Value>,
    ) -> Result<Option<serde_json::Value>> {
        #[cfg(feature = "review-test-support")]
        if self.fixture.is_some() {
            anyhow::bail!("Offline Viewed fixture does not support REST");
        }
        self.client.rest_json(method, endpoint, input)
    }
}

const RETAIN_FILE_BUDGET: usize = 64 * 1024 * 1024;
const RETAIN_OPERATION_BUDGET: usize = 256 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct ViewedRemainder {
    pub(super) paths: Vec<String>,
    total: usize,
    origin: ReviewOperation,
    viewed: bool,
    generation: u64,
}
impl ViewedRemainder {
    pub(super) fn summary(&self) -> String {
        format!(
            "{}/{} confirmed",
            self.total.saturating_sub(self.paths.len()),
            self.total
        )
    }

    fn validate_context(&self, context: &ReviewOperationContext, generation: u64) -> Result<()> {
        ensure!(
            self.origin.context == *context && self.generation == generation,
            "PR context changed since Viewed reconciliation; reconcile again before resuming"
        );
        ensure!(
            self.origin.state == ReviewOperationState::DeliveryUnknown
                && self.origin.kind == ReviewOperationKind::SetFileViewed
                && !self.paths.is_empty(),
            "Remaining Viewed intent is not eligible for recovery"
        );
        Ok(())
    }
}

fn viewed_readback_authoritative(current: &ReviewSession, observed: &ReviewSession) -> bool {
    current.id == observed.id
        && current.snapshot.generation == observed.snapshot.generation
        && observed.revision >= current.revision
}

fn remaining_after_read(
    operation: &ReviewOperation,
    paths: &[String],
    viewed: bool,
    generation: u64,
    remote: &[crate::github_review::RemoteViewedFile],
) -> Result<Option<ViewedRemainder>> {
    let state = reconcile_folder_viewed(paths, viewed, Some(remote))?;
    ensure!(
        state.unknown.is_empty(),
        "Viewed delivery remains unconfirmed: {} match, {} differ, {} unknown. No requests were replayed.",
        state.confirmed.len(),
        state.not_applied.len(),
        state.unknown.len()
    );
    Ok((!state.not_applied.is_empty()).then(|| ViewedRemainder {
        paths: state.not_applied,
        total: paths.len(),
        origin: operation.clone(),
        viewed,
        generation,
    }))
}

pub(super) struct ViewedWork {
    revision: u64,
    imports: Vec<String>,
    operation: Option<ReviewOperation>,
    remaining: Option<ViewedRemainder>,
}

#[cfg(test)]
impl ViewedWork {
    pub(super) fn observed_for_test(revision: u64) -> Self {
        Self {
            revision,
            imports: Vec::new(),
            operation: None,
            remaining: None,
        }
    }
}

impl ReviewView {
    fn github_viewed_context(&self) -> Result<(ReviewOperationContext, ReviewContext)> {
        ensure!(
            self.data.session.selector.supports_github_viewed(),
            "Only All PR changes synchronizes GitHub Viewed state"
        );
        let metadata = self
            .data
            .pull_request
            .clone()
            .context("PR metadata is missing")?;
        let operation = ReviewOperationContext {
            selector: self.data.session.selector.clone(),
            metadata,
        };
        operation.validate()?;
        let context = ReviewContext {
            reference: PullRequestReference::parse(&operation.metadata.url(), None)?,
            identity: operation.identity()?.clone(),
            head_oid: operation.metadata.head_ref_oid.clone(),
            base_tip_oid: operation.metadata.base_ref_oid.clone(),
        };
        context.validate()?;
        Ok((operation, context))
    }

    pub(super) fn sync_github_viewed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.github_viewed_busy || self.refreshing || self.outdated() {
            return;
        }
        let (_, context) = match self.github_viewed_context() {
            Ok(context) => context,
            Err(error) => {
                self.github_viewed_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let db = self.db.clone();
        let session = self.data.session.clone();
        let recovery = crate::review_outbox_runtime::ready(db.clone(), cx);
        self.begin_viewed_work("Reading GitHub Viewed state…", cx);
        let (client, guard) = GitHubClient::cancellable();
        self.github_viewed_guard = Some(guard);
        let cancelled = self.github_viewed_cancel.clone();
        let task = cx.background_spawn({
            let session = session.clone();
            async move {
                recovery
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let revision = db
                    .recover_github_viewed_receipts(sync_target(&session, session.revision))
                    .await?;
                let transport = ViewedTransport::new(client, &context)?;
                let service = ReviewService::new(&transport);
                let remote = service.load_viewed(&context)?;
                ensure!(
                    !cancelled.load(Ordering::Relaxed),
                    "Viewed import stopped before applying its server read"
                );
                let result = db
                    .sync_github_viewed(
                        sync_target(&session, revision),
                        context,
                        remote,
                        BTreeSet::new(),
                    )
                    .await?;
                Ok::<_, anyhow::Error>(ViewedWork {
                    revision: result.revision,
                    imports: result
                        .decisions
                        .into_iter()
                        .filter(|decision| decision.action == ViewedCheckpointAction::ImportCurrent)
                        .map(|decision| decision.path)
                        .collect(),
                    operation: None,
                    remaining: None,
                })
            }
        });
        self.finish_viewed_work(task, session, window, cx);
    }

    pub(super) fn begin_viewed_work(&mut self, message: &str, cx: &mut Context<Self>) {
        self.github_viewed_work_sequence = self.github_viewed_work_sequence.wrapping_add(1);
        self.github_viewed_busy = true;
        self.github_viewed_remaining = None;
        self.github_viewed_error = None;
        self.github_viewed_guard = None;
        self.github_viewed_status = Some(message.into());
        self.github_viewed_cancel = Arc::new(AtomicBool::new(false));
        cx.notify();
    }

    /// Completion belongs to a work token, not the revision captured before
    /// that work's own receipt/checkpoint writes. A reload may adopt those writes
    /// while retention is still running without transferring work ownership.
    pub(super) fn complete_viewed_work_owner(
        &mut self,
        sequence: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.github_viewed_work_sequence != sequence {
            return false;
        }
        self.github_viewed_busy = false;
        self.github_viewed_progress_task = None;
        self.github_viewed_guard = None;
        cx.notify();
        true
    }

    /// Local review intent overlays confirmed server state. It is never a
    /// substitute for an independently verified GitHub Viewed receipt.
    pub(super) fn effective_github_viewed(&self, path: &str) -> bool {
        self.github_viewed_pending_clicks
            .get(path)
            .or_else(|| self.github_viewed_accepting_clicks.get(path))
            .or_else(|| self.github_viewed_intents.get(path))
            .copied()
            .unwrap_or_else(|| self.data.viewed.contains(path))
    }

    pub(super) fn set_github_paths_viewed(
        &mut self,
        paths: Vec<String>,
        viewed: bool,
        existing: Option<ReviewOperation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.github_viewed_preparing {
            // Preserve every rapid user action while immutable contents are
            // retained. Latest click wins per path; saving is shown until durable.
            for path in paths {
                self.github_viewed_pending_clicks.insert(path, viewed);
            }
            cx.notify();
            return;
        }
        if self.refreshing {
            for path in paths {
                self.github_viewed_pending_clicks.insert(path, viewed);
            }
            cx.notify();
            return;
        }
        let (context, remote) = match self.github_viewed_context() {
            Ok(context) => context,
            Err(error) => {
                self.github_viewed_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let session = self.data.session.clone();
        if let Err(error) = viewed_intent(&session, paths.clone(), viewed) {
            self.github_viewed_error = Some(error.to_string());
            cx.notify();
            return;
        }
        // Transfer only after context/fingerprint validation. Busy file loading
        // does not block local intent persistence against this pinned snapshot.
        for path in &paths {
            if self.github_viewed_pending_clicks.get(path) == Some(&viewed) {
                self.github_viewed_pending_clicks.remove(path);
            }
        }
        self.github_viewed_preparing = true;
        self.github_viewed_accepting_clicks =
            paths.iter().map(|path| (path.clone(), viewed)).collect();
        self.begin_viewed_work("Saving review progress locally…", cx);
        let db = self.db.clone();
        let repository = self.repository.clone();
        let recovery = crate::review_outbox_runtime::ready(db.clone(), cx);
        let app = cx.to_async();
        let delivery_db = self.db.clone();
        cx.spawn_in(window, async move |this, cx| {
            let accepted: Result<(ReviewOperation, ReviewSession)> = async {
                recovery
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                let group = session
                    .snapshot
                    .groups
                    .first()
                    .context("PR comparison missing")?
                    .endpoints
                    .group
                    .clone();
                // Retain exact immutable bytes before the file collapses or its
                // editor can be evicted. Stage all reads before checkpoint writes.
                let mut retained = Vec::new();
                let mut retained_bytes = 0usize;
                for path in &paths {
                    let comparison = session.snapshot.file(&group, path)?.clone();
                    let contents = if viewed {
                        let contents = retained_comparison(
                            &repository,
                            &comparison,
                            RETAIN_FILE_BUDGET
                                .min(RETAIN_OPERATION_BUDGET.saturating_sub(retained_bytes)),
                            cx,
                        )
                        .await?;
                        retained_bytes = retained_bytes
                            .checked_add(contents.old.as_ref().map_or(0, Vec::len))
                            .and_then(|bytes| {
                                bytes.checked_add(contents.new.as_ref().map_or(0, Vec::len))
                            })
                            .context("Retained history size overflow")?;
                        Some(contents)
                    } else {
                        None
                    };
                    retained.push((path.clone(), comparison.fingerprint()?, contents));
                }
                let contents = retained
                    .into_iter()
                    .filter_map(|(path, _, contents)| contents.map(|contents| (path, contents)))
                    .collect::<BTreeMap<_, _>>();
                let intent = viewed_intent(&session, paths.clone(), viewed)?;
                let (operation, revision) = cx
                    .background_spawn({
                        let db = db.clone();
                        let context = context.clone();
                        let expected_session = session.clone();
                        async move {
                            let current = db.load_session(&expected_session.id)?;
                            ensure!(
                                current.snapshot.generation == expected_session.snapshot.generation
                                    && current.selector == expected_session.selector,
                                "Review changed before saving Viewed clicks"
                            );
                            let target = sync_target(&current, current.revision);
                            db.accept_github_viewed_intent(
                                target,
                                context,
                                intent,
                                contents,
                                existing,
                                RETAIN_FILE_BUDGET,
                            )
                            .await
                        }
                    })
                    .await?;
                let mut accepted_session = session.clone();
                accepted_session.revision = revision;
                Ok((operation, accepted_session))
            }
            .await;
            let delivery = accepted.as_ref().ok().map(|(operation, session)| {
                app.update(|cx| {
                    enqueue_viewed_delivery(
                        delivery_db,
                        operation.clone(),
                        remote,
                        session.clone(),
                        cx,
                    )
                })
            });
            this.update_in(cx, |this, window, cx| {
                this.github_viewed_preparing = false;
                this.github_viewed_accepting_clicks.clear();
                let accepted_ok = accepted.is_ok();
                match accepted {
                    Ok((_operation, accepted_session)) => {
                        this.data.session.revision = accepted_session.revision;
                        this.observed_revision = accepted_session.revision;
                        this.github_viewed_intent_sequence =
                            this.github_viewed_intent_sequence.saturating_add(1);
                        for path in &paths {
                            this.github_viewed_intents.insert(path.clone(), viewed);
                            if viewed {
                                this.collapsed_review_files.insert(path.clone());
                            } else {
                                this.collapsed_review_files.remove(path);
                            }
                        }
                        this.pin_locally_viewed_selection();
                        this.apply_review_file_folds(cx);
                        this.github_viewed_status =
                            Some("Saved locally · syncing Viewed state with GitHub…".into());
                        if let Some(panel) = &this.github_panel {
                            panel.update(cx, |panel, cx| panel.reload_local_operations(cx));
                        }
                        if let Some(delivery) = delivery {
                            this.observe_github_viewed_delivery(
                                delivery,
                                accepted_session,
                                window,
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        for path in &paths {
                            this.github_viewed_pending_clicks
                                .entry(path.clone())
                                .or_insert(viewed);
                        }
                        this.github_viewed_resume_origin = None;
                        this.github_viewed_busy = false;
                        this.github_viewed_status = None;
                        this.github_viewed_error = Some(format!(
                            "Review progress was not queued; current file remains open: {error:#}"
                        ));
                    }
                }
                if let Some(viewed) = this
                    .github_viewed_pending_clicks
                    .values()
                    .next()
                    .copied()
                    .filter(|_| accepted_ok)
                {
                    let paths = this
                        .github_viewed_pending_clicks
                        .iter()
                        .filter(|(_, value)| **value == viewed)
                        .take(20)
                        .map(|(path, _)| path.clone())
                        .collect::<Vec<_>>();
                    this.set_github_paths_viewed(paths, viewed, None, window, cx);
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn observe_github_viewed_delivery(
        &mut self,
        delivery: crate::review_outbox_runtime::Delivery,
        session: ReviewSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let db = self.db.clone();
        let work_sequence = self.github_viewed_work_sequence;
        cx.spawn_in(window, async move |this, cx| {
            let result = delivery.await;
            let id = session.id.clone();
            let data: Result<_> = cx
                .background_spawn(async move {
                    let session = db.load_session(&id)?;
                    let intents = db.github_viewed_local_intents(&session)?;
                    let in_flight = db
                        .operations(
                            session
                                .selector
                                .pull_request_identity()
                                .context("Missing account")?,
                        )?
                        .iter()
                        .any(|operation| {
                            operation.kind == ReviewOperationKind::SetFileViewed
                                && matches!(
                                    operation.state,
                                    ReviewOperationState::Queued | ReviewOperationState::Sending
                                )
                        });
                    Ok((read_data(&db, session)?, intents, in_flight))
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.github_viewed_work_sequence != work_sequence {
                    return;
                }
                // Read-back is per-path and ordered by durable operation ID,
                // not a whole-folder/global sequence. A response captured before
                // a newer local acceptance cannot replace that newer overlay.
                if let Ok((mut data, intents, in_flight)) = data {
                    if data.session.revision >= this.data.session.revision
                        && data.session.snapshot.generation == this.data.session.snapshot.generation
                    {
                        data.session.presentation = this.data.session.presentation.clone();
                        this.github_viewed_intents = intents;
                        this.github_viewed_busy = in_flight;
                        this.data = data;
                        this.observed_revision = this.data.session.revision;
                    }
                }
                let resume_original = match (&result, &this.github_viewed_resume_origin) {
                    (Ok(operation), Some((origin, expected_paths)))
                        if operation.state == ReviewOperationState::Sent
                            && !this.github_viewed_busy =>
                    {
                        let matches =
                            serde_json::from_value::<GitHubViewedIntent>(operation.payload.clone())
                                .ok()
                                .is_some_and(|intent| intent.paths == *expected_paths);
                        matches.then(|| origin.clone())
                    }
                    _ => None,
                };
                match result {
                    Ok(operation) if operation.state == ReviewOperationState::Sent => {
                        if this.github_viewed_intents.is_empty() {
                            this.github_viewed_error = None;
                            this.github_viewed_status =
                                Some("GitHub Viewed state synchronized.".into());
                        } else {
                            this.github_viewed_status = Some(format!(
                                "Saved locally · {} files awaiting GitHub confirmation",
                                this.github_viewed_intents.len()
                            ));
                        }
                    }
                    Ok(operation) => {
                        this.github_viewed_status =
                            Some("Saved locally · GitHub sync needs attention.".into());
                        this.github_viewed_error =
                            Some(operation.last_error.unwrap_or_else(|| {
                                "Open Threads & review to retry or reconcile this operation.".into()
                            }));
                    }
                    Err(error) => {
                        this.github_viewed_status =
                            Some("Saved locally · GitHub sync needs attention.".into());
                        this.github_viewed_error = Some(error.to_string());
                    }
                }
                this.apply_review_file_folds(cx);
                if let Some(panel) = &this.github_panel {
                    panel.update(cx, |panel, cx| panel.reload_local_operations(cx));
                }
                if let Some(origin) = resume_original {
                    this.github_viewed_resume_origin = None;
                    this.handle_github_viewed_operation(origin, true, window, cx);
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub(super) fn mark_github_folder(
        &mut self,
        folder: &str,
        viewed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match folder_viewed_batches(&self.data.session, folder) {
            Ok(batches) => self.set_github_paths_viewed(
                batches.into_iter().flatten().collect(),
                viewed,
                None,
                window,
                cx,
            ),
            Err(error) => {
                self.github_viewed_error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    pub(super) fn resume_github_viewed_remaining(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(remaining) = self.github_viewed_remaining.clone() else {
            return;
        };
        let eligible = self.github_viewed_context().and_then(|(context, _)| {
            remaining.validate_context(&context, self.data.session.snapshot.generation)?;
            let current = self.db.operation(&remaining.origin.operation_id)?;
            ensure!(
                current == remaining.origin,
                "Viewed operation changed; reconcile again before resuming"
            );
            Ok(())
        });
        if let Err(error) = eligible {
            self.github_viewed_remaining = None;
            self.github_viewed_error = Some(error.to_string());
            cx.notify();
            return;
        }
        self.set_github_paths_viewed(remaining.paths.clone(), remaining.viewed, None, window, cx);
        if self.github_viewed_preparing {
            self.github_viewed_resume_origin = Some((remaining.origin, remaining.paths));
        }
    }

    pub(super) fn handle_github_viewed_operation(
        &mut self,
        operation: ReviewOperation,
        reconcile: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.github_viewed_busy || self.refreshing || self.outdated() {
            return;
        }
        if !reconcile {
            match serde_json::from_value::<GitHubViewedIntent>(operation.payload.clone()) {
                Ok(intent) => self.set_github_paths_viewed(
                    intent.paths,
                    intent.viewed,
                    Some(operation),
                    window,
                    cx,
                ),
                Err(error) => {
                    self.github_viewed_error = Some(format!("Cannot read Viewed intent: {error}"));
                    cx.notify();
                }
            }
            return;
        }
        let (operation_context, context) = match self.github_viewed_context() {
            Ok(context) => context,
            Err(error) => {
                self.github_viewed_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        if operation.context != operation_context {
            self.github_viewed_error = Some("This Viewed intent targets another pinned PR head or base. Its delivery remains unknown; create a fresh explicit Viewed intent for the current revision.".into());
            cx.notify();
            return;
        }
        let db = self.db.clone();
        let session = self.data.session.clone();
        let recovery = crate::review_outbox_runtime::ready(db.clone(), cx);
        self.begin_viewed_work(
            "Reconciling Viewed state with GitHub; no requests will be replayed…",
            cx,
        );
        let task = cx.background_spawn({
            let session = session.clone();
            async move {
                recovery
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                ensure!(
                    operation.context == operation_context,
                    "Viewed reconciliation context changed"
                );
                ensure!(
                    operation.state == ReviewOperationState::DeliveryUnknown
                        && operation.kind == ReviewOperationKind::SetFileViewed,
                    "Only unconfirmed Viewed delivery can be reconciled here"
                );
                let intent: GitHubViewedIntent = serde_json::from_value(operation.payload.clone())?;
                intent.validate_for(&session)?;
                let client = GitHubClient::default();
                let transport = ViewedTransport::new(client, &context)?;
                let remote = ReviewService::new(&transport).load_viewed(&context)?;
                if let Some(remaining) = remaining_after_read(
                    &operation,
                    &intent.paths,
                    intent.viewed,
                    session.snapshot.generation,
                    &remote,
                )? {
                    return Ok(ViewedWork {
                        revision: session.revision,
                        imports: Vec::new(),
                        operation: None,
                        remaining: Some(remaining),
                    });
                }
                settle_viewed_reconciliation(&db, &session, context, operation, remote).await
            }
        });
        self.finish_viewed_work(task, session, window, cx);
    }

    pub(super) fn finish_viewed_work(
        &mut self,
        task: Task<Result<ViewedWork>>,
        session: ReviewSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let db = self.db.clone();
        let repository = self.repository.clone();
        let cancelled = self.github_viewed_cancel.clone();
        let work_sequence = self.github_viewed_work_sequence;
        // The write's background task remains owned until its durable outcome is
        // recorded even if its ReviewView closes. Only retention is stoppable.
        cx.spawn_in(window, async move |this, cx| {
            let mut remaining = None;
            let mut confirmed_paths = Vec::new();
            let mut confirmed_unviewed_paths = Vec::new();
            let result: Result<()> = async {
                let work = task.await?;
                if let Some(recovery) = work.remaining {
                    let total = serde_json::from_value::<GitHubViewedIntent>(recovery.origin.payload.clone())?.paths.len();
                    let message = format!("{} / {total} files independently confirmed. {} remaining; choose Sync remaining to continue. No requests were replayed.", total - recovery.paths.len(), recovery.paths.len());
                    remaining = Some(recovery);
                    anyhow::bail!(message);
                }
                if let Some(operation) = &work.operation {
                    ensure!(operation.state == ReviewOperationState::Sent, "{}", operation.last_error.as_deref().unwrap_or("Viewed change is not confirmed; use Discussions to reconcile or resume its durable operation"));
                }
                confirmed_paths = if work.operation.is_some() { work.imports.clone() } else { Vec::new() };
                if let Some(operation) = &work.operation {
                    let intent: GitHubViewedIntent = serde_json::from_value(operation.payload.clone())?;
                    if !intent.viewed { confirmed_unviewed_paths = intent.paths; }
                }
                // Publish the complete guarded server observation before slow
                // blob retention. Do not rebuild the editor or move selection.
                let id = session.id.clone();
                let observed = cx.background_spawn({ let db = db.clone(); async move {
                    let data = read_data(&db, db.load_session(&id)?)?;
                    let intents = db.github_viewed_local_intents(&data.session)?;
                    Ok::<_, anyhow::Error>((data, intents))
                }}).await?;
                this.update(cx, |this, cx| {
                    if this.github_viewed_work_sequence != work_sequence { return; }
                    if viewed_readback_authoritative(&this.data.session, &observed.0.session)
                        && this.data.session.selector == observed.0.session.selector
                    {
                        let (mut data, intents) = observed;
                        data.session.presentation = this.data.session.presentation.clone();
                        this.observed_revision = this.observed_revision.max(data.session.revision);
                        this.data = data;
                        this.github_viewed_intents = intents;
                    }
                    this.github_viewed_status = Some("Viewed flags confirmed · Saving review progress…".into());
                    cx.notify();
                }).log_err();
                #[cfg(test)]
                if let Some(pause) = this.update(cx, |this, _| this.github_viewed_retention_pause.take())? {
                    pause.await;
                }
                let identity = session.selector.pull_request_identity().context("PR account is missing")?.clone();
                let group = session.snapshot.groups.first().context("PR comparison missing")?.endpoints.group.clone();
                let mut revision = work.revision;
                let mut retained_bytes = 0usize;
                let total = work.imports.len();
                for (index, path) in work.imports.into_iter().enumerate() {
                    ensure!(!cancelled.load(Ordering::Relaxed), "Stopped retaining file history. Confirmed GitHub flags and prior checkpoints remain intact.");
                    if this.upgrade().is_none() { break; }
                    let comparison = session.snapshot.file(&group, &path)?.clone();
                    this.update(cx, |this, cx| { this.github_viewed_status = Some(format!("Retaining exact review history · {} / {total} · {path}", index + 1)); cx.notify(); }).log_err();
                    let contents = retained_comparison(&repository, &comparison, RETAIN_FILE_BUDGET.min(RETAIN_OPERATION_BUDGET.saturating_sub(retained_bytes)), cx).await?;
                    retained_bytes = retained_bytes.checked_add(contents.old.as_ref().map_or(0, Vec::len)).and_then(|total| total.checked_add(contents.new.as_ref().map_or(0, Vec::len))).context("Retained byte count overflow")?;
                    let target = ViewedTarget { session: session.id.clone(), expected_revision: revision, displayed_generation: session.snapshot.generation, group: group.clone(), path, fingerprint: comparison.fingerprint()? };
                    revision = cx.background_spawn({ let db = db.clone(); let identity = identity.clone(); async move { db.import_github_viewed_checkpoint(target, identity, contents, RETAIN_FILE_BUDGET).await } }).await?;
                }
                Ok(())
            }.await;
            let id = session.id.clone();
            let (mut data, local_intents) = cx.background_spawn({ let db = db.clone(); async move {
                let data = db.load_session(&id).and_then(|session| read_data(&db, session));
                let intents = data.as_ref().ok().and_then(|data| db.github_viewed_local_intents(&data.session).ok());
                (data, intents)
            } }).await;
            this.update_in(cx, |this, window, cx| {
                if !this.complete_viewed_work_owner(work_sequence, cx) { return; }
                // The current owner must finish even when Reload adopted its own
                // newer receipt revision. Only data/evidence installation needs
                // revision authority; a revision is never a work-owner token.
                if this.data.session.id != session.id
                    || this.data.session.snapshot.generation != session.snapshot.generation
                    || this.data.session.selector != session.selector
                { return; }
                if let Ok(observed) = &data {
                    if !viewed_readback_authoritative(&this.data.session, &observed.session) { return; }
                }
                if let Ok(data) = &mut data {
                    if data.session.snapshot.generation == this.data.session.snapshot.generation {
                        data.session.presentation = this.data.session.presentation.clone();
                    }
                }
                this.collapsed_review_files.extend(confirmed_paths);
                for path in confirmed_unviewed_paths { this.collapsed_review_files.remove(&path); }
                if let Some(intents) = local_intents { this.github_viewed_intents = intents; }
                this.finish(data, window, cx);
                this.github_viewed_remaining = remaining;
                match result {
                    Ok(()) => { this.github_viewed_status = Some("GitHub Viewed state and retained local review history are synchronized.".into()); this.github_viewed_error = None; }
                    Err(error) => { this.github_viewed_status = None; this.github_viewed_error = Some(format!("Viewed synchronization stopped: {error:#}")); }
                }
                if let Some(panel) = &this.github_panel { panel.update(cx, |panel, cx| panel.reload_local_operations(cx)); }
                cx.notify();
            }).log_err();
        }).detach();
    }
}

/// A guarded independent read settles the delivery record, then synchronizes
/// observations against the latest durable session. Never apply the original
/// whole-folder receipt after a newer remaining-path intent advanced state.
pub(super) async fn settle_viewed_reconciliation(
    db: &ReviewDb,
    session: &ReviewSession,
    context: ReviewContext,
    operation: ReviewOperation,
    remote: Vec<crate::github_review::RemoteViewedFile>,
) -> Result<ViewedWork> {
    let intent: GitHubViewedIntent = serde_json::from_value(operation.payload.clone())?;
    intent.validate_for(session)?;
    ensure!(
        remaining_after_read(
            &operation,
            &intent.paths,
            intent.viewed,
            session.snapshot.generation,
            &remote
        )?
        .is_none(),
        "Viewed reconciliation still has differing files"
    );
    let confirmed = db
        .reconcile_operation(
            operation.operation_id,
            operation.revision,
            ReviewReconciliation {
                viewer_node_id: context.identity.viewer_node_id.clone(),
                head_oid: context.head_oid.clone(),
                observed_server_ids: Vec::new(),
                outcome: ReviewReconciliationOutcome::Delivered {
                    server_ids: Vec::new(),
                },
            },
        )
        .await?;
    let current = db.load_session(&session.id)?;
    ensure!(
        current.snapshot.generation == session.snapshot.generation
            && current.selector == session.selector,
        "Viewed reconciliation snapshot changed"
    );
    let observed = db
        .sync_github_viewed(
            sync_target(&current, current.revision),
            context,
            remote,
            BTreeSet::new(),
        )
        .await?;
    Ok(ViewedWork {
        revision: observed.revision,
        imports: observed
            .decisions
            .into_iter()
            .filter(|decision| decision.action == ViewedCheckpointAction::ImportCurrent)
            .map(|decision| decision.path)
            .collect(),
        operation: Some(confirmed),
        remaining: None,
    })
}

/// Only compatible, non-overlapping queued intents may share a transport call.
/// Overlaps keep their durable lane ordering; in-flight/unknown writes never
/// become candidates for automatic replay.
fn viewed_delivery_batch(
    primary: ReviewOperation,
    mut operations: Vec<ReviewOperation>,
    session: &ReviewSession,
) -> Result<Vec<(ReviewOperation, GitHubViewedIntent)>> {
    let intent: GitHubViewedIntent = serde_json::from_value(primary.payload.clone())?;
    intent.validate_for(session)?;
    let mut paths = intent.paths.iter().cloned().collect::<BTreeSet<_>>();
    let mut batch = vec![(primary.clone(), intent.clone())];
    let accepted_revision = intent
        .origin
        .as_ref()
        .map_or(0, |origin| origin.expected_revision);
    operations.sort_by_key(|operation| {
        serde_json::from_value::<GitHubViewedIntent>(operation.payload.clone())
            .ok()
            .and_then(|intent| intent.origin)
            .map_or(0, |origin| origin.expected_revision)
    });
    // An older overlapping operation must retain its position in the lane.
    // Do not pull a newer intent ahead of any intervening opposite-state write.
    for operation in operations {
        if operation.operation_id == primary.operation_id {
            continue;
        }
        if operation.kind != ReviewOperationKind::SetFileViewed
            || operation.context != primary.context
        {
            continue;
        }
        if operation.state != ReviewOperationState::Queued {
            continue;
        }
        let candidate: GitHubViewedIntent = serde_json::from_value(operation.payload.clone())?;
        if candidate.validate_for(session).is_err()
            || candidate.origin.as_ref().is_none_or(|origin| {
                origin.session != session.id
                    || origin.displayed_generation != session.snapshot.generation
            })
        {
            continue;
        }
        if candidate
            .origin
            .as_ref()
            .is_some_and(|origin| origin.expected_revision < accepted_revision)
        {
            continue;
        }
        if candidate.viewed != intent.viewed
            || candidate.paths.iter().any(|path| paths.contains(path))
        {
            break;
        }
        if paths.len().saturating_add(candidate.paths.len()) > 20 {
            break;
        }
        paths.extend(candidate.paths.iter().cloned());
        batch.push((operation, candidate));
    }
    Ok(batch)
}

pub(super) fn enqueue_viewed_delivery(
    db: ReviewDb,
    operation: ReviewOperation,
    remote: ReviewContext,
    session: ReviewSession,
    cx: &mut gpui::App,
) -> crate::review_outbox_runtime::Delivery {
    let lane = format!(
        "{}:{}:{}:{}",
        remote.identity.host,
        remote.identity.repository_node_id,
        remote.identity.pull_request_node_id,
        remote.identity.viewer_node_id
    );
    let executor = cx.background_executor().clone();
    crate::review_outbox_runtime::deliver(
        lane,
        operation.operation_id.clone(),
        async move {
            // A previous combined dispatch may already have completed this row.
            // Reload after debounce as rapid durable toggles update its revision.
            let current = db.operation(&operation.operation_id)?;
            if current.state != ReviewOperationState::Queued {
                return Ok(current);
            }
            executor.timer(std::time::Duration::from_secs(1)).await;
            let current = db.operation(&operation.operation_id)?;
            if current.state != ReviewOperationState::Queued {
                return Ok(current);
            }
            let current_session = db.load_session(&session.id)?;
            ensure!(
                current_session.snapshot.generation == session.snapshot.generation
                    && current_session.selector == session.selector,
                "Viewed batch snapshot changed before dispatch"
            );
            let operations = db.operations(&remote.identity)?;
            let batch = viewed_delivery_batch(current, operations, &current_session)?;
            let transport = ViewedTransport::new(GitHubClient::default(), &remote);
            let mut claimed = Vec::new();
            for (operation, intent) in batch {
                match db
                    .claim_operation(operation.operation_id, operation.revision, Vec::new())
                    .await
                {
                    Ok(operation) => claimed.push((operation, intent)),
                    Err(error) => {
                        for (operation, _) in claimed {
                            db.finish_operation(operation.operation_id, operation.revision, ReviewOperationOutcome::Failed { reason: format!("Viewed batch changed before dispatch; no request sent: {error}") }).await?;
                        }
                        return Err(error);
                    }
                }
            }
            let paths = claimed
                .iter()
                .flat_map(|(_, intent)| intent.paths.iter().cloned())
                .collect::<Vec<_>>();
            let viewed = claimed.first().context("Empty Viewed dispatch")?.1.viewed;
            let batch_id = claimed.first().unwrap().0.operation_id.clone();
            let result = match transport.as_ref() {
                Ok(transport) => ReviewService::new(transport)
                    .set_viewed_batch_with_observation(&remote, &batch_id, &paths, viewed),
                Err(error) => Err(ReviewWriteError::NotSent(format!(
                    "Viewed transport was not initialized; no request dispatched: {error}"
                ))),
            };
            let observation = result
                .as_ref()
                .ok()
                .map(|(_, observation)| observation.clone());
            let mut completed = None;
            for (claimed, intent) in claimed {
                let outcome = match &result {
                    Ok(_) => ReviewOperationOutcome::Sent {
                        server_ids: Vec::new(),
                    },
                    Err(ReviewWriteError::NotSent(reason)) => ReviewOperationOutcome::Failed {
                        reason: reason.clone(),
                    },
                    Err(error @ ReviewWriteError::DeliveryUnknown { .. }) => {
                        ReviewOperationOutcome::DeliveryUnknown {
                            reason: error.to_string(),
                        }
                    }
                };
                let finished = db
                    .finish_operation(claimed.operation_id, claimed.revision, outcome)
                    .await?;
                if let Ok((receipt, _)) = &result {
                    let paths = intent.paths.iter().collect::<BTreeSet<_>>();
                    let receipts = receipt
                        .remote
                        .iter()
                        .filter(|file| paths.contains(&file.path))
                        .cloned()
                        .collect();
                    let current_session = db.load_session(&session.id)?;
                    if let Err(error) = db
                        .record_github_viewed_receipt(
                            sync_target(&current_session, current_session.revision),
                            finished.operation_id.clone(),
                            receipts,
                        )
                        .await
                    {
                        log::info!("Viewed receipt superseded by later local state: {error:#}");
                    }
                }
                if finished.operation_id == operation.operation_id {
                    completed = Some(finished);
                }
            }
            if let Some(observed) = observation {
                // The transport's independent final full read is shared by every
                // original durable operation; never paginate the PR again per click.
                let current = db.load_session(&session.id)?;
                if let Err(error) = db
                    .sync_github_viewed(
                        sync_target(&current, current.revision),
                        remote,
                        observed,
                        BTreeSet::new(),
                    )
                    .await
                {
                    log::info!("Viewed batch reconciliation deferred: {error:#}");
                }
            }
            log::info!(
                "Viewed batch dispatched {} paths in one logical transport operation",
                paths.len()
            );
            completed.context("Viewed batch omitted its owner")
        },
        cx,
    )
}

pub(super) fn sync_target(session: &ReviewSession, revision: u64) -> GitHubViewedSyncTarget {
    GitHubViewedSyncTarget {
        session: session.id.clone(),
        expected_revision: revision,
        displayed_generation: session.snapshot.generation,
    }
}
pub(super) fn viewed_intent(
    session: &ReviewSession,
    paths: Vec<String>,
    viewed: bool,
) -> Result<GitHubViewedIntent> {
    let group = session
        .snapshot
        .groups
        .first()
        .context("PR comparison missing")?;
    let fingerprints = paths
        .iter()
        .map(|path| {
            Ok((
                path.clone(),
                session
                    .snapshot
                    .file(&group.endpoints.group, path)?
                    .fingerprint()?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let intent = GitHubViewedIntent {
        paths,
        viewed,
        fingerprints,
        origin: Some(sync_target(session, session.revision)),
    };
    intent.validate_for(session)?;
    Ok(intent)
}
pub(super) async fn retained_comparison(
    repository: &Entity<Repository>,
    comparison: &FileComparison,
    budget: usize,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<RetainedContents> {
    let objects = comparison
        .old
        .iter()
        .chain(&comparison.new)
        .filter(|side| side.mode != 0o160000)
        .map(|side| side.object.hex.parse())
        .collect::<Result<Vec<_>, _>>()?;
    let sizes = repository
        .read_with(cx, |repository, cx| {
            repository.review_blob_sizes(objects, cx)
        })
        .await?;
    let size = sizes.into_iter().try_fold(0u64, |sum, size| {
        sum.checked_add(size).context("Blob size overflow")
    })?;
    ensure!(
        size <= budget as u64,
        "File history exceeds the bounded {} MiB retention budget; GitHub flag is confirmed but local Since review history was not overwritten",
        budget / (1024 * 1024)
    );
    let mut contents = Vec::new();
    for side in [&comparison.old, &comparison.new] {
        contents.push(
            if let Some(side) = side.as_ref().filter(|side| side.mode != 0o160000) {
                let object = side.object.hex.parse()?;
                Some(
                    repository
                        .read_with(cx, |repository, cx| repository.review_blob(object, cx))
                        .await?,
                )
            } else {
                None
            },
        );
    }
    let mut contents = contents.into_iter();
    let contents = RetainedContents {
        old: contents.next().flatten(),
        new: contents.next().flatten(),
    };
    comparison.validate_retained(&contents, budget)?;
    Ok(contents)
}

impl ReviewView {
    pub(super) fn load_since_review_selected(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor = None;
        self.github_blocks.clear();
        self.github_buffers.clear();
        self.github_capture = None;
        self.github_composer_capture = None;
        self.github_editor_subscription = None;
        self.github_layout_subscription = None;
        self.retained = None;
        self.multi_loaded = false;
        self.error = None;
        self.layout_note =
            Some("Since review · retained reviewed contents → current contents".into());
        if let Some(panel) = &self.github_panel {
            panel.update(cx, |panel, cx| panel.set_incremental_mode(true, cx));
        }
        let Some(path) = self.data.session.presentation.selected_path.clone() else {
            self.busy = false;
            cx.notify();
            return;
        };
        let comparison = match self
            .data
            .session
            .snapshot
            .file(active_group(&self.data), &path)
        {
            Ok(comparison) => comparison.clone(),
            Err(error) => {
                self.error = Some(error.to_string());
                self.busy = false;
                cx.notify();
                return;
            }
        };
        let session = self.data.session.clone();
        let group = active_group(&self.data).to_string();
        let db = self.db.clone();
        let repository = self.repository.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        self.busy = true;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let checkpoint = cx.background_spawn({ let db = db.clone(); let id = session.id.clone(); let group = group.clone(); let path = path.clone(); async move {
                    let metadata = db.reviewed_checkpoint_metadata(&id, &group)?;
                    match metadata.get(&path) { Some(checkpoint) => db.checkpoint(&id, &group, checkpoint.comparison.file_key()?), None => Ok(None) }
                } }).await?;
                let incremental = crate::review_github_viewed::since_review_comparison(&group, &comparison, checkpoint.as_ref().map(|(checkpoint, _)| checkpoint))?;
                let incremental = match incremental {
                    crate::review_github_viewed::SinceReviewComparison::Comparison(comparison) => comparison,
                    state => {
                        let message = match state {
                            crate::review_github_viewed::SinceReviewComparison::NoCheckpoint => "No retained review history for this file. Open Full PR comparison to review and mark it Viewed.",
                            crate::review_github_viewed::SinceReviewComparison::NoNewContent { full_comparison_changed: true } => "No new file contents since review. The full base comparison changed; open Full PR comparison to inspect those base-only changes.",
                            _ => "No new file contents since your retained review.",
                        };
                        this.update_in(cx, |this, _window, cx| {
                            this.layout_note = Some(message.into()); this.busy = false;
                            #[cfg(not(test))]
                            if this.github_viewed_initial_pending { this.github_viewed_initial_pending = false; this.sync_github_viewed(_window, cx); }
                            cx.notify();
                        })?;
                        return Ok(());
                    }
                };
                let old = checkpoint.context("Retained checkpoint disappeared")?.1.new;
                let new = if let Some(side) = incremental.new.as_ref().filter(|side| side.mode != 0o160000) {
                    let object = side.object.hex.parse()?;
                    let sizes = repository.read_with(cx, |repository, cx| repository.review_blob_sizes(vec![object], cx)).await?;
                    ensure!(sizes.first().copied().context("Missing blob size")? <= RETAIN_FILE_BUDGET as u64, "Current file exceeds the bounded Since review preview budget");
                    Some(repository.read_with(cx, |repository, cx| repository.review_blob(object, cx)).await?)
                } else { None };
                let retained = RetainedContents { old, new };
                incremental.validate_contents(&retained, RETAIN_FILE_BUDGET * 2)?;
                let binary = incremental.old.iter().chain(&incremental.new).any(|side| side.mode == 0o160000) || retained.old.iter().chain(&retained.new).any(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err());
                let (old_text, new_text) = if binary { (Some("Binary or submodule contents changed since review".into()), "Binary or submodule contents changed since review".into()) } else { (retained.old.map(String::from_utf8).transpose()?, retained.new.map(String::from_utf8).transpose()?.unwrap_or_default()) };
                let (registry, file) = cx.update(|_, cx| {
                    let repo_path = RepoPath::from_rel_path(RelPath::from_unix_str(&path)?);
                    let worktree_id = worktree_id_for_repo_path(repository.read(cx), project.read(cx), &repo_path, cx).context("Repository worktree unavailable")?;
                    Ok::<_, anyhow::Error>((project.read(cx).languages().clone(), Arc::new(GitBlob { path: repo_path, worktree_id, is_deleted: incremental.new.is_none(), is_binary: binary, display_name: path.clone() }) as Arc<dyn File>))
                })??;
                let buffer = build_buffer(new_text, file, &registry, cx).await?;
                let diff = build_buffer_diff(old_text, &buffer, &registry, cx).await?;
                this.update_in(cx, |this, window, cx| {
                    let workspace = workspace.upgrade().context("Workspace closed")?;
                    let multibuffer = cx.new(|cx| { let mut buffer = MultiBuffer::new(Capability::ReadOnly); buffer.set_all_diff_hunks_expanded(cx); buffer });
                    let style = session.presentation.split_view.map(|split| if split { DiffViewStyle::Split } else { DiffViewStyle::Unified }).unwrap_or(editor::EditorSettings::get_global(cx).diff_view_style);
                    let editor = cx.new(|cx| SplittableEditor::new(style, multibuffer, project, workspace, window, cx));
                    let snapshot = buffer.read(cx).snapshot();
                    let diff_snapshot = diff.read(cx).snapshot(cx);
                    let mut ranges = diff_snapshot.hunks(&snapshot).map(|hunk| hunk.buffer_range.to_point(&snapshot)).collect::<Vec<_>>();
                    if ranges.is_empty() { ranges.push(language::Point::zero()..snapshot.max_point()); }
                    let key = PathKey::with_sort_prefix(1, snapshot.file().context("Retained historical file missing")?.path().clone());
                    editor.update(cx, |editor, cx| {
                        editor.set_follow_global_diff_view_style(false);
                        editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                        editor.rhs_editor().update(cx, |editor, cx| { editor.set_show_bookmarks(false, cx); editor.set_show_breakpoints(false, cx); editor.set_show_diff_review_button(false, cx); });
                        editor.update_excerpts_for_path(key, buffer, ranges, multibuffer_context_lines(cx), diff, cx);
                        if style == DiffViewStyle::Split { editor.split(window, cx); }
                    });
                    this.editor = Some(editor);
                    this.busy = false;
                    #[cfg(not(test))]
                    if this.github_viewed_initial_pending { this.github_viewed_initial_pending = false; this.sync_github_viewed(window, cx); }
                    cx.notify();
                    Ok::<_, anyhow::Error>(())
                })??;
                Ok(())
            }.await;
            if let Err(error) = result { this.update(cx, |this, cx| { this.busy = false; this.error = Some(format!("Since review unavailable: {error:#}")); cx.notify(); }).log_err(); }
        }));
        cx.notify();
    }
}

#[cfg(test)]
mod partial_recovery_tests {
    use super::*;
    use crate::github_review::{RemoteViewedFile, ViewedState};
    use serde_json::json;

    fn operation() -> ReviewOperation {
        ReviewOperation {
            operation_id: "partial-folder".into(),
            revision: 3,
            context: ReviewOperationContext {
                selector: ComparisonSelector::PullRequest {
                    identity: PullRequestIdentity {
                        host: "github.com".into(),
                        repository_node_id: "generic_repo".into(),
                        pull_request_node_id: "generic_pr".into(),
                        viewer_node_id: "generic_viewer".into(),
                    },
                },
                metadata: PullRequestMetadata {
                    owner: "example".into(),
                    repository: "review".into(),
                    number: 1,
                    title: "Generic review".into(),
                    base_ref_name: "main".into(),
                    head_ref_name: "feature".into(),
                    base_ref_oid: "1".repeat(40),
                    head_ref_oid: "2".repeat(40),
                    commit_count: Some(1),
                },
            },
            kind: ReviewOperationKind::SetFileViewed,
            payload: json!({}),
            location: ReviewLocationState::Ready,
            state: ReviewOperationState::DeliveryUnknown,
            before_server_ids: Vec::new(),
            server_ids: Vec::new(),
            last_error: Some("Response lost".into()),
            created_at_unix_seconds: 0,
            updated_at_unix_seconds: 0,
        }
    }
    #[test]
    fn queued_viewed_batches_preserve_overlap_unknown_and_latest_acceptance_order() -> Result<()> {
        let prototype = operation();
        let files = (0..25)
            .map(|index| FileComparison {
                kind: ComparisonKind::Full,
                old: None,
                new: Some(FileSide {
                    path: format!("file-{index}.rs"),
                    object: ObjectId::for_blob(ObjectFormat::Sha1, b"example"),
                    mode: 0o100644,
                }),
            })
            .collect::<Vec<_>>();
        let session = ReviewSession {
            id: "fixture-session".into(),
            repository: RepositoryBinding {
                id: "fixture-repository".into(),
                common_directory: "/tmp/example/.git".into(),
                worktree_directory: "/tmp/example".into(),
                format: ObjectFormat::Sha1,
            },
            selector: prototype.context.selector.clone(),
            revision: 30,
            snapshot: ReviewSnapshot {
                generation: 1,
                incomplete_reason: None,
                groups: vec![ComparisonGroup {
                    endpoints: ResolvedEndpoints {
                        group: "comparison".into(),
                        left: ObjectId::for_blob(ObjectFormat::Sha1, b"base"),
                        right: ObjectId::for_blob(ObjectFormat::Sha1, b"head"),
                    },
                    files,
                }],
            },
            presentation: ReviewPresentation::default(),
        };
        let queued = |id: usize, path: usize, viewed: bool| -> Result<ReviewOperation> {
            let mut operation = prototype.clone();
            operation.operation_id = format!("operation-{id:03}");
            operation.state = ReviewOperationState::Queued;
            let mut intent = viewed_intent(&session, vec![format!("file-{path}.rs")], viewed)?;
            intent.origin.as_mut().unwrap().expected_revision = id as u64;
            operation.payload = serde_json::to_value(intent)?;
            Ok(operation)
        };
        let first = queued(1, 0, true)?;
        let second = queued(2, 1, true)?;
        let batch =
            viewed_delivery_batch(first.clone(), vec![second.clone(), first.clone()], &session)?;
        assert_eq!(
            batch.len(),
            2,
            "distinct files share one mutation and final readback"
        );
        let opposite = queued(3, 0, false)?;
        let later = queued(4, 2, true)?;
        assert_eq!(
            viewed_delivery_batch(
                first.clone(),
                vec![second.clone(), opposite, later],
                &session
            )?
            .len(),
            2,
            "opposite/overlapping intent is a serial barrier"
        );
        let mut unknown = second;
        unknown.state = ReviewOperationState::DeliveryUnknown;
        assert_eq!(
            viewed_delivery_batch(first.clone(), vec![unknown], &session)?.len(),
            1,
            "unknown deliveries are never replayed automatically"
        );
        let all = (1..25)
            .map(|id| queued(id, id, true))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(
            viewed_delivery_batch(first, all, &session)?.len(),
            20,
            "rapid individual flags fit a single bounded alias mutation"
        );
        Ok(())
    }

    #[test]
    fn partial_folder_recovery_settles_latest_observations_without_old_receipt_override()
    -> Result<()> {
        use db::sqlez::thread_safe_connection::ThreadSafeConnection;
        use futures::executor::block_on;
        block_on(async {
            let directory = tempfile::tempdir()?;
            let git_dir = directory.path().join(".git");
            std::fs::create_dir(&git_dir)?;
            let connection = ThreadSafeConnection::builder::<ReviewDb>(
                directory.path().join("review.sqlite").to_str().unwrap(),
                true,
            )
            .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
            .build()
            .await?;
            let db = ReviewDb::from_connection(connection)?;
            let binding = db
                .bind_repository(git_dir.clone(), git_dir, ObjectFormat::Sha1)
                .await?;
            let context = operation().context;
            let identity = context.identity()?.clone();
            let paths = (0..21)
                .map(|index| format!("docs/item-{index:02}.md"))
                .collect::<Vec<_>>();
            let old = b"before\n".to_vec();
            let new = b"after\n".to_vec();
            let files = paths
                .iter()
                .map(|path| FileComparison {
                    kind: ComparisonKind::Full,
                    old: Some(FileSide {
                        path: path.clone(),
                        object: ObjectId::for_blob(ObjectFormat::Sha1, &old),
                        mode: 0o100644,
                    }),
                    new: Some(FileSide {
                        path: path.clone(),
                        object: ObjectId::for_blob(ObjectFormat::Sha1, &new),
                        mode: 0o100644,
                    }),
                })
                .collect();
            let snapshot = ReviewSnapshot {
                generation: 1,
                groups: vec![ComparisonGroup {
                    endpoints: ResolvedEndpoints {
                        group: GROUP.into(),
                        left: ObjectId {
                            format: ObjectFormat::Sha1,
                            hex: context.metadata.base_ref_oid.clone(),
                        },
                        right: ObjectId {
                            format: ObjectFormat::Sha1,
                            hex: context.metadata.head_ref_oid.clone(),
                        },
                    },
                    files,
                }],
                incomplete_reason: None,
            };
            let session = db
                .deploy_pull_request(
                    binding.id,
                    identity.clone(),
                    context.metadata.clone(),
                    snapshot,
                    None,
                )
                .await?;
            let remote_context = ReviewContext {
                reference: PullRequestReference::parse(&context.metadata.url(), None)?,
                identity,
                head_oid: context.metadata.head_ref_oid.clone(),
                base_tip_oid: context.metadata.base_ref_oid.clone(),
            };
            let initial = paths
                .iter()
                .map(|path| RemoteViewedFile {
                    path: path.clone(),
                    viewer_viewed_state: ViewedState::Unviewed,
                })
                .collect();
            db.sync_github_viewed(
                sync_target(&session, session.revision),
                remote_context.clone(),
                initial,
                BTreeSet::new(),
            )
            .await?;
            let session = db.load_session(&session.id)?;
            let retained = paths
                .iter()
                .map(|path| {
                    (
                        path.clone(),
                        RetainedContents {
                            old: Some(old.clone()),
                            new: Some(new.clone()),
                        },
                    )
                })
                .collect();
            let (original, _) = db
                .accept_github_viewed_intent(
                    sync_target(&session, session.revision),
                    context.clone(),
                    viewed_intent(&session, paths.clone(), true)?,
                    retained,
                    None,
                    1024,
                )
                .await?;
            let original = db
                .claim_operation(original.operation_id, original.revision, Vec::new())
                .await?;
            let original = db
                .finish_operation(
                    original.operation_id,
                    original.revision,
                    ReviewOperationOutcome::DeliveryUnknown {
                        reason: "First chunk response lost".into(),
                    },
                )
                .await?;
            let session = db.load_session(&session.id)?;
            let last = paths.last().unwrap().clone();
            let (remaining, _) = db
                .accept_github_viewed_intent(
                    sync_target(&session, session.revision),
                    context,
                    viewed_intent(&session, vec![last.clone()], true)?,
                    BTreeMap::from([(
                        last.clone(),
                        RetainedContents {
                            old: Some(old),
                            new: Some(new),
                        },
                    )]),
                    None,
                    1024,
                )
                .await?;
            let remaining = db
                .claim_operation(remaining.operation_id, remaining.revision, Vec::new())
                .await?;
            let remaining = db
                .finish_operation(
                    remaining.operation_id,
                    remaining.revision,
                    ReviewOperationOutcome::Sent {
                        server_ids: Vec::new(),
                    },
                )
                .await?;
            let session = db.load_session(&session.id)?;
            db.record_github_viewed_receipt(
                sync_target(&session, session.revision),
                remaining.operation_id.clone(),
                vec![RemoteViewedFile {
                    path: last,
                    viewer_viewed_state: ViewedState::Viewed,
                }],
            )
            .await?;
            assert_eq!(
                db.operation(&original.operation_id)?.state,
                ReviewOperationState::DeliveryUnknown
            );
            let current = db.load_session(&session.id)?;
            let observed = paths
                .iter()
                .map(|path| RemoteViewedFile {
                    path: path.clone(),
                    viewer_viewed_state: ViewedState::Viewed,
                })
                .collect::<Vec<_>>();
            let work = settle_viewed_reconciliation(
                &db,
                &current,
                remote_context,
                original.clone(),
                observed.clone(),
            )
            .await?;
            assert!(
                work.imports.is_empty(),
                "All exact local checkpoints must be retained, not replaced"
            );
            let current = db.load_session(&session.id)?;
            assert_eq!(read_data(&db, current.clone())?.viewed.len(), 21);
            assert!(db.github_viewed_local_intents(&current)?.is_empty());
            assert_eq!(
                db.reviewed_checkpoint_metadata(&current.id, GROUP)?.len(),
                21
            );
            assert_eq!(
                db.operation(&original.operation_id)?.state,
                ReviewOperationState::Sent
            );
            assert_eq!(
                db.operation(&remaining.operation_id)?.state,
                ReviewOperationState::Sent
            );
            assert!(
                db.record_github_viewed_receipt(
                    sync_target(&current, current.revision),
                    original.operation_id,
                    observed
                )
                .await
                .is_err(),
                "Original receipt CAS remains strict after a newer subset intent"
            );
            Ok(())
        })
    }

    #[test]
    fn viewed_readback_rejects_stale_overlays_recovery_status_and_busy_state() {
        let current = ReviewSession {
            id: "session".into(),
            repository: RepositoryBinding {
                id: "repository".into(),
                common_directory: "/fixture/.git".into(),
                worktree_directory: "/fixture/.git".into(),
                format: ObjectFormat::Sha1,
            },
            selector: operation().context.selector,
            revision: 8,
            snapshot: ReviewSnapshot {
                generation: 2,
                groups: Vec::new(),
                incomplete_reason: None,
            },
            presentation: ReviewPresentation::default(),
        };
        let mut observed = current.clone();
        assert!(viewed_readback_authoritative(&current, &observed));
        observed.revision = 7;
        assert!(
            !viewed_readback_authoritative(&current, &observed),
            "A stale result cannot install any associated UI state"
        );
        observed.revision = 9;
        assert!(viewed_readback_authoritative(&current, &observed));
        observed.snapshot.generation = 1;
        assert!(!viewed_readback_authoritative(&current, &observed));
        observed.snapshot.generation = 2;
        observed.id = "another-session".into();
        assert!(!viewed_readback_authoritative(&current, &observed));
    }

    #[test]
    fn partial_viewed_recovery_rejects_changed_head_viewer_and_generation() {
        let original = operation();
        let recovery = ViewedRemainder {
            paths: vec!["docs/remaining.md".into()],
            total: 2,
            origin: original.clone(),
            viewed: true,
            generation: 4,
        };
        assert!(recovery.validate_context(&original.context, 4).is_ok());
        assert!(recovery.validate_context(&original.context, 5).is_err());
        let mut changed = original.context.clone();
        changed.metadata.head_ref_oid = "3".repeat(40);
        assert!(recovery.validate_context(&changed, 4).is_err());
        let ComparisonSelector::PullRequest { identity } = &mut changed.selector else {
            unreachable!()
        };
        identity.viewer_node_id = "another_viewer".into();
        assert!(recovery.validate_context(&changed, 4).is_err());
        assert_eq!(original.state, ReviewOperationState::DeliveryUnknown);
    }
    #[test]
    fn partial_viewed_recovery_only_uses_independently_observed_differing_paths() {
        let original = operation();
        let paths: Vec<String> = vec!["docs/confirmed.md".into(), "docs/remaining.md".into()];
        let remote = vec![
            RemoteViewedFile {
                path: paths[0].clone(),
                viewer_viewed_state: ViewedState::Viewed,
            },
            RemoteViewedFile {
                path: paths[1].clone(),
                viewer_viewed_state: ViewedState::Unviewed,
            },
        ];
        let recovery = remaining_after_read(&original, &paths, true, 1, &remote)
            .unwrap()
            .unwrap();
        assert_eq!(recovery.paths, vec!["docs/remaining.md"]);
        assert_eq!(recovery.summary(), "1/2 confirmed");
        assert_eq!(
            original.state,
            ReviewOperationState::DeliveryUnknown,
            "A partial read never falsely settles the original operation"
        );
        assert!(
            remaining_after_read(&original, &paths, true, 1, &remote[..1]).is_err(),
            "Incomplete reads cannot authorize a resume"
        );
        let mut complete = remote.clone();
        complete[1].viewer_viewed_state = ViewedState::Viewed;
        assert!(
            remaining_after_read(&original, &paths, true, 1, &complete)
                .unwrap()
                .is_none()
        );
        let incomplete = reconcile_folder_viewed(&paths, true, Some(&remote[..1])).unwrap();
        assert_eq!(incomplete.unknown, vec!["docs/remaining.md"]);
        assert!(
            incomplete.not_applied.is_empty(),
            "An absent path is not eligible for retry"
        );
    }
}
