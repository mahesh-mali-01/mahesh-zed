use super::*;
use crate::review::*;
use db::sqlez::thread_safe_connection::ThreadSafeConnection;
use futures::executor::block_on;
use std::collections::BTreeSet;

fn open(path: &std::path::Path) -> Result<ReviewDb> {
    ReviewDb::from_connection(block_on(
        ThreadSafeConnection::builder::<ReviewDb>(path.to_str().unwrap(), true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
            .build(),
    )?)
}
fn oid(value: u8) -> ObjectId {
    ObjectId {
        format: ObjectFormat::Sha1,
        hex: format!("{value:040x}"),
    }
}
fn identity(viewer: &str) -> PullRequestIdentity {
    PullRequestIdentity {
        host: "github.com".into(),
        repository_node_id: "R_example".into(),
        pull_request_node_id: "PR_example".into(),
        viewer_node_id: viewer.into(),
    }
}
fn metadata() -> PullRequestMetadata {
    PullRequestMetadata {
        owner: "example".into(),
        repository: "repository".into(),
        number: 7,
        title: "Review example".into(),
        base_ref_name: "main".into(),
        head_ref_name: "feature".into(),
        base_ref_oid: oid(1).hex,
        head_ref_oid: oid(4).hex,
        commit_count: Some(3),
    }
}
fn snapshot(left: u8, right: u8, old: &[u8], new: &[u8]) -> ReviewSnapshot {
    let side = |bytes| FileSide {
        path: "example.rs".into(),
        object: ObjectId::for_blob(ObjectFormat::Sha1, bytes),
        mode: 0o100644,
    };
    ReviewSnapshot {
        generation: 1,
        incomplete_reason: None,
        groups: vec![ComparisonGroup {
            endpoints: ResolvedEndpoints {
                group: "comparison".into(),
                left: oid(left),
                right: oid(right),
            },
            files: vec![FileComparison {
                kind: ComparisonKind::Full,
                old: Some(side(old)),
                new: Some(side(new)),
            }],
        }],
    }
}
fn target(session: &ReviewSession) -> Result<ViewedTarget> {
    Ok(ViewedTarget {
        session: session.id.clone(),
        expected_revision: session.revision,
        displayed_generation: session.snapshot.generation,
        group: "comparison".into(),
        path: "example.rs".into(),
        fingerprint: session
            .snapshot
            .file("comparison", "example.rs")?
            .fingerprint()?,
    })
}
fn scope(
    db: &ReviewDb,
    canonical: &ReviewSession,
    left: u8,
    right: u8,
    old: &[u8],
    new: &[u8],
) -> Result<ReviewSession> {
    block_on(db.create_pull_request_scope(
        canonical.repository.id.clone(),
        canonical.selector.pull_request_identity().unwrap().clone(),
        metadata(),
        oid(1),
        oid(4),
        PullRequestCommitScope::Commits {
            selections: vec![CommitSelection {
                commit: oid(right),
                parent: Some(oid(left)),
            }],
        },
        snapshot(left, right, old, new),
    ))
}
fn setup() -> Result<(tempfile::TempDir, ReviewDb, ReviewSession)> {
    let directory = tempfile::tempdir()?;
    let db = open(&directory.path().join("review.sqlite"))?;
    let git = directory.path().join("git");
    std::fs::create_dir(&git)?;
    let binding = block_on(db.bind_repository(git.clone(), git, ObjectFormat::Sha1))?;
    let session = block_on(db.deploy_pull_request(
        binding.id,
        identity("U_reviewer"),
        metadata(),
        snapshot(1, 4, b"a", b"d"),
        None,
    ))?;
    Ok((directory, db, session))
}
fn history() -> BTreeSet<String> {
    (1..=4).map(|value| oid(value).hex).collect()
}

#[test]
fn coverage_receipts_survive_restart_and_reject_gaps_and_stale_writes() -> Result<()> {
    let (directory, db, canonical) = setup()?;
    let last = scope(&db, &canonical, 3, 4, b"c", b"d")?;
    block_on(db.record_review_coverage(target(&last)?))?;
    assert!(
        block_on(db.record_review_coverage(target(&last)?))
            .unwrap_err()
            .downcast_ref::<StaleReview>()
            .is_some()
    );
    let first = scope(&db, &canonical, 1, 2, b"a", b"b")?;
    block_on(db.record_review_coverage(target(&first)?))?;
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    let middle = scope(&db, &canonical, 2, 3, b"b", b"c")?;
    block_on(db.record_review_coverage(target(&middle)?))?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    let middle = db.load_session(&middle.id)?;
    assert!(db.comparison_reviewed(&middle, "comparison", "example.rs")?);
    let mut rebased_history = history();
    rebased_history.remove(&oid(2).hex);
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &rebased_history)?);
    drop(db);
    let db = open(&directory.path().join("review.sqlite"))?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    let middle = db.load_session(&middle.id)?;
    block_on(db.revoke_review_coverage(target(&middle)?))?;
    let middle = db.load_session(&middle.id)?;
    assert!(!db.comparison_reviewed(&middle, "comparison", "example.rs")?);
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    Ok(())
}

#[test]
fn full_unmark_revokes_coverage_and_accounts_are_isolated() -> Result<()> {
    let (_directory, db, canonical) = setup()?;
    let last = scope(&db, &canonical, 1, 4, b"a", b"d")?;
    block_on(db.record_review_coverage(target(&last)?))?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    let other = block_on(db.deploy_pull_request(
        canonical.repository.id.clone(),
        identity("U_other"),
        metadata(),
        snapshot(1, 4, b"a", b"d"),
        None,
    ))?;
    assert!(!db.reviewed_file_coverage(&other, "example.rs", &history())?);
    block_on(db.mark_unviewed(target(&canonical)?))?;
    let canonical = db.load_session(&canonical.id)?;
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    Ok(())
}

#[test]
fn full_checkpoint_seeds_new_head_only_with_exact_original_base() -> Result<()> {
    let (_directory, db, canonical) = setup()?;
    block_on(db.mark_viewed(
        target(&canonical)?,
        RetainedContents {
            old: Some(b"a".to_vec()),
            new: Some(b"d".to_vec()),
        },
        100,
    ))?;
    let canonical = db.load_session(&canonical.id)?;
    let mut new_metadata = metadata();
    new_metadata.head_ref_oid = oid(5).hex;
    let updated = block_on(db.deploy_pull_request(
        canonical.repository.id.clone(),
        identity("U_reviewer"),
        new_metadata.clone(),
        snapshot(1, 5, b"a", b"e"),
        Some(canonical.revision),
    ))?;
    let selected = block_on(db.create_pull_request_scope(
        updated.repository.id.clone(),
        identity("U_reviewer"),
        new_metadata,
        oid(1),
        oid(5),
        PullRequestCommitScope::Commits {
            selections: vec![CommitSelection {
                commit: oid(5),
                parent: Some(oid(4)),
            }],
        },
        snapshot(4, 5, b"d", b"e"),
    ))?;
    block_on(db.record_review_coverage(target(&selected)?))?;
    let history = (1..=5).map(|value| oid(value).hex).collect();
    assert!(db.reviewed_file_coverage(&updated, "example.rs", &history)?);
    let mut changed_base = metadata();
    changed_base.head_ref_oid = oid(5).hex;
    changed_base.base_ref_oid = oid(6).hex;
    let changed = block_on(db.deploy_pull_request(
        updated.repository.id.clone(),
        identity("U_reviewer"),
        changed_base,
        snapshot(6, 5, b"different", b"e"),
        Some(updated.revision),
    ))?;
    let history = (1..=6).map(|value| oid(value).hex).collect();
    assert!(!db.reviewed_file_coverage(&changed, "example.rs", &history)?);
    Ok(())
}

#[test]
fn coverage_migration_preserves_existing_full_checkpoints_without_copying_contents() -> Result<()> {
    use db::sqlez::domain::Domain;
    struct PreviousDomain;
    impl Domain for PreviousDomain {
        const NAME: &'static str = "ReviewDb";
        const MIGRATIONS: &'static [&'static str] = &[
            ReviewDb::MIGRATIONS[0],
            ReviewDb::MIGRATIONS[1],
            ReviewDb::MIGRATIONS[2],
            ReviewDb::MIGRATIONS[3],
            ReviewDb::MIGRATIONS[4],
            ReviewDb::MIGRATIONS[5],
        ];
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("review.sqlite");
    let db = ReviewDb::from_connection(block_on(
        ThreadSafeConnection::builder::<PreviousDomain>(path.to_str().unwrap(), true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
            .build(),
    )?)?;
    let git = directory.path().join("git");
    std::fs::create_dir(&git)?;
    let binding = block_on(db.bind_repository(git.clone(), git, ObjectFormat::Sha1))?;
    let canonical = block_on(db.deploy_pull_request(
        binding.id,
        identity("U_reviewer"),
        metadata(),
        snapshot(1, 4, b"a", b"d"),
        None,
    ))?;
    block_on(db.mark_viewed(
        target(&canonical)?,
        RetainedContents {
            old: Some(b"a".to_vec()),
            new: Some(b"d".to_vec()),
        },
        100,
    ))?;
    drop(db);
    let db = open(&path)?;
    let canonical = db.load_session(&canonical.id)?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    assert_eq!(
        db.select_row::<i64>("SELECT count(*) FROM review_contents")?()?,
        Some(2)
    );
    assert_eq!(
        db.select_row::<i64>("SELECT count(*) FROM review_coverage")?()?,
        Some(0)
    );
    block_on(db.mark_unviewed(target(&canonical)?))?;
    let canonical = db.load_session(&canonical.id)?;
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    Ok(())
}

#[test]
fn queued_full_viewed_intent_records_and_clears_coverage_atomically() -> Result<()> {
    use std::collections::BTreeMap;
    let (_directory, db, canonical) = setup()?;
    let context = ReviewOperationContext {
        selector: canonical.selector.clone(),
        metadata: metadata(),
    };
    let intent = |session: &ReviewSession, viewed| -> Result<GitHubViewedIntent> {
        Ok(GitHubViewedIntent {
            origin: None,
            paths: vec!["example.rs".into()],
            viewed,
            fingerprints: BTreeMap::from([(
                "example.rs".into(),
                session
                    .snapshot
                    .file("comparison", "example.rs")?
                    .fingerprint()?,
            )]),
        })
    };
    let sync_target = |session: &ReviewSession| GitHubViewedSyncTarget {
        session: session.id.clone(),
        expected_revision: session.revision,
        displayed_generation: session.snapshot.generation,
    };
    let retained = || {
        BTreeMap::from([(
            "example.rs".into(),
            RetainedContents {
                old: Some(b"a".to_vec()),
                new: Some(b"d".to_vec()),
            },
        )])
    };
    let unrelated = block_on(db.create_operation(
        context.clone(),
        ReviewOperationKind::PostInlineComment,
        serde_json::json!({"body":"Example comment"}),
        ReviewLocationState::Ready,
    ))?;
    assert!(
        block_on(db.accept_github_viewed_intent(
            sync_target(&canonical),
            context.clone(),
            intent(&canonical, true)?,
            retained(),
            Some(unrelated),
            100
        ))
        .is_err()
    );
    assert!(!db.comparison_reviewed(&canonical, "comparison", "example.rs")?);
    block_on(db.accept_github_viewed_intent(
        sync_target(&canonical),
        context.clone(),
        intent(&canonical, true)?,
        retained(),
        None,
        100,
    ))?;
    let canonical = db.load_session(&canonical.id)?;
    assert!(db.comparison_reviewed(&canonical, "comparison", "example.rs")?);
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    block_on(db.accept_github_viewed_intent(
        sync_target(&canonical),
        context,
        intent(&canonical, false)?,
        BTreeMap::new(),
        None,
        100,
    ))?;
    let canonical = db.load_session(&canonical.id)?;
    assert!(!db.comparison_reviewed(&canonical, "comparison", "example.rs")?);
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    Ok(())
}

#[test]
fn inferred_approval_does_not_approve_itself_after_subset_revocation() -> Result<()> {
    use std::collections::BTreeMap;
    let (_directory, db, canonical) = setup()?;
    let selected = scope(&db, &canonical, 1, 4, b"a", b"d")?;
    block_on(db.record_review_coverage(target(&selected)?))?;
    let selected = db.load_session(&selected.id)?;
    assert_eq!(
        db.comparison_reviewed_paths(&selected)?,
        BTreeSet::from([("comparison".into(), "example.rs".into())])
    );
    let context = ReviewOperationContext {
        selector: canonical.selector.clone(),
        metadata: metadata(),
    };
    let intent = |session: &ReviewSession, viewed| -> Result<GitHubViewedIntent> {
        Ok(GitHubViewedIntent {
            origin: None,
            paths: vec!["example.rs".into()],
            viewed,
            fingerprints: BTreeMap::from([(
                "example.rs".into(),
                session
                    .snapshot
                    .file("comparison", "example.rs")?
                    .fingerprint()?,
            )]),
        })
    };
    let sync_target = |session: &ReviewSession| GitHubViewedSyncTarget {
        session: session.id.clone(),
        expected_revision: session.revision,
        displayed_generation: session.snapshot.generation,
    };
    let retained = || {
        BTreeMap::from([(
            "example.rs".into(),
            RetainedContents {
                old: Some(b"a".to_vec()),
                new: Some(b"d".to_vec()),
            },
        )])
    };
    let coverage_epoch = db.review_coverage_revision(&canonical)?;
    block_on(db.accept_covered_github_viewed_intent(
        sync_target(&canonical),
        context.clone(),
        intent(&canonical, true)?,
        retained(),
        None,
        100,
        db.review_coverage_revision(&canonical)?,
    ))?;
    let canonical = db.load_session(&canonical.id)?;
    assert_eq!(db.review_coverage_revision(&canonical)?, coverage_epoch);
    assert_eq!(
        db.coverage_promoted_paths(&canonical)?,
        BTreeSet::from(["example.rs".into()])
    );
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    block_on(db.revoke_review_coverage(target(&selected)?))?;
    let selected = db.load_session(&selected.id)?;
    assert!(db.comparison_reviewed_paths(&selected)?.is_empty());
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    block_on(db.accept_covered_github_viewed_intent(
        sync_target(&canonical),
        context.clone(),
        intent(&canonical, false)?,
        BTreeMap::new(),
        None,
        100,
        db.review_coverage_revision(&canonical)?,
    ))?;
    let canonical = db.load_session(&canonical.id)?;
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    // An explicit full-file review takes ownership and must survive subset unmark.
    block_on(db.accept_github_viewed_intent(
        sync_target(&canonical),
        context.clone(),
        intent(&canonical, true)?,
        retained(),
        None,
        100,
    ))?;
    let canonical = db.load_session(&canonical.id)?;
    assert!(db.coverage_promoted_paths(&canonical)?.is_empty());
    block_on(db.revoke_review_coverage(target(&selected)?))?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    assert!(
        block_on(db.accept_covered_github_viewed_intent(
            sync_target(&canonical),
            context,
            intent(&canonical, false)?,
            BTreeMap::new(),
            None,
            100,
            db.review_coverage_revision(&canonical)?,
        ))
        .is_err()
    );
    Ok(())
}

#[test]
fn bulk_coverage_is_atomic_and_survives_restart() -> Result<()> {
    let (directory, db, canonical) = setup()?;
    let mut manifest = snapshot(1, 4, b"a", b"d");
    let mut second = manifest.groups[0].files[0].clone();
    second.old.as_mut().unwrap().path = "second.rs".into();
    second.new.as_mut().unwrap().path = "second.rs".into();
    manifest.groups[0].files.push(second);
    let canonical = block_on(db.deploy_pull_request(
        canonical.repository.id.clone(),
        identity("U_reviewer"),
        metadata(),
        manifest.clone(),
        Some(canonical.revision),
    ))?;
    manifest.generation = 1;
    let selected = block_on(db.create_pull_request_scope(
        canonical.repository.id.clone(),
        identity("U_reviewer"),
        metadata(),
        oid(1),
        oid(4),
        PullRequestCommitScope::Commits {
            selections: vec![CommitSelection {
                commit: oid(4),
                parent: Some(oid(1)),
            }],
        },
        manifest,
    ))?;
    let first = target(&selected)?;
    let mut second = first.clone();
    second.path = "second.rs".into();
    second.fingerprint = selected
        .snapshot
        .file("comparison", "second.rs")?
        .fingerprint()?;
    let mut stale = second.clone();
    stale.expected_revision += 1;
    assert!(block_on(db.set_review_coverage(vec![first.clone(), stale], true)).is_err());
    assert!(db.comparison_reviewed_paths(&selected)?.is_empty());
    let revision = block_on(db.set_review_coverage(vec![first, second], true))?;
    assert_eq!(revision, selected.revision + 1);
    drop(db);
    let db = open(&directory.path().join("review.sqlite"))?;
    let selected = db.load_session(&selected.id)?;
    assert_eq!(
        db.comparison_reviewed_paths(&selected)?,
        BTreeSet::from([
            ("comparison".into(), "example.rs".into()),
            ("comparison".into(), "second.rs".into())
        ])
    );
    assert_eq!(
        db.reviewed_files_coverage(
            &canonical,
            &BTreeSet::from(["example.rs".into(), "second.rs".into()]),
            &history()
        )?,
        BTreeSet::from(["example.rs".into(), "second.rs".into()])
    );
    Ok(())
}

#[test]
fn manual_coverage_retains_validated_contents_and_bulk_unmark_clears_checkpoint() -> Result<()> {
    use std::collections::BTreeMap;
    let (_directory, db, canonical) = setup()?;
    let selected = scope(&db, &canonical, 1, 4, b"a", b"d")?;
    let key = ("comparison".into(), "example.rs".into());
    let bad = RetainedContents {
        old: Some(b"a".to_vec()),
        new: Some(b"wrong".to_vec()),
    };
    assert!(
        block_on(db.set_review_coverage_with_contents(
            vec![target(&selected)?],
            true,
            BTreeMap::from([(key.clone(), bad)]),
            100
        ))
        .is_err()
    );
    assert!(db.comparison_reviewed_paths(&selected)?.is_empty());
    assert!(
        db.checkpoint(&selected.id, "comparison", "example.rs")?
            .is_none()
    );
    let bytes = RetainedContents {
        old: Some(b"a".to_vec()),
        new: Some(b"d".to_vec()),
    };
    block_on(db.set_review_coverage_with_contents(
        vec![target(&selected)?],
        true,
        BTreeMap::from([(key, bytes.clone())]),
        100,
    ))?;
    let selected = db.load_session(&selected.id)?;
    assert!(db.comparison_reviewed(&selected, "comparison", "example.rs")?);
    assert_eq!(
        db.checkpoint(&selected.id, "comparison", "example.rs")?
            .context("Manual reviewed checkpoint missing")?
            .1,
        bytes
    );
    block_on(db.set_review_coverage(vec![target(&selected)?], false))?;
    let selected = db.load_session(&selected.id)?;
    assert!(db.comparison_reviewed_paths(&selected)?.is_empty());
    assert!(
        db.checkpoint(&selected.id, "comparison", "example.rs")?
            .is_none()
    );
    Ok(())
}

#[test]
fn inferred_acceptance_rejects_concurrent_scope_revocation_without_side_effects() -> Result<()> {
    use std::collections::BTreeMap;
    let (_directory, db, canonical) = setup()?;
    let selected = scope(&db, &canonical, 1, 4, b"a", b"d")?;
    block_on(db.record_review_coverage(target(&selected)?))?;
    let selected = db.load_session(&selected.id)?;
    let epoch = db.review_coverage_revision(&canonical)?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    block_on(db.revoke_review_coverage(target(&selected)?))?;
    assert!(db.review_coverage_revision(&canonical)? > epoch);
    assert_eq!(db.load_session(&canonical.id)?.revision, canonical.revision);
    let intent = GitHubViewedIntent {
        origin: None,
        paths: vec!["example.rs".into()],
        viewed: true,
        fingerprints: BTreeMap::from([(
            "example.rs".into(),
            canonical
                .snapshot
                .file("comparison", "example.rs")?
                .fingerprint()?,
        )]),
    };
    let result = block_on(db.accept_covered_github_viewed_intent(
        GitHubViewedSyncTarget {
            session: canonical.id.clone(),
            expected_revision: canonical.revision,
            displayed_generation: canonical.snapshot.generation,
        },
        ReviewOperationContext {
            selector: canonical.selector.clone(),
            metadata: metadata(),
        },
        intent,
        BTreeMap::from([(
            "example.rs".into(),
            RetainedContents {
                old: Some(b"a".to_vec()),
                new: Some(b"d".to_vec()),
            },
        )]),
        None,
        100,
        epoch,
    ));
    assert!(result.unwrap_err().downcast_ref::<StaleReview>().is_some());
    assert!(
        db.checkpoint(&canonical.id, "comparison", "example.rs")?
            .is_none()
    );
    assert!(db.coverage_promoted_paths(&canonical)?.is_empty());
    assert!(
        db.operations(canonical.selector.pull_request_identity().unwrap())?
            .is_empty()
    );
    assert_eq!(db.load_session(&canonical.id)?.revision, canonical.revision);
    Ok(())
}

#[test]
fn legacy_explicit_baseline_survives_inferred_promotion_revoke_and_new_head() -> Result<()> {
    use std::collections::BTreeMap;
    let (_directory, db, initial) = setup()?;
    let mut old_metadata = metadata();
    old_metadata.head_ref_oid = oid(2).hex;
    let old = block_on(db.deploy_pull_request(
        initial.repository.id.clone(),
        identity("U_reviewer"),
        old_metadata,
        snapshot(1, 2, b"a", b"b"),
        Some(initial.revision),
    ))?;
    block_on(db.mark_viewed(
        target(&old)?,
        RetainedContents {
            old: Some(b"a".to_vec()),
            new: Some(b"b".to_vec()),
        },
        100,
    ))?;
    // Simulate a valid pre-feature checkpoint with no transition receipt.
    block_on(db.write(|connection| connection.exec("DELETE FROM review_coverage")?()))?;
    let old = db.load_session(&old.id)?;
    let canonical = block_on(db.deploy_pull_request(
        old.repository.id.clone(),
        identity("U_reviewer"),
        metadata(),
        snapshot(1, 4, b"a", b"d"),
        Some(old.revision),
    ))?;
    let tail = scope(&db, &canonical, 2, 4, b"b", b"d")?;
    block_on(db.record_review_coverage(target(&tail)?))?;
    let epoch = db.review_coverage_revision(&canonical)?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    block_on(
        db.accept_covered_github_viewed_intent(
            GitHubViewedSyncTarget {
                session: canonical.id.clone(),
                expected_revision: canonical.revision,
                displayed_generation: canonical.snapshot.generation,
            },
            ReviewOperationContext {
                selector: canonical.selector.clone(),
                metadata: metadata(),
            },
            GitHubViewedIntent {
                origin: None,
                paths: vec!["example.rs".into()],
                viewed: true,
                fingerprints: BTreeMap::from([(
                    "example.rs".into(),
                    canonical
                        .snapshot
                        .file("comparison", "example.rs")?
                        .fingerprint()?,
                )]),
            },
            BTreeMap::from([(
                "example.rs".into(),
                RetainedContents {
                    old: Some(b"a".to_vec()),
                    new: Some(b"d".to_vec()),
                },
            )]),
            None,
            100,
            epoch,
        ),
    )?;
    let canonical = db.load_session(&canonical.id)?;
    assert_eq!(db.review_coverage_revision(&canonical)?, epoch);
    let tail = db.load_session(&tail.id)?;
    block_on(db.revoke_review_coverage(target(&tail)?))?;
    assert!(!db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    let tail = db.load_session(&tail.id)?;
    block_on(db.record_review_coverage(target(&tail)?))?;
    assert!(db.reviewed_file_coverage(&canonical, "example.rs", &history())?);
    let mut new_metadata = metadata();
    new_metadata.head_ref_oid = oid(5).hex;
    let newer = block_on(db.deploy_pull_request(
        canonical.repository.id.clone(),
        identity("U_reviewer"),
        new_metadata.clone(),
        snapshot(1, 5, b"a", b"e"),
        Some(canonical.revision),
    ))?;
    let new_tail = block_on(db.create_pull_request_scope(
        newer.repository.id.clone(),
        identity("U_reviewer"),
        new_metadata,
        oid(1),
        oid(5),
        PullRequestCommitScope::Commits {
            selections: vec![CommitSelection {
                commit: oid(5),
                parent: Some(oid(4)),
            }],
        },
        snapshot(4, 5, b"d", b"e"),
    ))?;
    block_on(db.record_review_coverage(target(&new_tail)?))?;
    let history = (1..=5).map(|value| oid(value).hex).collect();
    assert!(db.reviewed_file_coverage(&newer, "example.rs", &history)?);
    Ok(())
}
