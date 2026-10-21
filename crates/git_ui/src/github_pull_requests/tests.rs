fn activity_context() -> crate::github_review::ReviewContext {
    crate::github_review::ReviewContext {
        reference: super::PullRequestReference::parse(
            "https://github.com/example/repository/pull/42",
            None,
        )
        .unwrap(),
        identity: crate::review::PullRequestIdentity {
            host: "github.com".into(),
            repository_node_id: "repo_demo".into(),
            pull_request_node_id: "pr_demo".into(),
            viewer_node_id: "viewer".into(),
        },
        head_oid: "a".repeat(40),
        base_tip_oid: "b".repeat(40),
    }
}
#[test]
fn conversation_pagination_stops_each_exhausted_source_independently() {
    let mut cursor = ConversationCursor::default();
    assert!(cursor.has_more());
    cursor.comments = None;
    cursor.reviews = None;
    assert!(cursor.has_more(), "threads still have their first page");
    cursor.threads = Some("next-thread-page".into());
    assert!(cursor.has_more());
    cursor.threads_complete = true;
    assert!(!cursor.has_more());
}

#[test]
fn activity_uses_semantic_ids_and_preserves_same_timestamp_events() {
    use super::*;
    let event = |id, name| json!({"event":name,"id":id,"actor":{"login":"reviewer"},"created_at":"2026-01-01T00:00:00Z"});
    let items = assemble_activity(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![
            event(1, "closed"),
            event(2, "reopened"),
            event(1, "closed"),
            event(3, "reviewed"),
        ],
        &activity_context(),
    );
    assert_eq!(items.len(), 2);
    assert_ne!(items[0].id, items[1].id);
    let commit = json!({"event":"committed","sha":"abc","node_id":"commit-node","committer":{"date":"2026-01-01T00:00:00Z"}});
    assert_eq!(timeline_identity(&commit).unwrap(), "committed:commit-node");
    let cross = json!({"event":"cross-referenced","source":{"issue":{"node_id":"issue"}},"actor":{"login":"author"},"created_at":"date"});
    assert!(
        timeline_identity(&cross)
            .unwrap()
            .contains("issue:author:date")
    );
    assert!(timeline_identity(&json!({"event":"cross-referenced","created_at":"date"})).is_err());
}

#[test]
fn activity_attaches_thread_by_review_id_and_keeps_pending_private() {
    use super::*;
    let review = |node: &str, author: &str, state: &str| PullRequestReview {
        id: 1,
        node_id: node.into(),
        body: None,
        html_url: String::new(),
        user: Some(GitHubUser {
            node_id: Some(author.into()),
            login: author.into(),
            name: None,
            url: None,
        }),
        state: state.into(),
        submitted_at: Some("2026-01-01T00:00:00Z".into()),
        commit_id: String::new(),
    };
    let thread: crate::github_review::RemoteThread = serde_json::from_value(json!({
        "id":"thread","path":"src/example.rs","line":7,"startLine":null,"originalLine":7,"originalStartLine":null,
        "diffSide":"RIGHT","startDiffSide":null,"subjectType":"LINE","isResolved":true,"isOutdated":false,
        "viewerCanReply":true,"viewerCanResolve":false,"viewerCanUnresolve":true,
        "comments":[{"id":"inline","state":"SUBMITTED","pullRequestReview":{"id":"review","state":"COMMENTED"},
            "body":"A substantive discussion","url":"https://github.com/example/example/pull/1#discussion_r1",
            "author":{"login":"author","nodeId":"author"},"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z",
            "path":"src/example.rs","line":7,"startLine":null,"diffHunk":"@@ -1 +1 @@","commit":null,"originalCommit":null,
            "viewerCanUpdate":false,"viewerCanDelete":false,"viewerDidAuthor":false}]
    })).unwrap();
    let items = assemble_activity(
        Vec::new(),
        vec![
            review("review", "author", "DISMISSED"),
            review("private", "other", "PENDING"),
            review("own", "viewer", "PENDING"),
        ],
        vec![thread.clone()],
        Vec::new(),
        &activity_context(),
    );
    assert_eq!(items.len(), 2);
    let parent = items.iter().find(|i| i.id == "review").unwrap();
    assert_eq!(parent.action_label, "DISMISSED");
    let anchor = parent.children[0].anchor.as_ref().unwrap();
    assert_eq!(anchor.path, "src/example.rs");
    assert_eq!(anchor.line, Some(7));
    assert_eq!(anchor.side, crate::github_review::DiffSide::Right);
    assert_eq!(anchor.head_oid, "a".repeat(40));
    assert_eq!(parent.children[0].children[0].anchor.as_ref(), Some(anchor));
    assert_eq!(
        parent.children[0].children[0].body,
        "A substantive discussion"
    );
    assert_eq!(
        parent.children[0].location.as_deref(),
        Some("src/example.rs:7")
    );
    let mut snapshot = ConversationPage::default();
    let first_threads = (0..20)
        .map(|index| {
            let mut thread = thread.clone();
            thread.id = format!("thread-{index}");
            thread.comments[0].id = format!("inline-{index}");
            if index == 0 {
                thread.comments[0].pull_request_review.as_mut().unwrap().id = "late-review".into();
            }
            thread
        })
        .collect();
    let first = snapshot.append(
        ConversationPage {
            reviews: (0..20)
                .map(|index| {
                    review(
                        &if index == 0 {
                            "review".into()
                        } else {
                            format!("other-review-{index}")
                        },
                        "author",
                        "COMMENTED",
                    )
                })
                .collect(),
            threads: first_threads,
            ..Default::default()
        },
        &activity_context(),
    );
    assert!(
        first.iter().any(|entry| entry.id == "thread-0"),
        "a parent not loaded yet remains visible"
    );
    let mut later_thread = thread;
    later_thread.id = "thread-20".into();
    later_thread.comments[0].id = "inline-20".into();
    let complete = snapshot.append(
        ConversationPage {
            comments: vec![ConversationComment {
                id: 99,
                node_id: "early-comment".into(),
                body: "Earlier discussion".into(),
                html_url: String::new(),
                user: None,
                created_at: "2025-01-01T00:00:00Z".into(),
                updated_at: String::new(),
            }],
            reviews: vec![review("late-review", "author", "COMMENTED")],
            threads: vec![later_thread],
        },
        &activity_context(),
    );
    assert_eq!(
        complete.first().unwrap().id,
        "early-comment",
        "the loaded prefix is globally chronological"
    );
    let original = complete.iter().find(|entry| entry.id == "review").unwrap();
    assert_eq!(
        original.children.len(),
        20,
        "page-two thread attaches to its page-one review without losing earlier threads"
    );
    let late = complete
        .iter()
        .find(|entry| entry.id == "late-review")
        .unwrap();
    assert_eq!(
        late.children.len(),
        1,
        "page-one thread reparents when its review arrives later"
    );
    assert!(
        !complete.iter().any(|entry| entry.id.starts_with("thread-")),
        "no orphan or duplicate top-level threads remain"
    );
    assert_eq!(
        original
            .children
            .iter()
            .map(|thread| thread.children.len())
            .sum::<usize>()
            + late.children[0].children.len(),
        21,
        "all nested comments survive pagination"
    );
    assert!(!items.iter().any(|i| i.id == "private"));
    assert!(
        items
            .iter()
            .find(|i| i.id == "own")
            .unwrap()
            .action_label
            .contains("only you")
    );
}
use super::*;
#[test]
fn comparison_pagination_preserves_more_than_two_thousand_commits() {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for start in (0..2001).step_by(100) {
        let end = (start + 100).min(2001);
        let commits = (start..end).map(|index| json!({"sha":format!("{index:040x}"),"commit":{"message":format!("Change {index}\n\nBody"),"author":{"name":"Contributor"},"committer":{"date":"2026-01-01T00:00:00Z"}}})).collect::<Vec<_>>();
        let complete = append_comparison_commits(
            &json!({"total_commits":2001,"commits":commits}),
            2001,
            &mut seen,
            &mut result,
        )
        .unwrap();
        assert_eq!(complete, end == 2001);
    }
    assert_eq!(result.len(), 2001);
    assert_eq!(result.last().unwrap().message_headline, "Change 2000");
    assert!(
        append_comparison_commits(
            &json!({"total_commits":2002,"commits":[]}),
            2001,
            &mut seen,
            &mut result
        )
        .is_err()
    );
    let mut empty = Vec::new();
    assert!(
        append_comparison_commits(
            &json!({"total_commits":2001,"commits":[]}),
            2001,
            &mut HashSet::new(),
            &mut empty
        )
        .is_err()
    );
}

#[test]
fn complete_connections_reject_missing_and_repeated_cursors() {
    let mut seen = HashSet::new();
    let page = json!({"pageInfo":{"hasNextPage":true,"endCursor":"page-two"}});
    assert_eq!(
        collection_cursor(&page, &mut seen).unwrap().as_deref(),
        Some("page-two")
    );
    assert!(collection_cursor(&page, &mut seen).is_err());
    assert!(collection_cursor(&json!({"pageInfo":{"hasNextPage":true}}), &mut seen).is_err());
    assert!(collection_cursor(&json!({"pageInfo":{}}), &mut seen).is_err());
    assert_eq!(
        collection_cursor(&json!({"pageInfo":{"hasNextPage":false}}), &mut seen).unwrap(),
        None
    );
}

#[test]
fn dropping_request_guard_prevents_any_subprocess_or_following_page() {
    let (client, guard) = GitHubClient::cancellable();
    drop(guard);
    assert!(
        client
            .viewer()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(
        client
            .rest_pages::<ConversationComment>("unused")
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}

#[test]
fn references_are_explicit_and_reject_untrusted_hosts_paths_and_numbers() {
    let repo = RepositoryName::parse("owner/repository").unwrap();
    assert_eq!(
        PullRequestReference::parse("#42", Some(&repo))
            .unwrap()
            .url(),
        "https://github.com/owner/repository/pull/42"
    );
    assert_eq!(
        PullRequestReference::parse(
            "https://github.com/owner/repository/pull/42/changes#diff-abc",
            None
        )
        .unwrap()
        .number,
        42
    );
    for bad in [
        "#1",
        "https://github.com.evil/owner/repository/pull/1",
        "https://github.com/owner/repository/issues/1",
        "https://github.com/owner/repository/pull/1/other",
        "https://github.com/owner/repository/pull/0",
        "https://github.com/owner/repository/pull/2147483648",
    ] {
        assert!(PullRequestReference::parse(bad, None).is_err(), "{bad}");
    }
    assert!(RepositoryName::parse("../repo").is_err());
    assert!(RepositoryName::parse("owner/repo/extra").is_err());
}
#[test]
fn filters_quote_user_input_without_changing_repository_scope() {
    let repo = RepositoryName::parse("owner/repo").unwrap();
    let filter = PullRequestFilter {
        state: Some("OPEN".into()),
        text: "feature repo:elsewhere/project".into(),
        labels: vec!["a\" b".into()],
        ..Default::default()
    };
    let query = filter.query(&repo).unwrap();
    assert!(query.starts_with("repo:owner/repo is:pr sort:updated-desc is:open"));
    assert!(query.contains("label:\"a\\\" b\""));
    assert!(query.ends_with("\"feature repo:elsewhere/project\" in:title"));
    assert!(
        PullRequestFilter {
            state: Some("UNKNOWN".into()),
            ..Default::default()
        }
        .query(&repo)
        .is_err()
    );
}
#[test]
fn pagination_reports_search_cap_and_rejects_missing_or_repeated_cursor() {
    let mut page =
        json!({"issueCount":2001,"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":"next"}});
    let parsed = parse_search_page(&page, None).unwrap();
    assert_eq!(parsed.total_count, 2001);
    assert!(parsed.search_limit_reached);
    assert_eq!(parsed.next_cursor.as_deref(), Some("next"));
    assert!(parse_search_page(&page, Some("next")).is_err());
    page["pageInfo"]["endCursor"] = Value::Null;
    assert!(parse_search_page(&page, None).is_err());
    page["pageInfo"]["hasNextPage"] = json!(false);
    assert!(
        parse_search_page(&page, None)
            .unwrap()
            .next_cursor
            .is_none()
    );
    page["nodes"] = json!([null]);
    assert!(parse_search_page(&page, None).is_err());
}
#[test]
fn shared_rest_transport_rejects_alternate_hosts_and_path_escapes() {
    for path in [
        "https://example.com/repos/owner/repo/pulls/1",
        "//example.com/path",
        "/repos/owner/repo/pulls/1",
        "repos/owner/repo/../other",
        "repos/owner/repo/%2e%2e/other",
        "repos/owner/repo/pulls/1#fragment",
        "repos/owner/repo/pulls/1\n",
    ] {
        assert!(validate_rest_endpoint("POST", path).is_err(), "{path}");
    }
    assert!(validate_rest_endpoint("POST", "repos/owner/repo/pulls/7/comments").is_ok());
    assert!(
        validate_rest_endpoint(
            "GET",
            "repos/owner/repo/pulls/7/reviews?per_page=100&page=2"
        )
        .is_ok()
    );
    assert!(validate_rest_endpoint("OTHER", "repos/owner/repo/pulls/7/comments").is_err());
}

#[test]
fn associated_base_oid_is_not_a_live_branch_fetch_target() -> Result<()> {
    let associated = "1".repeat(40);
    let live = "2".repeat(40);
    let mut value = json!({"id":"PR_example","number":7,"title":"Example","url":"https://github.com/owner/repo/pull/7","state":"OPEN","isDraft":false,"author":null,"baseRefName":"main","headRefName":"feature","updatedAt":"now","body":"","baseRefOid":associated,"baseRef":{"name":"main","target":{"oid":live}},"headRefOid":"3".repeat(40),"changedFiles":1,"additions":1,"deletions":0,"mergeable":"MERGEABLE","reviewDecision":null,"repository":{"id":"R_example","nameWithOwner":"owner/repo","url":"https://github.com/owner/repo"},"headRepository":null,"commits":{"totalCount":1},"labels":{"totalCount":0,"nodes":[]},"reviewRequests":{"totalCount":0,"nodes":[]},"recentCommits":{"nodes":[]}});
    let details: PullRequestDetails = decode(value.clone())?;
    assert_eq!(details.base_ref_oid, associated);
    assert_eq!(details.base_tip_oid()?, live);
    assert_ne!(details.base_tip_oid()?, details.base_ref_oid);
    value["baseRef"] = Value::Null;
    assert!(
        decode::<PullRequestDetails>(value.clone())?
            .base_tip_oid()
            .unwrap_err()
            .to_string()
            .contains("deleted or unavailable")
    );
    value["baseRef"] = json!({"name":"other","target":{"oid":live}});
    assert!(
        decode::<PullRequestDetails>(value.clone())?
            .base_tip_oid()
            .is_err()
    );
    value["baseRef"] = json!({"name":"main","target":{"oid":"0".repeat(40)}});
    assert!(decode::<PullRequestDetails>(value)?.base_tip_oid().is_err());
    Ok(())
}

#[test]
fn viewer_node_identity_and_canonical_renames_decode_without_assumptions() {
    let viewer: GitHubUser = decode(json!({"nodeId":"U_viewer","login":"reviewer"})).unwrap();
    assert_eq!(viewer.node_id.as_deref(), Some("U_viewer"));
    let rest: GitHubUser = decode(json!({"node_id":"U_viewer","login":"reviewer"})).unwrap();
    assert_eq!(rest.node_id, viewer.node_id);
    let renamed: PullRequestFile =
        decode(json!({"filename":"new.ts","previous_filename":"old.ts","status":"renamed"}))
            .unwrap();
    assert_eq!(renamed.previous_filename.as_deref(), Some("old.ts"));
    let added: PullRequestFile = decode(json!({"filename":"new.ts","status":"added"})).unwrap();
    assert!(added.previous_filename.is_none());
}

#[test]
fn rest_users_with_api_and_web_urls_do_not_duplicate_fields() {
    let user: GitHubUser = decode(json!({"login":"reviewer","url":"https://api.github.com/users/reviewer","html_url":"https://github.com/reviewer"})).unwrap();
    assert_eq!(user.login, "reviewer");
}

#[test]
fn deleted_authors_and_nullable_reviews_decode() {
    let comment = json!({"id":1,"node_id":"comment","body":"test","html_url":"https://github.com","user":null,"created_at":"now","updated_at":"now"});
    assert!(
        decode::<ConversationComment>(comment)
            .unwrap()
            .user
            .is_none()
    );
    let review = json!({"id":2,"node_id":"review","body":null,"html_url":"https://github.com","user":null,"state":"PENDING","submitted_at":null,"commit_id":"abc"});
    assert!(
        decode::<PullRequestReview>(review)
            .unwrap()
            .submitted_at
            .is_none()
    );
}
#[test]
fn conversation_anchor_keeps_current_and_original_coordinates_distinct() {
    use super::*;
    let mut thread:crate::github_review::RemoteThread=serde_json::from_value(json!({"id":"thread_demo","path":"src/path:with-colon.rs","line":9,"startLine":7,"originalLine":42,"originalStartLine":40,"diffSide":"LEFT","startDiffSide":"LEFT","subjectType":"LINE","isResolved":false,"isOutdated":false,"viewerCanReply":true,"viewerCanResolve":false,"viewerCanUnresolve":false,"comments":[]})).unwrap();
    let anchor = ConversationAnchor::from_thread(&thread, &activity_context());
    assert_eq!(anchor.path, "src/path:with-colon.rs");
    assert_eq!(anchor.identity, activity_context().identity);
    assert_eq!((anchor.line, anchor.start_line), (Some(9), Some(7)));
    assert_eq!(
        (anchor.original_line, anchor.original_start_line),
        (Some(42), Some(40))
    );
    assert_eq!(anchor.side, crate::github_review::DiffSide::Left);
    assert_eq!(
        anchor.start_side,
        Some(crate::github_review::DiffSide::Left)
    );
    thread.is_outdated = true;
    thread.line = None;
    thread.start_line = None;
    let historical = ConversationAnchor::from_thread(&thread, &activity_context());
    assert!(historical.is_outdated);
    assert_eq!(historical.line, None);
    assert_eq!(historical.original_line, Some(42));
    thread.subject_type = crate::github_review::CommentSubject::File;
    thread.original_line = None;
    thread.original_start_line = None;
    let file = ConversationAnchor::from_thread(&thread, &activity_context());
    assert_eq!(
        file.subject_type,
        crate::github_review::CommentSubject::File
    );
    assert_eq!(file.line, None);
    assert_eq!(file.original_line, None);
}
