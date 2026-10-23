use super::presentation::{github_location, pending_publication_status, review_card_surface};
use super::*;
use crate::github_review::ReviewTransport;
use db::sqlez::thread_safe_connection::ThreadSafeConnection;
use editor::hover_markdown_style;
use futures::FutureExt as _;
use gpui::{Render, TestAppContext};
use markdown::MarkdownElement;
use serde_json::{Value, json};
use settings::SettingsStore;

fn context() -> ReviewOperationContext {
    ReviewOperationContext {
        selector: ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_example".into(),
                pull_request_node_id: "PR_example".into(),
                viewer_node_id: "U_viewer".into(),
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
async fn database(cx: &mut TestAppContext) -> (tempfile::TempDir, ReviewDb) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    let directory = tempfile::tempdir().expect("temporary database");
    let connection = ThreadSafeConnection::builder::<ReviewDb>(
        directory
            .path()
            .join("review.sqlite")
            .to_str()
            .expect("path"),
        true,
    )
    .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
    .build()
    .await
    .expect("database connection");
    let db = ReviewDb::from_connection(connection).expect("review database");
    (directory, db)
}

fn editable_thread() -> RemoteThread {
    serde_json::from_value(json!({"id":"thread:edit","path":"src/example.rs","line":3,"diffSide":"RIGHT","subjectType":"LINE","isResolved":false,"isOutdated":false,"viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":false,"comments":[{"id":"comment:edit","body":"Original body","state":"SUBMITTED","path":"src/example.rs","url":"","createdAt":"","updatedAt":"","diffHunk":"@@ -1,3 +1,3 @@","viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true}]})).unwrap()
}

#[gpui::test]
async fn flat_threads_default_to_unresolved_and_pending_actions_exclude_drafts(
    cx: &mut TestAppContext,
) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity, window, cx)
    });
    panel.update_in(cx, |panel, window, cx| {
        let mut thread = editable_thread();
        assert!(panel.thread_expanded(&thread));
        thread.is_resolved = true;
        assert!(!panel.thread_expanded(&thread));
        panel.thread_expansion.insert(thread.id.clone(), true);
        assert!(panel.thread_expanded(&thread));
        assert!(ThreadFilter::All.includes(true) && ThreadFilter::All.includes(false));
        assert!(ThreadFilter::Resolved.includes(true) && !ThreadFilter::Resolved.includes(false));
        assert!(
            ThreadFilter::Unresolved.includes(false) && !ThreadFilter::Unresolved.includes(true)
        );
        for (index, state) in [
            ReviewOperationState::LocalDraft,
            ReviewOperationState::Queued,
            ReviewOperationState::Sending,
            ReviewOperationState::Failed,
            ReviewOperationState::DeliveryUnknown,
            ReviewOperationState::Sent,
        ]
        .into_iter()
        .enumerate()
        {
            panel.operations.push(ReviewOperation {
                operation_id: format!("fixture-{index}"),
                revision: 1,
                context: panel.context.clone(),
                kind: ReviewOperationKind::EditComment,
                payload: json!({}),
                location: ReviewLocationState::NotRequired,
                state,
                before_server_ids: vec![],
                server_ids: vec![],
                last_error: None,
                created_at_unix_seconds: 1,
                updated_at_unix_seconds: 1,
            });
        }
        assert_eq!(panel.pending_action_count(), 4);
        panel.error = Some("Read failed".into());
        assert_eq!(panel.pending_action_count(), 5);
        panel.show_pending_actions(window, cx);
        assert!(panel.pending_actions_mode);
        panel.show_threads(cx);
        assert!(!panel.pending_actions_mode);
        // Never dispatch an unknown action through Retry; only reconciliation is allowed.
        let unknown = panel.operations[4].clone();
        panel.retry_failed_operation(unknown, window, cx);
        assert!(!panel.writing);
    });
}

#[gpui::test]
async fn edit_and_reply_render_inside_one_owned_thread(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) =
        cx.add_window_view(|window, cx| GitHubReviewPanel::new(db, context, identity, window, cx));
    let thread = editable_thread();
    panel.update_in(cx, |panel, window, cx| {
        panel.threads = vec![thread.clone()];
        panel.inline_composer = true;
        panel.error = Some("Old error".into());
        panel.begin_edit("comment:edit".into(), "Original body".into(), window, cx);
        assert!(panel.error.is_none());
        assert_eq!(panel.composer_thread(), Some("thread:edit"));
        panel.save_task = None;
    });
    let card = cx.new(|cx| GitHubInlineCard::new(&panel, Some(thread.id.clone()), cx));
    cx.draw(
        Default::default(),
        gpui::size(px(600.), px(700.)),
        |_, _| v_flex().w(px(600.)).h(px(700.)).child(card.clone()),
    );
    assert!(cx.debug_bounds("GITHUB_THREAD_EMBEDDED_COMPOSER").is_some());
    assert!(cx.debug_bounds("GITHUB_STANDALONE_COMPOSER").is_none());
    panel.update(cx, |panel, _| panel.inline_composer = false);
    cx.draw(
        Default::default(),
        gpui::size(px(600.), px(700.)),
        |_, _| v_flex().w(px(600.)).h(px(700.)).child(card.clone()),
    );
    assert!(
        cx.debug_bounds("GITHUB_THREAD_EMBEDDED_COMPOSER").is_none(),
        "the non-owning inline card must not render the sidebar editor again"
    );
    assert!(
        cx.debug_bounds("GITHUB_PANEL_EMBEDDED_COMPOSER").is_some(),
        "the owning flat sidebar renders exactly its own editor surface"
    );
    panel.update_in(cx, |panel, window, cx| {
        panel.inline_composer = true;
        panel.intent = None;
        panel.draft = None;
        panel.error = Some("Old edit error".into());
        panel.begin_reply(&thread, window, cx);
        panel.save_task = None;
        assert!(panel.error.is_none());
        assert_eq!(panel.composer_thread(), Some("thread:edit"));
    });
    cx.draw(
        Default::default(),
        gpui::size(px(600.), px(700.)),
        |_, _| v_flex().w(px(600.)).h(px(700.)).child(card.clone()),
    );
    assert!(cx.debug_bounds("GITHUB_THREAD_EMBEDDED_COMPOSER").is_some());
    assert!(cx.debug_bounds("GITHUB_STANDALONE_COMPOSER").is_none());
}

#[gpui::test]
async fn edit_and_reply_acceptance_show_thread_delivery_and_preserve_failure(
    cx: &mut TestAppContext,
) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity, window, cx)
    });
    let thread = editable_thread();
    for edit in [true, false] {
        let (release, gate) = futures::channel::oneshot::channel::<()>();
        panel.update_in(cx, |panel, window, cx| {
            panel.operations.clear();
            panel.threads = vec![thread.clone()];
            for comment in &thread.comments {
                panel.markdown.insert(
                    comment.id.clone(),
                    cx.new(|cx| Markdown::new(comment.body.clone().into(), None, None, cx)),
                );
            }
            panel.delivery_pause = Some(
                cx.background_spawn(async move {
                    gate.await.unwrap();
                    Err("Offline before dispatch".into())
                })
                .shared(),
            );
            if edit {
                panel.begin_edit("comment:edit".into(), "Original body".into(), window, cx)
            } else {
                panel.begin_reply(&thread, window, cx)
            }
            panel.body.update(cx, |body, cx| {
                body.set_text(if edit { "Edited body" } else { "Reply body" }, window, cx)
            });
            panel.save(true, window, cx);
        });
        cx.condition(&panel, |p, _| {
            !p.writing
                && p.intent.is_none()
                && p.operations
                    .iter()
                    .any(|op| op.state == ReviewOperationState::Queued)
        })
        .await;
        panel.read_with(cx, |p, _| {
            let ops = p.thread_delivery_operations(&thread);
            assert_eq!(ops.len(), 1);
            let intent: ReviewIntent = serde_json::from_value(ops[0].payload.clone()).unwrap();
            assert_eq!(
                intent.body(),
                if edit { "Edited body" } else { "Reply body" }
            );
            assert_eq!(
                db.operation(&ops[0].operation_id).unwrap().state,
                ReviewOperationState::Queued
            );
        });
        if edit {
            let card = cx.new(|cx| GitHubInlineCard::new(&panel, Some(thread.id.clone()), cx));
            cx.draw(
                Default::default(),
                gpui::size(px(600.), px(700.)),
                |_, _| v_flex().w(px(600.)).h(px(700.)).child(card.clone()),
            );
            assert!(
                cx.debug_bounds("GITHUB_COMMENT_EDIT_DELIVERY_BODY")
                    .is_some()
            );
            assert!(cx.debug_bounds("GITHUB_CANONICAL_COMMENT_BODY").is_none());
            assert!(cx.debug_bounds("GITHUB_STANDALONE_COMPOSER").is_none());
            assert!(cx.debug_bounds("GITHUB_THREAD_EMBEDDED_COMPOSER").is_none());
            panel.update(cx, |panel, _| {
                panel.operations[0].state = ReviewOperationState::LocalDraft;
            });
            cx.draw(
                Default::default(),
                gpui::size(px(600.), px(700.)),
                |_, _| v_flex().w(px(600.)).h(px(700.)).child(card.clone()),
            );
            assert!(
                cx.debug_bounds("GITHUB_COMMENT_EDIT_DELIVERY_BODY")
                    .is_none()
            );
            assert!(cx.debug_bounds("GITHUB_CANONICAL_COMMENT_BODY").is_some());
            panel.update(cx, |panel, _| {
                panel.operations[0].state = ReviewOperationState::Queued;
            });
        }
        release.send(()).unwrap();
        cx.condition(&panel, |p, _| {
            p.operations
                .iter()
                .any(|op| op.state == ReviewOperationState::Failed)
        })
        .await;
        panel.read_with(cx, |p, _| {
            let ops = p.thread_delivery_operations(&thread);
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].state, ReviewOperationState::Failed);
            assert!(ops[0].last_error.as_ref().unwrap().contains("Offline"));
        });
    }
}

#[gpui::test]
async fn canonical_thread_receipts_replace_local_inline_cards_by_identity(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db, context.clone(), identity, window, cx)
    });
    panel.update_in(cx, |panel, _window, _cx| {
        let anchor = CommentAnchor { path: "src/example.rs".into(), commit_oid: "2".repeat(40), side: crate::github_review::DiffSide::Right, line:Some(3),start_line:None,start_side:None,subject_type:crate::github_review::CommentSubject::Line };
        let thread: RemoteThread = serde_json::from_value(json!({"id":"thread:canonical","path":"src/example.rs","line":3,"diffSide":"RIGHT","subjectType":"LINE","isResolved":false,"isOutdated":false,"viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":false,"comments":[{"id":"comment:canonical","body":"Same body","state":"PENDING","path":"src/example.rs","url":"","createdAt":"","updatedAt":"","diffHunk":"@@ -1,3 +1,3 @@","viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true}]})).unwrap();
        let mut operation = ReviewOperation { operation_id:db::uuid::Uuid::now_v7().to_string(),revision:1,context:context.clone(),kind:ReviewOperationKind::AddPendingThread,payload:serde_json::to_value(ReviewIntent::AddToReview {intent:CommentIntent{context:panel.remote.clone(),anchor,body:"Same body".into(),publication:CommentPublication::Immediate}}).unwrap(),location:ReviewLocationState::Ready,state:ReviewOperationState::Sent,before_server_ids:vec![],server_ids:vec![thread.id.clone()],last_error:None,created_at_unix_seconds:1,updated_at_unix_seconds:1 };
        panel.threads = vec![];panel.operations = vec![operation.clone()];
        assert_eq!(panel.local_inline_comments().len(),1,"retain one local receipt until canonical readback");
        panel.threads = vec![thread.clone()];
        assert!(panel.local_inline_comments().is_empty(),"thread receipt must suppress local duplicate");
        operation.server_ids=vec!["comment:canonical".into()];panel.operations=vec![operation.clone()];
        assert!(panel.local_inline_comments().is_empty(),"comment receipt must suppress local duplicate");
        operation.server_ids=vec!["comment:distinct".into()];panel.operations=vec![operation.clone()];
        assert_eq!(panel.local_inline_comments().len(),1,"equal text/location must not hide a distinct comment");
        operation.state=ReviewOperationState::DeliveryUnknown;operation.server_ids=vec![thread.id];panel.operations=vec![operation];
        assert_eq!(panel.local_inline_comments().len(),1,"uncertain delivery must stay visible until explicit reconciliation");
        panel.operations[0].state = ReviewOperationState::Sent;
        panel.threads.clear(); panel.generation = 7;
        panel.canonical_threads_generation = Some(7); panel.loading = false;
        assert!(panel.local_inline_comments().is_empty(), "authoritative remote deletion must not resurrect a sent local comment");
        panel.generation = 8; panel.loading = true;
        assert_eq!(panel.local_inline_comments().len(), 1, "an incomplete newer read cannot settle a local receipt");
        panel.loading = false; panel.error = Some("Read failed".into());
        assert_eq!(panel.local_inline_comments().len(), 1, "a failed read is not authoritative absence");

    });
}

#[gpui::test]
async fn direct_post_explains_existing_pending_without_queueing_or_publishing(
    cx: &mut TestAppContext,
) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity.clone(), window, cx)
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.reviews = vec![RemoteReview {
            id: "review:preserved".into(),
            state: "PENDING".into(),
            body: "Existing unpublished body".into(),
            author: None,
            commit: None,
            url: String::new(),
            viewer_can_update: true,
            viewer_can_delete: true,
            viewer_did_author: true,
        }];
        panel.intent = Some(ReviewIntent::Comment {
            intent: CommentIntent {
                context: panel.remote.clone(),
                anchor: CommentAnchor {
                    path: "src/example.rs".into(),
                    commit_oid: "2".repeat(40),
                    side: crate::github_review::DiffSide::Right,
                    line: Some(3),
                    start_line: None,
                    start_side: None,
                    subject_type: crate::github_review::CommentSubject::Line,
                },
                body: "Keep saved draft".into(),
                publication: CommentPublication::Immediate,
            },
        });
        panel.post_comment_now(window, cx);
        assert!(panel.error.as_deref().unwrap().contains("pending review"));
        assert_eq!(panel.reviews[0].body, "Existing unpublished body");
        assert!(panel.intent.is_some());
        assert!(!panel.writing);
    });
    assert!(db.operations(&identity).unwrap().is_empty());
}

#[gpui::test]
async fn review_preview_requires_no_pending_review_and_does_not_mutate_remote(
    cx: &mut TestAppContext,
) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity.clone(), window, cx)
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.loading = false;
        panel.can_review = true;
        panel.begin_submit(ReviewEvent::Approve, window, cx);
        assert!(matches!(
            panel.intent,
            Some(ReviewIntent::SubmitNew {
                event: ReviewEvent::Approve,
                ..
            })
        ));
    });
    assert!(
        db.operations(&identity).unwrap().is_empty(),
        "opening a preview must not create a review or durable send"
    );
}

#[gpui::test]
async fn github_review_local_draft_survives_and_resume_cannot_replace_unsaved_text(
    cx: &mut TestAppContext,
) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().expect("identity").clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context.clone(), identity, window, cx)
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.begin_unlocated_comment("src/example.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text("Review text survives restart", window, cx)
        });
        panel.save(false, window, cx);
    });
    cx.condition(&panel, |panel, _| !panel.writing && panel.draft.is_some())
        .await;
    let operation = panel.read_with(cx, |panel, _| {
        panel.draft.clone().expect("saved local draft")
    });
    let durable = db
        .operation(&operation.operation_id)
        .expect("durable draft readback");
    assert_eq!(durable.state, ReviewOperationState::LocalDraft);
    assert_eq!(
        serde_json::from_value::<ReviewIntent>(durable.payload.clone())
            .expect("typed payload")
            .body(),
        "Review text survives restart"
    );
    panel.update_in(cx, |panel, window, cx| {
        panel
            .body
            .update(cx, |body, cx| body.set_text("New unsaved text", window, cx));
        panel.resume_draft(durable.clone(), window, cx);
        assert_eq!(panel.body.read(cx).text(cx), "New unsaved text");
        assert!(
            panel
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Close the current composer"))
        );
        panel.save_task = None;
        panel.intent = None;
        panel.draft = None;
        panel.resume_draft(durable, window, cx);
        assert_eq!(panel.body.read(cx).text(cx), "Review text survives restart");
    });
}

#[gpui::test]
async fn github_review_send_freezes_input_until_preflight_finishes(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().expect("identity").clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity, window, cx)
    });
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    panel.update_in(cx, |panel, window, cx| {
        panel.recovery = cx
            .background_spawn(async move {
                gate.await.expect("gate released");
                Err("Stopped before any GitHub dispatch".into())
            })
            .shared();
        panel.begin_unlocated_comment("src/example.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text("Body captured for publication", window, cx)
        });
        panel.save(true, window, cx);
        let editor = panel
            .body
            .read(cx)
            .editor()
            .as_any()
            .downcast_ref::<Entity<editor::Editor>>()
            .expect("input editor")
            .clone();
        assert!(
            editor.read(cx).read_only(cx),
            "in-flight send must prevent newer text from being cleared by its receipt"
        );
        assert!(panel.writing);
    });
    release.send(()).expect("release preflight");
    cx.condition(&panel, |panel, _| !panel.writing).await;
    panel.read_with(cx, |panel, cx| {
        let editor = panel
            .body
            .read(cx)
            .editor()
            .as_any()
            .downcast_ref::<Entity<editor::Editor>>()
            .expect("input editor");
        assert!(!editor.read(cx).read_only(cx));
        assert_eq!(
            panel.body.read(cx).text(cx),
            "Body captured for publication"
        );
        assert!(
            panel
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Stopped before any GitHub dispatch"))
        );
    });
}

#[gpui::test]
async fn github_review_durable_acceptance_frees_composer_before_network(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity, window, cx)
    });
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    panel.update_in(cx, |panel, window, cx| {
        panel.delivery_pause = Some(
            cx.background_spawn(async move {
                gate.await.unwrap();
                Err("Offline before any request".into())
            })
            .shared(),
        );
        panel.begin_unlocated_comment("src/example.rs".into(), window, cx);
        // A verified fixture anchor is required before publication can queue.
        if let Some(ReviewIntent::Comment { intent }) = &mut panel.intent {
            intent.anchor.line = Some(3);
            intent.anchor.subject_type = crate::github_review::CommentSubject::Line;
        }
        panel.body.update(cx, |body, cx| {
            body.set_text("First durable comment", window, cx)
        });
        panel.save(true, window, cx);
    });
    cx.condition(&panel, |panel, _| {
        !panel.writing
            && panel.intent.is_none()
            && panel
                .operations
                .iter()
                .any(|op| op.state == ReviewOperationState::Queued)
    })
    .await;
    let queued = panel.read_with(cx, |panel, _| panel.operations[0].clone());
    assert_eq!(
        db.operation(&queued.operation_id).unwrap().state,
        ReviewOperationState::Queued
    );
    panel.update_in(cx, |panel, window, cx| {
        panel.begin_unlocated_comment("src/another.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text("Second independent draft", window, cx)
        });
    });
    release.send(()).unwrap();
    cx.condition(&panel, |panel, _| {
        panel
            .operations
            .iter()
            .any(|op| op.state == ReviewOperationState::Failed)
    })
    .await;
    panel.read_with(cx, |panel, cx| {
        assert_eq!(panel.body.read(cx).text(cx), "Second independent draft");
        assert!(panel.intent.is_some());
        assert!(!panel.writing);
    });
    let failed = db.operation(&queued.operation_id).unwrap();
    assert_eq!(failed.state, ReviewOperationState::Failed);
    assert!(failed.last_error.as_deref().unwrap().contains("Offline"));
}

#[gpui::test]
async fn github_review_close_flushes_automatic_draft(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity.clone(), window, cx)
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.begin_unlocated_comment("src/example.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text("Close immediately, preserve me", window, cx)
        });
        panel.close_composer(window, cx);
    });
    cx.condition(&panel, |panel, _| !panel.writing && panel.intent.is_none())
        .await;
    let operations = db.operations(&identity).unwrap();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, ReviewOperationState::LocalDraft);
    assert_eq!(
        serde_json::from_value::<ReviewIntent>(operations[0].payload.clone())
            .unwrap()
            .body(),
        "Close immediately, preserve me"
    );
}

#[gpui::test]
async fn github_review_inline_multiline_editor_receives_the_card_width(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().expect("identity").clone();
    let (panel, cx) =
        cx.add_window_view(|window, cx| GitHubReviewPanel::new(db, context, identity, window, cx));
    panel.update_in(cx, |panel, window, cx| {
        panel.begin_unlocated_comment("src/example.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text("Visible multiline review text\nSecond line", window, cx)
        });
        panel.set_inline_composer(true, cx);
        panel.save_task = None;
    });
    let card = cx.new(|cx| GitHubInlineCard::new(&panel, None, cx));
    cx.draw(
        Default::default(),
        gpui::size(px(400.), px(360.)),
        |_, _| v_flex().w(px(400.)).h(px(360.)).child(card),
    );
    panel.read_with(cx, |panel, cx| {
        let editor = panel
            .body
            .read(cx)
            .editor()
            .as_any()
            .downcast_ref::<Entity<editor::Editor>>()
            .expect("input editor");
        let bounds = editor
            .read(cx)
            .last_bounds()
            .copied()
            .expect("rendered editor bounds");
        assert!(
            bounds.size.width > px(300.),
            "multiline text must get the card's actual width, not a zero-width row: {bounds:?}"
        );
        assert!(bounds.size.height > px(20.));
        assert_eq!(
            editor.read(cx).text(cx),
            "Visible multiline review text\nSecond line"
        );
    });
}

#[gpui::test]
async fn github_review_keyboard_actions_preserve_unknown_delivery_and_durable_text(
    cx: &mut TestAppContext,
) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().expect("identity").clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context.clone(), identity, window, cx)
    });
    let remote = panel.read_with(cx, |panel, _| panel.remote.clone());
    let intent = ReviewIntent::Comment {
        intent: CommentIntent {
            context: remote,
            anchor: CommentAnchor {
                path: "src/example.rs".into(),
                commit_oid: "2".repeat(40),
                side: crate::github_review::DiffSide::Right,
                line: Some(1),
                start_line: None,
                start_side: None,
                subject_type: crate::github_review::CommentSubject::Line,
            },
            body: "Unconfirmed comment text".into(),
            publication: CommentPublication::Immediate,
        },
    };
    let draft = db
        .create_operation(
            context,
            intent.kind(),
            serde_json::to_value(&intent).expect("typed intent"),
            ReviewLocationState::Ready,
        )
        .await
        .expect("local draft");
    let queued = db
        .queue_operation(draft.operation_id, draft.revision)
        .await
        .expect("queued");
    let claimed = db
        .claim_operation(queued.operation_id, queued.revision, Vec::new())
        .await
        .expect("claimed");
    let unknown = db
        .finish_operation(
            claimed.operation_id,
            claimed.revision,
            ReviewOperationOutcome::DeliveryUnknown {
                reason: "Unconfirmed test receipt".into(),
            },
        )
        .await
        .expect("unknown operation");
    panel.update_in(cx, |panel, window, cx| {
        panel.intent = Some(intent);
        panel.draft = Some(unknown.clone());
        panel.operations = vec![unknown.clone()];
        panel.body.update(cx, |body, cx| {
            body.set_text("Unconfirmed comment text", window, cx)
        });
        panel.post_comment_now(window, cx);
        assert!(
            !panel.writing,
            "palette action must not replay unknown delivery"
        );
        assert!(
            panel
                .error
                .as_deref()
                .is_some_and(|error| error.contains("reconciled"))
        );
        panel.close_composer(window, cx);
        assert!(panel.intent.is_none());
        assert!(panel.draft.is_none());
    });
    let retained = db
        .operation(&unknown.operation_id)
        .expect("durable operation");
    assert_eq!(retained.state, ReviewOperationState::DeliveryUnknown);
    assert_eq!(retained.revision, unknown.revision);
    assert_eq!(
        serde_json::from_value::<ReviewIntent>(retained.payload)
            .expect("durable text")
            .body(),
        "Unconfirmed comment text"
    );
}

#[gpui::test]
async fn github_review_pending_controls_fit_narrow_sidebar(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().expect("identity").clone();
    let (panel, cx) =
        cx.add_window_view(|window, cx| GitHubReviewPanel::new(db, context, identity, window, cx));
    panel.update(cx, |panel, cx| {
        panel.reviews = vec![RemoteReview {
            id: "review:pending".into(),
            state: "PENDING".into(),
            body: String::new(),
            author: None,
            commit: None,
            url: String::new(),
            viewer_can_update: true,
            viewer_can_delete: true,
            viewer_did_author: true,
        }];
        cx.notify();
    });
    cx.simulate_resize(gpui::size(px(400.), px(600.)));
    cx.run_until_parked();
    let status = cx
        .debug_bounds("GITHUB_PENDING_STATUS")
        .expect("pending status");
    assert!(status.right() <= px(400.));
    assert!(
        status.size.height >= px(100.),
        "notice and both native controls need visible space: {status:?}"
    );
}

#[gpui::test]
async fn github_review_card_keeps_markdown_and_footer_inside_intrinsic_bounds(
    cx: &mut TestAppContext,
) {
    let (_directory, _db) = database(cx).await;
    struct CardBoundsFixture {
        markdown: Entity<Markdown>,
    }
    impl Render for CardBoundsFixture {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .id("review-card-scroll-fixture")
                .w_full()
                .h_full()
                .overflow_y_scroll()
                .child(
                    review_card_surface(cx)
                        .debug_selector(|| "REVIEW_CARD_INTRINSIC".into())
                        .child(div().w_full().min_w_0().flex_shrink_0().p_3().child(
                            MarkdownElement::new(
                                self.markdown.clone(),
                                hover_markdown_style(window, cx),
                            ),
                        ))
                        .child(
                            div()
                                .debug_selector(|| "REVIEW_CARD_FOOTER".into())
                                .w_full()
                                .flex_shrink_0()
                                .h(px(24.))
                                .child(Label::new("Final visible line")),
                        ),
                )
        }
    }
    let (_, cx) = cx.add_window_view(|_, cx| CardBoundsFixture {
        markdown: cx.new(|cx| {
            Markdown::new(
                (0..24)
                    .map(|line| {
                        format!("Paragraph {line}: review content must remain fully readable.\n\n")
                    })
                    .collect::<String>()
                    .into(),
                None,
                None,
                cx,
            )
        }),
    });
    cx.simulate_resize(gpui::size(px(400.), px(160.)));
    cx.run_until_parked();
    let card = cx
        .debug_bounds("REVIEW_CARD_INTRINSIC")
        .expect("card bounds");
    let footer = cx
        .debug_bounds("REVIEW_CARD_FOOTER")
        .expect("last line bounds");
    assert!(
        card.size.height > px(160.),
        "scrollable review content must keep its intrinsic height: {card:?}"
    );
    assert!(
        footer.bottom() <= card.bottom(),
        "final Markdown/footer content must stay within the unclipped card: {footer:?} / {card:?}"
    );
}

#[gpui::test]
async fn failed_comment_can_join_existing_pending_review_without_replacing_open_draft(
    cx: &mut TestAppContext,
) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context.clone(), identity, window, cx)
    });
    let remote = panel.read_with(cx, |panel, _| panel.remote.clone());
    let intent = ReviewIntent::Comment {
        intent: CommentIntent {
            context: remote,
            anchor: CommentAnchor {
                path: "src/example.rs".into(),
                commit_oid: "2".repeat(40),
                side: crate::github_review::DiffSide::Right,
                line: Some(3),
                start_line: None,
                start_side: None,
                subject_type: crate::github_review::CommentSubject::Line,
            },
            body: "Previously unsent comment".into(),
            publication: CommentPublication::Immediate,
        },
    };
    let draft = db
        .create_operation(
            context,
            intent.kind(),
            serde_json::to_value(intent).unwrap(),
            ReviewLocationState::Ready,
        )
        .await
        .unwrap();
    let queued = db
        .queue_operation(draft.operation_id, draft.revision)
        .await
        .unwrap();
    let claimed = db
        .claim_operation(queued.operation_id, queued.revision, Vec::new())
        .await
        .unwrap();
    let failed = db
        .finish_operation(
            claimed.operation_id,
            claimed.revision,
            ReviewOperationOutcome::Failed {
                reason: "Existing pending review blocks immediate post".into(),
            },
        )
        .await
        .unwrap();
    let operation_id = failed.operation_id.clone();
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    panel.update_in(cx, |panel, window, cx| {
        panel.delivery_pause = Some(
            cx.background_spawn(async move {
                gate.await.unwrap();
                Err("Stopped before mutation for recovery test".into())
            })
            .shared(),
        );
        panel.reviews = vec![RemoteReview {
            id: "PRR_existing".into(),
            state: "PENDING".into(),
            body: String::new(),
            author: None,
            commit: None,
            url: String::new(),
            viewer_can_update: true,
            viewer_can_delete: true,
            viewer_did_author: true,
        }];
        panel.begin_unlocated_comment("src/unrelated.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text("Another draft remains open", window, cx)
        });
        panel.add_saved_comment_to_pending(failed, "PRR_existing".into(), window, cx);
    });
    cx.condition(&panel, |panel, _| {
        !panel.writing
            && panel.operations.iter().any(|operation| {
                operation.operation_id == operation_id
                    && operation.state == ReviewOperationState::Queued
            })
    })
    .await;
    let recovered = db.operation(&operation_id).unwrap();
    assert_eq!(recovered.kind, ReviewOperationKind::AddPendingThread);
    assert!(
        matches!(serde_json::from_value::<ReviewIntent>(recovered.payload).unwrap(), ReviewIntent::Comment { intent:CommentIntent { publication:CommentPublication::Pending { review_id }, .. } } if review_id == "PRR_existing")
    );
    panel.read_with(cx, |panel, cx| {
        assert_eq!(panel.body.read(cx).text(cx), "Another draft remains open");
        assert!(panel.intent.is_some());
    });
    release.send(()).unwrap();
    cx.condition(&panel, |panel, _| {
        panel.operations.iter().any(|operation| {
            operation.operation_id == operation_id
                && operation.state == ReviewOperationState::Failed
        })
    })
    .await;
    panel.read_with(cx, |panel, cx| {
        assert_eq!(panel.body.read(cx).text(cx), "Another draft remains open")
    });
}

#[gpui::test]
async fn github_review_long_thread_paths_fit_narrow_panel(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) =
        cx.add_window_view(|window, cx| GitHubReviewPanel::new(db, context, identity, window, cx));
    panel.update(cx, |panel, cx| {
        panel.threads = vec![RemoteThread {
            id: "thread:long-path".into(),
            path: format!("{}/example.rs", "long-directory/".repeat(24)),
            line: Some(42),
            start_line: None,
            original_line: Some(42),
            original_start_line: None,
            diff_side: crate::github_review::DiffSide::Right,
            start_diff_side: None,
            subject_type: crate::github_review::CommentSubject::Line,
            is_resolved: false,
            is_outdated: false,
            viewer_can_reply: true,
            viewer_can_resolve: true,
            viewer_can_unresolve: false,
            comments: Vec::new(),
        }];
        panel.selected_thread = Some("thread:long-path".into());
        cx.notify();
    });
    cx.simulate_resize(gpui::size(px(280.), px(800.)));
    cx.run_until_parked();
    let row = cx
        .debug_bounds("GITHUB_THREAD_SUMMARY")
        .expect("flat thread summary");
    let path = cx
        .debug_bounds("GITHUB_THREAD_CARD_PATH")
        .expect("canonical path label");
    assert!(
        row.right() <= px(280.),
        "row must stay inside the panel: {row:?}"
    );
    assert!(
        path.right() <= px(280.) && path.left() >= px(0.),
        "canonical path must truncate inside the panel: {path:?}"
    );
}

#[gpui::test]
async fn autosaved_comment_add_to_review_removes_superseded_draft_card(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity, window, cx)
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.begin_unlocated_comment("src/example.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text("Autosaved comment joins explicit review", window, cx)
        });
        panel.save(false, window, cx);
    });
    cx.condition(&panel, |panel, _| !panel.writing && panel.draft.is_some())
        .await;
    let old_id = panel.read_with(cx, |panel, _| {
        panel.draft.as_ref().unwrap().operation_id.clone()
    });
    let old_operation = db.operation(&old_id).unwrap();
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    panel.update_in(cx, |panel, window, cx| {
        panel.reviews = vec![RemoteReview {
            id: "PRR_known".into(),
            state: "PENDING".into(),
            body: String::new(),
            author: None,
            commit: Some(crate::github_review::CommitRef {
                oid: "2".repeat(40),
            }),
            url: String::new(),
            viewer_can_update: true,
            viewer_can_delete: true,
            viewer_did_author: true,
        }];
        panel.resumed_pending = None;
        panel.delivery_pause = Some(
            cx.background_spawn(async move {
                gate.await.unwrap();
                Err("Stopped before network in known-review test".into())
            })
            .shared(),
        );
        panel.add_current_to_review(window, cx);
    });
    cx.condition(&panel, |panel, _| !panel.writing && panel.intent.is_none())
        .await;
    panel.read_with(cx, |panel, _| {
        assert_eq!(
            panel.operations.len(),
            1,
            "superseded autosave must not remain as a duplicate card"
        );
        assert_ne!(panel.operations[0].operation_id, old_id);
        assert_eq!(
            panel.operations[0].kind,
            ReviewOperationKind::AddPendingThread
        );
        assert!(matches!(
            serde_json::from_value::<ReviewIntent>(panel.operations[0].payload.clone()).unwrap(),
            ReviewIntent::AddToReview { .. }
        ));
    });
    panel.update(cx, |panel, _| {
        panel.upsert_operation(old_operation);
        assert_eq!(
            panel.operations.len(),
            1,
            "late stale reads must not resurrect a superseded autosave"
        );
    });
    assert!(db.operation(&old_id).is_err());
    release.send(()).unwrap();
    cx.condition(&panel, |panel, _| {
        panel
            .operations
            .iter()
            .any(|operation| operation.state == ReviewOperationState::Failed)
    })
    .await;
}

#[gpui::test]
async fn older_pending_review_keeps_new_comment_local_without_queueing(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let (panel, cx) = cx.add_window_view(|window, cx| {
        GitHubReviewPanel::new(db.clone(), context, identity.clone(), window, cx)
    });
    panel.update_in(cx, |panel, window, cx| {
        panel.reviews = vec![RemoteReview {
            id: "PRR_older".into(),
            state: "PENDING".into(),
            body: String::new(),
            author: None,
            commit: Some(crate::github_review::CommitRef {
                oid: "1".repeat(40),
            }),
            url: String::new(),
            viewer_can_update: true,
            viewer_can_delete: true,
            viewer_did_author: true,
        }];
        panel.begin_unlocated_comment("src/example.rs".into(), window, cx);
        panel.body.update(cx, |body, cx| {
            body.set_text(
                "Preserve current-head text without mutating the earlier review",
                window,
                cx,
            )
        });
        panel.add_current_to_review(window, cx);
    });
    cx.condition(&panel, |panel, _| !panel.writing && panel.draft.is_some())
        .await;
    let operations = db.operations(&identity).unwrap();
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].state, ReviewOperationState::LocalDraft);
    panel.read_with(cx, |panel, cx| {
        assert!(panel.pending_review_is_older());
        assert_eq!(
            panel.body.read(cx).text(cx),
            "Preserve current-head text without mutating the earlier review"
        );
        assert!(panel.intent.is_some());
    });
}

#[test]
fn pending_comment_receipt_remains_private_until_canonical_readback() {
    let context = context();
    let remote = ReviewContext {
        reference: PullRequestReference {
            repository: RepositoryName {
                owner: "owner".into(),
                name: "repository".into(),
            },
            number: 7,
        },
        identity: context.identity().unwrap().clone(),
        head_oid: "2".repeat(40),
        base_tip_oid: "1".repeat(40),
    };
    let mut comment = CommentIntent {
        context: remote,
        anchor: CommentAnchor {
            path: "src/example.rs".into(),
            commit_oid: "2".repeat(40),
            side: crate::github_review::DiffSide::Right,
            line: Some(3),
            start_line: None,
            start_side: None,
            subject_type: crate::github_review::CommentSubject::Line,
        },
        body: "Private pending comment".into(),
        publication: CommentPublication::Immediate,
    };
    assert_eq!(
        pending_publication_status(&ReviewIntent::AddToReview {
            intent: comment.clone()
        }),
        "Pending review · only you"
    );
    assert_eq!(
        pending_publication_status(&ReviewIntent::Comment {
            intent: comment.clone()
        }),
        "Posted on GitHub"
    );
    comment.publication = CommentPublication::Pending {
        review_id: "PRR_existing".into(),
    };
    assert_eq!(
        pending_publication_status(&ReviewIntent::Comment { intent: comment }),
        "Pending review · only you"
    );
}

#[test]
fn github_review_location_labels_show_exact_side_and_normalized_ranges() {
    use crate::github_review::DiffSide::{Left, Right};
    assert_eq!(github_location(Right, None, Some(5)), "RIGHT 5");
    assert_eq!(github_location(Right, Some(5), Some(5)), "RIGHT 5");
    assert_eq!(github_location(Left, Some(4), Some(5)), "LEFT 4–5");
    assert_eq!(github_location(Right, Some(4), Some(5)), "RIGHT 4–5");
    assert_eq!(github_location(Left, None, None), "LEFT file");
}

struct AbsentResource;
impl ReviewTransport for AbsentResource {
    fn graphql(&self, query: &str, _variables: Value) -> Result<Value> {
        anyhow::ensure!(
            !query.starts_with("mutation"),
            "No mutation is allowed in reconciliation"
        );
        let pr = json!({"id":"PR_example","number":7,"headRefOid":"2".repeat(40),"baseRef":{"target":{"oid":"1".repeat(40)}},"repository":{"id":"R_example","nameWithOwner":"owner/repository"}});
        if query.contains("reviewThreads(first:") {
            let mut node = pr;
            node["reviewThreads"] = json!({"totalCount":0,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]});
            Ok(json!({"viewer":{"id":"U_viewer"},"node":node}))
        } else {
            Ok(json!({"viewer":{"id":"U_viewer"},"pullRequest":pr,"node":null}))
        }
    }
    fn rest_json(
        &self,
        _method: &str,
        _endpoint: &str,
        _input: Option<Value>,
    ) -> Result<Option<Value>> {
        anyhow::bail!("No mutation is allowed in reconciliation")
    }
}
#[test]
fn discarded_review_reconciles_from_complete_collection_without_deleted_node_lookup() -> Result<()>
{
    struct NoFurtherRequests;
    impl ReviewTransport for NoFurtherRequests {
        fn graphql(&self, _: &str, _: Value) -> Result<Value> {
            panic!("Complete review read-back must not query a deleted node")
        }
        fn rest_json(&self, _: &str, _: &str, _: Option<Value>) -> Result<Option<Value>> {
            panic!("Reconciliation never dispatches writes")
        }
    }
    let context = context();
    let remote = ReviewContext {
        reference: PullRequestReference::parse(&context.metadata.url(), None)?,
        identity: context.identity()?.clone(),
        head_oid: context.metadata.head_ref_oid.clone(),
        base_tip_oid: context.metadata.base_ref_oid.clone(),
    };
    let operation = ReviewOperation {
        operation_id: "discard-fixture".into(),
        revision: 1,
        context,
        kind: ReviewOperationKind::DeletePendingReview,
        payload: json!({}),
        location: ReviewLocationState::NotRequired,
        state: ReviewOperationState::DeliveryUnknown,
        before_server_ids: vec!["review:old".into()],
        server_ids: vec![],
        last_error: None,
        created_at_unix_seconds: 1,
        updated_at_unix_seconds: 1,
    };
    let service = ReviewService::new(&NoFurtherRequests);
    let intent = ReviewIntent::DeleteReview {
        review_id: "review:old".into(),
    };
    assert_eq!(
        reconciled_ids(&service, &remote, &operation, &intent, &[], &[])?,
        Some(Vec::new())
    );
    let present: RemoteReview = serde_json::from_value(
        json!({"id":"review:old","state":"PENDING","body":"Saved review","author":null,"commit":null,"url":"","viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true}),
    )?;
    assert_eq!(
        reconciled_ids(&service, &remote, &operation, &intent, &[], &[present])?,
        None
    );
    Ok(())
}

#[test]
fn github_review_delete_absence_receipt_reconciles_without_claiming_a_present_id() -> Result<()> {
    let context = context();
    let remote = ReviewContext {
        reference: PullRequestReference::parse(&context.metadata.url(), None)?,
        identity: context.identity()?.clone(),
        head_oid: context.metadata.head_ref_oid.clone(),
        base_tip_oid: context.metadata.base_ref_oid.clone(),
    };
    let service = ReviewService::new(&AbsentResource);
    let operation = ReviewOperation {
        operation_id: db::uuid::Uuid::now_v7().to_string(),
        revision: 1,
        context,
        kind: ReviewOperationKind::DeleteComment,
        payload: json!({}),
        location: ReviewLocationState::NotRequired,
        state: ReviewOperationState::DeliveryUnknown,
        before_server_ids: vec!["comment:old".into()],
        server_ids: Vec::new(),
        last_error: None,
        created_at_unix_seconds: 1,
        updated_at_unix_seconds: 1,
    };
    assert_eq!(
        reconciled_ids(
            &service,
            &remote,
            &operation,
            &ReviewIntent::Delete {
                comment_id: "comment:old".into()
            },
            &[],
            &[]
        )?,
        Some(Vec::new())
    );
    assert_eq!(
        reconciled_ids(
            &service,
            &remote,
            &operation,
            &ReviewIntent::DeleteReview {
                review_id: "review:old".into()
            },
            &[],
            &[]
        )?,
        Some(Vec::new())
    );
    assert_eq!(operation.state, ReviewOperationState::DeliveryUnknown);
    assert!(operation.server_ids.is_empty());
    Ok(())
}

#[gpui::test]
async fn recovery_archive_removes_cards_and_rejects_late_acknowledgements(cx: &mut TestAppContext) {
    let (_directory, db) = database(cx).await;
    let context = context();
    let identity = context.identity().unwrap().clone();
    let draft = db
        .create_operation(
            context.clone(),
            ReviewOperationKind::PostInlineComment,
            json!({"body":"Keep draft"}),
            ReviewLocationState::Ready,
        )
        .await
        .unwrap();
    let queued = db
        .queue_operation(draft.operation_id.clone(), draft.revision)
        .await
        .unwrap();
    db.archive_operation(queued.operation_id.clone(), queued.revision)
        .await
        .unwrap();
    let archived = db.archived_operation_ids(&identity).unwrap();
    let (panel, cx) =
        cx.add_window_view(|window, cx| GitHubReviewPanel::new(db, context, identity, window, cx));
    panel.update(cx, |panel, _| {
        panel.operations = vec![queued.clone()];
        panel.adopt_archived_operations(archived);
        panel.upsert_operation(queued);
        assert!(panel.operations.is_empty());
        assert_eq!(panel.pending_action_count(), 0);
        panel.restore_recovery_presentation((true, true, Some("Recovering…".into())));
        let state = panel.recovery_presentation();
        assert!(state.0 && state.1);
        assert_eq!(state.2.as_deref(), Some("Recovering…"));
    });
}
