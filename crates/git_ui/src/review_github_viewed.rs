//! Pure reconciliation policy for GitHub's weaker file-level Viewed flag and
//! retained local checkpoints. Callers persist observations independently of
//! checkpoint bytes and use the existing revision/generation CAS when importing.
use crate::{
    github_review::{RemoteViewedFile, ReviewContext, ViewedState},
    review::{
        ComparisonKind, FileComparison, PullRequestIdentity, PullRequestMetadata, ReviewSession,
        ReviewedCheckpoint,
    },
};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_VIEWED_BATCH: usize = 20;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubViewedObservation {
    pub identity: PullRequestIdentity,
    pub group: String,
    pub path: String,
    pub head_oid: String,
    pub fingerprint: String,
    pub state: ViewedState,
    /// False when a still-VIEWED server flag could refer to older reviewed
    /// contents. Persist this even when no current checkpoint can be imported.
    pub trusted_viewed: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewedCheckpointAction {
    Keep,
    /// Retain exact current raw bytes and then mark via the existing CAS. A
    /// server flag alone cannot manufacture a retained since-review checkpoint.
    ImportCurrent,
    /// Suppress current approval, but preserve older bytes for Since review.
    RetainHistory,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewedDecisionReason {
    InSync,
    RemoteReviewed,
    RemoteUnviewed,
    ChangedFingerprint,
    PendingLocalOperation,
}
#[derive(Clone, Debug)]
pub struct ViewedFileDecision {
    pub path: String,
    pub fingerprint: String,
    pub effective_viewed: bool,
    pub checkpoint_matches: bool,
    pub action: ViewedCheckpointAction,
    pub reason: ViewedDecisionReason,
    /// None while a local operation is unresolved: do not overwrite its last
    /// accepted observation with a conflicting in-flight server snapshot.
    pub observation: Option<GitHubViewedObservation>,
}
pub struct ViewedReconciliationInput<'a> {
    pub session: &'a ReviewSession,
    pub metadata: &'a PullRequestMetadata,
    pub context: &'a ReviewContext,
    pub remote: &'a [RemoteViewedFile],
    pub checkpoints: &'a BTreeMap<String, ReviewedCheckpoint>,
    pub observations: &'a BTreeMap<String, GitHubViewedObservation>,
    pub pending_paths: &'a BTreeSet<String>,
}
pub fn reconcile_github_viewed(
    input: ViewedReconciliationInput<'_>,
) -> Result<Vec<ViewedFileDecision>> {
    let ViewedReconciliationInput {
        session,
        metadata,
        context,
        remote,
        checkpoints,
        observations,
        pending_paths,
    } = input;
    ensure!(
        session.selector.supports_github_viewed(),
        "Only the complete PR can synchronize GitHub Viewed state"
    );
    ensure!(
        session.selector.pull_request_identity() == Some(&context.identity),
        "Viewed response belongs to another PR or account"
    );
    session.snapshot.validate(session.repository.format)?;
    metadata.validate_pinned_tips(session.repository.format)?;
    ensure!(
        session.snapshot.incomplete_reason.is_none() && session.snapshot.groups.len() == 1,
        "Viewed synchronization requires one complete PR comparison"
    );
    let group = &session.snapshot.groups[0];
    ensure!(
        context
            .reference
            .repository
            .owner
            .eq_ignore_ascii_case(&metadata.owner)
            && context
                .reference
                .repository
                .name
                .eq_ignore_ascii_case(&metadata.repository)
            && context.reference.number == metadata.number,
        "Viewed response repository identity changed"
    );
    ensure!(
        context.head_oid == metadata.head_ref_oid
            && context.base_tip_oid == metadata.base_ref_oid
            && context.head_oid == group.endpoints.right.hex,
        "Viewed response belongs to a different pinned PR revision"
    );
    let mut server = BTreeMap::new();
    for file in remote {
        ensure!(
            server
                .insert(file.path.as_str(), file.viewer_viewed_state)
                .is_none(),
            "Duplicate canonical path in GitHub Viewed response"
        );
    }
    ensure!(
        server.len() == group.files.len(),
        "GitHub Viewed response is incomplete for this PR comparison"
    );
    let mut decisions = Vec::with_capacity(group.files.len());
    for comparison in &group.files {
        let path = comparison.file_key()?;
        let state = *server
            .get(path)
            .context("GitHub Viewed response omits a canonical PR file")?;
        let checkpoint = checkpoints.get(path);
        if let Some(checkpoint) = checkpoint {
            ensure_checkpoint_file(checkpoint, &group.endpoints.group, comparison)?;
        }
        let fingerprint = comparison.fingerprint()?;
        let matches = checkpoint
            .map(|checkpoint| checkpoint.matches(&group.endpoints.group, comparison))
            .transpose()?
            .unwrap_or(false);
        if pending_paths.contains(path) {
            decisions.push(ViewedFileDecision {
                path: path.into(),
                fingerprint,
                effective_viewed: matches,
                checkpoint_matches: matches,
                action: ViewedCheckpointAction::Keep,
                reason: ViewedDecisionReason::PendingLocalOperation,
                observation: None,
            });
            continue;
        }
        let previous = observations.get(path);
        if let Some(previous) = previous {
            ensure!(
                previous.identity == context.identity
                    && previous.group == group.endpoints.group
                    && previous.path == path,
                "Cached Viewed observation belongs to another PR, account or file"
            );
        }
        // An observed UNVIEWED/DISMISSED -> VIEWED transition on these exact
        // contents can establish a new web review. A continuously VIEWED flag
        // cannot silently approve changed bytes or a changed base comparison.
        let fresh_remote_review = previous.is_some_and(|previous| {
            previous.fingerprint == fingerprint && previous.state != ViewedState::Viewed
        });
        let ambiguous_lineage = checkpoint.is_none()
            && comparison.old.as_ref().is_some_and(|old| {
                checkpoints.values().any(|prior| {
                    prior
                        .comparison
                        .old
                        .as_ref()
                        .is_some_and(|side| side.path == old.path)
                })
            });
        let changed_checkpoint = checkpoint.is_some() && !matches;
        let stale_observation = previous.is_some_and(|previous| {
            previous.state == ViewedState::Viewed
                && (previous.fingerprint != fingerprint || !previous.trusted_viewed)
        });
        // A confirmed current approval remains valid if retaining bytes was
        // interrupted. Older checkpoints must not undo that newer user intent.
        let trusted_current_review = previous.is_some_and(|previous| {
            previous.state == ViewedState::Viewed
                && previous.trusted_viewed
                && previous.fingerprint == fingerprint
                && previous.head_oid == context.head_oid
        });
        let stale_viewed = state == ViewedState::Viewed
            && !matches
            && (changed_checkpoint || stale_observation || ambiguous_lineage)
            && !fresh_remote_review
            && !trusted_current_review;
        let (effective_viewed, action, reason) = if stale_viewed {
            (
                false,
                ViewedCheckpointAction::RetainHistory,
                ViewedDecisionReason::ChangedFingerprint,
            )
        } else if state == ViewedState::Viewed {
            (
                true,
                if matches {
                    ViewedCheckpointAction::Keep
                } else {
                    ViewedCheckpointAction::ImportCurrent
                },
                if matches {
                    ViewedDecisionReason::InSync
                } else {
                    ViewedDecisionReason::RemoteReviewed
                },
            )
        } else {
            (
                false,
                if checkpoint.is_some() {
                    ViewedCheckpointAction::RetainHistory
                } else {
                    ViewedCheckpointAction::Keep
                },
                ViewedDecisionReason::RemoteUnviewed,
            )
        };
        let observation = GitHubViewedObservation {
            identity: context.identity.clone(),
            group: group.endpoints.group.clone(),
            path: path.into(),
            head_oid: context.head_oid.clone(),
            fingerprint: fingerprint.clone(),
            state,
            trusted_viewed: effective_viewed && state == ViewedState::Viewed,
        };
        decisions.push(ViewedFileDecision {
            path: path.into(),
            fingerprint,
            effective_viewed,
            checkpoint_matches: matches,
            action,
            reason,
            observation: Some(observation),
        });
    }
    Ok(decisions)
}
fn ensure_checkpoint_file(
    checkpoint: &ReviewedCheckpoint,
    group: &str,
    current: &FileComparison,
) -> Result<()> {
    ensure!(
        checkpoint.group == group && checkpoint.comparison.kind == ComparisonKind::Full,
        "Checkpoint belongs to another comparison scope"
    );
    let same_path = checkpoint.comparison.file_key()? == current.file_key()?;
    let canonical_rename = checkpoint
        .comparison
        .new
        .as_ref()
        .zip(current.old.as_ref())
        .is_some_and(|(reviewed, old)| reviewed.path == old.path);
    ensure!(
        same_path || canonical_rename,
        "Checkpoint belongs to another file; a verified rename mapping is required"
    );
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinceReviewComparison {
    NoCheckpoint,
    NoNewContent { full_comparison_changed: bool },
    Comparison(FileComparison),
}
/// Read the OLD side bytes from checkpoint retained.new and the NEW side bytes
/// from the current new Git object. SinceReview kind cannot approve Full state.
pub fn since_review_comparison(
    group: &str,
    current: &FileComparison,
    checkpoint: Option<&ReviewedCheckpoint>,
) -> Result<SinceReviewComparison> {
    let Some(checkpoint) = checkpoint else {
        return Ok(SinceReviewComparison::NoCheckpoint);
    };
    ensure_checkpoint_file(checkpoint, group, current)?;
    ensure!(
        current.kind == ComparisonKind::Full,
        "Since review requires a full current comparison"
    );
    let old = checkpoint.comparison.new.clone();
    let new = current.new.clone();
    if old == new {
        return Ok(SinceReviewComparison::NoNewContent {
            full_comparison_changed: !checkpoint.matches(group, current)?,
        });
    }
    let comparison = FileComparison {
        kind: ComparisonKind::SinceReview,
        old,
        new,
    };
    let format = comparison
        .new
        .as_ref()
        .or(comparison.old.as_ref())
        .context("Since-review comparison has no sides")?
        .object
        .format;
    comparison.validate(format)?;
    Ok(SinceReviewComparison::Comparison(comparison))
}
/// All canonical descendants are included, independently of a UI path filter.
/// A caller may create one durable operation per returned bounded batch.
pub fn folder_viewed_batches(session: &ReviewSession, folder: &str) -> Result<Vec<Vec<String>>> {
    ensure!(
        session.selector.supports_github_viewed(),
        "A subset cannot mark full PR folders viewed"
    );
    ensure!(
        session.snapshot.incomplete_reason.is_none() && session.snapshot.groups.len() == 1,
        "Folder synchronization requires a complete PR comparison"
    );
    session.snapshot.validate(session.repository.format)?;
    let folder = folder.trim_end_matches('/');
    ensure!(
        folder.is_empty()
            || folder
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..")
                && !folder.contains('\0'),
        "Invalid PR folder path"
    );
    let prefix = format!("{folder}/");
    let paths = session.snapshot.groups[0]
        .files
        .iter()
        .map(FileComparison::file_key)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| folder.is_empty() || path.starts_with(&prefix))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(!paths.is_empty(), "Folder has no changed PR files");
    Ok(paths
        .chunks(MAX_VIEWED_BATCH)
        .map(<[String]>::to_vec)
        .collect())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderViewedOutcome {
    pub confirmed: Vec<String>,
    pub not_applied: Vec<String>,
    pub unknown: Vec<String>,
}
/// A failed read leaves every target unknown. A partial response never becomes
/// an all-success result, even when some descendants have already changed.
pub fn reconcile_folder_viewed(
    paths: &[String],
    viewed: bool,
    remote: Option<&[RemoteViewedFile]>,
) -> Result<FolderViewedOutcome> {
    let mut unique = BTreeSet::new();
    for path in paths {
        ensure!(
            unique.insert(path),
            "Duplicate folder synchronization target"
        );
    }
    let mut server = BTreeMap::new();
    if let Some(remote) = remote {
        for file in remote {
            ensure!(
                server
                    .insert(file.path.as_str(), file.viewer_viewed_state)
                    .is_none(),
                "Duplicate server Viewed path"
            );
        }
    }
    let mut outcome = FolderViewedOutcome {
        confirmed: Vec::new(),
        not_applied: Vec::new(),
        unknown: Vec::new(),
    };
    for path in paths {
        match server.get(path.as_str()) {
            Some(state) if (*state == ViewedState::Viewed) == viewed => {
                outcome.confirmed.push(path.clone())
            }
            Some(_) => outcome.not_applied.push(path.clone()),
            None => outcome.unknown.push(path.clone()),
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        github_pull_requests::{PullRequestReference, RepositoryName},
        review::*,
    };
    use std::path::PathBuf;
    struct Fixture {
        session: ReviewSession,
        metadata: PullRequestMetadata,
        context: ReviewContext,
    }
    fn fixture() -> Fixture {
        let identity = PullRequestIdentity {
            host: "github.com".into(),
            repository_node_id: "R_example".into(),
            pull_request_node_id: "PR_example".into(),
            viewer_node_id: "U_viewer".into(),
        };
        let metadata = PullRequestMetadata {
            owner: "owner".into(),
            repository: "repository".into(),
            number: 7,
            title: "Example review".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(3),
        };
        let context = ReviewContext {
            reference: PullRequestReference {
                repository: RepositoryName {
                    owner: metadata.owner.clone(),
                    name: metadata.repository.clone(),
                },
                number: metadata.number,
            },
            identity: identity.clone(),
            head_oid: metadata.head_ref_oid.clone(),
            base_tip_oid: metadata.base_ref_oid.clone(),
        };
        let session = ReviewSession {
            id: "session".into(),
            repository: RepositoryBinding {
                id: "repository".into(),
                common_directory: PathBuf::from("/fixture/.git"),
                worktree_directory: PathBuf::from("/fixture"),
                format: ObjectFormat::Sha1,
            },
            selector: ComparisonSelector::PullRequest { identity },
            revision: 0,
            snapshot: ReviewSnapshot {
                generation: 1,
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
                            hex: metadata.head_ref_oid.clone(),
                        },
                    },
                    files: vec![file("src/example.rs", b"base", b"new")],
                }],
            },
            presentation: ReviewPresentation::default(),
        };
        Fixture {
            session,
            metadata,
            context,
        }
    }
    fn file(path: &str, old: &[u8], new: &[u8]) -> FileComparison {
        let side = |bytes| FileSide {
            path: path.into(),
            object: ObjectId::for_blob(ObjectFormat::Sha1, bytes),
            mode: 0o100644,
        };
        FileComparison {
            kind: ComparisonKind::Full,
            old: Some(side(old)),
            new: Some(side(new)),
        }
    }
    fn checkpoint(comparison: FileComparison) -> ReviewedCheckpoint {
        ReviewedCheckpoint {
            group: "comparison".into(),
            comparison,
            generation: 1,
            reviewed_at_unix_seconds: 1,
        }
    }
    fn remote(state: ViewedState) -> Vec<RemoteViewedFile> {
        vec![RemoteViewedFile {
            path: "src/example.rs".into(),
            viewer_viewed_state: state,
        }]
    }
    fn reconcile(
        fixture: &Fixture,
        remote: &[RemoteViewedFile],
        checkpoints: &BTreeMap<String, ReviewedCheckpoint>,
        observations: &BTreeMap<String, GitHubViewedObservation>,
        pending: &BTreeSet<String>,
    ) -> Result<Vec<ViewedFileDecision>> {
        reconcile_github_viewed(ViewedReconciliationInput {
            session: &fixture.session,
            metadata: &fixture.metadata,
            context: &fixture.context,
            remote,
            checkpoints,
            observations,
            pending_paths: pending,
        })
    }
    #[test]
    fn viewed_import_requires_current_pinned_account_and_complete_canonical_files() -> Result<()> {
        let mut fixture = fixture();
        let decisions = reconcile(
            &fixture,
            &remote(ViewedState::Viewed),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )?;
        assert!(decisions[0].effective_viewed);
        assert_eq!(decisions[0].action, ViewedCheckpointAction::ImportCurrent);
        fixture.context.identity.viewer_node_id = "U_other".into();
        assert!(
            reconcile(
                &fixture,
                &remote(ViewedState::Viewed),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeSet::new()
            )
            .is_err()
        );
        fixture.context.identity.viewer_node_id = "U_viewer".into();
        fixture.context.head_oid = "4".repeat(40);
        assert!(
            reconcile(
                &fixture,
                &remote(ViewedState::Viewed),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeSet::new()
            )
            .is_err()
        );
        fixture.context.head_oid = fixture.metadata.head_ref_oid.clone();
        assert!(
            reconcile(
                &fixture,
                &[],
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeSet::new()
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn continuously_viewed_remote_flag_never_approves_changed_checkpoint() -> Result<()> {
        let fixture = fixture();
        let old = checkpoint(file("src/example.rs", b"base", b"previous review"));
        let checkpoints = BTreeMap::from([("src/example.rs".into(), old.clone())]);
        let first = reconcile(
            &fixture,
            &remote(ViewedState::Viewed),
            &checkpoints,
            &BTreeMap::new(),
            &BTreeSet::new(),
        )?;
        assert!(!first[0].effective_viewed);
        assert_eq!(first[0].action, ViewedCheckpointAction::RetainHistory);
        let observations = BTreeMap::from([(
            "src/example.rs".into(),
            first[0].observation.clone().unwrap(),
        )]);
        let again = reconcile(
            &fixture,
            &remote(ViewedState::Viewed),
            &BTreeMap::new(),
            &observations,
            &BTreeSet::new(),
        )?;
        assert!(!again[0].effective_viewed); // Still blocked even if old checkpoint was removed.
        assert_eq!(
            old.comparison.new.as_ref().unwrap().object,
            ObjectId::for_blob(ObjectFormat::Sha1, b"previous review")
        );
        Ok(())
    }
    #[test]
    fn remote_unviewed_retains_history_and_explicit_new_web_review_can_replace_it() -> Result<()> {
        let fixture = fixture();
        let checkpoints = BTreeMap::from([(
            "src/example.rs".into(),
            checkpoint(file("src/example.rs", b"base", b"old")),
        )]);
        let unviewed = reconcile(
            &fixture,
            &remote(ViewedState::Unviewed),
            &checkpoints,
            &BTreeMap::new(),
            &BTreeSet::new(),
        )?;
        assert!(!unviewed[0].effective_viewed);
        assert_eq!(unviewed[0].action, ViewedCheckpointAction::RetainHistory);
        let observations = BTreeMap::from([(
            "src/example.rs".into(),
            unviewed[0].observation.clone().unwrap(),
        )]);
        let reviewed = reconcile(
            &fixture,
            &remote(ViewedState::Viewed),
            &checkpoints,
            &observations,
            &BTreeSet::new(),
        )?;
        assert!(reviewed[0].effective_viewed);
        assert_eq!(reviewed[0].action, ViewedCheckpointAction::ImportCurrent);
        Ok(())
    }
    #[test]
    fn pending_mutations_and_subset_scopes_cannot_import_server_approval() -> Result<()> {
        let mut fixture = fixture();
        let pending = BTreeSet::from(["src/example.rs".into()]);
        let decision = reconcile(
            &fixture,
            &remote(ViewedState::Viewed),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &pending,
        )?;
        assert!(!decision[0].effective_viewed);
        assert!(decision[0].observation.is_none());
        let base = ObjectId {
            format: ObjectFormat::Sha1,
            hex: fixture.metadata.base_ref_oid.clone(),
        };
        let head = ObjectId {
            format: ObjectFormat::Sha1,
            hex: fixture.metadata.head_ref_oid.clone(),
        };
        fixture.session.selector = ComparisonSelector::PullRequestScope {
            identity: fixture.context.identity.clone(),
            base_tip: base.clone(),
            head_tip: head.clone(),
            scope: PullRequestCommitScope::Commits {
                selections: vec![CommitSelection {
                    commit: head,
                    parent: Some(base),
                }],
            },
        };
        assert!(
            reconcile(
                &fixture,
                &remote(ViewedState::Viewed),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeSet::new()
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn since_review_uses_reviewed_new_side_and_handles_deletion_and_base_only_change() -> Result<()>
    {
        let old = checkpoint(file("src/example.rs", b"original base", b"reviewed"));
        let current = file("src/example.rs", b"different base", b"latest");
        let SinceReviewComparison::Comparison(comparison) =
            since_review_comparison("comparison", &current, Some(&old))?
        else {
            anyhow::bail!("Missing since-review diff")
        };
        assert_eq!(comparison.old, old.comparison.new);
        assert_eq!(comparison.new, current.new);
        assert_eq!(comparison.kind, ComparisonKind::SinceReview);
        let deleted = FileComparison {
            kind: ComparisonKind::Full,
            old: current.old,
            new: None,
        };
        let SinceReviewComparison::Comparison(deleted) =
            since_review_comparison("comparison", &deleted, Some(&old))?
        else {
            anyhow::bail!("Missing deletion")
        };
        assert_eq!(deleted.old, old.comparison.new);
        assert!(deleted.new.is_none());
        let base_only = file("src/example.rs", b"different base", b"reviewed");
        assert_eq!(
            since_review_comparison("comparison", &base_only, Some(&old))?,
            SinceReviewComparison::NoNewContent {
                full_comparison_changed: true
            }
        );
        assert!(
            comparison
                .validate_retained(
                    &RetainedContents {
                        old: Some(b"reviewed".to_vec()),
                        new: Some(b"latest".to_vec())
                    },
                    1024
                )
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn folders_include_all_descendants_in_batches_and_report_partial_unknown() -> Result<()> {
        let mut fixture = fixture();
        fixture.session.snapshot.groups[0].files = (0..43)
            .map(|index| file(&format!("src/nested/file-{index}.rs"), b"old", b"new"))
            .collect();
        fixture.session.snapshot.groups[0]
            .files
            .push(file("src-other/example.rs", b"old", b"new"));
        let batches = folder_viewed_batches(&fixture.session, "src/")?;
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![20, 20, 3]
        );
        let targets = batches.into_iter().flatten().collect::<Vec<_>>();
        let observed = vec![
            RemoteViewedFile {
                path: targets[0].clone(),
                viewer_viewed_state: ViewedState::Viewed,
            },
            RemoteViewedFile {
                path: targets[1].clone(),
                viewer_viewed_state: ViewedState::Unviewed,
            },
        ];
        let outcome = reconcile_folder_viewed(&targets, true, Some(&observed))?;
        assert_eq!(outcome.confirmed.len(), 1);
        assert_eq!(outcome.not_applied.len(), 1);
        assert_eq!(outcome.unknown.len(), 41);
        assert_eq!(
            reconcile_folder_viewed(&targets, true, None)?.unknown.len(),
            43
        );
        Ok(())
    }
    #[test]
    fn confirmed_current_approval_survives_cancelled_retention() -> Result<()> {
        let fixture = fixture();
        let checkpoints = BTreeMap::from([(
            "src/example.rs".into(),
            checkpoint(file("src/example.rs", b"base", b"old")),
        )]);
        let observations = BTreeMap::from([(
            "src/example.rs".into(),
            GitHubViewedObservation {
                identity: fixture.context.identity.clone(),
                group: "comparison".into(),
                path: "src/example.rs".into(),
                head_oid: fixture.context.head_oid.clone(),
                fingerprint: fixture.session.snapshot.groups[0].files[0].fingerprint()?,
                state: ViewedState::Viewed,
                trusted_viewed: true,
            },
        )]);
        let decision = reconcile(
            &fixture,
            &remote(ViewedState::Viewed),
            &checkpoints,
            &observations,
            &BTreeSet::new(),
        )?;
        assert!(decision[0].effective_viewed);
        assert_eq!(decision[0].action, ViewedCheckpointAction::ImportCurrent);
        let mut stale = observations;
        stale.get_mut("src/example.rs").unwrap().fingerprint = "0".repeat(64);
        assert!(
            !reconcile(
                &fixture,
                &remote(ViewedState::Viewed),
                &checkpoints,
                &stale,
                &BTreeSet::new()
            )?[0]
                .effective_viewed
        );
        Ok(())
    }
    #[test]
    fn since_review_raw_contents_validate_without_approving_full_comparison() -> Result<()> {
        let mut comparison = file("src/example.rs", b"reviewed", b"current");
        comparison.kind = ComparisonKind::SinceReview;
        let contents = RetainedContents {
            old: Some(b"reviewed".to_vec()),
            new: Some(b"current".to_vec()),
        };
        comparison.validate_contents(&contents, 1024)?;
        assert!(comparison.validate_retained(&contents, 1024).is_err());
        assert!(
            comparison
                .validate_contents(
                    &RetainedContents {
                        old: contents.old.clone(),
                        new: Some(b"wrong".to_vec())
                    },
                    1024
                )
                .is_err()
        );
        assert!(comparison.validate_contents(&contents, 1).is_err());
        Ok(())
    }
}
