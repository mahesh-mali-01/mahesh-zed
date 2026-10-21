use super::*;
use crate::review::*;
use db::sqlez::{domain::Migrator as _, thread_safe_connection::ThreadSafeConnection};
use futures::executor::block_on;
use std::{fs, process::Command};

fn connection(path: &std::path::Path) -> Result<ThreadSafeConnection> {
    block_on(
        ThreadSafeConnection::builder::<ReviewDb>(path.to_str().context("database path")?, true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=1000;")
            .build(),
    )
}

fn file(bytes: &[u8]) -> FileComparison {
    FileComparison {
        kind: ComparisonKind::Full,
        old: Some(FileSide {
            path: "auth.ts".into(),
            object: ObjectId::for_blob(ObjectFormat::Sha1, b"base\n"),
            mode: 0o100644,
        }),
        new: Some(FileSide {
            path: "auth.ts".into(),
            object: ObjectId::for_blob(ObjectFormat::Sha1, bytes),
            mode: 0o100644,
        }),
    }
}

fn snapshot(generation: u64, bytes: &[u8]) -> ReviewSnapshot {
    ReviewSnapshot {
        generation,
        incomplete_reason: None,
        groups: vec![ComparisonGroup {
            endpoints: ResolvedEndpoints {
                group: "branch".into(),
                left: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "1".repeat(40),
                },
                right: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: format!("{:040x}", generation + 1),
                },
            },
            files: vec![file(bytes)],
        }],
    }
}

fn setup() -> Result<(tempfile::TempDir, ReviewDb, ReviewSession)> {
    let directory = tempfile::tempdir()?;
    let db = ReviewDb::from_connection(connection(&directory.path().join("review.sqlite"))?)?;
    let git = directory.path().join("git");
    fs::create_dir(&git)?;
    let binding = block_on(db.bind_repository(git.clone(), git, ObjectFormat::Sha1))?;
    let session = block_on(db.create_session(
        binding.id,
        ComparisonSelector::Branch {
            base_ref: "main".into(),
            head_ref: "feature".into(),
        },
        snapshot(1, b"reviewed\r\n\0"),
    ))?;
    Ok((directory, db, session))
}

fn target(session: &ReviewSession) -> Result<ViewedTarget> {
    Ok(ViewedTarget {
        session: session.id.clone(),
        expected_revision: session.revision,
        displayed_generation: session.snapshot.generation,
        group: "branch".into(),
        path: "auth.ts".into(),
        fingerprint: session.snapshot.file("branch", "auth.ts")?.fingerprint()?,
    })
}

fn contents(bytes: &[u8]) -> RetainedContents {
    RetainedContents {
        old: Some(b"base\n".to_vec()),
        new: Some(bytes.to_vec()),
    }
}

#[test]
fn checkpoint_survives_process_restart() -> Result<()> {
    if let Ok(path) = std::env::var("ZED_REVIEW_REOPEN_DB") {
        let db = ReviewDb::from_connection(connection(std::path::Path::new(&path))?)?;
        let id = std::env::var("ZED_REVIEW_REOPEN_SESSION")?;
        let session = db.load_session(&id)?;
        assert_eq!(session.revision, 2);
        assert!(session.presentation.unreviewed_only);
        assert_eq!(session.presentation.split_view, Some(true));
        let (checkpoint, retained) = db
            .checkpoint(&id, "branch", "auth.ts")?
            .context("missing reopened checkpoint")?;
        assert!(checkpoint.matches("branch", session.snapshot.file("branch", "auth.ts")?)?);
        assert_eq!(retained, contents(b"reviewed\r\n\0"));
        let mut stale = target(&session)?;
        stale.expected_revision = 0;
        let error =
            block_on(db.mark_unviewed(stale)).expect_err("cross-process stale command must fail");
        assert!(error.downcast_ref::<StaleReview>().is_some());
        return Ok(());
    }
    let (directory, db, session) = setup()?;
    assert_eq!(
        block_on(db.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 100))?,
        1
    );
    block_on(db.save_presentation(
        session.id.clone(),
        1,
        ReviewPresentation {
            unreviewed_only: true,
            split_view: Some(true),
            selected_group: Some("branch".into()),
            selected_path: Some("auth.ts".into()),
            ..Default::default()
        },
    ))?;
    drop(db);
    #[expect(
        clippy::disallowed_methods,
        reason = "An isolated synchronous test must reopen SQLite in a genuinely fresh process"
    )]
    let result = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "review_store::tests::checkpoint_survives_process_restart",
            "--nocapture",
        ])
        .env(
            "ZED_REVIEW_REOPEN_DB",
            directory.path().join("review.sqlite"),
        )
        .env("ZED_REVIEW_REOPEN_SESSION", session.id)
        .output()?;
    ensure!(
        result.status.success(),
        "fresh process reopen failed: {} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&result.stdout).contains("1 passed"),
        "reopen child did not execute its test"
    );
    Ok(())
}

#[test]
fn both_refresh_viewed_orderings_are_safe() -> Result<()> {
    let (_directory, db, original) = setup()?;
    let stale_target = target(&original)?;
    block_on(db.activate_snapshot(original.id.clone(), 0, snapshot(2, b"changed\n")))?;
    let error = block_on(db.mark_viewed(stale_target, contents(b"reviewed\r\n\0"), 100))
        .expect_err("stale click must fail");
    assert!(error.downcast_ref::<StaleReview>().is_some());
    assert!(db.checkpoint(&original.id, "branch", "auth.ts")?.is_none());
    let current = db.load_session(&original.id)?;
    block_on(db.mark_viewed(target(&current)?, contents(b"changed\n"), 100))?;
    assert!(
        block_on(db.activate_snapshot(
            original.id.clone(),
            current.revision,
            snapshot(3, b"next\n")
        ))
        .is_err()
    );
    block_on(db.activate_snapshot(
        original.id.clone(),
        current.revision + 1,
        snapshot(3, b"next\n"),
    ))?;
    let refreshed = db.load_session(&original.id)?;
    let (checkpoint, _) = db
        .checkpoint(&original.id, "branch", "auth.ts")?
        .context("checkpoint lost")?;
    assert!(!checkpoint.matches("branch", refreshed.snapshot.file("branch", "auth.ts")?)?);
    Ok(())
}

#[test]
fn unrelated_refresh_keeps_approval_and_latest_checkpoint_wins() -> Result<()> {
    let (_directory, db, session) = setup()?;
    block_on(db.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 100))?;
    block_on(db.activate_snapshot(session.id.clone(), 1, snapshot(2, b"reviewed\r\n\0")))?;
    let current = db.load_session(&session.id)?;
    let (checkpoint, _) = db
        .checkpoint(&session.id, "branch", "auth.ts")?
        .context("checkpoint")?;
    assert!(checkpoint.matches("branch", current.snapshot.file("branch", "auth.ts")?)?);
    block_on(db.activate_snapshot(session.id.clone(), 2, snapshot(3, b"new\n")))?;
    let current = db.load_session(&session.id)?;
    block_on(db.mark_viewed(target(&current)?, contents(b"new\n"), 100))?;
    block_on(db.activate_snapshot(session.id.clone(), 4, snapshot(4, b"reviewed\r\n\0")))?;
    let current = db.load_session(&session.id)?;
    let (latest, _) = db
        .checkpoint(&session.id, "branch", "auth.ts")?
        .context("latest checkpoint")?;
    assert_eq!(latest.generation, 3);
    assert!(!latest.matches("branch", current.snapshot.file("branch", "auth.ts")?)?);
    Ok(())
}

#[test]
fn write_failure_rolls_back_retained_blobs_checkpoint_and_revision() -> Result<()> {
    let (_directory, db, session) = setup()?;
    block_on(db.write(|connection| connection.exec(
        "CREATE TRIGGER reject_revision BEFORE UPDATE ON review_sessions BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"
    )?()))?;
    assert!(block_on(db.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 100)).is_err());
    assert_eq!(db.load_session(&session.id)?.revision, 0);
    assert_eq!(
        db.select_row::<i64>("SELECT count(*) FROM review_contents")?()?,
        Some(0)
    );
    assert!(db.checkpoint(&session.id, "branch", "auth.ts")?.is_none());
    assert!(
        block_on(db.activate_snapshot(session.id.clone(), 0, snapshot(2, b"changed\n"))).is_err()
    );
    assert_eq!(
        db.select_row::<i64>("SELECT count(*) FROM review_snapshots")?()?,
        Some(1)
    );
    assert_eq!(db.load_session(&session.id)?, session);
    block_on(db.write(|connection| connection.exec("DROP TRIGGER reject_revision")?()))?;
    block_on(db.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 100))?;
    assert_eq!(db.load_session(&session.id)?.revision, 1);
    Ok(())
}

#[test]
fn invalid_content_budget_and_incomplete_refresh_preserve_state() -> Result<()> {
    let (_directory, db, session) = setup()?;
    assert!(block_on(db.mark_viewed(target(&session)?, contents(b"wrong\n"), 100)).is_err());
    assert!(block_on(db.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 1)).is_err());
    let mut incomplete = snapshot(2, b"other\n");
    incomplete.incomplete_reason = Some("object unavailable".into());
    assert!(block_on(db.activate_snapshot(session.id.clone(), 0, incomplete)).is_err());
    assert_eq!(db.load_session(&session.id)?, session);
    assert_eq!(
        db.select_row::<i64>("SELECT count(*) FROM review_contents")?()?,
        Some(0)
    );
    Ok(())
}

#[test]
fn two_connections_reject_stale_writes_and_keep_sessions_isolated() -> Result<()> {
    let (directory, db, session) = setup()?;
    let other = ReviewDb::from_connection(connection(&directory.path().join("review.sqlite"))?)?;
    let stale = other.load_session(&session.id)?;
    block_on(db.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 100))?;
    assert!(block_on(other.mark_unviewed(target(&stale)?)).is_err());
    let separate = block_on(other.create_session(
        session.repository.id.clone(),
        session.selector.clone(),
        session.snapshot.clone(),
    ))?;
    assert!(
        other
            .checkpoint(&separate.id, "branch", "auth.ts")?
            .is_none()
    );
    assert!(
        other
            .checkpoint(&session.id, "branch", "auth.ts")?
            .is_some()
    );
    let current = other.load_session(&session.id)?;
    block_on(other.mark_unviewed(target(&current)?))?;
    assert!(db.checkpoint(&session.id, "branch", "auth.ts")?.is_none());
    Ok(())
}

#[test]
#[cfg(unix)]
fn repository_bindings_canonicalize_and_separate_linked_worktrees() -> Result<()> {
    let (directory, db, original) = setup()?;
    let common = directory.path().join("git");
    let alias = directory.path().join("git-alias");
    std::os::unix::fs::symlink(&common, &alias)?;
    let same = block_on(db.bind_repository(alias.clone(), alias, ObjectFormat::Sha1))?;
    assert_eq!(same.id, original.repository.id);
    let linked = directory.path().join("linked-git-dir");
    fs::create_dir(&linked)?;
    let linked = block_on(db.bind_repository(common.clone(), linked, ObjectFormat::Sha1))?;
    assert_ne!(linked.id, same.id);
    let clone = directory.path().join("clone-git-dir");
    fs::create_dir(&clone)?;
    let clone = block_on(db.bind_repository(clone.clone(), clone, ObjectFormat::Sha1))?;
    assert_ne!(clone.id, same.id);
    assert!(block_on(db.bind_repository(common.clone(), common, ObjectFormat::Sha256)).is_err());
    Ok(())
}

#[test]
fn memory_fallback_and_failed_migrations_cannot_claim_durability() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let blocker = directory.path().join("file-not-directory");
    fs::write(&blocker, b"block directory creation")?;
    let fallback = block_on(db::open_db::<ReviewDb>(
        &blocker,
        release_channel::ReleaseChannel::Dev,
    ));
    assert!(ReviewDb::from_connection(fallback).is_err());
    struct BrokenDomain;
    impl Domain for BrokenDomain {
        const NAME: &'static str = "BrokenReviewMigration";
        const MIGRATIONS: &'static [&'static str] =
            &["CREATE TABLE should_rollback(id INTEGER); INVALID SQL;"];
    }
    let result = block_on(
        ThreadSafeConnection::builder::<BrokenDomain>(
            directory
                .path()
                .join("broken.sqlite")
                .to_str()
                .context("path")?,
            true,
        )
        .build(),
    );
    assert!(result.is_err());
    let reopened = Connection::open_file(
        directory
            .path()
            .join("broken.sqlite")
            .to_str()
            .context("path")?,
    );
    assert_eq!(
        reopened.select_row::<i64>(
            "SELECT count(*) FROM sqlite_master WHERE name = 'should_rollback'"
        )?()?,
        Some(0)
    );
    Ok(())
}

#[test]
fn migration_is_repeatable_and_has_no_workspace_cascade() -> Result<()> {
    let (_directory, db, session) = setup()?;
    block_on(db.write(|connection| ReviewDb::migrate(connection)))?;
    assert_eq!(db.load_session(&session.id)?, session);
    let foreign_keys = db.select::<(i64, i64, String, String, String, String, String, String)>(
        "PRAGMA foreign_key_list(review_sessions)",
    )?()?;
    assert!(foreign_keys.iter().all(|row| row.2 != "workspaces"));
    assert!(
        foreign_keys
            .iter()
            .any(|row| row.2 == "review_repositories")
    );
    Ok(())
}

#[test]
fn corrupt_retained_data_is_reported_instead_of_approved() -> Result<()> {
    let (_directory, db, session) = setup()?;
    block_on(db.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 100))?;
    block_on(
        db.write(|connection| connection.exec("UPDATE review_contents SET bytes = X'00'")?()),
    )?;
    assert!(db.checkpoint(&session.id, "branch", "auth.ts").is_err());
    Ok(())
}

#[test]
fn retained_checkpoint_survives_real_git_garbage_collection() -> Result<()> {
    use std::io::Write as _;
    use std::process::Stdio;
    let (directory, db, initial) = setup()?;
    let repository = directory.path().join("source");
    fs::create_dir(&repository)?;
    let git = |arguments: &[&str], input: &[u8]| -> Result<Vec<u8>> {
        let mut command = Command::new("git");
        command.current_dir(&repository).args(arguments);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "This isolated synchronous test exercises real Git garbage collection with piped input and output"
        )]
        let mut child = command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_AUTHOR_NAME", "Review test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Review test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().context("git stdin")?.write_all(input)?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output.stdout)
    };
    git(&["init", "-q", "--initial-branch=main"], b"")?;
    let bytes = b"unique retained content\r\n\0";
    let blob = String::from_utf8(git(&["hash-object", "-w", "--stdin"], bytes)?)?
        .trim()
        .to_owned();
    assert_eq!(blob, ObjectId::for_blob(ObjectFormat::Sha1, bytes).hex);
    let tree_input = format!("100644 blob {blob}\tauth.ts\n");
    let tree = String::from_utf8(git(&["mktree"], tree_input.as_bytes())?)?
        .trim()
        .to_owned();
    let commit = String::from_utf8(git(
        &["commit-tree", &tree],
        b"unreferenced review snapshot",
    )?)?
    .trim()
    .to_owned();
    let mut candidate = snapshot(2, bytes);
    candidate.groups[0].endpoints.right.hex = commit;
    block_on(db.activate_snapshot(initial.id.clone(), 0, candidate))?;
    let current = db.load_session(&initial.id)?;
    block_on(db.mark_viewed(target(&current)?, contents(bytes), 100))?;
    git(&["cat-file", "-e", &blob], b"")?;
    // The orphan objects were created only in this disposable source repository.
    git(&["gc", "--prune=now"], b"")?;
    assert!(git(&["cat-file", "-e", &blob], b"").is_err());
    let (checkpoint, retained) = db
        .checkpoint(&initial.id, "branch", "auth.ts")?
        .context("retained checkpoint")?;
    assert_eq!(retained.new.as_deref(), Some(bytes.as_slice()));
    assert!(checkpoint.matches("branch", current.snapshot.file("branch", "auth.ts")?)?);
    Ok(())
}

#[test]
fn nontext_checkpoints_round_trip_without_conflating_empty_or_missing() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let side = |path: &str, bytes: &[u8], mode| FileSide {
        path: path.into(),
        object: ObjectId::for_blob(ObjectFormat::Sha1, bytes),
        mode,
    };
    let cases = vec![
        (
            FileComparison {
                kind: ComparisonKind::Full,
                old: None,
                new: Some(side("empty", b"", 0o100644)),
            },
            RetainedContents {
                old: None,
                new: Some(vec![]),
            },
        ),
        (
            FileComparison {
                kind: ComparisonKind::Full,
                old: Some(side("deleted", b"base\n", 0o100644)),
                new: None,
            },
            RetainedContents {
                old: Some(b"base\n".to_vec()),
                new: None,
            },
        ),
        (
            FileComparison {
                kind: ComparisonKind::Full,
                old: Some(side("mode", b"base\n", 0o100644)),
                new: Some(side("mode", b"base\n", 0o100755)),
            },
            RetainedContents {
                old: Some(b"base\n".to_vec()),
                new: Some(b"base\n".to_vec()),
            },
        ),
        (
            FileComparison {
                kind: ComparisonKind::Full,
                old: None,
                new: Some(side("link", b"target.txt", 0o120000)),
            },
            RetainedContents {
                old: None,
                new: Some(b"target.txt".to_vec()),
            },
        ),
        (
            FileComparison {
                kind: ComparisonKind::Full,
                old: None,
                new: Some(side("module", b"commit identity", 0o160000)),
            },
            RetainedContents {
                old: None,
                new: None,
            },
        ),
        (
            FileComparison {
                kind: ComparisonKind::Full,
                old: None,
                new: Some(side("binary", b"\0hello", 0o100644)),
            },
            RetainedContents {
                old: None,
                new: Some(b"\0hello".to_vec()),
            },
        ),
    ];
    let mut candidate = snapshot(2, b"unused");
    candidate.groups[0].files = cases.iter().map(|(file, _)| file.clone()).collect();
    block_on(db.activate_snapshot(session.id.clone(), 0, candidate))?;
    for (file, contents) in cases {
        let current = db.load_session(&session.id)?;
        let target = ViewedTarget {
            session: session.id.clone(),
            expected_revision: current.revision,
            displayed_generation: 2,
            group: "branch".into(),
            path: file.file_key()?.into(),
            fingerprint: file.fingerprint()?,
        };
        block_on(db.mark_viewed(target, contents.clone(), 100))?;
        let (checkpoint, restored) = db
            .checkpoint(&session.id, "branch", file.file_key()?)?
            .context("typed checkpoint")?;
        assert_eq!(restored, contents);
        assert!(checkpoint.matches("branch", &file)?);
    }
    assert_eq!(
        db.select_row::<i64>("SELECT count(*) FROM review_contents")?()?,
        Some(4)
    );
    Ok(())
}

#[test]
fn review_item_migration_preserves_m3_progress_and_isolates_mappings() -> Result<()> {
    struct M3Domain;
    impl Domain for M3Domain {
        const NAME: &'static str = "ReviewDb";
        const MIGRATIONS: &'static [&'static str] = &[ReviewDb::MIGRATIONS[0]];
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("m3.sqlite");
    let old_connection = block_on(
        ThreadSafeConnection::builder::<M3Domain>(path.to_str().context("path")?, true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
            .build(),
    )?;
    let prior = ReviewDb::from_connection(old_connection)?;
    let git = directory.path().join("git");
    fs::create_dir(&git)?;
    let binding = block_on(prior.bind_repository(git.clone(), git, ObjectFormat::Sha1))?;
    let session = block_on(prior.create_session(
        binding.id,
        ComparisonSelector::Branch {
            base_ref: "main".into(),
            head_ref: "feature".into(),
        },
        snapshot(1, b"reviewed\r\n\0"),
    ))?;
    block_on(prior.mark_viewed(target(&session)?, contents(b"reviewed\r\n\0"), 100))?;
    let upgraded = ReviewDb::from_connection(connection(&path)?)?;
    let found = upgraded
        .find_session(&session.repository.id, &session.selector)?
        .context("existing session")?;
    assert_eq!(found.id, session.id);
    assert_eq!(found.revision, 1);
    assert!(
        upgraded
            .checkpoint(&session.id, "branch", "auth.ts")?
            .is_some()
    );
    let first = workspace::WorkspaceId::from_i64(11);
    let second = workspace::WorkspaceId::from_i64(12);
    block_on(upgraded.save_item(first, 22, session.id.clone()))?;
    assert_eq!(upgraded.item_session(first, 22)?.id, session.id);
    assert!(upgraded.item_session(second, 22).is_err());
    block_on(upgraded.write(|connection| connection.exec("DELETE FROM review_items")?()))?;
    assert_eq!(upgraded.load_session(&session.id)?.revision, 1);
    assert!(
        upgraded
            .checkpoint(&session.id, "branch", "auth.ts")?
            .is_some()
    );
    Ok(())
}

fn pull_request_identity(viewer: &str) -> PullRequestIdentity {
    PullRequestIdentity {
        host: "github.com".into(),
        repository_node_id: "R_repository".into(),
        pull_request_node_id: "PR_request".into(),
        viewer_node_id: viewer.into(),
    }
}
fn pull_request_metadata() -> PullRequestMetadata {
    PullRequestMetadata {
        owner: "owner".into(),
        repository: "repository".into(),
        number: 42,
        title: "Review feature".into(),
        base_ref_name: "main".into(),
        head_ref_name: "feature".into(),
        base_ref_oid: "1".repeat(40),
        head_ref_oid: format!("{:040x}", 2),
        commit_count: Some(3),
    }
}

#[test]
fn pull_request_reopen_activates_new_head_and_retains_reviewed_content() -> Result<()> {
    let (_directory, db, legacy) = setup()?;
    let identity = pull_request_identity("U_viewer");
    let first = block_on(db.deploy_pull_request(
        legacy.repository.id.clone(),
        identity.clone(),
        pull_request_metadata(),
        snapshot(1, b"reviewed\r\n\0"),
        None,
    ))?;
    assert_ne!(first.id, legacy.id);
    block_on(db.mark_viewed(target(&first)?, contents(b"reviewed\r\n\0"), 100))?;
    let prior = db
        .find_pull_request_session(&first.repository.id, &identity)?
        .context("existing PR")?;
    let mut metadata = pull_request_metadata();
    metadata.title = "Renamed PR".into();
    metadata.head_ref_name = "renamed-branch".into();
    metadata.head_ref_oid = "3".repeat(40);
    let mut next = snapshot(1, b"reviewed\r\n\0");
    next.groups[0].endpoints.right.hex = "3".repeat(40);
    let updated = block_on(db.deploy_pull_request(
        first.repository.id.clone(),
        identity.clone(),
        metadata.clone(),
        next.clone(),
        Some(prior.revision),
    ))?;
    assert_eq!(updated.id, first.id);
    assert_eq!(updated.snapshot.generation, 2);
    assert_eq!(updated.snapshot.groups, next.groups);
    assert_eq!(db.pull_request_metadata(&first.id)?, Some(metadata.clone()));
    let (checkpoint, retained) = db
        .checkpoint(&first.id, "branch", "auth.ts")?
        .context("retained checkpoint")?;
    assert!(checkpoint.matches("branch", updated.snapshot.file("branch", "auth.ts")?)?);
    assert_eq!(retained, contents(b"reviewed\r\n\0"));
    let stale = block_on(db.deploy_pull_request(
        first.repository.id.clone(),
        identity.clone(),
        metadata.clone(),
        snapshot(1, b"different"),
        Some(prior.revision),
    ))
    .unwrap_err();
    assert!(stale.downcast_ref::<StaleReview>().is_some());
    let same = block_on(db.deploy_pull_request(
        first.repository.id.clone(),
        identity.clone(),
        metadata,
        next,
        Some(updated.revision),
    ))?;
    assert_eq!(same.snapshot.generation, 2);
    assert_eq!(same.revision, updated.revision);
    let changed = block_on(db.deploy_pull_request(
        first.repository.id.clone(),
        identity,
        pull_request_metadata(),
        snapshot(1, b"changed content"),
        Some(same.revision),
    ))?;
    assert_eq!(changed.snapshot.generation, 3);
    let (checkpoint, retained) = db
        .checkpoint(&first.id, "branch", "auth.ts")?
        .context("prior reviewed bytes retained")?;
    assert!(!checkpoint.matches("branch", changed.snapshot.file("branch", "auth.ts")?)?);
    assert_eq!(retained, contents(b"reviewed\r\n\0"));
    assert_eq!(db.load_session(&legacy.id)?, legacy);
    assert!(db.pull_request_metadata(&legacy.id)?.is_none());
    Ok(())
}

#[test]
fn pull_request_account_and_checkout_are_isolated_and_duplicate_creation_is_stale() -> Result<()> {
    let (directory, db, legacy) = setup()?;
    let first = block_on(db.deploy_pull_request(
        legacy.repository.id.clone(),
        pull_request_identity("U_one"),
        pull_request_metadata(),
        snapshot(1, b"reviewed\r\n\0"),
        None,
    ))?;
    block_on(db.mark_viewed(target(&first)?, contents(b"reviewed\r\n\0"), 100))?;
    let second = block_on(db.deploy_pull_request(
        legacy.repository.id.clone(),
        pull_request_identity("U_two"),
        pull_request_metadata(),
        snapshot(1, b"reviewed\r\n\0"),
        None,
    ))?;
    assert_ne!(first.id, second.id);
    assert!(db.checkpoint(&second.id, "branch", "auth.ts")?.is_none());
    let duplicate = block_on(db.deploy_pull_request(
        legacy.repository.id,
        pull_request_identity("U_one"),
        pull_request_metadata(),
        snapshot(1, b"unseen"),
        None,
    ))
    .unwrap_err();
    assert!(duplicate.downcast_ref::<StaleReview>().is_some());
    let another_path = directory.path().join("another-checkout");
    fs::create_dir(&another_path)?;
    let another =
        block_on(db.bind_repository(another_path.clone(), another_path, ObjectFormat::Sha1))?;
    assert!(
        db.find_pull_request_session(&another.id, &pull_request_identity("U_one"))?
            .is_none()
    );
    let other = block_on(db.deploy_pull_request(
        another.id,
        pull_request_identity("U_one"),
        pull_request_metadata(),
        snapshot(1, b"reviewed\r\n\0"),
        None,
    ))?;
    assert_ne!(first.id, other.id);
    assert!(db.checkpoint(&other.id, "branch", "auth.ts")?.is_none());
    assert_eq!(db.load_session(&first.id)?.snapshot.generation, 1);
    Ok(())
}

#[test]
fn pull_request_migration_preserves_legacy_sessions_and_checkpoints() -> Result<()> {
    struct M6Domain;
    impl Domain for M6Domain {
        const NAME: &'static str = "ReviewDb";
        const MIGRATIONS: &'static [&'static str] =
            &[ReviewDb::MIGRATIONS[0], ReviewDb::MIGRATIONS[1]];
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.sqlite");
    let previous = block_on(
        ThreadSafeConnection::builder::<M6Domain>(path.to_str().context("path")?, true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
            .build(),
    )?;
    let previous = ReviewDb::from_connection(previous)?;
    let git = directory.path().join("git");
    fs::create_dir(&git)?;
    let binding = block_on(previous.bind_repository(git.clone(), git, ObjectFormat::Sha1))?;
    // Deserialize the old representation directly: no implicit PR conversion.
    let selector: ComparisonSelector =
        serde_json::from_str(r#"{"kind":"branch","base_ref":"main","head_ref":"feature"}"#)?;
    let legacy =
        block_on(previous.create_session(binding.id, selector, snapshot(1, b"reviewed\r\n\0")))?;
    block_on(previous.mark_viewed(target(&legacy)?, contents(b"reviewed\r\n\0"), 100))?;
    let upgraded = ReviewDb::from_connection(connection(&path)?)?;
    let restored = upgraded.load_session(&legacy.id)?;
    assert_eq!(restored.selector, legacy.selector);
    assert_eq!(restored.revision, 1);
    assert!(
        upgraded
            .checkpoint(&legacy.id, "branch", "auth.ts")?
            .is_some()
    );
    assert!(upgraded.pull_request_metadata(&legacy.id)?.is_none());
    assert!(
        upgraded
            .find_pull_request_session(&legacy.repository.id, &pull_request_identity("U_one"))?
            .is_none()
    );
    Ok(())
}

#[test]
fn pull_request_identity_and_legacy_presentation_validate_without_conversion() -> Result<()> {
    let identity = pull_request_identity("U_viewer");
    identity.validate()?;
    let mut invalid = identity.clone();
    invalid.host = "github.com.evil".into();
    assert!(invalid.validate().is_err());
    invalid = identity;
    invalid.viewer_node_id.clear();
    assert!(invalid.validate().is_err());
    let presentation: ReviewPresentation = serde_json::from_str(
        r#"{"unreviewed_only":false,"split_view":true,"selected_group":"branch","selected_path":"auth.ts"}"#,
    )?;
    assert!(presentation.collapsed_directories.is_empty());
    assert!(presentation.multi_file.is_none());
    assert!(presentation.path_filter.is_empty());
    let old = r#"{"kind":"direct","left_ref":"main","right_ref":"feature"}"#;
    let selector: ComparisonSelector = serde_json::from_str(old)?;
    assert_eq!(serde_json::to_string(&selector)?, old);
    Ok(())
}

#[test]
fn pull_request_pinned_tips_distinguish_base_tip_from_merge_base_and_require_new_writes()
-> Result<()> {
    let old = r#"{"owner":"owner","repository":"repository","number":42,"title":"Old","base_ref_name":"main","head_ref_name":"feature"}"#;
    let metadata: PullRequestMetadata = serde_json::from_str(old)?;
    metadata.validate()?;
    assert!(metadata.validate_pinned_tips(ObjectFormat::Sha1).is_err());
    let mut metadata = pull_request_metadata();
    metadata.base_ref_oid = "9".repeat(40);
    metadata.validate_pinned_tips(ObjectFormat::Sha1)?;
    let (_directory, db, legacy) = setup()?;
    let pr = block_on(db.deploy_pull_request(
        legacy.repository.id,
        pull_request_identity("U_viewer"),
        metadata.clone(),
        snapshot(1, b"bytes"),
        None,
    ))?;
    assert_ne!(
        metadata.base_ref_oid,
        pr.snapshot.groups[0].endpoints.left.hex
    );
    assert_eq!(
        db.pull_request_metadata(&pr.id)?
            .context("PR metadata")?
            .base_ref_oid,
        "9".repeat(40)
    );
    let mut invalid = metadata;
    invalid.base_ref_oid.clear();
    assert!(invalid.validate().is_err());
    Ok(())
}

fn pr_scope_tips() -> (ObjectId, ObjectId) {
    (
        ObjectId {
            format: ObjectFormat::Sha1,
            hex: "1".repeat(40),
        },
        ObjectId {
            format: ObjectFormat::Sha1,
            hex: format!("{:040x}", 2),
        },
    )
}
fn pr_scope_selection() -> Vec<CommitSelection> {
    let (base, head) = pr_scope_tips();
    vec![CommitSelection {
        commit: head,
        parent: Some(base),
    }]
}

#[test]
fn pull_request_subset_range_full_and_accounts_never_share_viewed_checkpoints() -> Result<()> {
    let (_directory, db, legacy) = setup()?;
    let identity = pull_request_identity("U_viewer");
    let full = block_on(db.deploy_pull_request(
        legacy.repository.id.clone(),
        identity.clone(),
        pull_request_metadata(),
        snapshot(1, b"reviewed\r\n\0"),
        None,
    ))?;
    let (base, head) = pr_scope_tips();
    let scope = PullRequestCommitScope::Commits {
        selections: pr_scope_selection(),
    };
    let subset = block_on(db.create_pull_request_scope(
        legacy.repository.id.clone(),
        identity.clone(),
        pull_request_metadata(),
        base.clone(),
        head.clone(),
        scope.clone(),
        snapshot(1, b"reviewed\r\n\0"),
    ))?;
    assert_ne!(subset.id, full.id);
    assert!(full.selector.supports_github_viewed());
    assert!(!subset.selector.supports_github_viewed());
    assert_eq!(subset.selector.pull_request_identity(), Some(&identity));
    block_on(db.mark_viewed(target(&subset)?, contents(b"reviewed\r\n\0"), 100))?;
    assert!(db.checkpoint(&full.id, "branch", "auth.ts")?.is_none());
    let range = block_on(db.create_pull_request_scope(
        legacy.repository.id.clone(),
        identity.clone(),
        pull_request_metadata(),
        base.clone(),
        head.clone(),
        PullRequestCommitScope::Range {
            selections: pr_scope_selection(),
        },
        snapshot(1, b"reviewed\r\n\0"),
    ))?;
    assert_ne!(range.id, subset.id);
    assert!(!range.selector.supports_github_viewed());
    assert!(db.checkpoint(&range.id, "branch", "auth.ts")?.is_none());
    let other = block_on(db.create_pull_request_scope(
        legacy.repository.id.clone(),
        pull_request_identity("U_other"),
        pull_request_metadata(),
        base.clone(),
        head.clone(),
        scope.clone(),
        snapshot(1, b"reviewed\r\n\0"),
    ))?;
    assert_ne!(other.id, subset.id);
    assert!(db.checkpoint(&other.id, "branch", "auth.ts")?.is_none());
    let resumed = block_on(db.create_pull_request_scope(
        legacy.repository.id,
        identity,
        pull_request_metadata(),
        base,
        head,
        scope,
        snapshot(1, b"reviewed\r\n\0"),
    ))?;
    assert_eq!(resumed.id, subset.id);
    assert_eq!(resumed.revision, 1);
    assert!(db.checkpoint(&resumed.id, "branch", "auth.ts")?.is_some());
    Ok(())
}

#[test]
fn pull_request_scope_restores_pinned_base_tip_after_full_pr_refresh() -> Result<()> {
    let (directory, db, legacy) = setup()?;
    let identity = pull_request_identity("U_viewer");
    let mut metadata = pull_request_metadata();
    metadata.base_ref_oid = "9".repeat(40);
    let base = ObjectId {
        format: ObjectFormat::Sha1,
        hex: metadata.base_ref_oid.clone(),
    };
    let head = ObjectId {
        format: ObjectFormat::Sha1,
        hex: metadata.head_ref_oid.clone(),
    };
    let mut selected = snapshot(1, b"reviewed\r\n\0");
    // Selected commit parent is unrelated to the original full PR's base tip.
    selected.groups[0].endpoints.left.hex = "4".repeat(40);
    let scope = PullRequestCommitScope::Commits {
        selections: vec![CommitSelection {
            commit: head.clone(),
            parent: Some(selected.groups[0].endpoints.left.clone()),
        }],
    };
    let subset = block_on(db.create_pull_request_scope(
        legacy.repository.id.clone(),
        identity.clone(),
        metadata.clone(),
        base.clone(),
        head.clone(),
        scope.clone(),
        selected,
    ))?;
    let full = block_on(db.deploy_pull_request(
        legacy.repository.id.clone(),
        identity.clone(),
        metadata.clone(),
        snapshot(1, b"reviewed\r\n\0"),
        None,
    ))?;
    let mut refreshed = metadata.clone();
    refreshed.base_ref_oid = "8".repeat(40);
    refreshed.head_ref_oid = "3".repeat(40);
    refreshed.head_ref_name = "renamed".into();
    block_on(db.deploy_pull_request(
        legacy.repository.id,
        identity.clone(),
        refreshed,
        snapshot(2, b"new full PR content"),
        Some(full.revision),
    ))?;
    drop(db);
    let db = ReviewDb::from_connection(connection(&directory.path().join("review.sqlite"))?)?;
    let restored = db.load_session(&subset.id)?;
    assert_eq!(
        restored.selector,
        ComparisonSelector::PullRequestScope {
            identity,
            base_tip: base,
            head_tip: head,
            scope
        }
    );
    assert_eq!(db.pull_request_metadata(&subset.id)?, Some(metadata));
    assert_eq!(
        restored.snapshot.groups[0].endpoints.left.hex,
        "4".repeat(40)
    );
    assert_ne!(
        restored.snapshot.groups[0].endpoints.left.hex,
        "9".repeat(40)
    );
    Ok(())
}

#[test]
fn pull_request_scope_rejects_wrong_metadata_manifest_and_gap_ranges() -> Result<()> {
    let (_directory, db, legacy) = setup()?;
    let (base, head) = pr_scope_tips();
    let scope = PullRequestCommitScope::Commits {
        selections: pr_scope_selection(),
    };
    let mut metadata = pull_request_metadata();
    metadata.base_ref_oid = "9".repeat(40);
    assert!(
        block_on(db.create_pull_request_scope(
            legacy.repository.id.clone(),
            pull_request_identity("U_viewer"),
            metadata,
            base.clone(),
            head.clone(),
            scope.clone(),
            snapshot(1, b"bytes")
        ))
        .is_err()
    );
    let mut wrong = snapshot(1, b"bytes");
    wrong.groups[0].endpoints.right.hex = "3".repeat(40);
    assert!(
        block_on(db.create_pull_request_scope(
            legacy.repository.id.clone(),
            pull_request_identity("U_viewer"),
            pull_request_metadata(),
            base.clone(),
            head.clone(),
            scope.clone(),
            wrong
        ))
        .is_err()
    );
    let created = block_on(db.create_pull_request_scope(
        legacy.repository.id.clone(),
        pull_request_identity("U_viewer"),
        pull_request_metadata(),
        base.clone(),
        head.clone(),
        scope.clone(),
        snapshot(1, b"bytes"),
    ))?;
    assert!(
        block_on(db.create_pull_request_scope(
            legacy.repository.id,
            pull_request_identity("U_viewer"),
            pull_request_metadata(),
            base,
            head,
            scope,
            snapshot(1, b"different contents")
        ))
        .is_err()
    );
    assert_eq!(db.load_session(&created.id)?.snapshot.generation, 1);
    let mut gap = pr_scope_selection();
    gap.push(CommitSelection {
        commit: ObjectId {
            format: ObjectFormat::Sha1,
            hex: "3".repeat(40),
        },
        parent: Some(ObjectId {
            format: ObjectFormat::Sha1,
            hex: "4".repeat(40),
        }),
    });
    assert!(
        PullRequestCommitScope::Range { selections: gap }
            .validate(ObjectFormat::Sha1)
            .is_err()
    );
    Ok(())
}

#[test]
fn pull_request_scope_persists_more_than_2000_selections_without_truncation() -> Result<()> {
    let (_directory, db, legacy) = setup()?;
    let id = |index: usize| ObjectId {
        format: ObjectFormat::Sha1,
        hex: format!("{index:040x}"),
    };
    let selections = (2..=2053)
        .map(|index| CommitSelection {
            commit: id(index),
            parent: Some(id(index - 1)),
        })
        .collect::<Vec<_>>();
    let mut metadata = pull_request_metadata();
    metadata.base_ref_oid = id(1).hex;
    metadata.head_ref_oid = id(2053).hex;
    metadata.commit_count = Some(selections.len());
    let scope = PullRequestCommitScope::Range {
        selections: selections.clone(),
    };
    let mut manifest = snapshot(1, b"bytes");
    manifest.groups[0].endpoints.left = id(1);
    manifest.groups[0].endpoints.right = id(2053);
    let created = block_on(db.create_pull_request_scope(
        legacy.repository.id,
        pull_request_identity("U_viewer"),
        metadata,
        id(1),
        id(2053),
        scope,
        manifest,
    ))?;
    let restored = db.load_session(&created.id)?;
    let ComparisonSelector::PullRequestScope { scope, .. } = restored.selector else {
        anyhow::bail!("PR scope lost during persistence")
    };
    assert_eq!(scope.selections().len(), 2052);
    assert_eq!(scope.selections(), selections);
    assert!(scope.is_range());
    Ok(())
}

#[test]
fn navigation_does_not_invalidate_review_receipts_but_rejects_old_generations() -> Result<()> {
    let (_directory, db, session) = setup()?;
    let receipt = target(&session)?;
    let mut presentation = session.presentation.clone();
    presentation.path_filter = "auth".into();
    block_on(db.save_navigation(
        session.id.clone(),
        session.snapshot.generation,
        presentation.clone(),
    ))?;
    let updated = db.load_session(&session.id)?;
    assert_eq!(updated.revision, session.revision);
    assert_eq!(updated.presentation, presentation);
    block_on(db.mark_viewed(receipt, contents(b"reviewed\r\n\0"), 1024))?;
    assert!(
        block_on(db.save_navigation(
            session.id,
            session.snapshot.generation + 1,
            ReviewPresentation::default()
        ))
        .is_err()
    );
    Ok(())
}

#[test]
fn applied_pr_scope_survives_reopen_and_ignores_tab_serialization_order() -> Result<()> {
    let (directory, db, legacy) = setup()?;
    let identity = pull_request_identity("U_viewer");
    let mut metadata = pull_request_metadata();
    metadata.head_ref_oid = "3".repeat(40);
    metadata.commit_count = Some(2);
    let mut full_snapshot = snapshot(1, b"full PR");
    full_snapshot.groups[0].endpoints.right.hex = metadata.head_ref_oid.clone();
    let full = block_on(db.deploy_pull_request(
        legacy.repository.id.clone(),
        identity.clone(),
        metadata.clone(),
        full_snapshot,
        None,
    ))?;
    let mut first = snapshot(1, b"first selected patch").groups.remove(0);
    first.endpoints.group = "first-selected".into();
    let mut second = first.clone();
    second.endpoints.group = "second-selected".into();
    second.endpoints.left = first.endpoints.right.clone();
    second.endpoints.right.hex = metadata.head_ref_oid.clone();
    let selections = [&first, &second]
        .iter()
        .map(|group| CommitSelection {
            commit: group.endpoints.right.clone(),
            parent: Some(group.endpoints.left.clone()),
        })
        .collect();
    let selected = block_on(db.create_pull_request_scope(
        legacy.repository.id.clone(),
        identity.clone(),
        metadata.clone(),
        first.endpoints.left.clone(),
        second.endpoints.right.clone(),
        PullRequestCommitScope::Commits { selections },
        ReviewSnapshot {
            generation: 1,
            groups: vec![first, second],
            incomplete_reason: None,
        },
    ))?;
    assert_eq!(
        db.applied_pull_request_session(&legacy.repository.id, &identity)?
            .unwrap()
            .id,
        selected.id
    );
    let workspace = workspace::WorkspaceId::from_i64(700);
    block_on(db.save_item(workspace, 1, full.id.clone()))?;
    block_on(db.save_item(workspace, 2, selected.id.clone()))?;
    block_on(db.apply_pull_request_session(full.id.clone()))?; // Explicit All commits.
    // A stale selected tab can serialize last while Conversation is active.
    block_on(db.save_item(workspace, 2, selected.id.clone()))?;
    assert_eq!(
        db.applied_pull_request_session(&legacy.repository.id, &identity)?
            .unwrap()
            .id,
        full.id
    );
    assert_eq!(
        db.restored_item_session(workspace, 2)?.id,
        full.id,
        "Selected fallback item resolves to explicit All commits after nonserialized Conversation"
    );
    drop(db);
    let db = ReviewDb::from_connection(connection(&directory.path().join("review.sqlite"))?)?;
    assert_eq!(
        db.applied_pull_request_session(&legacy.repository.id, &identity)?
            .unwrap()
            .id,
        full.id
    );
    assert!(
        db.applied_pull_request_session(&legacy.repository.id, &pull_request_identity("U_other"))?
            .is_none()
    );
    let other_path = directory.path().join("other-worktree");
    fs::create_dir(&other_path)?;
    let other_binding = block_on(db.bind_repository(
        legacy.repository.common_directory.clone(),
        other_path,
        ObjectFormat::Sha1,
    ))?;
    assert!(
        db.applied_pull_request_session(&other_binding.id, &identity)?
            .is_none()
    );
    block_on(db.apply_pull_request_session(selected.id.clone()))?; // Explicit Apply again.
    block_on(db.save_item(workspace, 1, full.id.clone()))?;
    assert_eq!(
        db.applied_pull_request_session(&legacy.repository.id, &identity)?
            .unwrap()
            .id,
        selected.id
    );
    let restored = db.restored_item_session(workspace, 1)?;
    assert_eq!(
        restored.id, selected.id,
        "Full fallback item respects last explicit Apply"
    );
    assert!(
        matches!(restored.selector, ComparisonSelector::PullRequestScope { scope: PullRequestCommitScope::Commits { ref selections }, .. } if selections.len() == 2)
    );
    // Refreshing full metadata does not undo the applied historical selection.
    block_on(db.deploy_pull_request(
        full.repository.id.clone(),
        identity.clone(),
        metadata,
        full.snapshot.clone(),
        Some(full.revision),
    ))?;
    assert_eq!(
        db.applied_pull_request_session(&legacy.repository.id, &identity)?
            .unwrap()
            .id,
        selected.id
    );
    Ok(())
}
