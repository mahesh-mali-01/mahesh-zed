use super::*;
use crate::{
    github_pull_requests::{PullRequestReference, RepositoryName},
    review_store::ReviewDb,
};
use db::sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection};
use futures::executor::block_on;
use std::fs;

fn open(path: &std::path::Path) -> Result<ReviewDb> {
    ReviewDb::from_connection(block_on(
        ThreadSafeConnection::builder::<ReviewDb>(path.to_str().context("database path")?, true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=1000;")
            .build(),
    )?)
}
fn identity(viewer: &str) -> PullRequestIdentity {
    PullRequestIdentity {
        host: "github.com".into(),
        repository_node_id: "R_example".into(),
        pull_request_node_id: "PR_example".into(),
        viewer_node_id: viewer.into(),
    }
}
fn pr_metadata() -> PullRequestMetadata {
    PullRequestMetadata {
        owner: "owner".into(),
        repository: "repository".into(),
        number: 7,
        title: "Example PR".into(),
        base_ref_name: "main".into(),
        head_ref_name: "feature".into(),
        base_ref_oid: "1".repeat(40),
        head_ref_oid: "2".repeat(40),
        commit_count: Some(3),
    }
}
fn context(session: &ReviewSession, metadata: &PullRequestMetadata) -> ReviewContext {
    ReviewContext {
        reference: PullRequestReference {
            repository: RepositoryName {
                owner: metadata.owner.clone(),
                name: metadata.repository.clone(),
            },
            number: metadata.number,
        },
        identity: session.selector.pull_request_identity().unwrap().clone(),
        head_oid: metadata.head_ref_oid.clone(),
        base_tip_oid: metadata.base_ref_oid.clone(),
    }
}
fn manifest(generation: u64, head: &str, bytes: &[u8]) -> ReviewSnapshot {
    let side = |bytes| FileSide {
        path: "src/example.rs".into(),
        object: ObjectId::for_blob(ObjectFormat::Sha1, bytes),
        mode: 0o100644,
    };
    ReviewSnapshot {
        generation,
        incomplete_reason: None,
        groups: vec![ComparisonGroup {
            endpoints: ResolvedEndpoints {
                group: "comparison".into(),
                left: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "3".repeat(40),
                },
                right: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: head.into(),
                },
            },
            files: vec![FileComparison {
                kind: ComparisonKind::Full,
                old: Some(side(b"base\n")),
                new: Some(side(bytes)),
            }],
        }],
    }
}
fn setup() -> Result<(tempfile::TempDir, ReviewDb, ReviewSession)> {
    let directory = tempfile::tempdir()?;
    let db = open(&directory.path().join("review.sqlite"))?;
    let git = directory.path().join("git");
    fs::create_dir(&git)?;
    let binding = block_on(db.bind_repository(git.clone(), git, ObjectFormat::Sha1))?;
    let metadata = pr_metadata();
    let session = block_on(db.deploy_pull_request(
        binding.id,
        identity("U_viewer"),
        metadata.clone(),
        manifest(1, &metadata.head_ref_oid, b"first\n"),
        None,
    ))?;
    Ok((directory, db, session))
}
fn target(session: &ReviewSession) -> GitHubViewedSyncTarget {
    GitHubViewedSyncTarget {
        session: session.id.clone(),
        expected_revision: session.revision,
        displayed_generation: session.snapshot.generation,
    }
}
fn file_target(session: &ReviewSession) -> Result<ViewedTarget> {
    Ok(ViewedTarget {
        session: session.id.clone(),
        expected_revision: session.revision,
        displayed_generation: session.snapshot.generation,
        group: "comparison".into(),
        path: "src/example.rs".into(),
        fingerprint: session
            .snapshot
            .file("comparison", "src/example.rs")?
            .fingerprint()?,
    })
}
fn bytes(new: &[u8]) -> RetainedContents {
    RetainedContents {
        old: Some(b"base\n".to_vec()),
        new: Some(new.to_vec()),
    }
}
fn remote(state: ViewedState) -> Vec<RemoteViewedFile> {
    vec![RemoteViewedFile {
        path: "src/example.rs".into(),
        viewer_viewed_state: state,
    }]
}
fn sync(
    db: &ReviewDb,
    session: &ReviewSession,
    metadata: &PullRequestMetadata,
    state: ViewedState,
) -> Result<GitHubViewedSyncResult> {
    block_on(db.sync_github_viewed(
        target(session),
        context(session, metadata),
        remote(state),
        BTreeSet::new(),
    ))
}
fn operation(
    db: &ReviewDb,
    session: &ReviewSession,
    metadata: &PullRequestMetadata,
    viewed: bool,
    sent: bool,
) -> Result<ReviewOperation> {
    let intent = GitHubViewedIntent {
        origin: Some(target(session)),
        paths: vec!["src/example.rs".into()],
        viewed,
        fingerprints: BTreeMap::from([(
            "src/example.rs".into(),
            session
                .snapshot
                .file("comparison", "src/example.rs")?
                .fingerprint()?,
        )]),
    };
    let draft = block_on(db.create_operation(
        ReviewOperationContext {
            selector: session.selector.clone(),
            metadata: metadata.clone(),
        },
        ReviewOperationKind::SetFileViewed,
        serde_json::to_value(intent)?,
        ReviewLocationState::NotRequired,
    ))?;
    let queued = block_on(db.queue_operation(draft.operation_id, draft.revision))?;
    let sending = block_on(db.claim_operation(queued.operation_id, queued.revision, vec![]))?;
    block_on(db.finish_operation(
        sending.operation_id,
        sending.revision,
        if sent {
            ReviewOperationOutcome::Sent { server_ids: vec![] }
        } else {
            ReviewOperationOutcome::DeliveryUnknown {
                reason: "Confirmation unavailable".into(),
            }
        },
    ))
}
#[test]
fn github_viewed_unviewed_suppression_survives_restart_without_deleting_history() -> Result<()> {
    let (directory, db, session) = setup()?;
    block_on(db.mark_viewed(file_target(&session)?, bytes(b"first\n"), 1024))?;
    let current = db.load_session(&session.id)?;
    sync(&db, &current, &pr_metadata(), ViewedState::Unviewed)?;
    let current = db.load_session(&session.id)?;
    assert_eq!(
        db.github_viewed_overrides(&current, "comparison")?
            .get("src/example.rs"),
        Some(&false)
    );
    let retained = db
        .checkpoint(&session.id, "comparison", "src/example.rs")?
        .context("Retained review")?;
    assert_eq!(retained.1, bytes(b"first\n"));
    drop(db);
    let db = open(&directory.path().join("review.sqlite"))?;
    let restored = db.load_session(&session.id)?;
    assert_eq!(
        db.github_viewed_overrides(&restored, "comparison")?
            .get("src/example.rs"),
        Some(&false)
    );
    assert_eq!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .context("Retained after restart")?
            .1,
        bytes(b"first\n")
    );
    Ok(())
}
#[test]
fn github_viewed_import_requires_exact_one_file_bytes_budget_and_fresh_cas() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let result = sync(&db, &session, &pr_metadata(), ViewedState::Viewed)?;
    assert!(result.decisions[0].effective_viewed);
    assert!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .is_none()
    );
    let current = db.load_session(&session.id)?;
    assert_eq!(current.revision, result.revision);
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&current)?,
            identity("U_viewer"),
            bytes(b"first\n"),
            1
        ))
        .is_err()
    );
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&current)?,
            identity("U_viewer"),
            bytes(b"incorrect"),
            1024
        ))
        .is_err()
    );
    assert_eq!(
        db.select_row::<i64>("SELECT count(*) FROM review_contents")?()?,
        Some(0)
    );
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&current)?,
            identity("U_other"),
            bytes(b"first\n"),
            1024
        ))
        .is_err()
    );
    block_on(db.import_github_viewed_checkpoint(
        file_target(&current)?,
        identity("U_viewer"),
        bytes(b"first\n"),
        1024,
    ))?;
    assert_eq!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .context("Imported exact review")?
            .1,
        bytes(b"first\n")
    );
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&current)?,
            identity("U_viewer"),
            bytes(b"first\n"),
            1024
        ))
        .unwrap_err()
        .downcast_ref::<StaleReview>()
        .is_some()
    );
    Ok(())
}
#[test]
fn github_viewed_stale_server_viewed_never_replaces_changed_checkpoint() -> Result<()> {
    let (_directory, db, session) = setup()?;
    block_on(db.mark_viewed(file_target(&session)?, bytes(b"first\n"), 1024))?;
    let before = db.load_session(&session.id)?;
    let mut metadata = pr_metadata();
    metadata.head_ref_oid = "4".repeat(40);
    block_on(db.deploy_pull_request(
        session.repository.id.clone(),
        identity("U_viewer"),
        metadata.clone(),
        manifest(1, &metadata.head_ref_oid, b"second\n"),
        Some(before.revision),
    ))?;
    let changed = db.load_session(&session.id)?;
    let result = sync(&db, &changed, &metadata, ViewedState::Viewed)?;
    assert!(!result.decisions[0].effective_viewed);
    let changed = db.load_session(&session.id)?;
    // The GitHub total and local current-content approval intentionally differ:
    // displaying the latter as the former hid already-fetched server flags.
    let observed = db.github_viewed_observations(&changed.id, "comparison")?;
    assert_eq!(observed["src/example.rs"].state, ViewedState::Viewed);
    assert!(!observed["src/example.rs"].trusted_viewed);
    assert_eq!(
        db.github_viewed_overrides(&changed, "comparison")?["src/example.rs"],
        false
    );
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&changed)?,
            identity("U_viewer"),
            bytes(b"second\n"),
            1024
        ))
        .is_err()
    );
    assert_eq!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .context("Old reviewed bytes remain")?
            .1,
        bytes(b"first\n")
    );
    sync(&db, &changed, &metadata, ViewedState::Unviewed)?;
    let changed = db.load_session(&session.id)?;
    let result = sync(&db, &changed, &metadata, ViewedState::Viewed)?;
    assert!(result.decisions[0].effective_viewed);
    let changed = db.load_session(&session.id)?;
    block_on(db.import_github_viewed_checkpoint(
        file_target(&changed)?,
        identity("U_viewer"),
        bytes(b"second\n"),
        1024,
    ))?;
    assert_eq!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .context("Explicit new web review imported")?
            .1,
        bytes(b"second\n")
    );
    Ok(())
}
#[test]
fn github_viewed_rejects_stale_foreign_subset_and_incomplete_contexts() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let mut other = context(&session, &pr_metadata());
    other.identity.viewer_node_id = "U_other".into();
    assert!(
        block_on(db.sync_github_viewed(
            target(&session),
            other,
            remote(ViewedState::Viewed),
            BTreeSet::new()
        ))
        .is_err()
    );
    assert!(
        block_on(db.sync_github_viewed(
            target(&session),
            context(&session, &pr_metadata()),
            vec![],
            BTreeSet::new()
        ))
        .is_err()
    );
    sync(&db, &session, &pr_metadata(), ViewedState::Unviewed)?;
    assert!(
        sync(&db, &session, &pr_metadata(), ViewedState::Viewed)
            .unwrap_err()
            .downcast_ref::<StaleReview>()
            .is_some()
    );
    let metadata = pr_metadata();
    let base = ObjectId {
        format: ObjectFormat::Sha1,
        hex: metadata.base_ref_oid.clone(),
    };
    let head = ObjectId {
        format: ObjectFormat::Sha1,
        hex: metadata.head_ref_oid.clone(),
    };
    let mut selected = manifest(1, &metadata.head_ref_oid, b"first\n");
    selected.groups[0].endpoints.left = base.clone();
    let subset = block_on(db.create_pull_request_scope(
        session.repository.id,
        identity("U_viewer"),
        metadata.clone(),
        base.clone(),
        head.clone(),
        PullRequestCommitScope::Commits {
            selections: vec![CommitSelection {
                commit: head,
                parent: Some(base),
            }],
        },
        selected,
    ))?;
    assert!(sync(&db, &subset, &metadata, ViewedState::Viewed).is_err());
    Ok(())
}
#[test]
fn github_viewed_unknown_local_intent_blocks_import_even_if_caller_omits_pending_paths()
-> Result<()> {
    let (_directory, db, session) = setup()?;
    operation(&db, &session, &pr_metadata(), true, false)?;
    let result = sync(&db, &session, &pr_metadata(), ViewedState::Viewed)?;
    assert!(result.decisions[0].observation.is_none());
    assert!(
        db.github_viewed_observations(&session.id, "comparison")?
            .is_empty()
    );
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&session)?,
            identity("U_viewer"),
            bytes(b"first\n"),
            1024
        ))
        .is_err()
    );
    Ok(())
}
#[test]
fn github_viewed_sent_receipts_preserve_unviewed_history_and_reject_partial_results() -> Result<()>
{
    let (_directory, db, session) = setup()?;
    block_on(db.mark_viewed(file_target(&session)?, bytes(b"first\n"), 1024))?;
    let current = db.load_session(&session.id)?;
    let sent = operation(&db, &current, &pr_metadata(), false, true)?;
    assert!(
        block_on(db.record_github_viewed_receipt(
            target(&current),
            sent.operation_id.clone(),
            vec![]
        ))
        .is_err()
    );
    block_on(db.record_github_viewed_receipt(
        target(&current),
        sent.operation_id,
        remote(ViewedState::Unviewed),
    ))?;
    let current = db.load_session(&session.id)?;
    assert_eq!(
        db.github_viewed_overrides(&current, "comparison")?
            .get("src/example.rs"),
        Some(&false)
    );
    assert_eq!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .context("Unmark preserved history")?
            .1,
        bytes(b"first\n")
    );
    let unknown = operation(&db, &current, &pr_metadata(), true, false)?;
    assert!(
        block_on(db.record_github_viewed_receipt(
            target(&current),
            unknown.operation_id,
            remote(ViewedState::Viewed)
        ))
        .is_err()
    );
    Ok(())
}
#[test]
fn github_viewed_cache_is_isolated_by_account_and_clone_binding() -> Result<()> {
    let (directory, db, session) = setup()?;
    sync(&db, &session, &pr_metadata(), ViewedState::Unviewed)?;
    let metadata = pr_metadata();
    let other = block_on(db.deploy_pull_request(
        session.repository.id,
        identity("U_other"),
        metadata.clone(),
        manifest(1, &metadata.head_ref_oid, b"first\n"),
        None,
    ))?;
    assert!(
        db.github_viewed_observations(&other.id, "comparison")?
            .is_empty()
    );
    let clone = directory.path().join("clone");
    fs::create_dir(&clone)?;
    let binding = block_on(db.bind_repository(clone.clone(), clone, ObjectFormat::Sha1))?;
    let cloned = block_on(db.deploy_pull_request(
        binding.id,
        identity("U_viewer"),
        metadata.clone(),
        manifest(1, &metadata.head_ref_oid, b"first\n"),
        None,
    ))?;
    assert!(
        db.github_viewed_observations(&cloned.id, "comparison")?
            .is_empty()
    );
    Ok(())
}
#[test]
fn github_viewed_migration_from_outbox_schema_keeps_checkpoints_and_operations() -> Result<()> {
    struct PreviousDomain;
    impl Domain for PreviousDomain {
        const NAME: &'static str = "ReviewDb";
        const MIGRATIONS: &'static [&'static str] = &[
            ReviewDb::MIGRATIONS[0],
            ReviewDb::MIGRATIONS[1],
            ReviewDb::MIGRATIONS[2],
            ReviewDb::MIGRATIONS[3],
        ];
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("previous.sqlite");
    let db = ReviewDb::from_connection(block_on(
        ThreadSafeConnection::builder::<PreviousDomain>(
            path.to_str().context("database path")?,
            true,
        )
        .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
        .build(),
    )?)?;
    let git = directory.path().join("git");
    fs::create_dir(&git)?;
    let binding = block_on(db.bind_repository(git.clone(), git, ObjectFormat::Sha1))?;
    let metadata = pr_metadata();
    // Seed the historical schema without calling the latest deployment writer,
    // which also persists applied scopes introduced by a later migration.
    let session = block_on(db.create_session(
        binding.id,
        ComparisonSelector::PullRequest {
            identity: identity("U_viewer"),
        },
        manifest(1, &metadata.head_ref_oid, b"first\n"),
    ))?;
    block_on(db.write({
        let id = session.id.clone();
        let metadata = serde_json::to_string(&metadata)?;
        move |connection| -> Result<()> {
            connection.exec_bound::<(String, String)>(
                "INSERT INTO review_pull_requests(session_id,metadata) VALUES (?,?)",
            )?((id, metadata))?;
            Ok(())
        }
    }))?;
    block_on(db.mark_viewed(file_target(&session)?, bytes(b"first\n"), 1024))?;
    let op = operation(&db, &session, &metadata, true, false)?;
    drop(db);
    let upgraded = open(&path)?;
    assert_eq!(
        upgraded
            .checkpoint(&session.id, "comparison", "src/example.rs")?
            .context("Pre-migration checkpoint")?
            .1,
        bytes(b"first\n")
    );
    assert_eq!(
        upgraded.operation(&op.operation_id)?.state,
        ReviewOperationState::DeliveryUnknown
    );
    assert!(
        upgraded
            .github_viewed_observations(&session.id, "comparison")?
            .is_empty()
    );
    Ok(())
}

#[test]
fn github_viewed_unresolved_rename_chain_withholds_approval_without_blocking_refresh() -> Result<()>
{
    let (_directory, db, initial) = setup()?;
    let mut first = manifest(1, &pr_metadata().head_ref_oid, b"first\n");
    first.groups[0].files[0].old.as_mut().unwrap().path = "src/original.rs".into();
    first.groups[0].files[0].new.as_mut().unwrap().path = "src/first.rs".into();
    let first = block_on(db.deploy_pull_request(
        initial.repository.id.clone(),
        identity("U_viewer"),
        pr_metadata(),
        first,
        Some(initial.revision),
    ))?;
    let first_target = ViewedTarget {
        session: first.id.clone(),
        expected_revision: first.revision,
        displayed_generation: first.snapshot.generation,
        group: "comparison".into(),
        path: "src/first.rs".into(),
        fingerprint: first
            .snapshot
            .file("comparison", "src/first.rs")?
            .fingerprint()?,
    };
    block_on(db.mark_viewed(first_target, bytes(b"first\n"), 1024))?;
    let current = db.load_session(&first.id)?;
    let mut metadata = pr_metadata();
    metadata.head_ref_oid = "4".repeat(40);
    let mut renamed = manifest(1, &metadata.head_ref_oid, b"second\n");
    renamed.groups[0].files[0].old.as_mut().unwrap().path = "src/original.rs".into();
    renamed.groups[0].files[0].new.as_mut().unwrap().path = "src/second.rs".into();
    let renamed = block_on(db.deploy_pull_request(
        current.repository.id,
        identity("U_viewer"),
        metadata.clone(),
        renamed,
        Some(current.revision),
    ))?;
    assert!(
        db.reviewed_checkpoint_metadata(&renamed.id, "comparison")?
            .contains_key("src/first.rs")
    );
    let result = block_on(db.sync_github_viewed(
        target(&renamed),
        context(&renamed, &metadata),
        vec![RemoteViewedFile {
            path: "src/second.rs".into(),
            viewer_viewed_state: ViewedState::Viewed,
        }],
        BTreeSet::new(),
    ))?;
    assert!(!result.decisions[0].effective_viewed);
    assert_eq!(
        result.decisions[0].action,
        crate::review_github_viewed::ViewedCheckpointAction::RetainHistory
    );
    let current = db.load_session(&renamed.id)?;
    let target = ViewedTarget {
        session: current.id.clone(),
        expected_revision: current.revision,
        displayed_generation: current.snapshot.generation,
        group: "comparison".into(),
        path: "src/second.rs".into(),
        fingerprint: current
            .snapshot
            .file("comparison", "src/second.rs")?
            .fingerprint()?,
    };
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            target,
            identity("U_viewer"),
            bytes(b"second\n"),
            1024
        ))
        .is_err()
    );
    assert_eq!(
        db.checkpoint(&first.id, "comparison", "src/first.rs")?
            .context("Prior checkpoint is intact")?
            .1,
        bytes(b"first\n")
    );
    let observations = db.github_viewed_observations(&first.id, "comparison")?;
    assert_eq!(observations["src/second.rs"].state, ViewedState::Viewed);
    assert!(!observations["src/second.rs"].trusted_viewed);
    assert_eq!(
        db.github_viewed_overrides(&current, "comparison")?["src/second.rs"],
        false
    );
    Ok(())
}

#[test]
fn github_viewed_metadata_read_validates_group_without_loading_retained_bytes() -> Result<()> {
    let (_directory, db, session) = setup()?;
    block_on(db.mark_viewed(file_target(&session)?, bytes(b"first\n"), 1024))?;
    let metadata = db.reviewed_checkpoint_metadata(&session.id, "comparison")?;
    assert_eq!(metadata.len(), 1);
    assert_eq!(
        metadata["src/example.rs"]
            .comparison
            .new
            .as_ref()
            .unwrap()
            .object,
        ObjectId::for_blob(ObjectFormat::Sha1, b"first\n")
    );
    assert!(
        db.reviewed_checkpoint_metadata(&session.id, "missing")
            .is_err()
    );
    // Metadata reads still work independently from the content table. This
    // distinguishes the API from checkpoint(), which retrieves retained blobs.
    assert!(metadata["src/example.rs"].matches(
        "comparison",
        session.snapshot.file("comparison", "src/example.rs")?
    )?);
    Ok(())
}

#[test]
fn github_viewed_old_head_unknown_does_not_block_current_observations() -> Result<()> {
    let (_directory, db, initial) = setup()?;
    let unknown = operation(&db, &initial, &pr_metadata(), true, false)?;
    let mut metadata = pr_metadata();
    metadata.head_ref_oid = "4".repeat(40);
    let current = block_on(db.deploy_pull_request(
        initial.repository.id.clone(),
        identity("U_viewer"),
        metadata.clone(),
        manifest(1, &metadata.head_ref_oid, b"second\n"),
        Some(initial.revision),
    ))?;
    let result = sync(&db, &current, &metadata, ViewedState::Viewed)?;
    assert!(result.decisions[0].observation.is_some());
    let current = db.load_session(&current.id)?;
    assert!(db.github_viewed_local_intents(&current)?.is_empty());
    block_on(db.import_github_viewed_checkpoint(
        file_target(&current)?,
        identity("U_viewer"),
        bytes(b"second\n"),
        1024,
    ))?;
    assert_eq!(
        db.operation(&unknown.operation_id)?,
        unknown,
        "Historical uncertain delivery must remain untouched"
    );
    let current = db.load_session(&current.id)?;
    metadata.head_ref_oid = "5".repeat(40);
    let later = block_on(db.deploy_pull_request(
        current.repository.id,
        identity("U_viewer"),
        metadata.clone(),
        manifest(1, &metadata.head_ref_oid, b"third\n"),
        Some(current.revision),
    ))?;
    let result = sync(&db, &later, &metadata, ViewedState::Viewed)?;
    assert!(result.decisions[0].observation.is_some());
    assert!(
        !result.decisions[0].effective_viewed,
        "Observing the current server flag does not approve changed retained bytes"
    );
    let later = db.load_session(&later.id)?;
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&later)?,
            identity("U_viewer"),
            bytes(b"third\n"),
            1024
        ))
        .is_err()
    );
    assert_eq!(db.operation(&unknown.operation_id)?, unknown);
    Ok(())
}

#[test]
fn github_viewed_same_head_unknown_is_not_superseded_by_another_receipt() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let unknown = operation(&db, &session, &pr_metadata(), true, false)?;
    let sent = operation(&db, &session, &pr_metadata(), true, true)?;
    block_on(db.record_github_viewed_receipt(
        target(&session),
        sent.operation_id,
        remote(ViewedState::Viewed),
    ))?;
    let session = db.load_session(&session.id)?;
    assert!(
        block_on(db.import_github_viewed_checkpoint(
            file_target(&session)?,
            identity("U_viewer"),
            bytes(b"first\n"),
            1024
        ))
        .is_err()
    );
    assert_eq!(
        db.operation(&unknown.operation_id)?.state,
        ReviewOperationState::DeliveryUnknown
    );
    Ok(())
}

#[test]
fn github_viewed_presentation_since_review_defaults_for_legacy_json() -> Result<()> {
    let presentation: ReviewPresentation = serde_json::from_value(
        serde_json::json!({"unreviewed_only":false,"split_view":null,"selected_group":null,"selected_path":null}),
    )?;
    assert!(!presentation.since_review);
    let mut changed = presentation;
    changed.since_review = true;
    assert!(
        serde_json::from_value::<ReviewPresentation>(serde_json::to_value(changed)?)?.since_review
    );
    Ok(())
}

#[test]
fn github_viewed_sent_receipt_recovers_after_restart_and_presentation_change_once() -> Result<()> {
    let (directory, db, session) = setup()?;
    block_on(db.mark_viewed(file_target(&session)?, bytes(b"first\n"), 1024))?;
    let session = db.load_session(&session.id)?;
    let mut metadata = pr_metadata();
    metadata.head_ref_oid = "4".repeat(40);
    let session = block_on(db.deploy_pull_request(
        session.repository.id,
        identity("U_viewer"),
        metadata.clone(),
        manifest(1, &metadata.head_ref_oid, b"second\n"),
        Some(session.revision),
    ))?;
    let sent = operation(&db, &session, &metadata, true, true)?;
    // Delivery is persisted, but no receipt import happens before the crash.
    block_on(db.save_presentation(
        session.id.clone(),
        session.revision,
        ReviewPresentation {
            since_review: true,
            ..Default::default()
        },
    ))?;
    drop(db);
    let db = open(&directory.path().join("review.sqlite"))?;
    let revision = block_on(db.recover_github_viewed_receipts(target(&session)))?;
    let session = db.load_session(&session.id)?;
    assert_eq!(session.revision, revision);
    assert!(session.presentation.since_review);
    assert_eq!(
        db.operation(&sent.operation_id)?.state,
        ReviewOperationState::Sent
    );
    let resynced = sync(&db, &session, &metadata, ViewedState::Viewed)?;
    assert!(resynced.decisions[0].effective_viewed);
    assert_eq!(
        resynced.decisions[0].action,
        crate::review_github_viewed::ViewedCheckpointAction::ImportCurrent
    );
    let session = db.load_session(&session.id)?;
    // A later web unviewed state must win over old Sent operations on restart.
    sync(&db, &session, &metadata, ViewedState::Unviewed)?;
    let session = db.load_session(&session.id)?;
    assert_eq!(
        block_on(db.recover_github_viewed_receipts(target(&session)))?,
        session.revision
    );
    assert_eq!(
        db.github_viewed_overrides(&session, "comparison")?
            .get("src/example.rs"),
        Some(&false)
    );
    Ok(())
}
#[test]
fn github_viewed_sent_recovery_cannot_override_newer_substantive_state_or_another_clone()
-> Result<()> {
    let (directory, db, session) = setup()?;
    let sent = operation(&db, &session, &pr_metadata(), true, true)?;
    sync(&db, &session, &pr_metadata(), ViewedState::Unviewed)?;
    let current = db.load_session(&session.id)?;
    assert!(
        block_on(db.record_github_viewed_receipt(
            target(&current),
            sent.operation_id,
            remote(ViewedState::Viewed)
        ))
        .is_err()
    );
    assert_eq!(
        block_on(db.recover_github_viewed_receipts(target(&current)))?,
        current.revision
    );
    assert_eq!(
        db.github_viewed_overrides(&current, "comparison")?
            .get("src/example.rs"),
        Some(&false)
    );
    let clone = directory.path().join("other-clone");
    fs::create_dir(&clone)?;
    let binding = block_on(db.bind_repository(clone.clone(), clone, ObjectFormat::Sha1))?;
    let clone = block_on(db.deploy_pull_request(
        binding.id,
        identity("U_viewer"),
        pr_metadata(),
        manifest(1, &pr_metadata().head_ref_oid, b"first\n"),
        None,
    ))?;
    assert_eq!(
        block_on(db.recover_github_viewed_receipts(target(&clone)))?,
        clone.revision
    );
    assert!(
        db.github_viewed_observations(&clone.id, "comparison")?
            .is_empty()
    );
    Ok(())
}

fn local_viewed_intent(session: &ReviewSession, viewed: bool) -> Result<GitHubViewedIntent> {
    Ok(GitHubViewedIntent {
        origin: Some(target(session)),
        paths: vec!["src/example.rs".into()],
        viewed,
        fingerprints: BTreeMap::from([(
            "src/example.rs".into(),
            session
                .snapshot
                .file("comparison", "src/example.rs")?
                .fingerprint()?,
        )]),
    })
}
fn local_context(session: &ReviewSession) -> ReviewOperationContext {
    ReviewOperationContext {
        selector: session.selector.clone(),
        metadata: pr_metadata(),
    }
}

#[test]
fn github_viewed_atomic_acceptance_rolls_back_checkpoint_when_queue_fails() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let unrelated = block_on(db.create_operation(
        local_context(&session),
        ReviewOperationKind::PostInlineComment,
        serde_json::json!({"body":"another operation"}),
        ReviewLocationState::Ready,
    ))?;
    let accepted = block_on(db.accept_github_viewed_intent(
        target(&session),
        local_context(&session),
        local_viewed_intent(&session, true)?,
        BTreeMap::from([("src/example.rs".into(), bytes(b"first\n"))]),
        Some(unrelated.clone()),
        1024,
    ));
    assert!(
        accepted.is_err(),
        "queue identity failure occurs after checkpoint staging inside the transaction"
    );
    assert!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .is_none()
    );
    assert_eq!(db.load_session(&session.id)?.revision, session.revision);
    assert_eq!(
        db.operations(session.selector.pull_request_identity().unwrap())?,
        vec![unrelated]
    );
    Ok(())
}

#[test]
fn github_viewed_atomic_intent_restores_latest_toggle_after_restart() -> Result<()> {
    let (directory, db, session) = setup()?;
    let (first, _) = block_on(db.accept_github_viewed_intent(
        target(&session),
        local_context(&session),
        local_viewed_intent(&session, true)?,
        BTreeMap::from([("src/example.rs".into(), bytes(b"first\n"))]),
        None,
        1024,
    ))?;
    assert_eq!(first.state, ReviewOperationState::Queued);
    assert_eq!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .unwrap()
            .1,
        bytes(b"first\n")
    );
    let current = db.load_session(&session.id)?;
    let (last, _) = block_on(db.accept_github_viewed_intent(
        target(&current),
        local_context(&current),
        local_viewed_intent(&current, false)?,
        BTreeMap::new(),
        None,
        1024,
    ))?;
    assert_eq!(
        last.operation_id, first.operation_id,
        "unsent exact-path toggles coalesce durably"
    );
    assert!(last.revision > first.revision);
    assert!(!serde_json::from_value::<GitHubViewedIntent>(last.payload.clone())?.viewed);
    drop(db);
    let db = open(&directory.path().join("review.sqlite"))?;
    let current = db.load_session(&session.id)?;
    assert_eq!(
        db.github_viewed_local_intents(&current)?,
        BTreeMap::from([("src/example.rs".into(), false)])
    );
    assert!(
        db.checkpoint(&session.id, "comparison", "src/example.rs")?
            .is_none()
    );
    assert_eq!(
        db.operation(&first.operation_id)?.state,
        ReviewOperationState::Queued
    );
    assert_eq!(db.operation(&last.operation_id)?.revision, last.revision);
    assert_eq!(
        db.operations(current.selector.pull_request_identity().unwrap())?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn github_viewed_retry_of_older_operation_wins_by_acceptance_revision() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let (first, _) = block_on(db.accept_github_viewed_intent(
        target(&session),
        local_context(&session),
        local_viewed_intent(&session, true)?,
        BTreeMap::from([("src/example.rs".into(), bytes(b"first\n"))]),
        None,
        1024,
    ))?;
    let claimed = block_on(db.claim_operation(first.operation_id, first.revision, Vec::new()))?;
    let failed = block_on(db.finish_operation(
        claimed.operation_id,
        claimed.revision,
        ReviewOperationOutcome::Failed {
            reason: "Offline before send".into(),
        },
    ))?;
    let current = db.load_session(&session.id)?;
    let (second, _) = block_on(db.accept_github_viewed_intent(
        target(&current),
        local_context(&current),
        local_viewed_intent(&current, false)?,
        BTreeMap::new(),
        None,
        1024,
    ))?;
    assert!(second.operation_id > failed.operation_id);
    let current = db.load_session(&session.id)?;
    let (retried, revision) = block_on(db.accept_github_viewed_intent(
        target(&current),
        local_context(&current),
        local_viewed_intent(&current, true)?,
        BTreeMap::from([("src/example.rs".into(), bytes(b"first\n"))]),
        Some(failed),
        1024,
    ))?;
    assert!(retried.operation_id < second.operation_id);
    let current = db.load_session(&session.id)?;
    assert_eq!(current.revision, revision);
    assert_eq!(
        db.github_viewed_local_intents(&current)?,
        BTreeMap::from([("src/example.rs".into(), true)])
    );
    Ok(())
}

#[test]
fn github_viewed_new_toggle_never_rewrites_a_sending_operation() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let (first, _) = block_on(db.accept_github_viewed_intent(
        target(&session),
        local_context(&session),
        local_viewed_intent(&session, true)?,
        BTreeMap::from([("src/example.rs".into(), bytes(b"first\n"))]),
        None,
        1024,
    ))?;
    let sending = block_on(db.claim_operation(first.operation_id, first.revision, Vec::new()))?;
    let current = db.load_session(&session.id)?;
    let (newer, _) = block_on(db.accept_github_viewed_intent(
        target(&current),
        local_context(&current),
        local_viewed_intent(&current, false)?,
        BTreeMap::new(),
        None,
        1024,
    ))?;
    assert_ne!(newer.operation_id, sending.operation_id);
    assert_eq!(
        db.operation(&sending.operation_id)?.state,
        ReviewOperationState::Sending
    );
    assert!(
        serde_json::from_value::<GitHubViewedIntent>(db.operation(&sending.operation_id)?.payload)?
            .viewed
    );
    let current = db.load_session(&session.id)?;
    assert_eq!(
        db.github_viewed_local_intents(&current)?,
        BTreeMap::from([("src/example.rs".into(), false)])
    );
    Ok(())
}

#[test]
fn completed_viewed_toggles_do_not_reappear_when_web_state_changes() -> Result<()> {
    for viewed in [true, false] {
        let (directory, db, session) = setup()?;
        let sent_state = if viewed {
            ViewedState::Viewed
        } else {
            ViewedState::Unviewed
        };
        let web_state = if viewed {
            ViewedState::Unviewed
        } else {
            ViewedState::Viewed
        };
        let sent = operation(&db, &session, &pr_metadata(), viewed, true)?;
        block_on(db.record_github_viewed_receipt(
            target(&session),
            sent.operation_id.clone(),
            remote(sent_state),
        ))?;
        let current = db.load_session(&session.id)?;
        assert!(db.github_viewed_local_intents(&current)?.is_empty());
        sync(&db, &current, &pr_metadata(), web_state)?;
        let current = db.load_session(&session.id)?;
        assert_eq!(
            db.github_viewed_observations(&current.id, "comparison")?["src/example.rs"].state,
            web_state
        );
        assert!(
            db.github_viewed_local_intents(&current)?.is_empty(),
            "A fresh remote change must not revive a completed opposite toggle"
        );
        assert_eq!(
            db.operation(&sent.operation_id)?.state,
            ReviewOperationState::Sent
        );
        drop(db);
        let db = open(&directory.path().join("review.sqlite"))?;
        let current = db.load_session(&session.id)?;
        assert_eq!(
            block_on(db.recover_github_viewed_receipts(target(&current)))?,
            current.revision,
            "Restart must not replay the completed receipt over newer remote state"
        );
        assert!(db.github_viewed_local_intents(&current)?.is_empty());
        assert_eq!(
            db.github_viewed_observations(&current.id, "comparison")?["src/example.rs"].state,
            web_state
        );
    }
    Ok(())
}

#[test]
fn latest_sent_viewed_toggle_suppresses_older_failed_intent_after_web_change() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let (first, _) = block_on(db.accept_github_viewed_intent(
        target(&session),
        local_context(&session),
        local_viewed_intent(&session, true)?,
        BTreeMap::from([("src/example.rs".into(), bytes(b"first\n"))]),
        None,
        1024,
    ))?;
    let first = block_on(db.claim_operation(first.operation_id, first.revision, Vec::new()))?;
    block_on(db.finish_operation(
        first.operation_id,
        first.revision,
        ReviewOperationOutcome::Failed {
            reason: "Offline".into(),
        },
    ))?;
    let current = db.load_session(&session.id)?;
    let (last, _) = block_on(db.accept_github_viewed_intent(
        target(&current),
        local_context(&current),
        local_viewed_intent(&current, false)?,
        BTreeMap::new(),
        None,
        1024,
    ))?;
    let last = block_on(db.claim_operation(last.operation_id, last.revision, Vec::new()))?;
    let last = block_on(db.finish_operation(
        last.operation_id,
        last.revision,
        ReviewOperationOutcome::Sent {
            server_ids: Vec::new(),
        },
    ))?;
    let current = db.load_session(&session.id)?;
    block_on(db.record_github_viewed_receipt(
        target(&current),
        last.operation_id,
        remote(ViewedState::Unviewed),
    ))?;
    let current = db.load_session(&session.id)?;
    sync(&db, &current, &pr_metadata(), ViewedState::Viewed)?;
    let current = db.load_session(&session.id)?;
    assert!(
        db.github_viewed_local_intents(&current)?.is_empty(),
        "Latest terminal intent remains an ordering barrier instead of resurfacing either old toggle"
    );
    Ok(())
}

#[test]
fn github_viewed_other_checkout_failures_do_not_hide_current_flags() -> Result<()> {
    for failed in [false, true] {
        let (directory, db, session) = setup()?;
        let (accepted, _) = block_on(db.accept_github_viewed_intent(
            target(&session),
            local_context(&session),
            local_viewed_intent(&session, true)?,
            BTreeMap::from([("src/example.rs".into(), bytes(b"first\n"))]),
            None,
            1024,
        ))?;
        let claimed =
            block_on(db.claim_operation(accepted.operation_id, accepted.revision, Vec::new()))?;
        let outcome = if failed {
            ReviewOperationOutcome::Failed {
                reason: "PR branches changed".into(),
            }
        } else {
            ReviewOperationOutcome::DeliveryUnknown {
                reason: "Read back unavailable".into(),
            }
        };
        let preserved =
            block_on(db.finish_operation(claimed.operation_id, claimed.revision, outcome))?;
        let current = db.load_session(&session.id)?;
        assert!(
            sync(&db, &current, &pr_metadata(), ViewedState::Unviewed)?.decisions[0]
                .observation
                .is_none(),
            "The exact original pending target still protects local intent"
        );
        let clone = directory.path().join("other-checkout");
        fs::create_dir(&clone)?;
        let binding = block_on(db.bind_repository(clone.clone(), clone, ObjectFormat::Sha1))?;
        let cloned = block_on(db.deploy_pull_request(
            binding.id,
            identity("U_viewer"),
            pr_metadata(),
            session.snapshot.clone(),
            None,
        ))?;
        let result = sync(&db, &cloned, &pr_metadata(), ViewedState::Viewed)?;
        assert!(
            result.decisions[0].observation.is_some(),
            "Same PR/account/path/head in a different checkout must receive complete server observations"
        );
        let cloned = db.load_session(&cloned.id)?;
        assert!(db.github_viewed_local_intents(&cloned)?.is_empty());
        assert_eq!(
            db.github_viewed_observations(&cloned.id, "comparison")?
                .len(),
            1
        );
        assert_eq!(
            db.operation(&preserved.operation_id)?,
            preserved,
            "Do not retry or rewrite historical failures"
        );
        assert_eq!(
            db.github_viewed_local_intents(&current)?,
            BTreeMap::from([("src/example.rs".into(), true)])
        );
    }
    Ok(())
}

#[test]
fn github_viewed_mismatched_tips_and_fingerprints_do_not_block_current_observations() -> Result<()>
{
    for mismatch in ["head", "base", "fingerprint"] {
        let (_directory, db, session) = setup()?;
        let mut stale = session.clone();
        let mut metadata = pr_metadata();
        match mismatch {
            "head" => metadata.head_ref_oid = "4".repeat(40),
            "base" => metadata.base_ref_oid = "4".repeat(40),
            _ => {
                stale.snapshot.groups[0].files[0]
                    .new
                    .as_mut()
                    .unwrap()
                    .object = ObjectId::for_blob(ObjectFormat::Sha1, b"other bytes\n")
            }
        }
        let preserved = operation(&db, &stale, &metadata, true, false)?;
        let result = sync(&db, &session, &pr_metadata(), ViewedState::Unviewed)?;
        assert!(
            result.decisions[0].observation.is_some(),
            "A mismatched {mismatch} must not suppress current flags"
        );
        let current = db.load_session(&session.id)?;
        assert_eq!(
            db.github_viewed_observations(&current.id, "comparison")?["src/example.rs"].state,
            ViewedState::Unviewed
        );
        assert_eq!(db.operation(&preserved.operation_id)?, preserved);
    }
    Ok(())
}
