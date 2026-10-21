use super::*;
use crate::github_pull_requests::{GitHubHttpError, RepositoryName};
use std::{cell::RefCell, collections::VecDeque};

struct FakeTransport {
    responses: RefCell<VecDeque<Result<Value>>>,
    rest: RefCell<VecDeque<Result<Option<Value>>>>,
    calls: RefCell<Vec<(String, Value)>>,
}
impl FakeTransport {
    fn new(responses: Vec<Result<Value>>) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
            rest: RefCell::new(VecDeque::new()),
            calls: RefCell::new(Vec::new()),
        }
    }
}
impl ReviewTransport for FakeTransport {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        self.calls.borrow_mut().push((query.into(), variables));
        self.responses
            .borrow_mut()
            .pop_front()
            .expect("Unexpected GraphQL request")
    }
    fn rest_json(
        &self,
        method: &str,
        endpoint: &str,
        input: Option<Value>,
    ) -> Result<Option<Value>> {
        self.calls
            .borrow_mut()
            .push((format!("{method} {endpoint}"), input.unwrap_or(Value::Null)));
        self.rest
            .borrow_mut()
            .pop_front()
            .expect("Unexpected REST request")
    }
}
fn context() -> ReviewContext {
    ReviewContext {
        reference: PullRequestReference {
            repository: RepositoryName::parse("example/repository").unwrap(),
            number: 42,
        },
        identity: PullRequestIdentity {
            host: "github.com".into(),
            repository_node_id: "repo_demo".into(),
            pull_request_node_id: "pr_demo".into(),
            viewer_node_id: "user_demo".into(),
        },
        head_oid: "a".repeat(40),
        base_tip_oid: "b".repeat(40),
    }
}
fn pr(context: &ReviewContext) -> Value {
    json!({"id":context.identity.pull_request_node_id,"number":context.reference.number,"headRefOid":context.head_oid,"baseRef":{"target":{"oid":context.base_tip_oid}},"repository":{"id":context.identity.repository_node_id,"nameWithOwner":context.reference.repository.full_name()},"author":{"nodeId":"other_user"}})
}
fn guarded(context: &ReviewContext) -> Value {
    json!({"viewer":{"id":context.identity.viewer_node_id},"node":pr(context)})
}
fn connection(items: Vec<Value>, total: usize, cursor: Option<&str>) -> Value {
    json!({"nodes":items,"totalCount":total,"pageInfo":{"hasNextPage":cursor.is_some(),"endCursor":cursor}})
}
fn comment(context: &ReviewContext, id: &str) -> Value {
    json!({"id":id,"fullDatabaseId":"100","state":"SUBMITTED","replyTo":null,"pullRequestReview":{"id":"review_demo","state":"COMMENTED"},"body":"Review fixture","url":"https://github.com/example/repository/pull/42","author":{"login":"reviewer","nodeId":context.identity.viewer_node_id},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","path":"src/example.rs","line":7,"startLine":null,"diffHunk":"@@ -1 +1 @@","commit":{"oid":context.head_oid},"originalCommit":{"oid":context.head_oid},"viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true})
}
fn thread(id: &str, comments: Value) -> Value {
    json!({"id":id,"path":"src/example.rs","line":7,"startLine":7,"originalLine":7,"originalStartLine":null,"diffSide":"RIGHT","startDiffSide":null,"subjectType":"LINE","isResolved":false,"isOutdated":false,"viewerCanReply":true,"viewerCanResolve":true,"viewerCanUnresolve":false,"comments":comments})
}
fn anchor(context: &ReviewContext) -> CommentAnchor {
    CommentAnchor {
        path: "src/example.rs".into(),
        commit_oid: context.head_oid.clone(),
        side: DiffSide::Right,
        line: Some(7),
        start_line: None,
        start_side: None,
        subject_type: CommentSubject::Line,
    }
}

#[test]
fn conversation_thread_page_finishes_nested_replies_without_loading_next_thread_page() {
    let context = context();
    let mut first = guarded(&context);
    first["node"]["reviewThreads"] = connection(
        vec![thread(
            "thread-1",
            connection(
                vec![comment(&context, "comment-1")],
                2,
                Some("replies-next"),
            ),
        )],
        21,
        Some("threads-next"),
    );
    let mut reply_page = guarded(&context);
    reply_page["pullRequest"] = pr(&context);
    reply_page["node"] = json!({"id":"thread-1","pullRequest":{"id":context.identity.pull_request_node_id},"comments":connection(vec![comment(&context,"comment-2")],2,None)});
    let transport = FakeTransport::new(vec![Ok(first), Ok(reply_page)]);
    let (threads, next) = ReviewService::new(&transport)
        .load_threads_page(&context, None)
        .unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].comments.len(), 2);
    assert_eq!(next.as_deref(), Some("threads-next"));
    assert_eq!(
        transport.calls.borrow().len(),
        2,
        "only nested reply continuation, no next thread page"
    );
}

#[test]
fn conversation_thread_page_rejects_repeated_continuation() {
    let context = context();
    let mut page = guarded(&context);
    page["node"]["reviewThreads"] = connection(Vec::new(), 30, Some("same"));
    let transport = FakeTransport::new(vec![Ok(page)]);
    assert!(
        ReviewService::new(&transport)
            .load_threads_page(&context, Some("same"))
            .is_err()
    );
}

#[test]
fn guard_rejects_other_account_repo_head_and_base_before_mutations() {
    let context = context();
    for pointer in [
        "/viewer/id",
        "/node/id",
        "/node/repository/id",
        "/node/headRefOid",
        "/node/baseRef/target/oid",
    ] {
        let mut value = guarded(&context);
        *value.pointer_mut(pointer).unwrap() = json!("unexpected");
        let transport = FakeTransport::new(vec![Ok(value)]);
        let result = ReviewService::new(&transport).create_pending_review(&context, "op-1");
        assert!(matches!(result, Err(ReviewWriteError::NotSent(_))));
        assert_eq!(transport.calls.borrow().len(), 1);
    }
}
#[test]
fn anchors_require_pinned_revision_and_valid_line_ranges() {
    let context = context();
    let valid = anchor(&context);
    valid.validate(&context).unwrap();
    for invalid in [
        CommentAnchor {
            line: Some(0),
            ..valid.clone()
        },
        CommentAnchor {
            start_line: Some(9),
            start_side: Some(DiffSide::Right),
            ..valid.clone()
        },
        CommentAnchor {
            commit_oid: "c".repeat(40),
            ..valid.clone()
        },
        CommentAnchor {
            path: "../example.rs".into(),
            ..valid.clone()
        },
        CommentAnchor {
            subject_type: CommentSubject::File,
            ..valid
        },
    ] {
        assert!(invalid.validate(&context).is_err());
    }
}
#[test]
fn thread_loader_fetches_nested_reply_pages_and_preserves_cached_comments() {
    let context = context();
    let mut first = guarded(&context);
    first["node"]["reviewThreads"] = connection(
        vec![thread(
            "thread_demo",
            connection(
                vec![comment(&context, "comment_one")],
                2,
                Some("reply-page-2"),
            ),
        )],
        1,
        None,
    );
    let second = json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(&context),"node":{"id":"thread_demo","pullRequest":{"id":context.identity.pull_request_node_id},"comments":connection(vec![comment(&context,"comment_two")],2,None)}});
    let transport = FakeTransport::new(vec![Ok(first), Ok(second)]);
    let loaded = ReviewService::new(&transport)
        .load_threads(&context)
        .unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].comments.len(), 2);
    assert_eq!(transport.calls.borrow()[1].1["cursor"], "reply-page-2");
    let cached: Vec<RemoteThread> =
        serde_json::from_value(serde_json::to_value(&loaded).unwrap()).unwrap();
    assert_eq!(cached[0].comments.len(), 2);
}
#[test]
fn pagination_rejects_repeated_cursors_and_incomplete_collections() {
    let mut pager = Pager::default();
    let page = connection(Vec::new(), 1, Some("same"));
    assert!(pager.advance(&page).unwrap());
    assert!(pager.advance(&page).is_err());
    let context = context();
    let mut response = guarded(&context);
    response["node"]["files"] = connection(
        vec![json!({"path":"src/example.rs","viewerViewedState":"UNVIEWED"})],
        2,
        None,
    );
    let transport = FakeTransport::new(vec![Ok(response)]);
    assert!(
        ReviewService::new(&transport)
            .load_viewed(&context)
            .is_err()
    );
}
#[test]
fn dispatched_errors_remain_unknown_without_automatic_replay() {
    let context = context();
    let mut reviews = guarded(&context);
    reviews["node"]["reviews"] = connection(Vec::new(), 0, None);
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(reviews),
        Err(anyhow::anyhow!("timeout")),
    ]);
    let error = ReviewService::new(&transport)
        .create_pending_review(&context, "op-unknown")
        .unwrap_err();
    assert_eq!(
        error,
        ReviewWriteError::DeliveryUnknown {
            operation_id: "op-unknown".into(),
            remote_id: None
        }
    );
    assert_eq!(transport.calls.borrow().len(), 3);
    assert!(transport.calls.borrow()[2].0.starts_with("mutation"));
}
#[test]
fn graphql_http_rejection_is_not_sent_and_ambiguous_failure_never_replays() {
    for status in [401, 403, 422, 200, 429, 503] {
        let transport = FakeTransport::new(vec![Err(GitHubHttpError::status(status).into())]);
        let error = ReviewService::new(&transport)
            .mutate(
                "op-http",
                "updatePullRequestReviewComment",
                "UpdatePullRequestReviewCommentInput",
                json!({}),
                "clientMutationId",
            )
            .unwrap_err();
        assert_eq!(
            matches!(error, ReviewWriteError::NotSent(_)),
            matches!(status, 401 | 403 | 422)
        );
        assert_eq!(
            transport.calls.borrow().len(),
            1,
            "Never replay ambiguous writes"
        );
    }
}
#[test]
fn authoritative_rest_rejection_is_not_sent_but_other_outcomes_stay_unknown() {
    let context = context();
    for status in [
        400, 401, 403, 404, 405, 410, 415, 422, 408, 409, 429, 500, 502, 503,
    ] {
        let mut root = thread(
            "thread_demo",
            connection(vec![comment(&context, "comment_root")], 1, None),
        );
        root["pullRequest"] = json!({"id":context.identity.pull_request_node_id});
        let lookup = json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(&context),"node":root});
        let transport = FakeTransport::new(vec![Ok(guarded(&context)), Ok(lookup)]);
        transport
            .rest
            .borrow_mut()
            .push_back(Err(GitHubHttpError::status(status).into()));
        let error = ReviewService::new(&transport)
            .reply(&context, "op-http", "thread_demo", None, "Review fixture")
            .unwrap_err();
        if matches!(status, 400 | 401 | 403 | 404 | 405 | 410 | 415 | 422) {
            assert!(
                matches!(error, ReviewWriteError::NotSent(_)),
                "status {status}"
            );
            assert!(error.to_string().contains(&status.to_string()));
        } else {
            assert!(
                matches!(error, ReviewWriteError::DeliveryUnknown { .. }),
                "status {status}"
            );
        }
        assert_eq!(transport.calls.borrow().len(), 3, "No automatic retry");
    }
}

#[test]
fn immediate_comment_uses_pinned_graphql_thread_without_retry_fallback() {
    let context = context();
    let mut before = guarded(&context);
    before["node"]["reviews"] = connection(Vec::new(), 0, None);
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(before),
        Err(anyhow::anyhow!("uncertain response")),
    ]);
    let mut anchor = anchor(&context);
    anchor.start_line = Some(3);
    anchor.start_side = Some(DiffSide::Right);
    let error = ReviewService::new(&transport)
        .post_comment(&context, "op-comment", &anchor, "Review fixture")
        .unwrap_err();
    assert!(matches!(error, ReviewWriteError::DeliveryUnknown { .. }));
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 3);
    assert!(calls[2].0.contains("AddPullRequestReviewInput"));
    let input = &calls[2].1["input"];
    assert_eq!(input["commitOID"], context.head_oid);
    assert_eq!(input["event"], "COMMENT");
    assert_eq!(input["threads"][0]["side"], "RIGHT");
    assert_eq!(input["threads"][0]["line"], 7);
    assert_eq!(input["threads"][0]["startLine"], 3);
    assert_eq!(input["threads"][0]["startSide"], "RIGHT");
    assert!(input["threads"][0].get("position").is_none());
    assert!(input["threads"][0].get("subjectType").is_none());
    assert!(!calls.iter().any(|call| call.0.starts_with("POST ")));
}
#[test]
fn immediate_graphql_comment_preserves_left_and_right_range_coordinates() {
    let context = context();
    for side in [DiffSide::Left, DiffSide::Right] {
        let mut before = guarded(&context);
        before["node"]["reviews"] = connection(Vec::new(), 0, None);
        let transport = FakeTransport::new(vec![
            Ok(guarded(&context)),
            Ok(before),
            Err(anyhow::anyhow!("uncertain response")),
        ]);
        let mut anchor = anchor(&context);
        anchor.side = side;
        anchor.start_line = Some(3);
        anchor.start_side = Some(side);
        assert!(matches!(
            ReviewService::new(&transport).post_comment(
                &context,
                "op-range",
                &anchor,
                "Review fixture"
            ),
            Err(ReviewWriteError::DeliveryUnknown { .. })
        ));
        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 3);
        let input = &calls[2].1["input"];
        assert_eq!(input["commitOID"], context.head_oid);
        assert_eq!(input["event"], "COMMENT");
        assert_eq!(input["threads"][0]["side"], json!(side));
        assert_eq!(input["threads"][0]["startSide"], json!(side));
        assert_eq!(input["threads"][0]["startLine"], 3);
        assert_eq!(input["threads"][0]["line"], 7);
        assert!(!calls.iter().any(|call| call.0.starts_with("POST ")
            || call.0.contains("submitPullRequestReview")
            || call.0.contains("deletePullRequestReview")));
    }
}
#[test]
fn immediate_comment_does_not_submit_existing_pending_or_guess_file_positioning() {
    let context = context();
    let mut before = guarded(&context);
    before["node"]["reviews"] = connection(
        vec![
            json!({"id":"pending_demo","state":"PENDING","body":"Saved draft","author":{"login":"reviewer","nodeId":context.identity.viewer_node_id},"commit":{"oid":context.head_oid},"url":"https://github.com/example/repository/pull/42","viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true}),
        ],
        1,
        None,
    );
    let transport = FakeTransport::new(vec![Ok(guarded(&context)), Ok(before)]);
    assert!(matches!(
        ReviewService::new(&transport).post_comment(
            &context,
            "op-pending",
            &anchor(&context),
            "Review fixture"
        ),
        Err(ReviewWriteError::NotSent(_))
    ));
    assert!(
        !transport
            .calls
            .borrow()
            .iter()
            .any(|call| call.0.starts_with("mutation"))
    );
    let mut file = anchor(&context);
    file.subject_type = CommentSubject::File;
    file.line = None;
    let transport = FakeTransport::new(vec![Ok(guarded(&context))]);
    assert!(matches!(
        ReviewService::new(&transport).post_comment(&context, "op-file", &file, "Review fixture"),
        Err(ReviewWriteError::NotSent(_))
    ));
    assert_eq!(transport.calls.borrow().len(), 1);
}
#[test]
fn immediate_comment_confirms_submitted_comment_and_exact_thread_anchor() {
    let context = context();
    let mut before = guarded(&context);
    before["node"]["reviews"] = connection(Vec::new(), 0, None);
    let payload = json!({"addPullRequestReview":{"pullRequestReview":{"id":"review_demo","state":"COMMENTED","comments":{"totalCount":1,"nodes":[{"id":"comment_demo"}]}}}});
    let mut posted = comment(&context, "comment_demo");
    posted["pullRequest"] = json!({"id":context.identity.pull_request_node_id});
    let lookup = json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(&context),"node":posted});
    let mut after = guarded(&context);
    after["node"]["reviewThreads"] = connection(
        vec![thread(
            "thread_demo",
            connection(vec![comment(&context, "comment_demo")], 1, None),
        )],
        1,
        None,
    );
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(before),
        Ok(payload),
        Ok(lookup),
        Ok(after),
    ]);
    let result = ReviewService::new(&transport)
        .post_comment(
            &context,
            "op-confirmed",
            &anchor(&context),
            "Review fixture",
        )
        .unwrap();
    assert_eq!(result.remote.state, "SUBMITTED");
    assert_eq!(result.remote.id, "comment_demo");
    assert_eq!(transport.calls.borrow().len(), 5);
}
#[test]
fn standalone_comment_rejects_malformed_response_identity_without_guessing() {
    for value in [
        json!({"id":0,"node_id":"comment_demo"}),
        json!({"id":100}),
        json!({"id":"bad","node_id":"comment_demo"}),
    ] {
        assert!(matches!(
            rest_comment_identity(&value, "op"),
            Err(ReviewWriteError::DeliveryUnknown { .. })
        ));
    }
    assert_eq!(
        rest_comment_identity(&json!({"id":"100","node_id":"comment_demo"}), "op").unwrap(),
        ("comment_demo".into(), "100".into())
    );
}
#[test]
fn standalone_review_refuses_to_publish_an_existing_pending_review() {
    let context = context();
    let mut before = guarded(&context);
    before["node"]["reviews"] = connection(
        vec![
            json!({"id":"pending_demo","state":"PENDING","body":"Saved draft","author":{"login":"reviewer","nodeId":context.identity.viewer_node_id},"commit":{"oid":context.head_oid},"url":"https://github.com/example/repository/pull/42","viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true}),
        ],
        1,
        None,
    );
    let transport = FakeTransport::new(vec![Ok(guarded(&context)), Ok(before)]);
    assert!(matches!(
        ReviewService::new(&transport).post_review(
            &context,
            "op-review",
            ReviewEvent::Comment,
            "Review fixture"
        ),
        Err(ReviewWriteError::NotSent(_))
    ));
    assert_eq!(transport.calls.borrow().len(), 2);
    assert!(
        !transport
            .calls
            .borrow()
            .iter()
            .any(|call| call.0.starts_with("mutation") || call.0.starts_with("POST"))
    );
}
#[test]
fn standalone_review_publishes_new_pinned_review_without_draft_lifecycle() {
    let context = context();
    let mut before = guarded(&context);
    before["node"]["reviews"] = connection(Vec::new(), 0, None);
    let review = json!({"id":"review_new","state":"COMMENTED","body":"Review fixture","author":{"login":"reviewer","nodeId":context.identity.viewer_node_id},"commit":{"oid":context.head_oid},"url":"https://github.com/example/repository/pull/42","viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true,"pullRequest":{"id":context.identity.pull_request_node_id}});
    let lookup = json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(&context),"node":review});
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(before),
        Ok(json!({"addPullRequestReview":{"pullRequestReview":{"id":"review_new"}}})),
        Ok(lookup),
    ]);
    let receipt = ReviewService::new(&transport)
        .post_review(
            &context,
            "op-review",
            ReviewEvent::Comment,
            "Review fixture",
        )
        .unwrap();
    assert_eq!(receipt.remote.id, "review_new");
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 4);
    assert!(calls[2].0.contains("AddPullRequestReviewInput"));
    assert_eq!(calls[2].1["input"]["commitOID"], context.head_oid);
    assert_eq!(calls[2].1["input"]["event"], "COMMENT");
    assert!(calls[2].1["input"].get("threads").is_none());
    assert!(
        !calls
            .iter()
            .any(|call| call.0.contains("submitPullRequestReview")
                || call.0.contains("deletePullRequestReview"))
    );
}
#[test]
fn immediate_reply_targets_root_comment_instead_of_pending_review() {
    let context = context();
    let mut root = thread(
        "thread_demo",
        connection(vec![comment(&context, "comment_root")], 1, None),
    );
    root["pullRequest"] = json!({"id":context.identity.pull_request_node_id});
    let lookup = json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(&context),"node":root});
    let transport = FakeTransport::new(vec![Ok(guarded(&context)), Ok(lookup)]);
    transport
        .rest
        .borrow_mut()
        .push_back(Err(anyhow::anyhow!("uncertain response")));
    assert!(matches!(
        ReviewService::new(&transport).reply(
            &context,
            "op-reply",
            "thread_demo",
            None,
            "Review fixture"
        ),
        Err(ReviewWriteError::DeliveryUnknown { .. })
    ));
    let calls = transport.calls.borrow();
    assert_eq!(
        calls[2].0,
        "POST repos/example/repository/pulls/42/comments/100/replies"
    );
    assert_eq!(calls[2].1, json!({"body":"Review fixture"}));
}

#[test]
fn viewed_mutation_uses_documented_input_and_independent_readback() {
    let context = context();
    let mut before = guarded(&context);
    before["node"]["files"] = connection(
        vec![json!({"path":"src/example.rs","viewerViewedState":"UNVIEWED"})],
        1,
        None,
    );
    let mut after = guarded(&context);
    after["node"]["files"] = connection(
        vec![json!({"path":"src/example.rs","viewerViewedState":"VIEWED"})],
        1,
        None,
    );
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(before),
        Ok(
            json!({"markFileAsViewed":{"pullRequest":{"id":context.identity.pull_request_node_id}}}),
        ),
        Ok(after),
    ]);
    let result = ReviewService::new(&transport)
        .set_viewed(&context, "op-viewed", "src/example.rs", true)
        .unwrap();
    assert_eq!(result.remote.viewer_viewed_state, ViewedState::Viewed);
    let calls = transport.calls.borrow();
    assert!(calls[2].0.contains("MarkFileAsViewedInput"));
    assert_eq!(calls[2].1["input"]["path"], "src/example.rs");
    assert!(calls[2].1["input"].get("expectedHeadOid").is_none());
    assert_eq!(calls.len(), 4);
}
#[test]
fn folder_viewed_chunks_are_bounded_and_partial_delivery_stays_unknown() {
    let context = context();
    let paths: Vec<String> = (0..21)
        .map(|index| format!("docs/item-{index}.md"))
        .collect();
    let mut before = guarded(&context);
    before["node"]["files"] = connection(
        paths
            .iter()
            .map(|path| json!({"path":path,"viewerViewedState":"UNVIEWED"}))
            .collect(),
        paths.len(),
        None,
    );
    let mut first = serde_json::Map::new();
    for index in 0..20 {
        first.insert(
            format!("file{index}"),
            json!({"pullRequest":{"id":context.identity.pull_request_node_id}}),
        );
    }
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(before),
        Ok(guarded(&context)),
        Ok(Value::Object(first)),
        Ok(guarded(&context)),
        Err(anyhow::anyhow!("uncertain second chunk")),
    ]);
    assert!(matches!(
        ReviewService::new(&transport).set_viewed_batch(&context, "op-folder", &paths, true),
        Err(ReviewWriteError::DeliveryUnknown { .. })
    ));
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[3].1.as_object().unwrap().len(), 20);
    assert_eq!(calls[5].1.as_object().unwrap().len(), 1);
}

#[test]
fn reconciliation_excludes_wrong_side_old_ids_and_ambiguous_matches() {
    let context = context();
    let intent = CommentIntent {
        context: context.clone(),
        anchor: anchor(&context),
        body: "Review fixture".into(),
        publication: CommentPublication::Immediate,
    };
    let mut row = decode_thread(&thread("thread_demo", Value::Null)).unwrap();
    row.comments = vec![decode(comment(&context, "comment_one")).unwrap()];
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row.clone()]),
        CommentReconciliation::Confirmed(_)
    ));
    assert!(matches!(
        reconcile_comment(
            &intent,
            &HashSet::from_iter(["comment_one".into()]),
            &[row.clone()]
        ),
        CommentReconciliation::NotObserved
    ));
    row.diff_side = DiffSide::Left;
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row.clone()]),
        CommentReconciliation::NotObserved
    ));
    row.diff_side = DiffSide::Right;
    row.comments
        .push(decode(comment(&context, "comment_two")).unwrap());
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row]),
        CommentReconciliation::Ambiguous
    ));
}
#[test]
fn reconciliation_requires_publication_mode_and_handles_concurrent_submission() {
    let context = context();
    let mut intent = CommentIntent {
        context: context.clone(),
        anchor: anchor(&context),
        body: "Review fixture".into(),
        publication: CommentPublication::Immediate,
    };
    let mut row = decode_thread(&thread("thread_demo", Value::Null)).unwrap();
    let mut draft: RemoteComment = decode(comment(&context, "draft_demo")).unwrap();
    draft.state = "PENDING".into();
    draft.pull_request_review = Some(CommentReview {
        id: "pending_demo".into(),
        state: "PENDING".into(),
    });
    row.comments = vec![draft];
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row.clone()]),
        CommentReconciliation::NotObserved
    ));
    intent.publication = CommentPublication::Pending {
        review_id: "different_review".into(),
    };
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row.clone()]),
        CommentReconciliation::NotObserved
    ));
    intent.publication = CommentPublication::Pending {
        review_id: "pending_demo".into(),
    };
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row.clone()]),
        CommentReconciliation::Confirmed(_)
    ));
    // A review can be submitted on GitHub while a local draft POST times out.
    row.comments[0].state = "SUBMITTED".into();
    row.comments[0].pull_request_review.as_mut().unwrap().state = "COMMENTED".into();
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row.clone()]),
        CommentReconciliation::SubmittedPendingReview(_)
    ));
    // An inconsistent transition must be refreshed, not confirmed or retried.
    row.comments[0].state = "PENDING".into();
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row]),
        CommentReconciliation::NotObserved
    ));
    let mut encoded = serde_json::to_value(&intent).unwrap();
    encoded.as_object_mut().unwrap().remove("publication");
    assert!(serde_json::from_value::<CommentIntent>(encoded).is_err());
}
#[test]
fn single_line_normalization_keeps_real_ranges_and_sides_precise() {
    let context = context();
    let single = anchor(&context);
    let mut row = decode_thread(&thread("thread_demo", Value::Null)).unwrap();
    row.comments = vec![decode(comment(&context, "comment_demo")).unwrap()];
    // Observed GitHub shape: thread.startLine=end; comment.startLine=null.
    assert_eq!(row.start_line, Some(7));
    assert!(row.comments[0].start_line.is_none());
    assert!(thread_matches_anchor(&row, &single));
    let intent = CommentIntent {
        context: context.clone(),
        anchor: single.clone(),
        body: "Review fixture".into(),
        publication: CommentPublication::Immediate,
    };
    assert!(matches!(
        reconcile_comment(&intent, &HashSet::new(), &[row.clone()]),
        CommentReconciliation::Confirmed(_)
    ));
    row.start_line = None;
    assert!(thread_matches_anchor(&row, &single));
    row.start_line = Some(7);
    row.start_diff_side = Some(DiffSide::Right);
    assert!(thread_matches_anchor(&row, &single));
    row.start_diff_side = Some(DiffSide::Left);
    assert!(!thread_matches_anchor(&row, &single));
    row.start_diff_side = None;
    let degenerate = CommentAnchor {
        start_line: Some(7),
        start_side: Some(DiffSide::Right),
        ..single.clone()
    };
    assert!(thread_matches_anchor(&row, &degenerate));
    assert!(comment_matches(
        &row.comments[0],
        &context,
        &degenerate,
        "Review fixture"
    ));
    // A true two-line range never collapses to a single line.
    let range = CommentAnchor {
        start_line: Some(6),
        start_side: Some(DiffSide::Right),
        ..single.clone()
    };
    assert!(!thread_matches_anchor(&row, &range));
    row.start_line = Some(6);
    row.start_diff_side = Some(DiffSide::Right);
    assert!(thread_matches_anchor(&row, &range));
    assert!(!thread_matches_anchor(&row, &single));
    row.start_diff_side = None;
    assert!(!thread_matches_anchor(&row, &range));
    row.start_diff_side = Some(DiffSide::Left);
    assert!(!thread_matches_anchor(&row, &range));
    row.start_diff_side = Some(DiffSide::Right);
    row.start_line = Some(5);
    assert!(!thread_matches_anchor(&row, &range));
    row.start_line = Some(6);
    row.diff_side = DiffSide::Left;
    assert!(!thread_matches_anchor(&row, &range));
}
fn owned_comment_lookup(context: &ReviewContext, id: &str) -> Value {
    let mut target = comment(context, id);
    target["pullRequest"] = json!({"id":context.identity.pull_request_node_id});
    json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(context),"node":target})
}
fn thread_collection(context: &ReviewContext, items: Vec<Value>, total: usize) -> Value {
    let mut value = guarded(context);
    value["node"]["reviewThreads"] = connection(items, total, None);
    value
}
#[test]
fn deleting_pending_review_confirms_guarded_collection_absence() {
    let context = context();
    let pending = json!({"id":"review_pending","state":"PENDING","body":"Saved draft","author":{"login":"reviewer","nodeId":context.identity.viewer_node_id},"commit":{"oid":context.head_oid},"url":"","viewerCanUpdate":true,"viewerCanDelete":true,"viewerDidAuthor":true,"pullRequest":{"id":context.identity.pull_request_node_id}});
    let lookup = json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(&context),"node":pending});
    let mut absent = guarded(&context);
    absent["node"]["reviews"] = connection(Vec::new(), 0, None);
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(lookup),
        Ok(json!({"deletePullRequestReview":{"clientMutationId":"op-discard"}})),
        Ok(absent),
    ]);
    ReviewService::new(&transport)
        .delete_pending_review(&context, "op-discard", "review_pending")
        .unwrap();
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 4);
    assert!(calls[3].0.contains("reviews(first:"));
    assert!(
        calls[3].1.get("id").is_none(),
        "Deleted review node must not be queried"
    );
}

#[test]
fn deleting_last_comment_confirms_absence_without_querying_deleted_node() {
    let context = context();
    let transport = FakeTransport::new(vec![
        Ok(guarded(&context)),
        Ok(owned_comment_lookup(&context, "comment_demo")),
        Ok(json!({"deletePullRequestReviewComment":{"clientMutationId":"op-delete"}})),
        Ok(thread_collection(&context, vec![], 0)),
    ]);
    ReviewService::new(&transport)
        .delete_comment(&context, "op-delete", "comment_demo")
        .unwrap();
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 4);
    assert!(calls[3].0.contains("reviewThreads(first:50"));
    assert!(!calls[3].0.contains("node(id:$id)"));
}
#[test]
fn delete_ack_does_not_turn_failed_foreign_or_incomplete_readback_into_success() {
    let context = context();
    let mut foreign = thread_collection(&context, vec![], 0);
    foreign["viewer"]["id"] = json!("other_user");
    for readback in [
        Err(anyhow::anyhow!("permission denied")),
        Err(anyhow::anyhow!("network unavailable")),
        Ok(foreign),
        Ok(thread_collection(&context, vec![], 1)),
        Ok(thread_collection(
            &context,
            vec![thread(
                "thread_demo",
                connection(vec![comment(&context, "comment_demo")], 1, None),
            )],
            1,
        )),
    ] {
        let transport = FakeTransport::new(vec![
            Ok(guarded(&context)),
            Ok(owned_comment_lookup(&context, "comment_demo")),
            Ok(json!({"deletePullRequestReviewComment":{"clientMutationId":"op-delete"}})),
            readback,
        ]);
        assert!(matches!(
            ReviewService::new(&transport).delete_comment(&context, "op-delete", "comment_demo"),
            Err(ReviewWriteError::DeliveryUnknown { .. })
        ));
    }
}
#[test]
fn absence_proof_includes_comments_on_later_pages() {
    let context = context();
    let first = thread_collection(
        &context,
        vec![thread(
            "thread_demo",
            connection(
                vec![comment(&context, "comment_one")],
                2,
                Some("next-comments"),
            ),
        )],
        1,
    );
    let second = json!({"viewer":{"id":context.identity.viewer_node_id},"pullRequest":pr(&context),"node":{"id":"thread_demo","pullRequest":{"id":context.identity.pull_request_node_id},"comments":connection(vec![comment(&context,"comment_target")],2,None)}});
    let transport = FakeTransport::new(vec![Ok(first), Ok(second)]);
    assert!(
        !ReviewService::new(&transport)
            .comment_is_absent(&context, "comment_target")
            .unwrap()
    );
    assert_eq!(transport.calls.borrow().len(), 2);
    assert_eq!(transport.calls.borrow()[1].1["cursor"], "next-comments");
}
#[cfg(unix)]
#[test]
#[ignore = "Writes GitHub comments; requires explicit ZED_LIVE_REVIEW_CONFIG"]
fn production_review_service_roundtrip() -> Result<()> {
    use std::{
        fs::OpenOptions,
        io::Write as _,
        os::unix::fs::OpenOptionsExt as _,
        time::{SystemTime, UNIX_EPOCH},
    };
    #[derive(Deserialize)]
    struct LiveConfig {
        context: ReviewContext,
        anchor: CommentAnchor,
        body: String,
        blocked_context: Option<ReviewContext>,
        blocked_anchor: Option<CommentAnchor>,
    }
    let config_path = std::path::PathBuf::from(
        std::env::var("ZED_LIVE_REVIEW_CONFIG")
            .context("Explicit live-test configuration is required")?,
    );
    let config: LiveConfig = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    config.context.validate()?;
    config.anchor.validate(&config.context)?;
    ensure!(
        config.body.starts_with("zed-review-live-") && config.body.len() >= 40,
        "Use a unique zed-review-live- marker as the comment body"
    );
    ensure!(
        config.blocked_context.is_some() == config.blocked_anchor.is_some(),
        "Both blocked_context and blocked_anchor are required together"
    );
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let ledger_path = config_path.with_extension(format!("roundtrip-{nonce}.jsonl"));
    let mut ledger = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&ledger_path)?;
    let mut record = |value: Value| -> Result<()> {
        serde_json::to_writer(&mut ledger, &value)?;
        ledger.write_all(b"\n")?;
        ledger.sync_all()?;
        Ok(())
    };
    let client = GitHubClient::default();
    let service = ReviewService::new(&client);
    let before = service.load_threads(&config.context)?;
    let before_threads: HashSet<String> = before.iter().map(|thread| thread.id.clone()).collect();
    let before_comments: HashSet<String> = before
        .iter()
        .flat_map(|thread| thread.comments.iter().map(|comment| comment.id.clone()))
        .collect();
    let pending_ids = |context: &ReviewContext| -> Result<HashSet<String>> {
        Ok(service
            .load_reviews(context)?
            .into_iter()
            .filter(|review| {
                review.state == "PENDING"
                    && actor_id(&review.author) == Some(context.identity.viewer_node_id.as_str())
            })
            .map(|review| review.id)
            .collect())
    };
    let before_pending = pending_ids(&config.context)?;
    ensure!(
        before_pending.is_empty(),
        "Roundtrip target must have no existing pending review"
    );
    // Reusing a marker would make uncertain-delivery reconciliation ambiguous.
    ensure!(
        !before
            .iter()
            .flat_map(|thread| &thread.comments)
            .any(|comment| comment.body.starts_with(&config.body)),
        "Marker already exists on the target PR"
    );
    record(
        json!({"phase":"baseline","thread_ids":before_threads,"comment_ids":before_comments,"pending_ids":before_pending}),
    )?;
    let mut created = Vec::<String>::new();
    let outcome = (|| -> Result<()> {
        if let (Some(context), Some(anchor)) = (&config.blocked_context, &config.blocked_anchor) {
            context.validate()?;
            anchor.validate(context)?;
            let saved_pending = pending_ids(context)?;
            ensure!(
                !saved_pending.is_empty(),
                "Blocked target must contain an owned pending review"
            );
            let saved_threads = service.load_threads(context)?;
            let saved_ids: HashSet<_> = saved_threads
                .iter()
                .flat_map(|thread| &thread.comments)
                .map(|comment| comment.id.clone())
                .collect();
            let result = service.post_comment(
                context,
                &format!("live-blocked-{nonce}"),
                anchor,
                &config.body,
            );
            record(json!({"phase":"blocked","result":format!("{result:?}")}))?;
            ensure!(
                matches!(result, Err(ReviewWriteError::NotSent(_))),
                "Existing pending review must refuse publication before dispatch"
            );
            ensure!(
                pending_ids(context)? == saved_pending,
                "Blocked pending review identities changed"
            );
            let after_ids: HashSet<_> = service
                .load_threads(context)?
                .iter()
                .flat_map(|thread| &thread.comments)
                .map(|comment| comment.id.clone())
                .collect();
            ensure!(after_ids == saved_ids, "Blocked target comments changed");
        }
        let posted = service.post_comment(
            &config.context,
            &format!("live-post-{nonce}"),
            &config.anchor,
            &config.body,
        );
        if let Ok(receipt) = &posted {
            created.push(receipt.remote.id.clone());
        }
        record(json!({"phase":"post","created_ids":created,"result":format!("{posted:?}")}))?;
        let posted = posted?;
        let current = service
            .load_comment(&config.context, &posted.remote.id)?
            .context("Posted comment missing")?;
        ensure!(
            current.body == config.body
                && comment_matches(&current, &config.context, &config.anchor, &config.body),
            "Posted comment readback differs"
        );
        let threads = service.load_threads(&config.context)?;
        let thread = threads
            .iter()
            .find(|thread| {
                thread
                    .comments
                    .iter()
                    .any(|comment| comment.id == posted.remote.id)
            })
            .context("Posted thread missing")?;
        ensure!(
            !before_threads.contains(&thread.id),
            "Standalone comment unexpectedly joined an existing thread"
        );
        let reply_body = format!("{} reply", config.body);
        let reply = service.reply(
            &config.context,
            &format!("live-reply-{nonce}"),
            &thread.id,
            None,
            &reply_body,
        );
        if let Ok(receipt) = &reply {
            created.push(receipt.remote.id.clone());
        }
        record(json!({"phase":"reply","created_ids":created,"result":format!("{reply:?}")}))?;
        let reply = reply?;
        let reread = service
            .load_comment(&config.context, &reply.remote.id)?
            .context("Reply missing")?;
        ensure!(
            reread.body == reply_body
                && reread.reply_to.as_ref().map(|node| &node.id) == Some(&posted.remote.id),
            "Reply readback differs"
        );
        let edited_body = format!("{} edited", config.body);
        let edited = service.edit_comment(
            &config.context,
            &format!("live-edit-{nonce}"),
            &posted.remote.id,
            &edited_body,
        )?;
        ensure!(
            edited.remote.id == posted.remote.id,
            "Edit returned another comment"
        );
        ensure!(
            service
                .load_comment(&config.context, &posted.remote.id)?
                .context("Edited comment missing")?
                .body
                == edited_body,
            "Edit readback differs"
        );
        record(json!({"phase":"edit","created_ids":created}))?;
        Ok(())
    })();
    let mut cleanup_errors = Vec::new();
    // Only identities returned by our successful writes are eligible. Never
    // guess an uncertain response ID or remove the enclosing review.
    for (index, id) in created.iter().rev().enumerate() {
        let result = (|| -> Result<()> {
            if service.comment_is_absent(&config.context, id)? {
                return Ok(());
            }
            let comment = service
                .load_comment(&config.context, id)?
                .context("Cleanup comment unavailable")?;
            ensure!(
                comment.viewer_did_author
                    && actor_id(&comment.author)
                        == Some(config.context.identity.viewer_node_id.as_str())
                    && comment.body.starts_with(&config.body)
                    && !before_comments.contains(id),
                "Cleanup ownership cannot be proven"
            );
            service.delete_comment(&config.context, &format!("live-clean-{nonce}-{index}"), id)?;
            ensure!(
                service.comment_is_absent(&config.context, id)?,
                "Deleted comment still present"
            );
            Ok(())
        })();
        if let Err(error) = result {
            cleanup_errors.push(format!("{id}: {error:#}"));
        }
    }
    let preservation = (|| -> Result<()> {
        let after = service.load_threads(&config.context)?;
        let threads: HashSet<_> = after.iter().map(|thread| thread.id.clone()).collect();
        let comments: HashSet<_> = after
            .iter()
            .flat_map(|thread| &thread.comments)
            .map(|comment| comment.id.clone())
            .collect();
        ensure!(
            before_threads.is_subset(&threads) && before_comments.is_subset(&comments),
            "Existing thread/comment identity was lost"
        );
        ensure!(
            pending_ids(&config.context)? == before_pending,
            "Existing pending review identities changed"
        );
        Ok(())
    })();
    record(
        json!({"phase":"complete","created_ids":created,"outcome":format!("{outcome:?}"),"cleanup_errors":cleanup_errors,"preservation":format!("{preservation:?}")}),
    )?;
    eprintln!("Live roundtrip evidence: {}", ledger_path.display());
    ensure!(
        cleanup_errors.is_empty(),
        "Owned comment cleanup incomplete: {cleanup_errors:?}; inspect the private ledger"
    );
    preservation?;
    outcome
}
