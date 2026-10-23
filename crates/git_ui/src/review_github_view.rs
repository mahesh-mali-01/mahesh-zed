mod delivery;
mod presentation;
#[cfg(test)]
mod tests;

use delivery::{
    all_ids, enqueue_comment_delivery, operation_matches_thread, pending_operation, reconciled_ids,
};
pub(crate) use presentation::GitHubInlineCard;

use crate::{
    github_pull_requests::{
        GitHubClient, GitHubRequestGuard, PullRequestReference, RepositoryName,
    },
    github_review::{
        CommentAnchor, CommentIntent, CommentPublication, RemoteReview, RemoteThread,
        ReviewContext, ReviewEvent, ReviewService,
    },
    review::*,
    review_store::ReviewDb,
};
use anyhow::Result;
use gpui::{App, Context, Entity, EventEmitter, Focusable, Subscription, Task, Window};
use markdown::{Markdown, MarkdownOptions};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use ui::prelude::*;
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ReviewIntent {
    Comment {
        intent: CommentIntent,
    },
    AddToReview {
        intent: CommentIntent,
    },
    Reply {
        thread_id: String,
        pending_review_id: Option<String>,
        body: String,
    },
    Edit {
        comment_id: String,
        body: String,
    },
    Delete {
        comment_id: String,
    },
    StartReview,
    DeleteReview {
        review_id: String,
    },
    Submit {
        review_id: String,
        event: ReviewEvent,
        body: String,
    },
    SubmitNew {
        event: ReviewEvent,
        body: String,
    },
    Resolve {
        thread_id: String,
        resolved: bool,
    },
}

impl ReviewIntent {
    fn kind(&self) -> ReviewOperationKind {
        match self {
            Self::Comment { intent } => match intent.publication {
                CommentPublication::Immediate => ReviewOperationKind::PostInlineComment,
                CommentPublication::Pending { .. } => ReviewOperationKind::AddPendingThread,
            },
            Self::AddToReview { .. } => ReviewOperationKind::AddPendingThread,
            Self::Reply { .. } => ReviewOperationKind::Reply,
            Self::Edit { .. } => ReviewOperationKind::EditComment,
            Self::Delete { .. } => ReviewOperationKind::DeleteComment,
            Self::StartReview => ReviewOperationKind::CreatePendingReview,
            Self::DeleteReview { .. } => ReviewOperationKind::DeletePendingReview,
            Self::Submit { .. } | Self::SubmitNew { .. } => ReviewOperationKind::SubmitReview,
            Self::Resolve { .. } => ReviewOperationKind::SetThreadResolved,
        }
    }
    fn body(&self) -> &str {
        match self {
            Self::Comment { intent } | Self::AddToReview { intent } => &intent.body,
            Self::Reply { body, .. }
            | Self::Edit { body, .. }
            | Self::Submit { body, .. }
            | Self::SubmitNew { body, .. } => body,
            _ => "",
        }
    }
    fn set_body(&mut self, value: String) {
        match self {
            Self::Comment { intent } | Self::AddToReview { intent } => intent.body = value,
            Self::Reply { body, .. }
            | Self::Edit { body, .. }
            | Self::Submit { body, .. }
            | Self::SubmitNew { body, .. } => *body = value,
            _ => {}
        }
    }
}

pub(crate) enum GitHubReviewEvent {
    ThreadsChanged,
    LocalOperationsChanged,
    ClosePanel,
    Compose,
    ViewedOperation(ReviewOperation, bool),
    Recover(crate::review_view::RecoveryAction),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ThreadFilter {
    #[default]
    All,
    Unresolved,
    Resolved,
}
impl ThreadFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All threads",
            Self::Unresolved => "Unresolved",
            Self::Resolved => "Resolved",
        }
    }
    fn includes(self, resolved: bool) -> bool {
        match self {
            Self::All => true,
            Self::Unresolved => !resolved,
            Self::Resolved => resolved,
        }
    }
}

pub(crate) struct GitHubReviewPanel {
    db: ReviewDb,
    recovery: crate::review_outbox_runtime::Recovery,
    context: ReviewOperationContext,
    remote: ReviewContext,
    patches: BTreeMap<String, Option<String>>,
    threads: Vec<RemoteThread>,
    reviews: Vec<RemoteReview>,
    operations: Vec<ReviewOperation>,
    superseded_operations: HashSet<String>,
    markdown: BTreeMap<String, Entity<Markdown>>,
    operation_markdown: BTreeMap<String, (String, Entity<Markdown>)>,
    body: Entity<InputField>,
    intent: Option<ReviewIntent>,
    draft: Option<ReviewOperation>,
    anchor: Option<CommentAnchor>,
    anchor_error: Option<String>,
    resumed_pending: Option<String>,
    can_review: bool,
    viewer_login: Option<String>,
    selected_thread: Option<String>,
    thread_filter: ThreadFilter,
    thread_expansion: BTreeMap<String, bool>,
    pending_actions_mode: bool,
    inline_composer: bool,
    incremental_mode: bool,
    composer_generation: u64,
    close_after_save: bool,
    error: Option<String>,
    notice: Option<String>,
    loading: bool,
    preview: bool,
    #[cfg(test)]
    delivery_pause: Option<crate::review_outbox_runtime::Recovery>,
    writing: bool,
    bulk_running: bool,
    bulk_status: Option<String>,
    reconciling: bool,
    generation: u64,
    canonical_threads_generation: Option<u64>,
    body_revision: u64,
    read_guard: Option<GitHubRequestGuard>,
    read_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GitHubReviewEvent> for GitHubReviewPanel {}

impl GitHubReviewPanel {
    pub(crate) fn new(
        db: ReviewDb,
        context: ReviewOperationContext,
        identity: PullRequestIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let remote = ReviewContext {
            reference: PullRequestReference {
                repository: RepositoryName {
                    owner: context.metadata.owner.clone(),
                    name: context.metadata.repository.clone(),
                },
                number: context.metadata.number,
            },
            identity,
            head_oid: context.metadata.head_ref_oid.clone(),
            base_tip_oid: context.metadata.base_ref_oid.clone(),
        };

        let body = cx.new(|cx| InputField::new(window, cx, "Write a GitHub review comment…"));
        let body_editor = body.read(cx).editor().clone();
        body_editor.set_multiline(Some(8), window, cx);
        let weak = cx.weak_entity();
        let subscription = body_editor.subscribe(
            Box::new(move |event, window, cx| {
                if matches!(
                    event,
                    ErasedEditorEvent::BufferEdited | ErasedEditorEvent::Blurred
                ) {
                    weak.update(cx, |this, cx| this.schedule_save(window, cx))
                        .log_err();
                }
            }),
            window,
            cx,
        );
        let recovery = crate::review_outbox_runtime::ready(db.clone(), cx);
        let view = Self {
            db,
            recovery,
            context,
            remote,
            patches: BTreeMap::new(),
            threads: Vec::new(),
            reviews: Vec::new(),
            operations: Vec::new(),
            superseded_operations: HashSet::new(),
            markdown: BTreeMap::new(),
            operation_markdown: BTreeMap::new(),
            body,
            intent: None,
            draft: None,
            anchor: None,
            anchor_error: None,
            resumed_pending: None,
            can_review: false,
            viewer_login: None,
            selected_thread: None,
            thread_filter: ThreadFilter::All,
            thread_expansion: BTreeMap::new(),
            pending_actions_mode: false,
            inline_composer: false,
            incremental_mode: false,
            composer_generation: 0,
            close_after_save: false,
            error: None,
            notice: None,
            loading: false,
            preview: false,
            #[cfg(test)]
            delivery_pause: None,
            writing: false,
            bulk_running: false,
            bulk_status: None,
            reconciling: false,
            generation: 0,
            canonical_threads_generation: None,
            body_revision: 0,
            read_guard: None,
            read_task: None,
            save_task: None,
            _subscriptions: vec![subscription],
        };
        #[cfg(not(test))]
        cx.defer_in(window, |this, window, cx| {
            if !this.preview {
                this.reload(window, cx);
            }
        });
        view
    }

    /// The real native card/composer with generic read-only fixture data.
    /// This surface never fetches, autosaves or publishes to GitHub.
    #[cfg(any(test, feature = "review-test-support"))]
    pub(crate) fn preview(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let identity = PullRequestIdentity {
            host: "github.com".into(),
            repository_node_id: "R_preview".into(),
            pull_request_node_id: "PR_preview".into(),
            viewer_node_id: "U_preview".into(),
        };
        let context = ReviewOperationContext {
            selector: ComparisonSelector::PullRequest {
                identity: identity.clone(),
            },
            metadata: PullRequestMetadata {
                owner: "example".into(),
                repository: "review-preview".into(),
                number: 7,
                title: "Improve native review navigation".into(),
                base_ref_name: "main".into(),
                head_ref_name: "feature/review".into(),
                base_ref_oid: "1".repeat(40),
                head_ref_oid: "2".repeat(40),
                commit_count: Some(3),
            },
        };
        let mut panel = Self::new(ReviewDb::global(cx), context, identity, window, cx);
        panel.preview = true;
        panel.viewer_login = Some("reviewer".into());
        let comment: crate::github_review::RemoteComment = serde_json::from_value(serde_json::json!({
            "id":"preview-comment", "state":"PENDING", "body":"Could we preserve the active diff while the next file loads?\n\nThis should keep **keyboard navigation** responsive.\n\n```rust\nlet next = prepare_diff(snapshot).await?;\n```", "url":"https://github.com/example/review-preview/pull/7#discussion_r1", "author":{"login":"reviewer"}, "createdAt":"Today, 10:42", "updatedAt":"Today, 10:42", "path":"src/review.rs", "line":42, "startLine":40, "diffHunk":"@@ -40,3 +40,3 @@", "viewerCanUpdate":true, "viewerCanDelete":true, "viewerDidAuthor":true
        })).expect("generic review comment fixture");
        panel.markdown.insert(
            comment.id.clone(),
            cx.new(|cx| {
                Markdown::new_with_options(
                    comment.body.clone().into(),
                    None,
                    None,
                    MarkdownOptions {
                        parse_html: true,
                        ..Default::default()
                    },
                    cx,
                )
            }),
        );
        panel.threads.push(crate::github_review::RemoteThread {
            id: "preview-thread".into(),
            path: "src/review.rs".into(),
            line: Some(42),
            start_line: Some(40),
            original_line: Some(42),
            original_start_line: Some(40),
            diff_side: crate::github_review::DiffSide::Right,
            start_diff_side: Some(crate::github_review::DiffSide::Right),
            subject_type: crate::github_review::CommentSubject::Line,
            is_resolved: false,
            is_outdated: false,
            viewer_can_reply: true,
            viewer_can_resolve: true,
            viewer_can_unresolve: false,
            comments: vec![comment],
        });
        for (id, state, error) in [
            (
                "preview-failed",
                ReviewOperationState::Failed,
                "GitHub rejected the request (HTTP 403); check repository permissions. This is offline fixture data.",
            ),
            (
                "preview-unknown",
                ReviewOperationState::DeliveryUnknown,
                "Connection interrupted; delivery is unconfirmed. Reconcile before retrying. This is offline fixture data.",
            ),
        ] {
            panel.operations.push(ReviewOperation {
                operation_id: id.into(),
                revision: 1,
                context: panel.context.clone(),
                kind: ReviewOperationKind::SetThreadResolved,
                payload: serde_json::to_value(ReviewIntent::Resolve {
                    thread_id: "preview-thread".into(),
                    resolved: true,
                })
                .unwrap(),
                location: ReviewLocationState::NotRequired,
                state,
                before_server_ids: vec![],
                server_ids: vec![],
                last_error: Some(error.into()),
                created_at_unix_seconds: 1,
                updated_at_unix_seconds: 1,
            });
        }
        panel.selected_thread = Some("preview-thread".into());
        panel.begin_unlocated_comment("src/review.rs".into(), window, cx);
        panel.anchor_error = None;
        panel.body.update(cx, |body, cx| {
            body.set_text(
                "Please keep this draft while I continue reviewing another file.",
                window,
                cx,
            )
        });
        panel
    }

    pub(crate) fn threads(&self) -> &[RemoteThread] {
        &self.threads
    }
    pub(crate) fn threads_read_finished(&self) -> bool {
        self.generation > 0 && !self.loading
    }
    pub(crate) fn set_anchor(&mut self, anchor: Result<CommentAnchor>, cx: &mut Context<Self>) {
        match anchor {
            Ok(anchor) => {
                self.anchor = Some(anchor);
                self.anchor_error = None;
            }
            Err(error) => {
                self.anchor = None;
                self.anchor_error = Some(error.to_string());
            }
        }
        cx.notify();
    }
    pub(crate) fn begin_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing {
            return;
        }
        if self.incremental_mode {
            self.error =
                Some("Open Full PR comparison to comment on canonical GitHub lines.".into());
            cx.notify();
            return;
        }
        let Some(anchor) = self.anchor.clone() else {
            self.error = Some(self.anchor_error.clone().unwrap_or_else(|| {
                "Select a line or a contiguous line range in the PR diff first.".into()
            }));
            cx.notify();
            return;
        };
        if self.intent.is_some() && self.body.read(cx).text(cx).trim().len() > 0 {
            self.error =
                Some("Save or close the current draft before starting another comment.".into());
            cx.notify();
            return;
        }
        self.composer_generation += 1;
        self.intent = Some(ReviewIntent::Comment {
            intent: CommentIntent {
                context: self.remote.clone(),
                anchor,
                body: String::new(),
                publication: CommentPublication::Immediate,
            },
        });
        self.draft = None;
        self.error = None;
        self.body.update(cx, |body, cx| body.clear(window, cx));
        self.body.focus_handle(cx).focus(window, cx);
        cx.emit(GitHubReviewEvent::Compose);
        cx.notify();
    }

    pub(crate) fn set_bulk_status(
        &mut self,
        running: bool,
        status: String,
        cx: &mut Context<Self>,
    ) {
        self.bulk_running = running;
        self.bulk_status = Some(status);
        cx.notify();
    }
    pub(crate) fn recovery_presentation(&self) -> (bool, bool, Option<String>) {
        (
            self.pending_actions_mode,
            self.bulk_running,
            self.bulk_status.clone(),
        )
    }
    pub(crate) fn restore_recovery_presentation(&mut self, state: (bool, bool, Option<String>)) {
        (
            self.pending_actions_mode,
            self.bulk_running,
            self.bulk_status,
        ) = state;
    }

    pub(crate) fn local_operations(&self) -> &[ReviewOperation] {
        &self.operations
    }

    pub(crate) fn delivery_busy(&self) -> bool {
        self.writing
    }
    pub(crate) fn has_composer(&self) -> bool {
        self.intent.is_some()
    }

    pub(crate) fn showing_pending_actions(&self) -> bool {
        self.pending_actions_mode
    }
    pub(crate) fn pending_action_count(&self) -> usize {
        self.operations
            .iter()
            .filter(|operation| pending_operation(operation))
            .count()
            + usize::from(self.error.is_some())
    }
    pub(crate) fn show_pending_actions(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.pending_actions_mode = true;
        if self.preview {
            self.intent = None;
            self.draft = None;
        }
        cx.notify();
    }
    pub(crate) fn show_threads(&mut self, cx: &mut Context<Self>) {
        self.pending_actions_mode = false;
        cx.notify();
    }
    fn thread_expanded(&self, thread: &RemoteThread) -> bool {
        self.thread_expansion
            .get(&thread.id)
            .copied()
            .unwrap_or(!thread.is_resolved)
    }
    pub(crate) fn retry_failed_operation(
        &mut self,
        operation: ReviewOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview {
            self.notice = Some("Offline preview: Retry action does not contact GitHub.".into());
            cx.notify();
            return;
        }
        if operation.state != ReviewOperationState::Failed
            || operation.context != self.context
            || self.writing
            || self.intent.is_some()
        {
            return;
        }
        if operation.kind == ReviewOperationKind::SetFileViewed {
            cx.emit(GitHubReviewEvent::ViewedOperation(operation, false));
            return;
        }
        if operation.location == ReviewLocationState::NeedsLocation {
            self.resume_operation(operation, window, cx);
            return;
        }
        match serde_json::from_value::<ReviewIntent>(operation.payload.clone()) {
            Ok(intent) => self.persist(intent, Some(operation), true, window, cx),
            Err(_) => {
                self.error = Some(
                    "Saved action could not be read; preserve it and refresh before retrying."
                        .into(),
                );
                cx.notify();
            }
        }
    }
    pub(crate) fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let remote = self.remote.clone();
        let db = self.db.clone();
        let recovery = self.recovery.clone();
        let (client, guard) = GitHubClient::cancellable();
        self.read_guard = Some(guard);
        self.loading = true;
        self.error = None;
        let local_db = self.db.clone();
        let local_identity = self.remote.identity.clone();
        let local_recovery = recovery.clone();
        self.read_task = Some(cx.spawn_in(window, async move |this, cx| {
            let local = cx
                .background_spawn(async move {
                    local_recovery
                        .await
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    local_db.operations(&local_identity)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                match local {
                    Ok(operations) => {
                        this.operations = operations
                            .into_iter()
                            .filter(|operation| {
                                !this.superseded_operations.contains(&operation.operation_id)
                            })
                            .collect();
                        this.resume_queued_deliveries(window, cx);
                    }
                    Err(error) => {
                        this.error = Some(format!("Local review drafts unavailable: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
            let result = cx
                .background_spawn(async move {
                    recovery
                        .await
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    let service = ReviewService::new(&client);
                    let details = client.pull_request(&remote.reference)?;
                    let viewer = client.viewer()?;
                    let can_review = details.summary.state == "OPEN"
                        && details
                            .summary
                            .author
                            .as_ref()
                            .is_some_and(|author| author.login != viewer.login);
                    let files = client.files(&remote.reference)?;
                    anyhow::ensure!(
                        !files.limit_reached && files.files.len() == details.changed_files,
                        "Canonical PR file list is incomplete"
                    );
                    let patches = files
                        .files
                        .into_iter()
                        .map(|file| (file.filename, file.patch))
                        .collect::<BTreeMap<_, _>>();
                    let threads = service.load_threads(&remote)?;
                    let reviews = service.load_reviews(&remote)?;
                    db.invalidate_operation_locations(
                        remote.identity.clone(),
                        remote.head_oid.clone(),
                    )
                    .await?;
                    let operations = db.operations(&remote.identity)?;
                    service.verify_context(&remote)?;
                    Ok::<_, anyhow::Error>((
                        threads,
                        reviews,
                        operations,
                        can_review,
                        patches,
                        viewer.login,
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok((threads, reviews, operations, can_review, patches, viewer_login)) => {
                        this.can_review = can_review;
                        this.viewer_login = Some(viewer_login);
                        this.patches = patches;
                        let mut markdown = BTreeMap::new();
                        for comment in threads.iter().flat_map(|thread| &thread.comments) {
                            let entity = if let Some(entity) = this.markdown.remove(&comment.id) {
                                if entity.read(cx).source().as_ref() != comment.body {
                                    entity.update(cx, |markdown, cx| {
                                        markdown.reset(comment.body.clone().into(), cx)
                                    });
                                }
                                entity
                            } else {
                                cx.new(|cx| {
                                    Markdown::new_with_options(
                                        comment.body.clone().into(),
                                        None,
                                        None,
                                        MarkdownOptions {
                                            parse_html: true,
                                            ..Default::default()
                                        },
                                        cx,
                                    )
                                })
                            };
                            markdown.insert(comment.id.clone(), entity);
                        }
                        this.markdown = markdown;
                        this.threads = threads;
                        this.canonical_threads_generation = Some(generation);
                        this.reviews = reviews;
                        this.operations = operations
                            .into_iter()
                            .filter(|operation| {
                                !this.superseded_operations.contains(&operation.operation_id)
                            })
                            .collect();
                        if this.resumed_pending.as_ref().is_some_and(|id| {
                            !this
                                .reviews
                                .iter()
                                .any(|review| &review.id == id && review.state == "PENDING")
                        }) {
                            this.resumed_pending = None;
                        }
                        if this.resumed_pending.is_none() {
                            this.resumed_pending = this
                                .reviews
                                .iter()
                                .filter(|review| {
                                    review.viewer_did_author && review.state == "PENDING"
                                })
                                .find(|review| {
                                    this.operations.iter().any(|operation| {
                                        operation.state == ReviewOperationState::Sent
                                            && (operation.kind
                                                == ReviewOperationKind::CreatePendingReview
                                                && operation.server_ids.contains(&review.id)
                                                || operation.kind
                                                    == ReviewOperationKind::AddPendingThread
                                                    && this
                                                        .threads
                                                        .iter()
                                                        .flat_map(|thread| &thread.comments)
                                                        .any(|comment| {
                                                            operation
                                                                .server_ids
                                                                .contains(&comment.id)
                                                                && comment
                                                                    .pull_request_review
                                                                    .as_ref()
                                                                    .is_some_and(|parent| {
                                                                        parent.id == review.id
                                                                    })
                                                        }))
                                    })
                                })
                                .map(|review| review.id.clone());
                        }
                        cx.emit(GitHubReviewEvent::ThreadsChanged);
                    }
                    Err(error) => {
                        this.error = Some(format!("GitHub discussions unavailable: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn pending(&self) -> Option<&RemoteReview> {
        self.reviews
            .iter()
            .find(|review| review.state == "PENDING" && review.viewer_did_author)
    }
    fn schedule_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.body_revision += 1;
        if self.preview || self.intent.is_none() || self.writing {
            return;
        }
        let revision = self.body_revision;
        self.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(450))
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.body_revision == revision {
                    this.save(false, window, cx);
                }
            })
            .log_err();
        }));
    }
    fn current_intent(&self, cx: &App) -> Option<ReviewIntent> {
        let mut intent = self.intent.clone()?;
        intent.set_body(self.body.read(cx).text(cx));
        Some(intent)
    }
    fn save(&mut self, send: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing {
            return;
        }
        let Some(intent) = self.current_intent(cx) else {
            return;
        };
        if intent.body().trim().is_empty()
            && !matches!(
                intent,
                ReviewIntent::Submit {
                    event: ReviewEvent::Comment | ReviewEvent::Approve,
                    ..
                } | ReviewIntent::SubmitNew {
                    event: ReviewEvent::Approve,
                    ..
                }
            )
        {
            self.error = Some("Write a comment before saving or sending.".into());
            cx.notify();
            return;
        }
        if self.draft.as_ref().is_some_and(|draft| {
            matches!(
                draft.state,
                ReviewOperationState::Sending
                    | ReviewOperationState::DeliveryUnknown
                    | ReviewOperationState::Queued
            )
        }) {
            self.error = Some("This operation must be reconciled or explicitly resumed before editing or sending.".into());
            cx.notify();
            return;
        }
        self.save_task = None;
        self.persist(intent, self.draft.clone(), send, window, cx);
    }
    fn persist(
        &mut self,
        intent: ReviewIntent,
        existing: Option<ReviewOperation>,
        send: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview || self.writing {
            return;
        }
        self.writing = true;
        self.body
            .read(cx)
            .editor()
            .clone()
            .set_read_only(send || self.close_after_save, cx);
        self.error = None;
        let db = self.db.clone();
        let context = self.context.clone();
        let remote = self.remote.clone();
        let generation = self.composer_generation;
        let body_revision = self.body_revision;
        let updates_composer = matches!(
            intent,
            ReviewIntent::Comment { .. }
                | ReviewIntent::AddToReview { .. }
                | ReviewIntent::Reply { .. }
                | ReviewIntent::Edit { .. }
                | ReviewIntent::Submit { .. }
                | ReviewIntent::SubmitNew { .. }
        );
        let intent_for_ui = intent.clone();
        let recovery = self.recovery.clone();
        let superseded = existing
            .as_ref()
            .filter(|draft| draft.kind != intent.kind())
            .cloned();
        let superseded_id = superseded
            .as_ref()
            .map(|operation| operation.operation_id.clone());
        let existing = existing.filter(|draft| draft.kind == intent.kind());
        let task = cx.background_spawn(async move {
            recovery
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let location = if intent.kind().needs_location() {
                if context.selector.supports_github_viewed()
                    && existing.as_ref().is_none_or(|draft| {
                        draft.location != ReviewLocationState::NeedsLocation
                            && draft.context == context
                    })
                    && match &intent {
                        ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } => {
                            intent.anchor.commit_oid == remote.head_oid
                                && intent.context.head_oid == remote.head_oid
                        }
                        _ => true,
                    }
                {
                    ReviewLocationState::Ready
                } else {
                    ReviewLocationState::NeedsLocation
                }
            } else {
                ReviewLocationState::NotRequired
            };
            let payload = serde_json::to_value(&intent)?;
            let operation = match existing {
                Some(existing) => {
                    db.update_operation_draft(
                        existing.operation_id,
                        existing.revision,
                        context,
                        payload,
                        location,
                    )
                    .await?
                }
                None => {
                    db.create_operation(context, intent.kind(), payload, location)
                        .await?
                }
            };
            if let Some(old) = superseded {
                db.delete_operation_draft(old.operation_id, old.revision)
                    .await?;
            }
            if send {
                db.queue_operation(operation.operation_id, operation.revision)
                    .await
            } else {
                Ok::<_, anyhow::Error>(operation)
            }
        });
        let app = cx.to_async();
        let delivery_db = self.db.clone();
        let delivery_remote = self.remote.clone();
        #[cfg(test)]
        let pause = self.delivery_pause.clone();
        #[cfg(not(test))]
        let pause = None;
        // Local acceptance is independent of network delivery. A failed local
        // write leaves the original composer untouched; queued delivery owns
        // an immutable copy of its body and target before the editor is freed.
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let delivery = if send {
                result.as_ref().ok().map(|operation| {
                    app.update(|cx| {
                        enqueue_comment_delivery(
                            delivery_db,
                            delivery_remote,
                            operation.clone(),
                            intent_for_ui.clone(),
                            pause,
                            cx,
                        )
                    })
                })
            } else {
                None
            };
            this.update_in(cx, |this, window, cx| {
                this.writing = false;
                this.body.read(cx).editor().clone().set_read_only(false, cx);
                match result {
                    Ok(operation) => {
                        this.notice = Some(if send {
                            "Saved locally · sending to GitHub in the background.".into()
                        } else {
                            "Draft saved on this computer.".into()
                        });
                        if updates_composer && this.composer_generation == generation {
                            if send || this.close_after_save {
                                this.close_after_save = false;
                                this.intent = None;
                                this.draft = None;
                                this.body.update(cx, |body, cx| body.clear(window, cx));
                            } else {
                                if this.body_revision == body_revision {
                                    this.intent = Some(intent_for_ui.clone());
                                }
                                this.draft = Some(operation.clone());
                            }
                        }
                        if let Some(id) = superseded_id {
                            this.superseded_operations.insert(id.clone());
                            this.operations
                                .retain(|operation| operation.operation_id != id);
                            this.operation_markdown.remove(&id);
                        }
                        this.upsert_operation(operation);
                        if let Some(delivery) = delivery {
                            this.observe_delivery(delivery, window, cx);
                        }
                        if send || this.intent.is_none() {
                            cx.emit(GitHubReviewEvent::Compose);
                        }
                        if updates_composer
                            && !send
                            && this.body_revision != body_revision
                            && this.intent.is_some()
                        {
                            this.schedule_save(window, cx);
                        }
                    }
                    Err(error) => {
                        this.close_after_save = false;
                        this.error = Some(format!(
                            "Could not save locally; your text remains open: {error:#}"
                        ));
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn upsert_operation(&mut self, operation: ReviewOperation) {
        if self.superseded_operations.contains(&operation.operation_id) {
            return;
        }
        if let Some(old) = self
            .operations
            .iter_mut()
            .find(|old| old.operation_id == operation.operation_id)
        {
            // A slower acknowledgement cannot replace a newer operation state.
            if old.revision <= operation.revision {
                *old = operation;
            }
        } else {
            self.operations.push(operation);
        }
    }

    fn deliver(
        &mut self,
        operation: ReviewOperation,
        intent: ReviewIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        let pause = self.delivery_pause.clone();
        #[cfg(not(test))]
        let pause = None;
        let delivery = enqueue_comment_delivery(
            self.db.clone(),
            self.remote.clone(),
            operation,
            intent,
            pause,
            cx,
        );
        self.observe_delivery(delivery, window, cx);
    }

    fn observe_delivery(
        &mut self,
        delivery: crate::review_outbox_runtime::Delivery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = delivery.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(operation) => {
                        let sent = operation.state == ReviewOperationState::Sent;
                        this.upsert_operation(operation.clone());
                        this.notice = Some(if sent {
                            "GitHub confirmed the change.".into()
                        } else {
                            "Saved locally · delivery needs attention.".into()
                        });
                        if let Some(error) = operation.last_error {
                            this.error = Some(error);
                        }
                        if sent {
                            this.refresh_discussion(window, cx);
                        }
                        cx.emit(GitHubReviewEvent::ThreadsChanged);
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn pending_review_is_older(&self) -> bool {
        self.pending()
            .and_then(|review| review.commit.as_ref())
            .is_some_and(|commit| commit.oid != self.remote.head_oid)
    }

    fn add_current_to_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_review_is_older() {
            self.notice = Some(OLDER_PENDING_REVIEW_WARNING.into());
            self.save(false, window, cx);
            cx.notify();
            return;
        }
        let Some(current) = self.current_intent(cx) else {
            return;
        };
        self.intent = Some(match current {
            ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } => {
                ReviewIntent::AddToReview { intent }
            }
            ReviewIntent::Reply {
                thread_id, body, ..
            } => ReviewIntent::Reply {
                thread_id,
                body,
                pending_review_id: self
                    .resumed_pending
                    .clone()
                    .or_else(|| self.pending().map(|review| review.id.clone())),
            },
            _ => return,
        });
        self.save(true, window, cx);
    }

    fn add_saved_comment_to_pending(
        &mut self,
        operation: ReviewOperation,
        review_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview
            || self.writing
            || operation.context != self.context
            || !self.reviews.iter().any(|review| {
                review.id == review_id && review.viewer_did_author && review.state == "PENDING"
            })
        {
            return;
        }
        if self.pending_review_is_older() {
            self.notice = Some(OLDER_PENDING_REVIEW_WARNING.into());
            cx.notify();
            return;
        }
        let Ok(saved) = serde_json::from_value::<ReviewIntent>(operation.payload.clone()) else {
            return;
        };
        let mut intent = match saved {
            ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } => intent,
            _ => return,
        };
        intent.publication = CommentPublication::Pending { review_id };
        self.writing = true;
        self.error = None;
        let db = self.db.clone();
        let context = self.context.clone();
        let remote = self.remote.clone();
        let app = cx.to_async();
        #[cfg(test)]
        let pause = self.delivery_pause.clone();
        #[cfg(not(test))]
        let pause = None;
        cx.spawn_in(window, async move |this, cx| {
            let saved = cx.background_spawn({ let db = db.clone(); let intent = intent.clone(); async move { db.queue_comment_in_pending_review(operation.operation_id, operation.revision, context, intent).await } }).await;
            let delivery = saved.as_ref().ok().map(|operation| app.update(|cx| enqueue_comment_delivery(db, remote, operation.clone(), ReviewIntent::Comment { intent }, pause, cx)));
            this.update_in(cx, |this, window, cx| {
                this.writing = false;
                match saved {
                    Ok(operation) => {
                        this.upsert_operation(operation);
                        this.notice = Some("Saved locally · adding to your existing pending review.".into());
                        if let Some(delivery) = delivery { this.observe_delivery(delivery, window, cx); }
                        cx.emit(GitHubReviewEvent::ThreadsChanged);
                    }
                    Err(error) => this.error = Some(format!("Comment remains saved; could not queue it in the pending review: {error:#}")),
                }
                if this.intent.is_some() { this.schedule_save(window, cx); }
                cx.notify();
            }).log_err();
        }).detach();
        cx.notify();
    }

    /// Focused canonical read-back after a comment mutation. No changed-file,
    /// patch, viewer or Git-object reload is needed for this same pinned head.
    fn refresh_discussion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        let remote = self.remote.clone();
        let (client, guard) = GitHubClient::cancellable();
        self.read_guard = Some(guard);
        self.read_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move {
                let service = ReviewService::new(&client);
                let threads = service.load_threads(&remote)?;
                let reviews = service.load_reviews(&remote)?;
                service.verify_context(&remote)?;
                Ok::<_, anyhow::Error>((threads, reviews))
            }).await;
            this.update(cx, |this, cx| {
                if this.generation != generation { return; }
                this.loading = false;
                match result {
                    Ok((threads, reviews)) => {
                        let mut markdown = BTreeMap::new();
                        for comment in threads.iter().flat_map(|thread| &thread.comments) {
                            let entity = if let Some(entity) = this.markdown.remove(&comment.id) {
                                if entity.read(cx).source().as_ref() != comment.body { entity.update(cx, |markdown, cx| markdown.reset(comment.body.clone().into(), cx)); }
                                entity
                            } else { cx.new(|cx| Markdown::new_with_options(comment.body.clone().into(), None, None, MarkdownOptions { parse_html:true, ..Default::default() }, cx)) };
                            markdown.insert(comment.id.clone(), entity);
                        }
                        this.markdown = markdown;
                        this.threads = threads;
                        this.canonical_threads_generation = Some(generation);
                        this.reviews = reviews;
                        if this.resumed_pending.as_ref().is_some_and(|id| !this.reviews.iter().any(|review| &review.id == id && review.viewer_did_author && review.state == "PENDING")) { this.resumed_pending = None; }

                        if let Some(review) = this.reviews.iter().find(|review| review.viewer_did_author && review.state == "PENDING" && (this.operations.iter().any(|operation| operation.state == ReviewOperationState::Sent && operation.kind == ReviewOperationKind::CreatePendingReview && operation.server_ids.contains(&review.id)) || this.threads.iter().flat_map(|thread| &thread.comments).any(|comment| comment.pull_request_review.as_ref().is_some_and(|parent| parent.id == review.id) && this.operations.iter().any(|operation| operation.state == ReviewOperationState::Sent && operation.server_ids.contains(&comment.id))))) { this.resumed_pending = Some(review.id.clone()); }
                        cx.emit(GitHubReviewEvent::ThreadsChanged);
                    }
                    Err(error) => this.error = Some(format!("Comment is saved; canonical GitHub thread could not be refreshed: {error:#}")),
                }
                cx.notify();
            }).log_err();
        }));
        cx.notify();
    }

    fn resume_queued_deliveries(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            return;
        }
        for operation in self.operations.clone() {
            if operation.state != ReviewOperationState::Queued
                || operation.context != self.context
                || crate::review_outbox_runtime::is_in_flight(&operation.operation_id, cx)
            {
                continue;
            }
            // Queued means a committed user publication intent that was never
            // claimed. Sending/Unknown are deliberately excluded after recovery.
            if let Ok(intent) = serde_json::from_value::<ReviewIntent>(operation.payload.clone()) {
                self.deliver(operation, intent, window, cx);
            }
        }
    }

    fn begin_reply(&mut self, thread: &RemoteThread, window: &mut Window, cx: &mut Context<Self>) {
        if self.intent.is_some() {
            self.error = Some("Close or save the current composer before replying.".into());
            cx.notify();
            return;
        }
        self.composer_generation += 1;
        self.error = None;
        self.selected_thread = Some(thread.id.clone());
        self.draft = None;
        self.intent = Some(ReviewIntent::Reply {
            thread_id: thread.id.clone(),
            pending_review_id: None,
            body: String::new(),
        });
        self.body.update(cx, |body, cx| body.clear(window, cx));
        self.body.focus_handle(cx).focus(window, cx);
        cx.notify();
        cx.emit(GitHubReviewEvent::Compose);
    }
    fn begin_edit(
        &mut self,
        id: String,
        body: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.intent.is_some() {
            self.error = Some("Close the current composer before editing another comment.".into());
            cx.notify();
            return;
        }
        self.composer_generation += 1;
        self.error = None;
        self.draft = None;
        self.intent = Some(ReviewIntent::Edit {
            comment_id: id,
            body: body.clone(),
        });
        self.body
            .update(cx, |input, cx| input.set_text(&body, window, cx));
        self.body.focus_handle(cx).focus(window, cx);
        cx.emit(GitHubReviewEvent::Compose);
        cx.notify();
    }
    fn resume_draft(
        &mut self,
        operation: ReviewOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writing {
            return;
        }
        if self.intent.is_some() {
            self.error =
                Some("Close the current composer before resuming another local draft.".into());
            cx.notify();
            return;
        }
        match serde_json::from_value::<ReviewIntent>(operation.payload.clone()) {
            Ok(intent) => {
                self.composer_generation += 1;
                let body = intent.body().to_string();
                self.intent = Some(intent);
                self.draft = Some(operation);
                self.body
                    .update(cx, |input, cx| input.set_text(&body, window, cx));
                self.error = None;
            }
            Err(error) => self.error = Some(format!("Cannot read draft: {error}")),
        }
        cx.emit(GitHubReviewEvent::Compose);
        cx.notify();
    }
    fn begin_submit(&mut self, event: ReviewEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing
            || self.loading
            || self.composer_delivery_locked()
            || self.operations.iter().any(|operation| {
                operation.kind == ReviewOperationKind::SubmitReview
                    && matches!(
                        operation.state,
                        ReviewOperationState::Queued
                            | ReviewOperationState::Sending
                            | ReviewOperationState::DeliveryUnknown
                    )
            })
        {
            self.error = Some(
                "Wait for or verify the existing review operation before submitting another."
                    .into(),
            );
            cx.notify();
            return;
        }
        let review_id = self.pending().map(|review| review.id.clone());
        if self.intent.is_some() {
            self.error = Some("Close the current composer before submitting a review.".into());
            cx.notify();
            return;
        }
        self.composer_generation += 1;
        if event != ReviewEvent::Comment && !self.can_review {
            self.error =
                Some("Approval and requested changes require another author’s open PR.".into());
            cx.notify();
            return;
        }
        let summary = self
            .reviews
            .iter()
            .find(|review| Some(&review.id) == review_id.as_ref())
            .map(|review| review.body.clone())
            .unwrap_or_default();
        self.draft = None;
        self.intent = Some(match review_id {
            Some(review_id) => ReviewIntent::Submit {
                review_id,
                event,
                body: summary.clone(),
            },
            None => ReviewIntent::SubmitNew {
                event,
                body: summary.clone(),
            },
        });
        self.body
            .update(cx, |input, cx| input.set_text(&summary, window, cx));
        self.body.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(crate) fn reconcile(
        &mut self,
        operation: ReviewOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview {
            self.notice = Some(
                "Offline preview: reconciliation does not contact GitHub or resend this action."
                    .into(),
            );
            cx.notify();
            return;
        }
        if operation.kind == ReviewOperationKind::SetFileViewed {
            cx.emit(GitHubReviewEvent::ViewedOperation(operation, true));
            return;
        }
        if self.writing {
            return;
        }
        self.writing = true;
        self.reconciling = true;
        self.error = None;
        let db = self.db.clone();
        let remote = self.remote.clone();
        let task = cx.background_spawn(async move {
            let client = GitHubClient::default();
            let service = ReviewService::new(&client);
            let intent: ReviewIntent = serde_json::from_value(operation.payload.clone())?;
            // Review deletion needs only the guarded review collection; avoid scanning
            // every inline comment on a large PR for an unrelated review operation.
            let threads = if matches!(&intent, ReviewIntent::DeleteReview { .. }) {
                Vec::new()
            } else {
                service.load_threads(&remote)?
            };
            let reviews = service.load_reviews(&remote)?;
            let observed = all_ids(&threads, &reviews);
            let delivered =
                reconciled_ids(&service, &remote, &operation, &intent, &threads, &reviews)?;
            match delivered {
                Some(ids) => Ok::<_, anyhow::Error>(Some(
                    db.reconcile_operation(
                        operation.operation_id,
                        operation.revision,
                        ReviewReconciliation {
                            viewer_node_id: remote.identity.viewer_node_id,
                            head_oid: remote.head_oid,
                            observed_server_ids: observed,
                            outcome: ReviewReconciliationOutcome::Delivered { server_ids: ids },
                        },
                    )
                    .await?,
                )),
                None => Ok(None),
            }
        });
        cx.spawn_in(window,async move |this,cx|{
            let result=task.await;this.update_in(cx,|this,window,cx|{
                this.writing=false;this.reconciling=false;match result{
                    Ok(Some(operation))=>{
                        if this.draft.as_ref().is_some_and(|draft| draft.operation_id == operation.operation_id) {
                            this.intent = None;
                            this.draft = None;
                            this.body.read(cx).editor().clone().set_read_only(false, cx);
                            this.body.update(cx, |body, cx| body.clear(window, cx));
                            cx.emit(GitHubReviewEvent::Compose);
                        }
                        this.notice=Some("Delivery confirmed from GitHub read-back.".into());this.reload(window,cx);
                    },
                    Ok(None)=>this.error=Some("Delivery is still unknown. No exact read-back match was found; this operation will not be replayed. Keep the draft and check GitHub before deciding what to do next.".into()),
                    Err(error)=>this.error=Some(format!("Cannot reconcile: {error:#}")),
                }cx.notify();
            }).log_err();
        }).detach();
        cx.notify();
    }

    fn confirm_delete(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Delete this GitHub comment?",
            Some("This removes the comment from GitHub."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if matches!(answer.await, Ok(0)) {
                this.update_in(cx, |this, window, cx| {
                    this.persist(
                        ReviewIntent::Delete { comment_id: id },
                        None,
                        true,
                        window,
                        cx,
                    )
                })
                .log_err();
            }
        })
        .detach();
    }
}

impl GitHubReviewPanel {
    pub(crate) fn remote_context(&self) -> &ReviewContext {
        &self.remote
    }
    pub(crate) fn composer_thread(&self) -> Option<&str> {
        match self.intent.as_ref() {
            Some(ReviewIntent::Reply { thread_id, .. }) => Some(thread_id),
            Some(ReviewIntent::Edit { comment_id, .. }) => self
                .threads
                .iter()
                .find(|thread| {
                    thread
                        .comments
                        .iter()
                        .any(|comment| &comment.id == comment_id)
                })
                .map(|thread| thread.id.as_str()),
            _ => None,
        }
    }
    pub(crate) fn set_inline_composer(&mut self, inline: bool, cx: &mut Context<Self>) {
        self.inline_composer = inline;
        cx.notify();
    }
}

impl GitHubReviewPanel {
    pub(crate) fn has_inline_composer(&self) -> bool {
        matches!(
            self.intent,
            Some(
                ReviewIntent::Comment { .. }
                    | ReviewIntent::AddToReview { .. }
                    | ReviewIntent::Reply { .. }
                    | ReviewIntent::Edit { .. }
            )
        )
    }
}

impl GitHubReviewPanel {
    pub(crate) fn begin_unlocated_comment(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writing || self.intent.is_some() {
            self.error = Some("Close the current composer before starting another draft.".into());
            cx.notify();
            return;
        }
        self.composer_generation += 1;
        self.draft = None;
        self.intent = Some(ReviewIntent::Comment {
            intent: CommentIntent {
                context: self.remote.clone(),
                anchor: CommentAnchor {
                    path,
                    commit_oid: self.remote.head_oid.clone(),
                    side: crate::github_review::DiffSide::Right,
                    line: None,
                    start_line: None,
                    start_side: None,
                    subject_type: crate::github_review::CommentSubject::File,
                },
                body: String::new(),
                publication: CommentPublication::Immediate,
            },
        });
        self.body.update(cx, |input, cx| input.clear(window, cx));
        self.body.focus_handle(cx).focus(window, cx);
        cx.notify();
    }
    fn confirm_discard_review(
        &mut self,
        review_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(gpui::PromptLevel::Warning, "Discard your pending GitHub review?", Some("All comments in this pending review will be removed from GitHub. Local drafts remain."), &["Discard review", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if matches!(answer.await, Ok(0)) {
                this.update_in(cx, |this, window, cx| {
                    this.persist(
                        ReviewIntent::DeleteReview { review_id },
                        None,
                        true,
                        window,
                        cx,
                    )
                })
                .log_err();
            }
        })
        .detach();
    }
    fn resume_operation(
        &mut self,
        operation: ReviewOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if operation.kind == ReviewOperationKind::SetFileViewed {
            cx.emit(GitHubReviewEvent::ViewedOperation(operation, false));
            return;
        }
        if operation.state != ReviewOperationState::Queued {
            self.resume_draft(operation, window, cx);
            return;
        }
        if self.writing {
            return;
        }
        self.writing = true;
        let db = self.db.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    db.reset_queued_operation(operation.operation_id, operation.revision)
                        .await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.writing = false;
                match result {
                    Ok(operation) => this.resume_draft(operation, window, cx),
                    Err(error) => {
                        this.error = Some(format!("Cannot resume queued operation: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }
    fn archive_local_action(
        &mut self,
        operation: ReviewOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unknown = operation.state == ReviewOperationState::DeliveryUnknown;
        let answer = window.prompt(gpui::PromptLevel::Warning,
            if unknown { "Stop tracking this unconfirmed action?" } else { "Cancel this unsent action?" },
            Some(if unknown { "This archives local tracking only. GitHub may already contain the change; it will not be undone or resent." }
                 else { "The action will be archived locally. An already sending request cannot be cancelled." }),
            &[if unknown { "Stop tracking" } else { "Cancel action" }, "Keep action"], cx);
        let db = self.db.clone();
        cx.spawn_in(window, async move |panel, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let id = operation.operation_id.clone();
            let result = cx
                .background_spawn(async move {
                    db.archive_operation(operation.operation_id, operation.revision)
                        .await
                })
                .await;
            panel
                .update(cx, |panel, cx| {
                    match result {
                        Ok(()) => {
                            panel.adopt_archived_operations(vec![id.clone()]);
                            if panel
                                .draft
                                .as_ref()
                                .is_some_and(|draft| draft.operation_id == id)
                            {
                                panel.intent = None;
                                panel.draft = None;
                                cx.emit(GitHubReviewEvent::Compose);
                            }
                            panel.notice =
                                Some("Local action archived. GitHub was not modified.".into());
                            cx.emit(GitHubReviewEvent::LocalOperationsChanged);
                        }
                        Err(error) => {
                            panel.error = Some(format!("Cannot archive this action: {error:#}"))
                        }
                    }
                    cx.notify();
                })
                .log_err();
        })
        .detach();
    }

    fn discard_draft(
        &mut self,
        operation: ReviewOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writing {
            return;
        }
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            "Discard this local draft?",
            Some("This removes only the unsent local text. GitHub comments remain unchanged."),
            &["Discard draft", "Cancel"],
            cx,
        );
        let db = self.db.clone();
        cx.spawn_in(window, async move |this, cx| {
            if matches!(answer.await, Ok(0)) {
                let id = operation.operation_id.clone();
                let result = cx
                    .background_spawn(async move {
                        db.delete_operation_draft(operation.operation_id, operation.revision)
                            .await
                    })
                    .await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            this.operations
                                .retain(|operation| operation.operation_id != id);
                            if this
                                .draft
                                .as_ref()
                                .is_some_and(|draft| draft.operation_id == id)
                            {
                                this.draft = None;
                                this.intent = None;
                                cx.emit(GitHubReviewEvent::Compose);
                            }
                        }
                        Err(error) => this.error = Some(format!("Cannot discard draft: {error:#}")),
                    }
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }
}

impl GitHubReviewPanel {
    pub(crate) fn canonical_patches(&self) -> &BTreeMap<String, Option<String>> {
        &self.patches
    }
    #[cfg(test)]
    pub(crate) fn set_canonical_patch_for_test(&mut self, path: &str, patch: &str) {
        self.patches.insert(path.to_owned(), Some(patch.to_owned()));
    }
    pub(crate) fn operation_context(&self) -> &ReviewOperationContext {
        &self.context
    }
}

impl GitHubReviewPanel {
    pub(crate) fn composer_anchor(&self) -> Option<&CommentAnchor> {
        match &self.intent {
            Some(ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent }) => {
                Some(&intent.anchor)
            }
            _ => None,
        }
    }
}

impl GitHubReviewPanel {
    fn immediate_post_block_reason(&self, intent: &ReviewIntent) -> Option<&'static str> {
        match intent {
            ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent }
                if intent.anchor.subject_type == crate::github_review::CommentSubject::File =>
            {
                Some(
                    "File-level comments use review submission. Add this file comment to a review, then publish that review explicitly.",
                )
            }
            ReviewIntent::Comment { .. } | ReviewIntent::AddToReview { .. }
                if self.pending().is_some() =>
            {
                Some(
                    "GitHub blocks immediate comments while your pending review exists. Publish or discard that review explicitly in Threads & review. Your draft stays saved.",
                )
            }
            _ => None,
        }
    }

    pub(crate) fn post_comment_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.incremental_mode
            && matches!(
                self.intent,
                Some(ReviewIntent::Comment { .. } | ReviewIntent::AddToReview { .. })
            )
        {
            self.error = Some("Open Full PR comparison to post this inline draft.".into());
            cx.notify();
            return;
        }
        if let Some(reason) = self
            .intent
            .as_ref()
            .and_then(|intent| self.immediate_post_block_reason(intent))
        {
            self.error = Some(reason.into());
            cx.notify();
            return;
        }
        if self.draft.as_ref().is_some_and(|draft| {
            matches!(
                draft.state,
                ReviewOperationState::Sending
                    | ReviewOperationState::DeliveryUnknown
                    | ReviewOperationState::Queued
            )
        }) {
            self.error = Some(
                "This operation must be reconciled or explicitly resumed before posting.".into(),
            );
            cx.notify();
            return;
        }
        if let Some(ReviewIntent::AddToReview { intent }) = self.intent.clone() {
            self.intent = Some(ReviewIntent::Comment { intent });
        }
        match &mut self.intent {
            Some(ReviewIntent::Comment { intent }) => {
                intent.publication = CommentPublication::Immediate
            }
            Some(ReviewIntent::Reply {
                pending_review_id, ..
            }) => *pending_review_id = None,
            _ => {
                self.error = Some("Open an inline comment or reply composer before using Post GitHub comment now. Review submission remains a separate action.".into());
                cx.notify();
                return;
            }
        }
        self.save(true, window, cx);
    }
    pub(crate) fn save_local_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.intent.is_none() {
            self.error = Some("There is no open GitHub composer to save.".into());
            cx.notify();
            return;
        }
        self.save(false, window, cx);
    }
    pub(crate) fn close_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writing {
            self.error = Some(
                "Wait for the current draft save or GitHub operation before closing this composer."
                    .into(),
            );
            cx.notify();
            return;
        }
        if self.draft.as_ref().is_some_and(|draft| {
            matches!(
                draft.state,
                ReviewOperationState::Sending
                    | ReviewOperationState::DeliveryUnknown
                    | ReviewOperationState::Queued
            )
        }) {
            self.save_task = None;
            self.intent = None;
            self.draft = None;
            self.close_after_save = false;
            self.body.read(cx).editor().clone().set_read_only(false, cx);
            self.notice = Some("Composer closed. Its durable delivery record is preserved; reconcile it before any retry.".into());
            cx.emit(GitHubReviewEvent::Compose);
            cx.notify();
            return;
        }
        if self.intent.is_some() && !self.body.read(cx).text(cx).trim().is_empty() {
            self.close_after_save = true;
            self.save(false, window, cx);
        } else {
            self.intent = None;
            self.draft = None;
            cx.emit(GitHubReviewEvent::Compose);
            cx.notify();
        }
    }
    pub(crate) fn reconcile_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self
            .draft
            .as_ref()
            .filter(|draft| draft.state == ReviewOperationState::DeliveryUnknown)
            .cloned();
        let unknown = self
            .operations
            .iter()
            .filter(|operation| operation.state == ReviewOperationState::DeliveryUnknown)
            .cloned()
            .collect::<Vec<_>>();
        let operation = selected.or_else(|| {
            if unknown.len() == 1 {
                unknown.into_iter().next()
            } else {
                None
            }
        });
        if let Some(operation) = operation {
            self.reconcile(operation, window, cx);
        } else {
            self.error = Some("Select an unknown-delivery operation in Discussions to reconcile it. This action never resends a request.".into());
            cx.notify();
        }
    }
}

impl GitHubReviewPanel {
    fn composer_delivery_locked(&self) -> bool {
        self.draft.as_ref().is_some_and(|draft| {
            matches!(
                draft.state,
                ReviewOperationState::Sending
                    | ReviewOperationState::Queued
                    | ReviewOperationState::DeliveryUnknown
            )
        })
    }
}

const OLDER_PENDING_REVIEW_WARNING: &str = "Your pending review targets an earlier commit. This draft stays on this computer. Finish that earlier review on GitHub before adding comments to the current head.";

impl GitHubReviewPanel {
    pub(crate) fn set_incremental_mode(&mut self, incremental: bool, cx: &mut Context<Self>) {
        self.incremental_mode = incremental;
        self.inline_composer = false;
        cx.notify();
    }
    fn adopt_archived_operations(&mut self, archived: Vec<String>) {
        self.superseded_operations.extend(archived);
        self.operations
            .retain(|operation| !self.superseded_operations.contains(&operation.operation_id));
    }

    pub(crate) fn reload_local_operations(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let identity = self.remote.identity.clone();
        cx.spawn(async move |this, cx| {
            let operations = cx
                .background_spawn(async move {
                    Ok::<_, anyhow::Error>((
                        db.operations(&identity)?,
                        db.archived_operation_ids(&identity)?,
                    ))
                })
                .await;
            this.update(cx, |this, cx| {
                match operations {
                    Ok((operations, archived)) => {
                        this.adopt_archived_operations(archived);
                        for operation in operations {
                            this.upsert_operation(operation);
                        }
                        cx.emit(GitHubReviewEvent::LocalOperationsChanged);
                        cx.emit(GitHubReviewEvent::ThreadsChanged);
                    }
                    Err(error) => {
                        this.error = Some(format!("Cannot read local review operations: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }
}
