use crate::{
    branch_picker,
    commit_view::{GitBlob, build_buffer, build_buffer_diff, worktree_id_for_repo_path},
    github_pull_requests::{
        GitHubClient, GitHubRequestGuard, GitHubUser, PullRequestDetails, PullRequestFile,
        PullRequestReference,
    },
    pull_request_view::repository_from_remote,
    review::*,
    review_comment_anchor::{self, CanonicalCommentBuffer, CapturedCommentAnchor},
    review_commit_picker::ReviewCommitPicker,
    review_github_view::{GitHubInlineCard, GitHubReviewEvent, GitHubReviewPanel},
    review_store::{ReviewDb, ViewedTarget},
};
use anyhow::{Context as _, Result, ensure};
use askpass::AskPassDelegate;
use buffer_diff::BufferDiff;
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{
    Editor, EditorEvent, HiddenDiffHunkRenderer, MultiBuffer, SplittableEditor,
    multibuffer_context_lines,
};
use file_icons::FileIcons;
use git::repository::{RepoPath, ReviewPrFetch};
use git_ui_core::askpass_modal::AskPassModal;
use gpui::{
    Action as _, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, Render, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, uniform_list,
};
use language::{Capability, File, OffsetRangeExt as _};
use multi_buffer::PathKey;
use project::{
    Project,
    git_store::{Repository, RepositoryEvent},
};
use settings::{DiffViewStyle, Settings};
use std::{
    any::TypeId,
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Instant,
};
use ui::{Checkbox, ContextMenu, DropdownMenu, ListItem, prelude::*};
use ui_input::InputField;
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{
    Item, SerializableItem, Workspace, item::ItemEvent, notifications::NotifyTaskExt,
    searchable::SearchableItemHandle,
};

actions!(
    git,
    [
        /// Review committed changes between two branches using their merge base.
        OpenReview,
        /// Open the marked offline native folder Viewed acceptance fixture.
        PreviewReviewViewedFaults,
        /// Review the exact endpoint difference between two branches.
        OpenDirectReview,
        /// Choose commits for individual, grouped or linear-range review.
        OpenCommitReview,
        /// Check selected refs while keeping the displayed snapshot frozen.
        CheckReviewUpdates,
        /// Deliberately refresh the review from its selected refs.
        RefreshReview,
        /// Reload the shared review state changed by another window.
        ReloadReview,
        /// Mark the displayed file comparison viewed or unviewed.
        ToggleReviewViewed,
        /// Toggle the review's unreviewed-only file list.
        ToggleUnreviewedFiles,
        /// Open the next file in this review.
        NextReviewFile,
        /// Open the previous file in this review.
        PreviousReviewFile,
        /// Make this review's file tree narrower.
        NarrowReviewTree,
        /// Make this review's file tree wider.
        WidenReviewTree,
        /// Restore this review's default file tree width.
        ResetReviewTreeWidth,
        /// Add a GitHub inline comment at the selected PR diff lines.
        CommentOnPullRequestSelection,
        /// Open GitHub PR discussions and pending review actions.
        TogglePullRequestDiscussions,
        /// Post the current GitHub inline comment or reply immediately.
        PostGitHubCommentNow,
        /// Save the current GitHub composer as a durable local draft.
        SaveGitHubDraft,
        /// Close the current GitHub composer while preserving its durable text.
        CloseGitHubComposer,
        /// Check GitHub for the selected comment's unconfirmed delivery, without replaying it.
        ReconcileGitHubDelivery,
        /// Read the full PR's Viewed state from GitHub and retain exact review history.
        SyncGitHubViewed,
        /// Show retained reviewed contents against the current file contents.
        ShowChangesSinceReview,
        /// Return to the full pinned PR comparison.
        ShowFullPRComparison,
        /// Stop retaining file history after the current bounded read; GitHub writes are not replayed or cancelled.
        StopGitHubViewedRetention,
    ]
);

#[path = "review_view_github_viewed.rs"]
mod github_viewed;

#[path = "review_view_coverage.rs"]
mod coverage;
mod navigation;
mod preparation;
mod presentation;
mod recovery;
pub(crate) use recovery::RecoveryAction;

const GROUP: &str = "comparison";
// Bound raw old + new preview bytes before loading either historical blob.
const REVIEW_PREVIEW_BUDGET: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct PullRequestReviewContext {
    pub identity: PullRequestIdentity,
    pub metadata: PullRequestMetadata,
    pub selection: Option<PullRequestCommitScope>,
}

#[derive(Clone)]
struct CommitHistoryOrigin {
    session: String,
    selector: ComparisonSelector,
    base_tip: String,
    head_tip: String,
}

#[derive(Clone)]
struct ReviewData {
    pull_request: Option<PullRequestMetadata>,
    session: ReviewSession,
    viewed: BTreeSet<String>,
    // Retained-content approval is separate from the GitHub Viewed UI.
    checkpoint_viewed: BTreeSet<String>,
    // GitHub flags are distinct from exact-content local approval.
    github_viewed: Option<BTreeSet<String>>,
    // Durable overlays travel with the loaded session so reusing an existing
    // tab cannot retain older intents over newly fetched authoritative state.
    durable_viewed_intents: BTreeMap<String, bool>,
    #[cfg(test)]
    changed: BTreeSet<String>,
    files: Vec<String>,
    #[cfg(test)]
    group_progress: Vec<(usize, usize)>,
    pinned_path: Option<String>,
}

fn read_data(db: &ReviewDb, session: ReviewSession) -> Result<ReviewData> {
    let mut viewed = BTreeSet::new();
    let mut changed = BTreeSet::new();
    let mut files = Vec::new();
    let mut session = session;
    if !session
        .snapshot
        .groups
        .iter()
        .any(|group| Some(&group.endpoints.group) == session.presentation.selected_group.as_ref())
    {
        session.presentation.selected_group = session
            .snapshot
            .groups
            .first()
            .map(|group| group.endpoints.group.clone());
    }
    let coverage_reviews = if matches!(
        session.selector,
        ComparisonSelector::PullRequestScope { .. }
    ) {
        db.comparison_reviewed_paths(&session)?
    } else {
        BTreeSet::new()
    };
    let mut group_progress = Vec::new();
    for group in &session.snapshot.groups {
        let active = matches!(
            session.selector,
            ComparisonSelector::PullRequestScope { .. }
        ) || Some(&group.endpoints.group)
            == session.presentation.selected_group.as_ref();
        let mut count = 0;
        let overrides = if session.selector.supports_github_viewed() {
            db.github_viewed_overrides(&session, &group.endpoints.group)?
        } else {
            BTreeMap::new()
        };
        let checkpoint_metadata =
            db.reviewed_checkpoint_metadata(&session.id, &group.endpoints.group)?;
        for file in &group.files {
            let path = file.file_key()?;
            let checkpoint = checkpoint_metadata.get(path);
            let checkpoint_reviewed = checkpoint
                .map(|checkpoint| checkpoint.matches(&group.endpoints.group, file))
                .transpose()?
                .unwrap_or(false)
                || coverage_reviews.contains(&(group.endpoints.group.clone(), path.to_owned()));
            let reviewed = overrides.get(path).copied().unwrap_or(checkpoint_reviewed);
            count += usize::from(reviewed);
            if !active {
                continue;
            }
            files.push(path.to_owned());
            if reviewed {
                viewed.insert(path.to_owned());
            } else if checkpoint.is_some() && !checkpoint_reviewed {
                changed.insert(path.to_owned());
            }
        }
        group_progress.push((count, group.files.len()));
    }
    if matches!(
        session.selector,
        ComparisonSelector::PullRequestScope { .. }
    ) {
        files.sort();
        files.dedup();
        viewed.retain(|path| {
            session.snapshot.groups.iter().all(|group| {
                group
                    .files
                    .iter()
                    .find(|file| file.file_key().ok() == Some(path.as_str()))
                    .is_none_or(|file| {
                        coverage_reviews.contains(&(group.endpoints.group.clone(), path.to_owned()))
                            || db
                                .checkpoint(&session.id, &group.endpoints.group, path)
                                .ok()
                                .flatten()
                                .is_some_and(|(checkpoint, _)| {
                                    checkpoint
                                        .matches(&group.endpoints.group, file)
                                        .unwrap_or(false)
                                })
                    })
            })
        });
    }
    let github_viewed = if session.selector.supports_github_viewed() {
        let group = &session.snapshot.groups[0];
        let observations = db.github_viewed_observations(&session.id, &group.endpoints.group)?;
        // The cache retains observations for removed/renamed paths across
        // generations. Completeness and progress belong to the current PR's
        // canonical files, not to the entire historical cache.
        let mut complete = true;
        let mut flags = BTreeSet::new();
        for file in &group.files {
            let path = file.file_key()?;
            let Some(observation) = observations.get(path) else {
                complete = false;
                break;
            };
            if observation.fingerprint != file.fingerprint()? {
                complete = false;
                break;
            }
            if observation.state == crate::github_review::ViewedState::Viewed {
                flags.insert(path.to_owned());
            }
        }
        complete.then_some(flags)
    } else {
        None
    };
    let checkpoint_viewed = viewed.clone();
    if let Some(remote) = &github_viewed {
        // One authoritative Viewed set drives all UI: header, checkboxes,
        // folders, navigation and Unreviewed. Retained history remains separate.
        viewed = remote.clone();
        if let Some(progress) = group_progress.first_mut() {
            progress.0 = viewed.len();
        }
    }
    let pull_request = db.pull_request_metadata(&session.id)?;
    let durable_viewed_intents = db.github_viewed_local_intents(&session)?;
    Ok(ReviewData {
        pull_request,
        session,
        viewed,
        checkpoint_viewed,
        github_viewed,
        durable_viewed_intents,
        #[cfg(test)]
        changed,
        files,
        #[cfg(test)]
        group_progress,
        pinned_path: None,
    })
}

fn paths(data: &ReviewData) -> Vec<String> {
    data.files
        .iter()
        .filter(|path| {
            !data.session.presentation.unreviewed_only
                || !data.viewed.contains(*path)
                || data.pinned_path.as_ref() == Some(*path)
        })
        .filter(|path| {
            path.to_lowercase()
                .contains(&data.session.presentation.path_filter.to_lowercase())
        })
        .cloned()
        .collect()
}

#[derive(Clone)]
struct ReviewTreeEntry {
    path: String,
    depth: usize,
    directory: bool,
    #[cfg(test)]
    total: usize,
    #[cfg(test)]
    viewed: usize,
}

#[cfg(test)]
fn folder_viewed_action(data: &ReviewData, folder: &str) -> (usize, bool, String) {
    let prefix = format!("{folder}/");
    let descendants = data
        .files
        .iter()
        .filter(|path| path.starts_with(&prefix))
        .collect::<Vec<_>>();
    let count = descendants.len();
    let viewed = descendants.iter().any(|path| !data.viewed.contains(*path));
    (
        count,
        viewed,
        format!(
            "Mark {count} {}",
            if viewed { "viewed" } else { "unviewed" }
        ),
    )
}

fn review_tree(data: &ReviewData) -> Vec<ReviewTreeEntry> {
    review_tree_with_collapsed(data, &data.session.presentation.collapsed_directories)
}

fn review_tree_with_collapsed(
    data: &ReviewData,
    collapsed: &BTreeSet<String>,
) -> Vec<ReviewTreeEntry> {
    #[derive(Default)]
    struct Node {
        directories: BTreeMap<String, Node>,
        files: Vec<String>,
        total: usize,
        viewed: usize,
    }
    let mut root = Node::default();
    for path in paths(data) {
        let components = path.split('/').collect::<Vec<_>>();
        let mut node = &mut root;
        node.total += 1;
        node.viewed += usize::from(data.viewed.contains(&path));
        for component in components.iter().take(components.len().saturating_sub(1)) {
            node = node.directories.entry((*component).into()).or_default();
            node.total += 1;
            node.viewed += usize::from(data.viewed.contains(&path));
        }
        node.files.push(path);
    }
    fn flatten(
        node: &Node,
        prefix: &str,
        depth: usize,
        collapsed: &BTreeSet<String>,
        rows: &mut Vec<ReviewTreeEntry>,
    ) {
        for (name, child) in &node.directories {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            rows.push(ReviewTreeEntry {
                path: path.clone(),
                depth,
                directory: true,
                #[cfg(test)]
                total: child.total,
                #[cfg(test)]
                viewed: child.viewed,
            });
            if !collapsed.contains(&path) {
                flatten(child, &path, depth + 1, collapsed, rows);
            }
        }
        for path in &node.files {
            rows.push(ReviewTreeEntry {
                path: path.clone(),
                depth,
                directory: false,
                #[cfg(test)]
                total: 1,
                #[cfg(test)]
                viewed: 0,
            });
        }
    }
    let mut rows = Vec::new();
    flatten(&root, "", 0, collapsed, &mut rows);
    rows
}

fn tree_navigation_target(data: &ReviewData, forward: bool) -> Option<String> {
    let visible = review_tree(data)
        .into_iter()
        .filter(|row| !row.directory)
        .map(|row| row.path)
        .collect::<Vec<_>>();
    let current = data.session.presentation.selected_path.as_ref();
    if let Some(index) = current.and_then(|current| visible.iter().position(|path| path == current))
    {
        let target = if forward {
            index.checked_add(1)
        } else {
            index.checked_sub(1)
        }?;
        return visible.get(target).cloned();
    }
    let full = review_tree_with_collapsed(data, &BTreeSet::new())
        .into_iter()
        .filter(|row| !row.directory)
        .map(|row| row.path)
        .collect::<Vec<_>>();
    let Some(index) = current.and_then(|current| full.iter().position(|path| path == current))
    else {
        return visible.first().cloned();
    };
    if forward {
        full.iter()
            .skip(index + 1)
            .find(|path| visible.contains(path))
            .cloned()
    } else {
        full.iter()
            .take(index)
            .rev()
            .find(|path| visible.contains(path))
            .cloned()
    }
}

fn bounded_tree_width(width: f32, available: f32) -> u32 {
    let maximum = (available * 0.6).min(available - 240.0).clamp(180.0, 520.0);
    width.clamp(180.0, maximum).round() as u32
}

#[derive(Clone)]
struct DraggedReviewTree;
#[derive(Clone)]
struct DraggedReviewThreads;

fn choose_visible(data: &mut ReviewData) {
    let available = paths(data);
    if data
        .session
        .presentation
        .selected_path
        .as_ref()
        .is_none_or(|selected| !available.contains(selected))
    {
        data.session.presentation.selected_path = review_tree(data)
            .into_iter()
            .find(|row| !row.directory)
            .map(|row| row.path);
    }
}

fn active_group(data: &ReviewData) -> &str {
    data.session
        .presentation
        .selected_group
        .as_deref()
        .unwrap_or(GROUP)
}

fn validate_pr_selections(
    history: &[git::review::ReviewCommit],
    selections: &[git::review::ReviewCommitSelection],
) -> Result<()> {
    let members = history
        .iter()
        .map(|commit| (commit.commit.to_string(), commit))
        .collect::<BTreeMap<_, _>>();
    let mut selected = BTreeSet::new();
    ensure!(!selections.is_empty(), "Select at least one PR commit");
    for selection in selections {
        let commit = members
            .get(&selection.commit.to_string())
            .context("Selected commit is outside the pinned PR history")?;
        ensure!(
            selected.insert(selection.commit.to_string()),
            "Duplicate PR commit selection"
        );
        ensure!(
            match selection.parent {
                Some(parent) => commit.parents.contains(&parent),
                None => commit.parents.is_empty(),
            },
            "Selected parent is not a parent of this PR commit"
        );
    }
    Ok(())
}

fn commit_manifest(
    comparisons: Vec<git::review::ReviewComparison>,
    range: bool,
) -> Result<ReviewSnapshot> {
    let mut groups = Vec::new();
    for comparison in comparisons {
        let mut group = manifest(comparison)?
            .groups
            .into_iter()
            .next()
            .context("missing commit comparison")?;
        group.endpoints.group = if range {
            "range".into()
        } else {
            format!(
                "commit:{}:{}",
                group.endpoints.left.hex, group.endpoints.right.hex
            )
        };
        groups.push(group);
    }
    let format = groups
        .first()
        .context("empty commit selection")?
        .endpoints
        .right
        .format;
    let snapshot = ReviewSnapshot {
        generation: 1,
        groups,
        incomplete_reason: None,
    };
    snapshot.validate(format)?;
    Ok(snapshot)
}

fn object(object: git::Oid) -> ObjectId {
    ObjectId {
        format: if object.as_bytes().len() == 20 {
            ObjectFormat::Sha1
        } else {
            ObjectFormat::Sha256
        },
        hex: object.to_string(),
    }
}

fn manifest(comparison: git::review::ReviewComparison) -> Result<ReviewSnapshot> {
    let format = object(comparison.right).format;
    let files = comparison
        .files
        .into_iter()
        .map(|file| {
            let path = file.path.as_unix_str().to_owned();
            let side = |side: git::review::ReviewSide| FileSide {
                path: path.clone(),
                object: object(side.object),
                mode: side.mode,
            };
            FileComparison {
                kind: ComparisonKind::Full,
                old: file.old.map(side),
                new: file.new.map(side),
            }
        })
        .collect();
    let snapshot = ReviewSnapshot {
        generation: 1,
        incomplete_reason: None,
        groups: vec![ComparisonGroup {
            endpoints: ResolvedEndpoints {
                group: GROUP.to_owned(),
                left: object(comparison.left),
                right: object(comparison.right),
            },
            files,
        }],
    };
    snapshot.validate(format)?;
    Ok(snapshot)
}

struct ReviewChangeBus;
impl EventEmitter<ReviewChanged> for ReviewChangeBus {}
struct ReviewChanged {
    session: String,
    revision: u64,
}
#[derive(Clone)]
struct ReviewChanges(Entity<ReviewChangeBus>);
impl Global for ReviewChanges {}
impl ReviewChanges {
    fn global(cx: &mut App) -> Entity<ReviewChangeBus> {
        if let Some(changes) = cx.try_global::<Self>() {
            return changes.0.clone();
        }
        let changes = cx.new(|_| ReviewChangeBus);
        cx.set_global(Self(changes.clone()));
        changes
    }
}

fn selector_refs(selector: &ComparisonSelector) -> Result<(String, String, bool)> {
    match selector {
        ComparisonSelector::Branch { base_ref, head_ref } => {
            Ok((base_ref.clone(), head_ref.clone(), true))
        }
        ComparisonSelector::Direct {
            left_ref,
            right_ref,
        } => Ok((left_ref.clone(), right_ref.clone(), false)),
        _ => anyhow::bail!("refresh for this comparison scope is not implemented"),
    }
}

fn review_askpass(
    workspace: WeakEntity<Workspace>,
    window: &Window,
    cx: &mut Context<Workspace>,
) -> AskPassDelegate {
    let window = window.window_handle();
    AskPassDelegate::new_with_cancellation(
        &mut cx.to_async(),
        move |prompt, sender, cancellation, cx| {
            window
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.toggle_modal(window, cx, |window, cx| {
                            AskPassModal::new(
                                "Fetch PR review".into(),
                                prompt.into(),
                                sender,
                                cancellation,
                                window,
                                cx,
                            )
                        });
                    })
                })
                .log_err();
        },
    )
}

fn reconcile_pr_files(snapshot: &mut ReviewSnapshot, canonical: &[PullRequestFile]) -> Result<()> {
    let group = snapshot
        .groups
        .first_mut()
        .context("PR comparison missing")?;
    let mut local = group
        .files
        .iter()
        .map(|file| Ok((file.file_key()?.to_string(), file.clone())))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut files = Vec::new();
    for server in canonical {
        let mut file = local.remove(&server.filename).with_context(|| {
            format!("PR file absent from local comparison: {}", server.filename)
        })?;
        if server.status == "renamed" {
            let previous = server
                .previous_filename
                .as_ref()
                .context("Renamed PR file missing previous path")?;
            if previous != &server.filename {
                let old = local
                    .remove(previous)
                    .with_context(|| format!("Renamed PR source absent: {previous}"))?;
                ensure!(
                    old.new.is_none() && file.old.is_none(),
                    "Ambiguous renamed PR comparison"
                );
                file.old = old.old;
            }
        }
        files.push(file);
    }
    ensure!(
        local.is_empty(),
        "Local comparison contains {} files absent from GitHub metadata; snapshot not activated",
        local.len()
    );
    files.sort_by(|left, right| {
        left.file_key()
            .unwrap_or_default()
            .cmp(right.file_key().unwrap_or_default())
    });
    group.files = files;
    let format = group.endpoints.right.format;
    snapshot.validate(format)?;
    Ok(())
}

type PendingReviewKey = (gpui::EntityId, Option<gpui::EntityId>, String, String);
#[derive(Clone, Default)]
struct PendingReviewOpens(
    Arc<
        std::sync::Mutex<
            BTreeMap<
                PendingReviewKey,
                Vec<futures::channel::oneshot::Sender<std::result::Result<(), String>>>,
            >,
        >,
    >,
);
impl Global for PendingReviewOpens {}
#[derive(Default)]
struct LastReviewScopes(BTreeMap<PendingReviewKey, String>);
impl Global for LastReviewScopes {}
#[derive(Default)]
struct RestoredReviewViews(BTreeMap<(gpui::EntityId, String), WeakEntity<ReviewView>>);
impl Global for RestoredReviewViews {}

fn remember_review_scope(
    workspace: gpui::EntityId,
    repository: gpui::EntityId,
    session: &ReviewSession,
    cx: &mut App,
) {
    let Some(identity) = session.selector.pull_request_identity() else {
        return;
    };
    if cx.try_global::<LastReviewScopes>().is_none() {
        cx.set_global(LastReviewScopes::default());
    }
    cx.global_mut::<LastReviewScopes>().0.insert(
        (
            workspace,
            Some(repository),
            identity.pull_request_node_id.clone(),
            identity.viewer_node_id.clone(),
        ),
        session.id.clone(),
    );
}
enum PendingReviewJoin {
    Owner(PendingReviewOpen),
    Waiting(futures::channel::oneshot::Receiver<std::result::Result<(), String>>),
}
impl PendingReviewOpens {
    fn join(&self, key: PendingReviewKey) -> PendingReviewJoin {
        let mut requests = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(waiters) = requests.get_mut(&key) {
            let (sender, receiver) = futures::channel::oneshot::channel();
            waiters.push(sender);
            PendingReviewJoin::Waiting(receiver)
        } else {
            requests.insert(key.clone(), Vec::new());
            PendingReviewJoin::Owner(PendingReviewOpen {
                registry: self.clone(),
                key,
                completed: false,
            })
        }
    }
}
struct PendingReviewOpen {
    registry: PendingReviewOpens,
    key: PendingReviewKey,
    completed: bool,
}
impl PendingReviewOpen {
    fn complete(&mut self, result: &Result<()>) {
        let result = result
            .as_ref()
            .map(|_| ())
            .map_err(|error| format!("{error:#}"));
        self.resolve(result);
    }
    fn resolve(&mut self, result: std::result::Result<(), String>) {
        let waiters = self
            .registry
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.key)
            .unwrap_or_default();
        self.completed = true;
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }
}
impl Drop for PendingReviewOpen {
    fn drop(&mut self) {
        if !self.completed {
            self.resolve(Err(
                "Pull request opening was cancelled before completion; retry Files.".into(),
            ));
        }
    }
}

fn cached_pr_remote_matches(
    reference: &PullRequestReference,
    initial_repository_name: &str,
    remote: &crate::github_pull_requests::RepositoryName,
    metadata: &PullRequestMetadata,
) -> bool {
    let expected = reference.repository.full_name();
    remote.full_name().eq_ignore_ascii_case(&expected)
        && initial_repository_name.eq_ignore_ascii_case(&expected)
        && format!("{}/{}", metadata.owner, metadata.repository).eq_ignore_ascii_case(&expected)
        && metadata.number == reference.number
}

#[cfg(test)]
mod pending_review_open_tests {
    use super::*;
    use futures::FutureExt as _;

    fn key() -> PendingReviewKey {
        (
            gpui::EntityId::from(1),
            Some(gpui::EntityId::from(2)),
            "PR_generic".into(),
            "U_generic".into(),
        )
    }
    fn owner(registry: &PendingReviewOpens) -> PendingReviewOpen {
        match registry.join(key()) {
            PendingReviewJoin::Owner(owner) => owner,
            _ => panic!("first open must own work"),
        }
    }
    fn waiter(
        registry: &PendingReviewOpens,
    ) -> futures::channel::oneshot::Receiver<std::result::Result<(), String>> {
        match registry.join(key()) {
            PendingReviewJoin::Waiting(waiter) => waiter,
            _ => panic!("duplicate open must wait"),
        }
    }

    #[test]
    fn duplicate_open_waits_for_real_success_instead_of_reporting_early_completion() {
        let registry = PendingReviewOpens::default();
        let mut owner = owner(&registry);
        let mut first = waiter(&registry);
        let second = waiter(&registry);
        assert!((&mut first).now_or_never().is_none());
        owner.complete(&Ok(()));
        assert!(futures::executor::block_on(first).unwrap().is_ok());
        assert!(futures::executor::block_on(second).unwrap().is_ok());
        assert!(registry.0.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_open_receives_failure_and_can_retry_after_owner_cancellation() {
        let registry = PendingReviewOpens::default();
        let mut first_owner = owner(&registry);
        let first = waiter(&registry);
        first_owner.complete(&Err(anyhow::anyhow!("Generic transport failure")));
        assert_eq!(
            futures::executor::block_on(first).unwrap().unwrap_err(),
            "Generic transport failure"
        );
        let second_owner = owner(&registry);
        let second = waiter(&registry);
        drop(second_owner);
        assert!(
            futures::executor::block_on(second)
                .unwrap()
                .unwrap_err()
                .contains("cancelled")
        );
        assert!(matches!(registry.join(key()), PendingReviewJoin::Owner(_)));
    }

    #[test]
    fn cold_cache_requires_remote_metadata_and_pr_number_agreement() {
        let reference =
            PullRequestReference::parse("https://github.com/example/repository/pull/7", None)
                .unwrap();
        let remote = reference.repository.clone();
        let mut metadata = PullRequestMetadata {
            owner: "example".into(),
            repository: "repository".into(),
            number: 7,
            title: "Generic review".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(2),
        };
        assert!(cached_pr_remote_matches(
            &reference,
            "EXAMPLE/REPOSITORY",
            &remote,
            &metadata
        ));
        assert!(!cached_pr_remote_matches(
            &reference,
            "other/repository",
            &remote,
            &metadata
        ));
        metadata.number = 8;
        assert!(!cached_pr_remote_matches(
            &reference,
            "example/repository",
            &remote,
            &metadata
        ));
        metadata.number = 7;
        metadata.owner = "other".into();
        assert!(!cached_pr_remote_matches(
            &reference,
            "example/repository",
            &remote,
            &metadata
        ));
    }
}

/// Immutable, prepared data is shared across file selections, not active editors.
/// Eviction never destroys the active editor: it retains its own entity handles.
#[derive(Clone)]
struct PreparedReviewFile {
    fingerprint: String,
    buffer: Entity<language::Buffer>,
    diff: Entity<BufferDiff>,
    retained: Arc<RetainedContents>,
    cost: usize,
    binary: bool,
}

fn review_file_window(file_count: usize, selected: usize) -> std::ops::Range<usize> {
    if file_count <= 50 {
        return 0..file_count;
    }
    let selected = selected.min(file_count.saturating_sub(1));
    selected.saturating_sub(5)..selected.saturating_add(6).min(file_count)
}

struct ReviewTreeSettings {
    indent_size: f32,
    spacing: ui::ListItemSpacing,
    file_icons: bool,
    bold_folders: bool,
    indent_guides: bool,
}
impl Settings for ReviewTreeSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let settings = content
            .project_panel
            .as_ref()
            .expect("default ProjectPanel settings");
        Self {
            indent_size: **settings.indent_size.as_ref().expect("default indentation"),
            spacing: match settings.entry_spacing.expect("default entry spacing") {
                settings::ProjectPanelEntrySpacing::Comfortable => ui::ListItemSpacing::Dense,
                settings::ProjectPanelEntrySpacing::Standard => ui::ListItemSpacing::ExtraDense,
            },
            file_icons: settings.file_icons.unwrap_or(true),
            bold_folders: settings.bold_folder_labels.unwrap_or(false),
            indent_guides: settings
                .indent_guides
                .as_ref()
                .and_then(|guides| guides.show)
                .unwrap_or(settings::ShowIndentGuides::Always)
                == settings::ShowIndentGuides::Always,
        }
    }
}

pub struct ReviewView {
    project: Entity<Project>,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    db: ReviewDb,
    data: ReviewData,
    path_input: Entity<InputField>,
    path_filter_task: Option<Task<()>>,
    recovery_task: Option<Task<()>>,
    progress_reload_task: Option<Task<()>>,
    recovery_running: bool,
    history_request: u64,
    history_loading: bool,
    history_error: Option<String>,
    history_task: Option<Task<()>>,
    tree_scroll: UniformListScrollHandle,
    tree_width_preview: Option<u32>,
    tree_resizing: bool,
    review_content_width: f32,
    editor: Option<Entity<SplittableEditor>>,
    github_panel: Option<Entity<GitHubReviewPanel>>,
    github_panel_error: Option<String>,
    github_open: bool,
    github_panel_width: f32,
    github_viewed_intents: BTreeMap<String, bool>,
    github_viewed_pending_clicks: BTreeMap<String, bool>,
    github_viewed_accepting_clicks: BTreeMap<String, bool>,
    github_viewed_intent_sequence: u64,
    github_viewed_remaining: Option<github_viewed::ViewedRemainder>,
    github_viewed_resume_origin: Option<(ReviewOperation, Vec<String>)>,
    presentation_task: Option<Task<()>>,
    navigation_pending: bool,
    navigation_sequence: u64,
    file_request: u64,
    cached_unverified: bool,
    prepared_files: VecDeque<PreparedReviewFile>,
    prepared_bytes: usize,
    loading_files: BTreeMap<
        String,
        futures::future::Shared<Task<Result<PreparedReviewFile, Arc<anyhow::Error>>>>,
    >,
    prewarm_task: Option<Task<()>>,
    preparation_lock: Arc<futures::lock::Mutex<()>>,
    copied_path: Option<String>,
    copied_path_task: Option<Task<()>>,
    selected_patch_contents: Vec<(String, FileComparison, RetainedContents)>,
    github_buffers: Vec<CanonicalCommentBuffer>,
    github_capture: Option<CapturedCommentAnchor>,
    github_composer_capture: Option<CapturedCommentAnchor>,
    github_blocks: Vec<(Entity<Editor>, CustomBlockId)>,
    github_block_keys: Vec<(Option<String>, multi_buffer::Anchor, u32)>,
    github_selection_block: Option<(Entity<Editor>, CustomBlockId)>,
    github_subscriptions: Vec<Subscription>,
    github_editor_subscription: Option<Subscription>,
    github_layout_subscription: Option<Subscription>,
    github_reveal: Option<String>,
    github_conversation_anchor: Option<crate::github_pull_requests::ConversationAnchor>,
    github_viewed_busy: bool,
    github_viewed_work_sequence: u64,
    github_viewed_preparing: bool,
    github_viewed_progress_task: Option<Task<()>>,
    github_viewed_guard: Option<GitHubRequestGuard>,
    github_viewed_status: Option<String>,
    github_viewed_error: Option<String>,
    github_viewed_initial_pending: bool,
    github_viewed_cancel: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    github_viewed_retention_pause: Option<Task<()>>,
    #[cfg(test)]
    github_viewed_sync_override: Option<Task<Result<github_viewed::ViewedWork>>>,
    retained: Option<RetainedContents>,
    multi_loaded: bool,
    loaded_window_scope: Option<(u64, String, bool)>,
    rolling_scroll: bool,
    rolling_file_load: bool,
    rolling_file_task: Option<Task<()>>,
    collapsed_review_files: BTreeSet<String>,
    local_viewed_pin: Option<String>,
    layout_note: Option<String>,
    busy: bool,
    error: Option<String>,
    focus: FocusHandle,
    task: Option<Task<()>>,
    checking: bool,
    refreshing: bool,
    refresh_task: Option<Task<()>>,
    full_refresh_pending: bool,
    update_available: bool,
    check_error: Option<String>,
    check_request: u64,
    check_task: Option<Task<()>>,
    pr_check_guard: Option<GitHubRequestGuard>,
    observed_revision: u64,
    changes: Entity<ReviewChangeBus>,
    _subscriptions: Vec<Subscription>,
}

/// Isolate the full checkbox gesture, including when disabled, from row/editor actions.
fn review_checkbox(
    id: impl Into<gpui::ElementId>,
    checkbox: Checkbox,
    disabled: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .debug_selector(|| "REVIEW_CHECKBOX_TARGET".into())
        .flex_none()
        .min_w(px(24.))
        .min_h(px(24.))
        .justify_center()
        .block_mouse_except_scroll()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            if !disabled {
                on_click(window, cx);
            }
        })
        .child(checkbox.disabled(disabled))
}

struct ReviewFileHeaderAddon {
    review: WeakEntity<ReviewView>,
}

impl editor::Addon for ReviewFileHeaderAddon {
    fn definitions(
        &self,
        buffer: &Entity<language::Buffer>,
        position: text::Anchor,
        kind: editor::GotoDefinitionKind,
        cx: &mut App,
    ) -> Option<Task<Result<Option<Vec<project::LocationLink>>>>> {
        navigation::checkout_definitions(&self.review, buffer, position, kind, cx)
    }

    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn render_buffer_header_trailing_controls(
        &self,
        _: &multi_buffer::ExcerptBoundaryInfo,
        buffer: &language::BufferSnapshot,
        _: &Window,
        cx: &App,
    ) -> Option<gpui::AnyElement> {
        let path = buffer.file()?.path().to_string();
        let copied = self.review.upgrade()?.read(cx).copied_path.as_ref() == Some(&path);
        let review = self.review.clone();
        Some(
            IconButton::new(
                SharedString::from(format!("review-copy-path-{path}")),
                if copied {
                    IconName::Check
                } else {
                    IconName::Copy
                },
            )
            .tooltip(ui::Tooltip::text(if copied {
                "Copied relative path"
            } else {
                "Copy relative path"
            }))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.clone()));
                review
                    .update(cx, |view, cx| {
                        view.copied_path = Some(path.clone());
                        view.copied_path_task = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(std::time::Duration::from_secs(2))
                                .await;
                            this.update(cx, |view, cx| {
                                view.copied_path = None;
                                cx.notify();
                            })
                            .log_err();
                        }));
                        cx.notify();
                    })
                    .log_err();
            })
            .into_any_element(),
        )
    }

    fn open_buffer_header(
        &self,
        buffer: &language::BufferSnapshot,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(path) = buffer.file().map(|file| file.path().to_string()) else {
            return false;
        };
        let Some(review) = self.review.upgrade() else {
            return false;
        };
        if review
            .read(cx)
            .data
            .session
            .presentation
            .open_reviewed_revision
        {
            return false;
        }
        review.update(cx, |view, cx| view.open_current_file(&path, window, cx));
        true
    }

    fn render_buffer_header_controls(
        &self,
        _: &multi_buffer::ExcerptBoundaryInfo,
        buffer: &language::BufferSnapshot,
        _: &Window,
        cx: &App,
    ) -> Option<gpui::AnyElement> {
        let review = self.review.upgrade()?;
        let view = review.read(cx);
        let path = buffer.file()?.path().to_string();
        let full_pr = view.data.session.selector.supports_github_viewed();
        let is_selected = view.data.session.presentation.selected_path.as_ref() == Some(&path);
        let viewed = view.effective_github_viewed(&path);
        let disabled = view.recovery_running
            || view.busy
            || view.checking
            || view.refreshing
            || view.outdated()
            || view.data.session.presentation.since_review
            || (!full_pr && !is_selected);
        let review = self.review.clone();
        Some(
            review_checkbox(
                gpui::SharedString::from(format!("review-header-viewed-target-{path}")),
                Checkbox::new(
                    gpui::SharedString::from(format!("review-header-viewed-{path}")),
                    viewed.into(),
                )
                .fill()
                .style(ui::ToggleStyle::Custom(cx.theme().colors().text))
                .label(if full_pr { "Viewed" } else { "Reviewed" }),
                disabled,
                move |window, cx| {
                    review
                        .update(cx, |view, cx| {
                            if full_pr {
                                view.set_github_paths_viewed(
                                    vec![path.clone()],
                                    !viewed,
                                    None,
                                    window,
                                    cx,
                                );
                            } else {
                                view.toggle_viewed(&ToggleReviewViewed, window, cx);
                            }
                        })
                        .log_err();
                },
            )
            .into_any_element(),
        )
    }
}

impl ReviewView {
    fn ensure_github_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(feature = "review-test-support")]
        if github_viewed::fixture::matches(&self.data.session.selector) {
            return;
        }
        if self.cached_unverified {
            return;
        }
        let Some(metadata) = self.data.pull_request.clone() else {
            return;
        };
        let Some(identity) = self.data.session.selector.pull_request_identity() else {
            return;
        };
        if self.github_panel.as_ref().is_some_and(|panel| {
            let context = panel.read(cx).remote_context();
            panel.read(cx).operation_context().selector == self.data.session.selector
                && &context.identity == identity
                && context.head_oid == metadata.head_ref_oid
                && context.base_tip_oid == metadata.base_ref_oid
        }) {
            return;
        }
        let context = ReviewOperationContext {
            selector: self.data.session.selector.clone(),
            metadata,
        };
        if let Err(error) = context.validate() {
            self.github_panel_error = Some(format!("GitHub review unavailable: {error:#}"));
            return;
        }
        let identity = identity.clone();
        let panel =
            cx.new(|cx| GitHubReviewPanel::new(self.db.clone(), context, identity, window, cx));
        // Construction errors must leave the local comparison usable.
        {
            {
                self.github_subscriptions.clear();
                self.github_subscriptions
                    .push(cx.observe(&panel, |_, _, cx| cx.notify()));
                self.github_subscriptions.push(cx.subscribe_in(
                    &panel,
                    window,
                    |this, _, event, window, cx| {
                        match event {
                            GitHubReviewEvent::LocalOperationsChanged => {
                                this.reload_shared_progress(cx)
                            }
                            GitHubReviewEvent::ThreadsChanged => {
                                this.capture_github_selection(cx);
                                this.rebuild_github_blocks(window, cx)
                            }
                            GitHubReviewEvent::Compose => this.rebuild_github_blocks(window, cx),
                            GitHubReviewEvent::Recover(action) => {
                                this.start_recovery(*action, false, window, cx)
                            }
                            GitHubReviewEvent::ClosePanel => {
                                this.github_open = false;
                                cx.notify();
                            }
                            GitHubReviewEvent::ViewedOperation(operation, reconcile) => this
                                .handle_github_viewed_operation(
                                    operation.clone(),
                                    *reconcile,
                                    window,
                                    cx,
                                ),
                        }
                        cx.notify();
                    },
                ));
                self.github_panel = Some(panel);
                self.github_panel_error = None;
            }
        }
    }

    fn capture_github_selection(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.github_panel.clone() else {
            return;
        };
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let context = panel.read(cx).remote_context().clone();
        let mut buffers = self.github_buffers.clone();
        for buffer in &mut buffers {
            let patch = panel
                .read(cx)
                .canonical_patches()
                .get(&buffer.path)
                .and_then(|patch| patch.as_deref());
            if let Err(error) =
                review_comment_anchor::restrict_to_github_patch(std::slice::from_mut(buffer), patch)
            {
                buffer.allowed_lines.clear();
                log::debug!("GitHub inline location unavailable: {error}");
            }
        }
        let captured = editor.update(cx, |editor, cx| {
            review_comment_anchor::capture_selection(
                editor,
                &buffers,
                &context,
                &self.data.session.selector,
                cx,
            )
        });
        match captured {
            Ok(captured) => {
                panel.update(cx, |panel, cx| {
                    panel.set_anchor(Ok(captured.anchor.clone()), cx)
                });
                self.github_capture = Some(captured);
            }
            Err(error) => {
                panel.update(cx, |panel, cx| panel.set_anchor(Err(error), cx));
                self.github_capture = None;
            }
        }
    }

    fn clear_selection_affordance(&mut self, cx: &mut Context<Self>) {
        if let Some((owner, id)) = self.github_selection_block.take() {
            owner.update(cx, |editor, cx| {
                editor.remove_blocks([id].into_iter().collect(), None, cx)
            });
        }
    }

    fn update_selection_affordance(&mut self, cx: &mut Context<Self>) {
        self.clear_selection_affordance(cx);
        let Some(captured) = &self.github_capture else {
            return;
        };
        if self.cached_unverified || self.data.session.presentation.since_review {
            return;
        }
        let nonempty = captured
            .editor
            .read(cx)
            .selections
            .newest_anchor()
            .range()
            .start
            != captured
                .editor
                .read(cx)
                .selections
                .newest_anchor()
                .range()
                .end;
        if !nonempty {
            return;
        }
        let owner = captured.editor.clone();
        let placement = captured.placement;
        let view = cx.weak_entity();
        let render = Arc::new(move |_: &mut editor::display_map::BlockContext| {
            let view = view.clone();
            h_flex()
                .w_full()
                .justify_end()
                .pr_2()
                .child(
                    Button::new("review-selection-comment", "Comment")
                        .style(ButtonStyle::Filled)
                        .start_icon(Icon::new(IconName::Chat))
                        .on_click(move |_, window, cx| {
                            view.update(cx, |view, cx| view.comment_on_selection(window, cx))
                                .log_err();
                        }),
                )
                .into_any_element()
        });
        let ids = owner.update(cx, |editor, cx| {
            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Near(placement),
                    style: BlockStyle::Flex,
                    height: Some(2),
                    render,
                    priority: 1,
                }],
                None,
                cx,
            )
        });
        if let Some(id) = ids.into_iter().next() {
            self.github_selection_block = Some((owner, id));
        }
    }

    fn review_context_menu(
        editor: &mut Editor,
        review: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Entity<ContextMenu> {
        let has_selection = editor.has_non_empty_selection(&editor.display_snapshot(cx));
        let editor_id = cx.entity_id();
        // The editor is leased while this factory runs. Read only the already
        // captured canonical selection; never reenter it from the menu builder.
        let can_comment = has_selection
            && review
                .read_with(cx, |view, _| {
                    !view.busy
                        && !view.cached_unverified
                        && !view.outdated()
                        && !view.data.session.presentation.since_review
                        && view.data.session.selector.supports_github_viewed()
                        && view
                            .github_capture
                            .as_ref()
                            .is_some_and(|capture| capture.editor.entity_id() == editor_id)
                })
                .unwrap_or(false);
        let focus = editor.focus_handle(cx);
        ContextMenu::build(window, cx, move |menu, _, _| {
            menu.context(focus.clone())
                .when(can_comment, |menu| {
                    let focus = focus.clone();
                    menu.entry("Comment on selection", None, move |window, cx| {
                        let review = review.clone();
                        let focus = focus.clone();
                        // Menu/editor listeners have both released their leases
                        // before the shared composer recaptures the active side.
                        window.defer(cx, move |window, cx| {
                            focus.focus(window, cx);
                            review
                                .update(cx, |view, cx| view.comment_on_selection(window, cx))
                                .log_err();
                        });
                    })
                    .separator()
                })
                .action_disabled_when(!has_selection, "Copy", Box::new(editor::actions::Copy))
        })
    }

    fn install_review_context_menus(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let panes = std::iter::once(editor.read(cx).rhs_editor().clone())
            .chain(editor.read(cx).lhs_editor().cloned())
            .collect::<Vec<_>>();
        for pane in panes {
            let review = cx.weak_entity();
            pane.update(cx, |editor, _| {
                editor.set_custom_context_menu(move |editor, _, window, cx| {
                    Some(Self::review_context_menu(
                        editor,
                        review.clone(),
                        window,
                        cx,
                    ))
                });
            });
        }
    }

    fn comment_on_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.data.session.presentation.since_review {
            self.github_viewed_error =
                Some("Open Full PR comparison to comment on canonical GitHub diff lines.".into());
            cx.notify();
            return;
        }
        let Some(panel) = self.github_panel.clone() else {
            return;
        };
        // A pane can gain focus without changing its selection. Selection-event
        // captures may also precede the split editor's focus notification.
        self.capture_github_selection(cx);
        self.clear_selection_affordance(cx);
        if self.data.session.selector.supports_github_viewed() {
            panel.update(cx, |panel, cx| panel.begin_comment(window, cx));
        } else if let Some(path) = self.data.session.presentation.selected_path.clone() {
            panel.update(cx, |panel, cx| {
                panel.begin_unlocated_comment(path, window, cx)
            });
        }
        cx.notify();
    }

    /// Cross-links carry immutable API coordinates, never parsed display text.
    /// A pending target survives file preparation and asynchronous thread reads.
    pub(crate) fn reveal_conversation_anchor(
        &mut self,
        anchor: crate::github_pull_requests::ConversationAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let result = self.reveal_conversation_anchor_impl(anchor, window, cx);
        if let Err(error) = &result {
            self.check_error = Some(format!("Cannot reveal conversation location: {error:#}"));
            cx.notify();
        }
        result
    }

    fn reveal_conversation_anchor_impl(
        &mut self,
        anchor: crate::github_pull_requests::ConversationAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        ensure!(
            !self.outdated(),
            "Review snapshot changed; Refresh Files before revealing this conversation location"
        );
        ensure!(
            self.data.session.selector.supports_github_viewed(),
            "Open All commits before revealing a current PR conversation location"
        );
        ensure!(
            self.data.session.selector.pull_request_identity() == Some(&anchor.identity),
            "The conversation location belongs to a different PR, repository or account"
        );
        ensure!(
            !anchor.is_outdated,
            "This thread is outdated; its historical coordinates cannot be mapped to the current PR diff"
        );
        ensure!(
            !anchor.thread_id.is_empty(),
            "The conversation thread has no stable identity"
        );
        let metadata = self
            .data
            .pull_request
            .as_ref()
            .context("PR context unavailable")?;
        ensure!(
            metadata.head_ref_oid == anchor.head_oid,
            "The PR head changed; Refresh Files before opening this conversation location"
        );
        ensure!(
            self.data.files.contains(&anchor.path),
            "The conversation file is absent from this PR snapshot"
        );
        ensure!(
            anchor.subject_type == crate::github_review::CommentSubject::File
                || anchor.line.is_some_and(|line| line > 0),
            "The thread has no current diff line; original historical lines are never substituted"
        );
        if let Some(start) = anchor.start_line {
            ensure!(
                anchor.line.is_some_and(|line| start > 0 && start <= line)
                    && anchor.start_side == Some(anchor.side),
                "The conversation range is invalid or crosses diff sides"
            );
        }
        let path = anchor.path.clone();
        self.github_conversation_anchor = Some(anchor);
        self.collapsed_review_files.remove(&path);
        self.apply_review_file_folds(cx);
        self.check_error = Some("Opening conversation location…".into());
        let mut presentation = self.data.session.presentation.clone();
        // A direct link explicitly reveals its file, even if a filter/fold hid it.
        presentation.path_filter.clear();
        presentation.unreviewed_only = false;
        presentation.selected_path = Some(path.clone());
        let mut ancestor = path.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            presentation.collapsed_directories.remove(parent);
            ancestor = parent;
        }
        self.change_presentation(presentation, window, cx);
        if !self.busy {
            self.rebuild_github_blocks(window, cx);
        }
        cx.notify();
        Ok(())
    }

    fn reveal_pending_conversation_anchor(&mut self, cx: &mut Context<Self>) -> Result<bool> {
        let Some(anchor) = self.github_conversation_anchor.as_ref() else {
            return Ok(false);
        };
        if self.busy || self.data.session.presentation.selected_path.as_ref() != Some(&anchor.path)
        {
            return Ok(false);
        }
        let Some(panel) = &self.github_panel else {
            return Ok(false);
        };
        let Some(thread) = panel
            .read(cx)
            .threads()
            .iter()
            .find(|thread| thread.id == anchor.thread_id)
        else {
            ensure!(
                !panel.read(cx).threads_read_finished(),
                "The conversation thread is unavailable in the completed Files discussion read; Refresh Conversation and try again"
            );
            return Ok(false);
        };
        ensure!(
            !thread.is_outdated
                && thread.path == anchor.path
                && thread.diff_side == anchor.side
                && thread.line == anchor.line
                && thread.start_line == anchor.start_line
                && thread.start_diff_side == anchor.start_side
                && thread.subject_type == anchor.subject_type
                && thread.original_line == anchor.original_line
                && thread.original_start_line == anchor.original_start_line,
            "The thread location changed while opening; refresh Conversation and try again"
        );
        let editor = self
            .editor
            .as_ref()
            .context("The conversation diff is not loaded")?;
        let (owner, placement) =
            if anchor.subject_type == crate::github_review::CommentSubject::File {
                let owner = editor.read(cx).rhs_editor().clone();
                let multibuffer = owner.read(cx).buffer().read(cx);
                let buffer = multibuffer
                    .all_buffers_iter()
                    .find(|buffer| {
                        buffer
                            .read(cx)
                            .file()
                            .is_some_and(|file| file.path().to_string() == anchor.path)
                    })
                    .context("The conversation file is not rendered")?;
                let placement = multibuffer
                    .snapshot(cx)
                    .anchor_in_excerpt(
                        buffer
                            .read(cx)
                            .snapshot()
                            .anchor_before(language::Point::zero()),
                    )
                    .context("The conversation file header is not visible")?;
                (owner, placement)
            } else {
                review_comment_anchor::placement_for_anchor(
                    editor.read(cx),
                    &self.github_buffers,
                    &anchor.path,
                    anchor.side,
                    anchor.line.context("Conversation line unavailable")?,
                    cx,
                )?
            };
        self.github_reveal = Some(anchor.thread_id.clone());
        owner.update(cx, |editor, cx| {
            editor.request_autoscroll(editor::scroll::Autoscroll::fit().for_anchor(placement), cx)
        });
        self.github_conversation_anchor = None;
        self.check_error = None;
        Ok(true)
    }

    fn rebuild_github_blocks(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.github_panel.clone() else {
            return;
        };
        let Some(editor) = self.editor.clone() else {
            return;
        };
        for buffer in &mut self.github_buffers {
            let patch = panel
                .read(cx)
                .canonical_patches()
                .get(&buffer.path)
                .and_then(|patch| patch.as_deref());
            if let Err(error) =
                review_comment_anchor::restrict_to_github_patch(std::slice::from_mut(buffer), patch)
            {
                buffer.allowed_lines.clear();
                log::debug!("GitHub inline block location unavailable: {error}");
            }
        }
        if let Err(error) = self.reveal_pending_conversation_anchor(cx) {
            self.github_conversation_anchor = None;
            self.check_error = Some(format!("Cannot reveal conversation location: {error:#}"));
        }
        let mut previous = std::mem::take(&mut self.github_blocks)
            .into_iter()
            .zip(std::mem::take(&mut self.github_block_keys))
            .collect::<Vec<_>>();
        let threads = panel.read(cx).threads().to_vec();
        let mut placements = Vec::new();
        for thread in &threads {
            if let Ok((owner, placement)) = review_comment_anchor::placement_for_thread(
                editor.read(cx),
                &self.github_buffers,
                thread,
                cx,
            ) {
                placements.push((owner, placement, Some(thread.id.clone())));
            }
        }
        for (operation, anchor) in panel.read(cx).local_inline_comments() {
            if let Some(line) = anchor.line {
                if let Ok((owner, placement)) = review_comment_anchor::placement_for_anchor(
                    editor.read(cx),
                    &self.github_buffers,
                    &anchor.path,
                    anchor.side,
                    line,
                    cx,
                ) {
                    placements.push((owner, placement, Some(format!("operation:{operation}"))));
                }
            }
        }
        let composer_anchor = panel.read(cx).composer_anchor().cloned();
        let same_location =
            |left: &crate::github_review::CommentAnchor,
             right: &crate::github_review::CommentAnchor| {
                left.path == right.path
                    && left.side == right.side
                    && left.line == right.line
                    && left.start_line == right.start_line
                    && left.commit_oid == right.commit_oid
            };
        if !self
            .github_composer_capture
            .as_ref()
            .is_some_and(|captured| {
                composer_anchor
                    .as_ref()
                    .is_some_and(|anchor| same_location(&captured.anchor, anchor))
            })
        {
            self.github_composer_capture = self
                .github_capture
                .as_ref()
                .filter(|captured| {
                    composer_anchor
                        .as_ref()
                        .is_some_and(|anchor| same_location(&captured.anchor, anchor))
                })
                .map(|captured| CapturedCommentAnchor {
                    anchor: captured.anchor.clone(),
                    editor: captured.editor.clone(),
                    placement: captured.placement,
                });
        }
        let composer_thread = panel.read(cx).composer_thread().map(str::to_string);
        let composer_placement = if let Some(id) = composer_thread.as_ref() {
            threads
                .iter()
                .find(|thread| &thread.id == id)
                .and_then(|thread| {
                    review_comment_anchor::placement_for_thread(
                        editor.read(cx),
                        &self.github_buffers,
                        thread,
                        cx,
                    )
                    .ok()
                })
        } else {
            self.github_composer_capture
                .as_ref()
                .filter(|captured| {
                    let current = editor.read(cx);
                    captured.editor == *current.rhs_editor()
                        || current
                            .lhs_editor()
                            .is_some_and(|lhs| lhs == &captured.editor)
                })
                .map(|captured| (captured.editor.clone(), captured.placement))
                .or_else(|| {
                    let anchor = composer_anchor.as_ref()?;
                    review_comment_anchor::placement_for_anchor(
                        editor.read(cx),
                        &self.github_buffers,
                        &anchor.path,
                        anchor.side,
                        anchor.line?,
                        cx,
                    )
                    .ok()
                })
        };
        let has_composer = panel.read(cx).has_inline_composer();
        panel.update(cx, |panel, cx| {
            panel.set_inline_composer(has_composer && composer_placement.is_some(), cx)
        });
        if has_composer && composer_thread.is_none() {
            if let Some((owner, placement)) = composer_placement {
                placements.push((owner, placement, None));
            } else {
                self.github_panel_error = Some("The selected historical location is no longer visible. Select its lines again to continue the saved draft.".into());
            }
        }
        for (owner, placement, thread_id) in placements {
            let composing_thread = thread_id
                .as_ref()
                .is_some_and(|id| composer_thread.as_ref() == Some(id));
            let reveal = thread_id.as_ref() == self.github_reveal.as_ref()
                || thread_id.is_none()
                || composing_thread;
            let delivering = thread_id
                .as_deref()
                .is_some_and(|id| panel.read(cx).thread_has_delivery(id));
            let height = if composing_thread || delivering {
                22
            } else if thread_id.is_some() {
                14
            } else {
                12
            };
            if let Some(index) =
                previous
                    .iter()
                    .position(|((existing, _), (id, anchor, old_height))| {
                        existing == &owner
                            && id == &thread_id
                            && anchor == &placement
                            && old_height == &height
                    })
            {
                let (block, key) = previous.swap_remove(index);
                self.github_blocks.push(block);
                self.github_block_keys.push(key);
                if reveal {
                    owner.update(cx, |editor, cx| {
                        editor.request_autoscroll(
                            editor::scroll::Autoscroll::fit().for_anchor(placement),
                            cx,
                        )
                    });
                }
                continue;
            }
            let card = cx.new(|cx| {
                if let Some(operation) = thread_id
                    .as_deref()
                    .and_then(|id| id.strip_prefix("operation:"))
                {
                    GitHubInlineCard::new_operation(&panel, operation.to_owned(), cx)
                } else {
                    GitHubInlineCard::new(&panel, thread_id.clone(), cx)
                }
            });
            let render = Arc::new(move |context: &mut editor::display_map::BlockContext| {
                div()
                    .id("github-inline-review")
                    .w_full()
                    .h(context.line_height * height as f32)
                    .overflow_y_scroll()
                    .block_mouse_except_scroll()
                    .child(card.clone())
                    .into_any_element()
            });
            let blocks = owner.update(cx, |editor, cx| {
                editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Below(placement),
                        style: BlockStyle::Flex,
                        height: Some(height),
                        render,
                        priority: 0,
                    }],
                    if reveal {
                        Some(editor::scroll::Autoscroll::fit().for_anchor(placement))
                    } else {
                        None
                    },
                    cx,
                )
            });
            for id in blocks {
                self.github_blocks.push((owner.clone(), id));
                self.github_block_keys
                    .push((thread_id.clone(), placement, height));
            }
        }
        for ((owner, id), _) in previous {
            owner.update(cx, |editor, cx| {
                editor.remove_blocks([id].into_iter().collect(), None, cx)
            });
        }
        self.github_reveal = None;
        cx.notify();
    }

    pub(crate) fn open_pull_request(
        workspace: &mut Workspace,
        initial: PullRequestDetails,
        initial_viewer: GitHubUser,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<Result<()>> {
        Self::open_pull_request_impl(workspace, initial, initial_viewer, false, window, cx)
    }

    fn open_pull_request_impl(
        workspace: &mut Workspace,
        initial: PullRequestDetails,
        initial_viewer: GitHubUser,
        refresh: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Task<Result<()>> {
        // Workspace/worktree, immutable PR identity and account must all match.
        // Navigation is deliberately independent of network revalidation; writes
        // still use the panel's existing account/revision/anchor guards.
        if !refresh {
            let active = workspace.project().read(cx).active_repository(cx);
            let key = (
                workspace.weak_handle().entity_id(),
                active.as_ref().map(Entity::entity_id),
                initial.summary.id.clone(),
                initial_viewer.node_id.clone().unwrap_or_default(),
            );
            let preferred = cx
                .try_global::<LastReviewScopes>()
                .and_then(|scopes| scopes.0.get(&key))
                .cloned();
            let opening = cx
                .try_global::<PendingReviewOpens>()
                .is_some_and(|pending| {
                    pending
                        .0
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .contains_key(&key)
                });
            let existing = workspace
                .items_of_type::<Self>(cx)
                .filter(|item| {
                    let view = item.read(cx);
                    !opening
                        && !view.cached_unverified
                        && active
                            .as_ref()
                            .is_some_and(|active| active == &view.repository)
                        && view
                            .data
                            .session
                            .selector
                            .pull_request_identity()
                            .is_some_and(|identity| {
                                identity.repository_node_id == initial.repository.id
                                    && identity.pull_request_node_id == initial.summary.id
                                    && initial_viewer.node_id.as_deref()
                                        == Some(identity.viewer_node_id.as_str())
                            })
                })
                .max_by_key(|item| {
                    usize::from(
                        preferred.as_deref() == Some(item.read(cx).data.session.id.as_str()),
                    )
                });
            if let Some(existing) = existing {
                // A Files-button listener still holds its PullRequestView lease.
                // Activation deactivates that item synchronously, so defer until
                // the listener returns. Completion means activation actually ran.
                let workspace = workspace.weak_handle();
                let (complete, activated) = futures::channel::oneshot::channel();
                window.defer(cx, move |window, cx| {
                    let result = workspace.update(cx, |workspace, cx| {
                        workspace.activate_item(&existing, true, true, window, cx);
                    });
                    let _ = complete.send(result);
                });
                log::info!("PR Files warm activation: no GitHub, fetch or comparison work");
                return window.spawn(cx, async move |_| {
                    activated
                        .await
                        .context("PR Files activation was cancelled")??;
                    Ok(())
                });
            }
        }
        if cx.try_global::<PendingReviewOpens>().is_none() {
            cx.set_global(PendingReviewOpens::default());
        }
        let pending = cx.global::<PendingReviewOpens>().clone();
        let key = (
            workspace.weak_handle().entity_id(),
            workspace
                .project()
                .read(cx)
                .active_repository(cx)
                .map(|repository| repository.entity_id()),
            initial.summary.id.clone(),
            initial_viewer.node_id.clone().unwrap_or_default(),
        );
        let pending_open = match pending.join(key) {
            PendingReviewJoin::Owner(owner) => owner,
            PendingReviewJoin::Waiting(receiver) => {
                log::info!("PR Files open already pending; awaiting its completion");
                return window.spawn(cx, async move |_| {
                    receiver
                        .await
                        .context("PR open completion channel closed")?
                        .map_err(anyhow::Error::msg)
                });
            }
        };
        let started = Instant::now();
        let reference = PullRequestReference::parse(&initial.summary.url, None);
        let candidates = workspace
            .project()
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .filter_map(|repository| {
                let name = repository
                    .read(cx)
                    .default_remote_url()
                    .and_then(|remote| repository_from_remote(&remote))?;
                Some((repository.clone(), name))
            })
            .collect::<Vec<_>>();
        let active = workspace
            .project()
            .read(cx)
            .active_repository(cx)
            .map(|repository| repository.entity_id());
        let project = workspace.project().clone();
        let weak = workspace.weak_handle();
        let db = ReviewDb::global(cx);
        let askpass_delegates = vec![
            review_askpass(weak.clone(), window, cx),
            review_askpass(weak.clone(), window, cx),
        ];
        window.spawn(cx, async move |cx| {
            let mut pending_open = pending_open;
            let result = async {
            let reference = reference?;
            let expected_viewer = initial_viewer.node_id.context("GitHub account identity unavailable; reconnect")?;
            let identity = PullRequestIdentity { host: "github.com".into(), repository_node_id: initial.repository.id.clone(), pull_request_node_id: initial.summary.id.clone(), viewer_node_id: expected_viewer.clone() };
            // A persisted immutable snapshot can be read before transport only
            // when this exact active worktree, PR/account identity and stored
            // repository metadata all agree. Writes remain revoked until the
            // existing live identity/revision path below succeeds.
            if !refresh {
                if let Some((repository, remote)) = candidates.iter().find(|(repository, remote)| Some(repository.entity_id()) == active && remote.full_name().eq_ignore_ascii_case(&reference.repository.full_name()) && initial.repository.name_with_owner.eq_ignore_ascii_case(&reference.repository.full_name())) {
                    let repository = repository.clone();
                    let remote = remote.clone();
                    let repo_snapshot = repository.read_with(cx, |repository, _| repository.snapshot());
                    let format = object(initial.head_ref_oid.parse()?).format;
                    let cached_data = cx.background_spawn({
                        let db = db.clone(); let identity = identity.clone(); let reference = reference.clone(); let repository_name = initial.repository.name_with_owner.clone();
                        async move {
                            let binding = db.bind_repository(repo_snapshot.common_dir_abs_path.to_path_buf(), repo_snapshot.repository_dir_abs_path.to_path_buf(), format).await?;
                            let Some(session) = db.find_pull_request_session(&binding.id, &identity)? else { return Ok::<_, anyhow::Error>(None); };
                            let Some(metadata) = db.pull_request_metadata(&session.id)? else { return Ok(None); };
                            if !cached_pr_remote_matches(&reference, &repository_name, &remote, &metadata) { return Ok(None); }
                            Ok(Some(read_data(&db, session)?))
                        }
                    }).await?;
                    if let Some(cached_data) = cached_data {
                        weak.update_in(cx, |workspace, window, cx| {
                            let existing = workspace.items_of_type::<Self>(cx).find(|item| item.read(cx).data.session.id == cached_data.session.id);
                            if let Some(existing) = existing {
                                workspace.activate_item(&existing, true, true, window, cx);
                            } else {
                                let view = cx.new(|cx| Self::new(project.clone(), repository.clone(), workspace.weak_handle(), db.clone(), cached_data, window, cx));
                                view.update(cx, |view, cx| { view.cached_unverified = true; view.github_panel = None; view.github_subscriptions.clear(); view.check_error = Some("Cached PR snapshot · verifying account and revision before enabling GitHub writes".into()); cx.notify(); });
                                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                            }
                        })?;
                        log::info!("PR Files cold cached snapshot displayed before GitHub identity verification");
                    }
                }
            }
            let (client, _guard) = GitHubClient::cancellable();
            let repository_names = candidates.iter().map(|(_, name)| name.clone()).collect::<Vec<_>>();
            let target_id = initial.repository.id.clone();
            let lookup_client = client.clone();
            let matches = cx.background_spawn(async move {
                let mut matches = Vec::new();
                for (index, name) in repository_names.iter().enumerate() {
                    match lookup_client.repository_identity(name) {
                        Ok(identity) if identity.id == target_id => matches.push(index),
                        Ok(_) => {},
                        Err(error) => log::debug!("Unable to identify PR checkout candidate: {error}"),
                    }
                }
                matches
            }).await;
            let index = matches.iter().copied().find(|index| candidates.get(*index).is_some_and(|(repository, _)| Some(repository.entity_id()) == active))
                .or_else(|| (matches.len() == 1).then(|| matches.first().copied()).flatten())
                .context("Open a matching GitHub repository checkout before reviewing this PR (choose one active checkout if several match)")?;
            let repository = candidates.get(index).context("PR checkout unavailable")?.0.clone();
            let repo_snapshot = repository.read_with(cx, |repository, _| repository.snapshot());
            let format = object(initial.head_ref_oid.parse()?).format;
            let binding = cx.background_spawn({
                let db = db.clone();
                async move { db.bind_repository(repo_snapshot.common_dir_abs_path.to_path_buf(), repo_snapshot.repository_dir_abs_path.to_path_buf(), format).await }
            }).await?;

            let cached = cx.background_spawn({
                let db = db.clone(); let binding = binding.id.clone(); let identity = identity.clone();
                async move { db.find_pull_request_session(&binding, &identity) }
            }).await?;
            let expected_revision = cached.as_ref().map(|session| session.revision);
            if let Some(session) = cached.filter(|_| !refresh) {
                let cached_data = cx.background_spawn({ let db = db.clone(); async move { read_data(&db, session) } }).await?;
                weak.update_in(cx, |workspace, window, cx| {
                    let existing = workspace.items_of_type::<Self>(cx).find(|item| item.read(cx).data.session.id == cached_data.session.id);
                    if let Some(existing) = existing {
                        workspace.activate_item(&existing, true, true, window, cx);
                        return;
                    }
                    let view = cx.new(|cx| Self::new(project.clone(), repository.clone(), workspace.weak_handle(), db.clone(), cached_data, window, cx));
                    view.update(cx, |view, cx| {
                        view.cached_unverified = true;
                        view.github_panel = None;
                        view.github_subscriptions.clear();
                        view.check_error = Some("Cached PR snapshot · verifying account and revision before enabling GitHub writes".into());
                        cx.notify();
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })?;
            }
            let mut completed = None;
            for (attempt, askpass) in askpass_delegates.into_iter().enumerate() {
                let read_client = client.clone(); let task_reference = reference.clone();
                let (details, viewer) = cx.background_spawn(async move { Ok::<_, anyhow::Error>((read_client.pull_request(&task_reference)?, read_client.viewer()?)) }).await?;
                ensure!(viewer.node_id.as_deref() == Some(expected_viewer.as_str()), "GitHub account changed; reconnect before reviewing");
                ensure!(details.summary.id == identity.pull_request_node_id && details.repository.id == identity.repository_node_id, "GitHub PR identity changed");
                let base_tip = details.base_tip_oid()?.to_string();
                let request = ReviewPrFetch { owner: reference.repository.owner.clone(), repository: reference.repository.name.clone(), base_branch: details.summary.base_ref_name.clone(), number: reference.number, expected_base: base_tip.parse()?, expected_head: details.head_ref_oid.parse()? };
                let fetched = repository.update(cx, |repository, cx| repository.fetch_review_pr(request, askpass, cx)).await;
                let fetched = match fetched { Ok(fetched) => fetched, Err(error) if attempt == 0 => { log::debug!("Retrying changed PR refs: {error}"); continue; }, Err(error) => return Err(error) };
                let comparison = repository.read_with(cx, |repository, cx| repository.review_comparison(fetched.base.to_string(), fetched.head.to_string(), true, cx)).await?;
                let read_client = client.clone(); let task_reference = reference.clone();
                let (after, files, viewer) = cx.background_spawn(async move { { let files = read_client.files(&task_reference)?; let after = read_client.pull_request(&task_reference)?; let viewer = read_client.viewer()?; Ok::<_, anyhow::Error>((after, files, viewer)) } }).await?;
                ensure!(viewer.node_id.as_deref() == Some(expected_viewer.as_str()), "GitHub account changed during PR fetch");
                let after_base_tip = after.base_tip_oid()?.to_string();
                if after.head_ref_oid != details.head_ref_oid || after_base_tip != base_tip { ensure!(attempt == 0, "PR changed repeatedly while fetching; refresh again"); continue; }
                ensure!(files.files.len() == after.changed_files, "GitHub file listing incomplete: received {} of {} (provider limit reached: {})", files.files.len(), after.changed_files, files.limit_reached);
                let mut snapshot = manifest(comparison)?;
                reconcile_pr_files(&mut snapshot, &files.files)?;
                let metadata = PullRequestMetadata { owner: reference.repository.owner.clone(), repository: reference.repository.name.clone(), number: reference.number, title: after.summary.title, base_ref_name: after.summary.base_ref_name, head_ref_name: after.summary.head_ref_name, base_ref_oid: after_base_tip, head_ref_oid: after.head_ref_oid, commit_count: Some(after.commits.total_count) };
                completed = Some((snapshot, metadata)); break;
            }
            let (snapshot, metadata) = completed.context("PR changed while fetching; retry")?;
            let data = cx.background_spawn({ let db = db.clone(); async move {
                let session = db.deploy_pull_request(binding.id, identity, metadata, snapshot, expected_revision).await?;
                read_data(&db, session)
            }}).await?;
            log::info!("pull request review PR manifest ready: {} files in {} ms (excludes paint)", data.files.len(), started.elapsed().as_millis());
            weak.update_in(cx, |workspace, window, cx| {
                let existing = workspace.items_of_type::<Self>(cx).find(|item| item.read(cx).data.session.id == data.session.id);
                if let Some(existing) = existing {
                    existing.update(cx, |view, cx| view.finish(Ok(data), window, cx));
                    workspace.activate_item(&existing, true, true, window, cx);
                } else {
                    let view = cx.new(|cx| Self::new(project, repository, workspace.weak_handle(), db, data, window, cx));
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                }
            })?;
            Ok(())
            }.await;
            pending_open.complete(&result);
            result
        })
    }

    pub fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        ReviewTreeSettings::register(cx);
        #[cfg(feature = "review-test-support")]
        workspace.register_action(|workspace, _: &PreviewReviewViewedFaults, window, cx| {
            github_viewed::fixture::open(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenReview, window, cx| {
            Self::pick(workspace, false, window, cx)
        });
        workspace.register_action(|workspace, _: &OpenDirectReview, window, cx| {
            Self::pick(workspace, true, window, cx)
        });
        workspace.register_action(|workspace, _: &OpenCommitReview, window, cx| {
            if let Some(repository) = workspace.project().read(cx).active_repository(cx) {
                Self::pick_commits(workspace, repository, None, None, window, cx);
            }
        });
        workspace::register_serializable_item::<Self>(cx);
    }

    fn pick(
        workspace: &mut Workspace,
        direct: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            git_ui_core::notifications::show_error_toast(
                cx.entity(),
                "Open code review",
                anyhow::anyhow!("No active Git repository"),
                cx,
            );
            return;
        };
        let weak = workspace.weak_handle();
        let on_base = Arc::new({
            let weak = weak.clone();
            let repository = repository.clone();
            move |base: git::repository::Branch, window: &mut Window, cx: &mut App| {
                let weak = weak.clone();
                let repository = repository.clone();
                window.defer(cx, move |window, cx| {
                    let on_head = Arc::new({
                        let weak = weak.clone();
                        let repository = repository.clone();
                        move |head: git::repository::Branch, window: &mut Window, cx: &mut App| {
                            weak.update(cx, |workspace, cx| {
                                Self::deploy(
                                    workspace,
                                    repository.clone(),
                                    base.ref_name.to_string(),
                                    head.ref_name.to_string(),
                                    direct,
                                    window,
                                    cx,
                                )
                            })
                            .log_err();
                        }
                    });
                    weak.update(cx, |workspace, cx| {
                        // The select callback runs before the old picker's dismiss event.
                        // Remove its subscription before installing another BranchList modal.
                        workspace.hide_modal(window, cx);
                        let selected = repository
                            .read(cx)
                            .branch
                            .as_ref()
                            .map(|branch| branch.ref_name.clone());
                        let weak = workspace.weak_handle();
                        workspace.toggle_modal(window, cx, |window, cx| {
                            branch_picker::select_modal(
                                weak,
                                Some(repository),
                                selected,
                                on_head,
                                window,
                                cx,
                            )
                            .with_selection_prompt(
                                "Select review branch (step 2 of 2)…",
                                window,
                                cx,
                            )
                        });
                    })
                    .log_err();
                });
            }
        });
        workspace.toggle_modal(window, cx, |window, cx| {
            branch_picker::select_modal(weak, Some(repository), None, on_base, window, cx)
                .with_selection_prompt("Select base branch (step 1 of 2)…", window, cx)
        });
    }

    pub(crate) fn pick_commits(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        base: Option<git::Oid>,
        head: Option<git::Oid>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let weak = workspace.weak_handle();
        let notify = weak.clone();
        let endpoint = head.is_none().then(|| {
            repository
                .read(cx)
                .review_comparison("HEAD".into(), "HEAD".into(), false, cx)
        });
        window
            .spawn(cx, async move |cx| {
                let head = match head {
                    Some(head) => head,
                    None => endpoint.context("missing HEAD request")?.await?.right,
                };
                let commits = repository
                    .read_with(cx, |repository, cx| {
                        repository.review_history(base, head, cx)
                    })
                    .await?;
                ensure!(
                    !commits.is_empty(),
                    "no commits in the selected snapshot scope"
                );
                weak.update_in(cx, |workspace, window, cx| {
                    let weak = workspace.weak_handle();
                    workspace.toggle_modal(window, cx, |window, cx| {
                        ReviewCommitPicker::new(weak, repository, commits, None, window, cx)
                    });
                })?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(notify, window, cx);
    }

    fn pull_request_context(&self) -> Option<PullRequestReviewContext> {
        Some(PullRequestReviewContext {
            identity: self.data.session.selector.pull_request_identity()?.clone(),
            metadata: self.data.pull_request.clone()?,
            selection: match &self.data.session.selector {
                ComparisonSelector::PullRequestScope { scope, .. } => Some(scope.clone()),
                _ => None,
            },
        })
    }

    fn pick_pull_request_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(feature = "review-test-support")]
        if github_viewed::fixture::matches(&self.data.session.selector) {
            return;
        }
        if self.history_loading {
            return;
        }
        let Some(context) = self.pull_request_context() else {
            return;
        };
        self.history_request += 1;
        let request = self.history_request;
        let origin = CommitHistoryOrigin {
            session: self.data.session.id.clone(),
            selector: self.data.session.selector.clone(),
            base_tip: context.metadata.base_ref_oid.clone(),
            head_tip: context.metadata.head_ref_oid.clone(),
        };
        let repository = self.repository.clone();
        self.history_loading = true;
        self.history_error = None;
        self.history_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let base = context.metadata.base_ref_oid.parse()?;
                let head = context.metadata.head_ref_oid.parse()?;
                let commits = repository.read_with(cx, |repository, cx| repository.review_history(Some(base), head, cx)).await?;
                ensure!(!commits.is_empty(), "No commits in this pinned PR revision");
                if let Some(expected) = context.metadata.commit_count { ensure!(commits.len() == expected, "PR history count mismatch: local graph has {} commits; GitHub reported {expected}. Refresh or fetch missing history before selecting.", commits.len()); }
                anyhow::Ok(commits)
            }.await;
            this.update_in(cx, |this, window, cx| this.finish_commit_history(request, origin, context, result, window, cx)).log_err();
        }));
        cx.notify();
    }

    fn finish_commit_history(
        &mut self,
        request: u64,
        origin: CommitHistoryOrigin,
        context: PullRequestReviewContext,
        result: Result<Vec<git::review::ReviewCommit>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if request != self.history_request {
            return;
        }
        self.history_loading = false;
        let matches_origin = self.data.session.id == origin.session
            && self.data.session.selector == origin.selector
            && self.data.pull_request.as_ref().is_some_and(|metadata| {
                metadata.base_ref_oid == origin.base_tip && metadata.head_ref_oid == origin.head_tip
            });
        if !matches_origin {
            cx.notify();
            return;
        }
        match result {
            Ok(commits) => {
                let repository = self.repository.clone();
                let origin_view = cx.entity().entity_id();
                if let Err(error) = self.workspace.update(cx, |workspace, cx| {
                    if workspace
                        .active_item_as::<Self>(cx)
                        .is_none_or(|active| active.entity_id() != origin_view)
                        || workspace.has_active_modal(window, cx)
                    {
                        return;
                    }
                    let weak = workspace.weak_handle();
                    workspace.toggle_modal(window, cx, |window, cx| {
                        ReviewCommitPicker::new(
                            weak,
                            repository,
                            commits,
                            Some(context),
                            window,
                            cx,
                        )
                    });
                }) {
                    self.history_error = Some(error.to_string());
                }
            }
            Err(error) => {
                self.history_error = Some(format!("Unable to load PR commits: {error:#}"))
            }
        }
        cx.notify();
    }

    pub(crate) fn deploy_pull_request_commits(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        context: PullRequestReviewContext,
        selections: Vec<git::review::ReviewCommitSelection>,
        range: bool,
        mark_reviewed: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let weak = workspace.weak_handle();
        let notify = weak.clone();
        let db = ReviewDb::global(cx);
        let project = workspace.project().clone();
        let repository_snapshot = repository.read(cx).snapshot();
        window
            .spawn(cx, async move |cx| {
                let base: git::Oid = context.metadata.base_ref_oid.parse()?;
                let head: git::Oid = context.metadata.head_ref_oid.parse()?;
                let history = repository
                    .read_with(cx, |repository, cx| {
                        repository.review_history(Some(base), head, cx)
                    })
                    .await?;
                validate_pr_selections(&history, &selections)?;
                let comparisons = repository
                    .read_with(cx, |repository, cx| {
                        repository.review_commits(selections.clone(), range, cx)
                    })
                    .await?;
                let snapshot = commit_manifest(comparisons, range)?;
                let selection = selections
                    .into_iter()
                    .map(|selection| CommitSelection {
                        commit: object(selection.commit),
                        parent: selection.parent.map(object),
                    })
                    .collect();
                let scope = if range {
                    PullRequestCommitScope::Range {
                        selections: selection,
                    }
                } else {
                    PullRequestCommitScope::Commits {
                        selections: selection,
                    }
                };
                let data = cx
                    .background_spawn({
                        let db = db.clone();
                        async move {
                            let binding = db
                                .bind_repository(
                                    repository_snapshot.common_dir_abs_path.to_path_buf(),
                                    repository_snapshot.repository_dir_abs_path.to_path_buf(),
                                    object(head).format,
                                )
                                .await?;
                            let session = db
                                .create_pull_request_scope(
                                    binding.id,
                                    context.identity,
                                    context.metadata,
                                    object(base),
                                    object(head),
                                    scope,
                                    snapshot,
                                )
                                .await?;
                            read_data(&db, session)
                        }
                    })
                    .await?;
                weak.update_in(cx, |workspace, window, cx| {
                    let id = data.session.id.clone();
                    Self::show_data(workspace, project, repository, db, data, window, cx);
                    if mark_reviewed {
                        let view = workspace
                            .items_of_type::<Self>(cx)
                            .find(|view| view.read(cx).data.session.id == id);
                        if let Some(view) = view {
                            view.update(cx, |view, cx| {
                                view.mark_selected_commits_reviewed(window, cx)
                            });
                        }
                    }
                })?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(notify, window, cx);
    }

    pub(crate) fn reset_pull_request_scope(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        context: PullRequestReviewContext,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let db = ReviewDb::global(cx);
        let project = workspace.project().clone();
        let weak = workspace.weak_handle();
        let notify = weak.clone();
        let snapshot = repository.read(cx).snapshot();
        window.spawn(cx, async move |cx| {
            let data = cx.background_spawn({
                let db = db.clone();
                async move {
                    let binding = db.bind_repository(snapshot.common_dir_abs_path.to_path_buf(), snapshot.repository_dir_abs_path.to_path_buf(), object(context.metadata.head_ref_oid.parse()?).format).await?;
                    let session = db.find_pull_request_session(&binding.id, &context.identity)?.context("Full PR snapshot is unavailable; reopen Files from Conversation")?;
                    db.apply_pull_request_session(session.id.clone()).await?;
                    read_data(&db, session)
                }
            }).await?;
            weak.update_in(cx, |workspace, window, cx| Self::show_data(workspace, project, repository, db, data, window, cx))?;
            anyhow::Ok(())
        }).detach_and_notify_err(notify, window, cx);
    }

    fn show_data(
        workspace: &mut Workspace,
        project: Entity<Project>,
        repository: Entity<Repository>,
        db: ReviewDb,
        data: ReviewData,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        remember_review_scope(
            workspace.weak_handle().entity_id(),
            repository.entity_id(),
            &data.session,
            cx,
        );
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).data.session.id == data.session.id);
        if let Some(existing) = existing {
            existing.update(cx, |view, cx| view.finish(Ok(data), window, cx));
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let view = cx.new(|cx| {
                Self::new(
                    project,
                    repository,
                    workspace.weak_handle(),
                    db,
                    data,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        }
    }

    pub(crate) fn deploy_commits(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        selections: Vec<git::review::ReviewCommitSelection>,
        range: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let selector_selections = selections
            .iter()
            .map(|selection| CommitSelection {
                commit: object(selection.commit),
                parent: selection.parent.map(object),
            })
            .collect();
        let selector = if range {
            ComparisonSelector::Range {
                selections: selector_selections,
            }
        } else {
            ComparisonSelector::Commits {
                selections: selector_selections,
            }
        };
        let task = repository.read(cx).review_commits(selections, range, cx);
        let snapshot = cx.background_spawn(async move { commit_manifest(task.await?, range) });
        Self::deploy_snapshot(workspace, repository, selector, snapshot, window, cx);
    }

    fn deploy(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        base: String,
        head: String,
        direct: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let task = repository
            .read(cx)
            .review_comparison(base.clone(), head.clone(), !direct, cx);
        let selector = if direct {
            ComparisonSelector::Direct {
                left_ref: base,
                right_ref: head,
            }
        } else {
            ComparisonSelector::Branch {
                base_ref: base,
                head_ref: head,
            }
        };
        let snapshot = cx.background_spawn(async move { manifest(task.await?) });
        Self::deploy_snapshot(workspace, repository, selector, snapshot, window, cx);
    }

    fn deploy_snapshot(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        selector: ComparisonSelector,
        snapshot: Task<Result<ReviewSnapshot>>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let weak = workspace.weak_handle();
        let db = ReviewDb::global(cx);
        let repo_snapshot = repository.read(cx).snapshot();
        let notify = weak.clone();
        window
            .spawn(cx, async move |cx| {
                let snapshot = snapshot.await?;
                let format = snapshot
                    .groups
                    .first()
                    .context("missing comparison group")?
                    .endpoints
                    .right
                    .format;
                let data = cx
                    .background_spawn({
                        let db = db.clone();
                        async move {
                            let binding = db
                                .bind_repository(
                                    repo_snapshot.common_dir_abs_path.to_path_buf(),
                                    repo_snapshot.repository_dir_abs_path.to_path_buf(),
                                    format,
                                )
                                .await?;
                            let session =
                                if let Some(session) = db.find_session(&binding.id, &selector)? {
                                    session
                                } else {
                                    db.create_session(binding.id, selector, snapshot).await?
                                };
                            read_data(&db, session)
                        }
                    })
                    .await?;
                weak.update_in(cx, |workspace, window, cx| {
                    let existing = workspace
                        .items_of_type::<Self>(cx)
                        .find(|item| item.read(cx).data.session.id == data.session.id);
                    if let Some(existing) = existing {
                        workspace.activate_item(&existing, true, true, window, cx);
                    } else {
                        let view = cx.new(|cx| {
                            Self::new(
                                project,
                                repository,
                                workspace.weak_handle(),
                                db,
                                data,
                                window,
                                cx,
                            )
                        });
                        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                    }
                })?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(notify, window, cx);
    }

    fn new(
        project: Entity<Project>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        db: ReviewDb,
        mut data: ReviewData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        choose_visible(&mut data);
        let changes = ReviewChanges::global(cx);
        let mut subscriptions = vec![
            cx.subscribe_in(&repository, window, |this, _, event, window, cx| {
                if matches!(
                    event,
                    RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged
                ) {
                    this.check_for_updates(window, cx);
                }
            }),
            cx.subscribe_in(&changes, window, |this, _, event: &ReviewChanged, _, cx| {
                if event.session == this.data.session.id
                    && event.revision > this.data.session.revision
                {
                    this.observed_revision = this.observed_revision.max(event.revision);
                    this.reload_shared_progress(cx);
                    cx.notify();
                }
            }),
        ];
        let path_input = cx.new(|cx| {
            InputField::new(window, cx, "Search files…")
                .start_icon(IconName::MagnifyingGlass)
                .clear_button(true)
        });
        path_input.update(cx, |input, cx| {
            input.set_text(&data.session.presentation.path_filter, window, cx)
        });
        let view = cx.weak_entity();
        let path_editor = path_input.read(cx).editor().clone();
        subscriptions.push(path_editor.subscribe(
            Box::new(move |event, window, cx| {
                if event != ui_input::ErasedEditorEvent::BufferEdited {
                    return;
                }
                view.update(cx, |this, cx| {
                    let text = this.path_input.read(cx).text(cx).trim().to_owned();
                    if text == this.data.session.presentation.path_filter {
                        return;
                    }
                    this.path_filter_task = Some(cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(200))
                            .await;
                        this.update_in(cx, |this, window, cx| {
                            let mut presentation = this.data.session.presentation.clone();
                            presentation.path_filter = text;
                            this.change_presentation(presentation, window, cx);
                        })
                        .log_err();
                    }));
                })
                .log_err();
            }),
            window,
            cx,
        ));
        let github_viewed_initial_pending = data.session.selector.supports_github_viewed();
        let github_viewed_intents = data.durable_viewed_intents.clone();
        let locally_collapsed = github_viewed_intents
            .iter()
            .filter(|(_, viewed)| **viewed)
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let local_viewed_pin = data
            .session
            .presentation
            .selected_path
            .as_ref()
            .filter(|path| {
                data.session.presentation.unreviewed_only && locally_collapsed.contains(*path)
            })
            .cloned();
        let mut view = Self {
            path_input,
            path_filter_task: None,
            recovery_task: None,
            progress_reload_task: None,
            recovery_running: false,
            history_request: 0,
            history_loading: false,
            history_error: None,
            history_task: None,
            tree_scroll: UniformListScrollHandle::new(),
            tree_width_preview: None,
            tree_resizing: false,
            review_content_width: 0.,
            observed_revision: data.session.revision,
            changes,
            _subscriptions: subscriptions,
            checking: false,
            refreshing: false,
            refresh_task: None,
            full_refresh_pending: false,
            update_available: false,
            check_error: None,
            check_request: 0,
            check_task: None,
            pr_check_guard: None,
            project,
            repository,
            workspace,
            db,
            data,
            editor: None,
            github_panel: None,
            github_panel_error: None,
            github_open: false,
            github_panel_width: 360.,
            github_viewed_intents,
            github_viewed_pending_clicks: BTreeMap::new(),
            github_viewed_accepting_clicks: BTreeMap::new(),
            github_viewed_intent_sequence: 0,
            github_viewed_remaining: None,
            github_viewed_resume_origin: None,
            presentation_task: None,
            navigation_pending: false,
            navigation_sequence: 0,
            file_request: 0,
            cached_unverified: false,
            prepared_files: VecDeque::new(),
            prepared_bytes: 0,
            loading_files: BTreeMap::new(),
            prewarm_task: None,
            preparation_lock: Arc::new(futures::lock::Mutex::new(())),
            copied_path: None,
            copied_path_task: None,
            selected_patch_contents: Vec::new(),
            github_buffers: Vec::new(),
            github_capture: None,
            github_composer_capture: None,
            github_blocks: Vec::new(),
            github_block_keys: Vec::new(),
            github_selection_block: None,
            github_subscriptions: Vec::new(),
            github_editor_subscription: None,
            github_layout_subscription: None,
            github_reveal: None,
            github_conversation_anchor: None,
            github_viewed_busy: false,
            github_viewed_work_sequence: 0,
            github_viewed_preparing: false,
            github_viewed_progress_task: None,
            github_viewed_guard: None,
            github_viewed_status: None,
            github_viewed_error: None,
            github_viewed_initial_pending,
            github_viewed_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            github_viewed_retention_pause: None,
            #[cfg(test)]
            github_viewed_sync_override: None,
            retained: None,
            multi_loaded: false,
            loaded_window_scope: None,
            rolling_scroll: false,
            rolling_file_load: false,
            rolling_file_task: None,
            collapsed_review_files: locally_collapsed,
            local_viewed_pin,
            layout_note: None,
            busy: false,
            error: None,
            focus: cx.focus_handle(),
            task: None,
        };
        view.ensure_github_panel(window, cx);
        cx.defer_in(window, |this, window, cx| {
            this.reveal_selected_tree_row();
            this.load_selected(window, cx);
            this.check_for_updates(window, cx);
            #[cfg(not(test))]
            this.drain_github_viewed_sync(window, cx);
        });
        view
    }

    /// Only a local intent accepted while actively reviewing Unreviewed files
    /// creates a focus pin. Ordinary server Viewed state and collapsed buffers
    /// never create one. Its origin survives acknowledgement until navigation.
    fn pin_locally_viewed_selection(&mut self) {
        if !self.data.session.selector.supports_github_viewed()
            || !self.data.session.presentation.unreviewed_only
        {
            return;
        }
        if let Some(path) = self.data.session.presentation.selected_path.as_ref() {
            match self.github_viewed_intents.get(path) {
                Some(true) => self.local_viewed_pin = Some(path.clone()),
                Some(false) => self.local_viewed_pin = None,
                None => {}
            }
        }
    }

    fn effective_data(&self) -> ReviewData {
        let mut data = self.data.clone();
        // A saved local intent has already retained exact bytes. It can update
        // local-history approval while pending/accepting clicks only update UI.
        for (path, viewed) in &self.github_viewed_intents {
            if *viewed {
                data.checkpoint_viewed.insert(path.clone());
            } else {
                data.checkpoint_viewed.remove(path);
            }
        }
        for (path, viewed) in self
            .github_viewed_intents
            .iter()
            .chain(&self.github_viewed_accepting_clicks)
            .chain(&self.github_viewed_pending_clicks)
        {
            if *viewed {
                data.viewed.insert(path.clone());
            } else {
                data.viewed.remove(path);
            }
            if let Some(remote) = &mut data.github_viewed {
                if *viewed {
                    remote.insert(path.clone());
                } else {
                    remote.remove(path);
                }
            }
        }
        data.pinned_path = self
            .local_viewed_pin
            .as_ref()
            .filter(|path| {
                data.session.selector.supports_github_viewed()
                    && data.session.presentation.selected_path.as_ref() == Some(*path)
                    && self.effective_github_viewed(path)
            })
            .cloned();
        data
    }

    fn reload_shared_progress(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let id = self.data.session.id.clone();
        self.progress_reload_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(20))
                .await;
            let result = cx
                .background_spawn(async move { read_data(&db, db.load_session(&id)?) })
                .await;
            view.update(cx, |view, cx| {
                if let Ok(mut data) = result {
                    // New snapshots remain pinned until an explicit Refresh.
                    // Progress-only changes can be adopted without rebuilding buffers.
                    if data.session.selector == view.data.session.selector
                        && data.session.snapshot.generation == view.data.session.snapshot.generation
                        && data.session.revision >= view.data.session.revision
                    {
                        data.session.presentation = view.data.session.presentation.clone();
                        view.observed_revision = view.observed_revision.max(data.session.revision);
                        view.github_viewed_intents = data.durable_viewed_intents.clone();
                        view.data = data;
                        cx.notify();
                    }
                }
            })
            .log_err();
        }));
    }

    fn outdated(&self) -> bool {
        self.observed_revision > self.data.session.revision
    }

    fn check_for_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(feature = "review-test-support")]
        if github_viewed::fixture::matches(&self.data.session.selector) {
            github_viewed::fixture::verify_restored(self, window, cx);
            return;
        }
        if matches!(
            self.data.session.selector,
            ComparisonSelector::PullRequestScope { .. }
        ) {
            if self.cached_unverified {
                self.refresh(window, cx);
            }
            return;
        }
        if let Some(metadata) = self.data.pull_request.clone() {
            let reference = PullRequestReference::parse(&metadata.url(), None);
            let expected_identity = self.data.session.selector.pull_request_identity().cloned();
            self.checking = true;
            self.check_request += 1;
            let request = self.check_request;
            let generation = self.data.session.snapshot.generation;
            let (client, guard) = GitHubClient::cancellable();
            self.pr_check_guard = Some(guard);
            self.check_task = Some(cx.spawn_in(window, async move |this, cx| {
                let result = cx
                    .background_spawn(async move {
                        let details = client.pull_request(&reference?)?;
                        if let Some(identity) = expected_identity {
                            let viewer = client.viewer()?;
                            ensure!(viewer.node_id.as_deref() == Some(identity.viewer_node_id.as_str()), "GitHub account changed; reconnect before reviewing");
                            ensure!(details.summary.id == identity.pull_request_node_id && details.repository.id == identity.repository_node_id, "GitHub PR identity changed");
                        }
                        Ok::<_, anyhow::Error>(details)
                    })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    if this.check_request != request
                        || this.data.session.snapshot.generation != generation
                    {
                        return;
                    }
                    this.checking = false;
                    match result {
                        Ok(details) => match details.base_tip_oid() {
                            Ok(base_tip) => {
                                this.update_available = base_tip != metadata.base_ref_oid
                                    || details.head_ref_oid != metadata.head_ref_oid
                                    || details.summary.base_ref_name != metadata.base_ref_name;
                                if this.cached_unverified && !this.update_available {
                                    this.cached_unverified = false;
                                    this.ensure_github_panel(window, cx);
                                    this.capture_github_selection(cx);
                                    this.rebuild_github_blocks(window, cx);
                                    #[cfg(not(test))]
                                    this.drain_github_viewed_sync(window, cx);
                                }
                                this.check_error = if this.cached_unverified {
                                    Some("Cached PR revision differs from GitHub. Refresh to load current changes; saved drafts remain local.".into())
                                } else { None };
                            }
                            Err(error) => {
                                this.check_error =
                                    Some(format!("Unable to check PR base branch: {error}"))
                            }
                        },
                        Err(error) => {
                            this.cached_unverified = true;
                            this.check_error = Some(format!("Cached review is read-only: unable to verify account and PR updates: {error}"))
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }));
            cx.notify();
            return;
        }
        if matches!(
            self.data.session.selector,
            ComparisonSelector::Commits { .. } | ComparisonSelector::Range { .. }
        ) {
            return;
        }
        let (base, head, merge_base) = match selector_refs(&self.data.session.selector) {
            Ok(refs) => refs,
            Err(error) => {
                self.check_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.check_request = self.check_request.saturating_add(1);
        let request = self.check_request;
        let snapshot = self.data.session.snapshot.clone();
        self.checking = true;
        self.check_error = None;
        let task = self
            .repository
            .read(cx)
            .review_comparison(base, head, merge_base, cx);
        self.check_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = task.await.and_then(manifest);
            this.update(cx, |this, cx| {
                if this.check_request != request
                    || this.data.session.snapshot.generation != snapshot.generation
                {
                    return;
                }
                this.checking = false;
                match result {
                    Ok(candidate) => {
                        this.update_available = candidate.groups != snapshot.groups;
                        this.check_error = None;
                    }
                    Err(error) => {
                        this.update_available = false;
                        this.check_error =
                            Some(format!("Unable to check review updates: {error:#}"));
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn drain_requested_full_refresh(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.full_refresh_pending
            || self.github_viewed_preparing
            || !self.github_viewed_pending_clicks.is_empty()
            || self.github_viewed_busy
            || self.refreshing
        {
            return false;
        }
        self.full_refresh_pending = false;
        self.refresh(window, cx);
        true
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.github_viewed_preparing || !self.github_viewed_pending_clicks.is_empty() {
            self.full_refresh_pending |= self.data.session.selector.supports_github_viewed();
            self.github_viewed_status =
                Some("Finish saving local Viewed changes before refreshing the PR".into());
            cx.notify();
            return;
        }
        #[cfg(feature = "review-test-support")]
        if github_viewed::fixture::matches(&self.data.session.selector) {
            self.sync_github_viewed(window, cx);
            return;
        }
        if let ComparisonSelector::PullRequestScope {
            identity,
            scope,
            head_tip,
            base_tip,
        } = &self.data.session.selector
        {
            if self.refreshing {
                return;
            }
            let identity = identity.clone();
            let scope = scope.clone();
            let pinned_head = head_tip.hex.clone();
            let pinned_base = base_tip.hex.clone();
            let metadata = self.data.pull_request.clone();
            self.refreshing = true;
            self.check_error = None;
            self.refresh_task = Some(cx.spawn_in(window, async move |this, cx| {
                let result = cx.background_spawn(async move {
                    let reference = PullRequestReference::parse(&metadata.context("PR metadata missing")?.url(), None)?;
                    let client = GitHubClient::default();
                    let viewer = client.viewer()?;
                    ensure!(viewer.node_id.as_deref() == Some(identity.viewer_node_id.as_str()), "GitHub account changed; reconnect before continuing");
                    let details = client.pull_request(&reference)?;
                    ensure!(details.summary.id == identity.pull_request_node_id && details.repository.id == identity.repository_node_id, "PR identity changed");
                    let commits = client.pull_request_commits(&reference)?;
                    let members = commits.iter().map(|commit| commit.oid.as_str()).collect::<BTreeSet<_>>();
                    let missing = scope.selections().iter().filter(|selection| !members.contains(selection.commit.hex.as_str())).count();
                    let changed = details.head_ref_oid != pinned_head || details.base_tip_oid()? != pinned_base;
                    Ok::<_, anyhow::Error>((missing, changed))
                }).await;
                this.update_in(cx, |this, window, cx| {
                    this.refreshing = false;
                    match result {
                        Ok((missing, changed)) => {
                            this.cached_unverified = missing > 0 || changed;
                            this.check_error = if missing > 0 { Some(format!("{missing} selected commits no longer belong to this PR. The pinned snapshot is preserved read-only; choose All commits to use the current PR.")) }
                                else if changed { Some("PR has new revisions. Selected immutable patches remain pinned and read-only; All commits opens the latest PR.".into()) } else { None };
                            if !this.cached_unverified { this.ensure_github_panel(window, cx); }
                            if let Some(panel) = &this.github_panel { panel.update(cx, |panel, cx| panel.reload(window, cx)); }
                        }
                        Err(error) => this.check_error = Some(format!("Could not refresh selected commits: {error:#}")),
                    }
                    cx.notify();
                }).log_err();
            }));
            cx.notify();
            return;
        }
        if self.data.pull_request.is_some() {
            if self.github_viewed_busy || self.refreshing {
                self.full_refresh_pending = true;
                cx.notify();
                return;
            }
            let metadata = self.data.pull_request.clone();
            let identity = match &self.data.session.selector {
                ComparisonSelector::PullRequest { identity } => identity.clone(),
                _ => return,
            };
            let weak = self.workspace.clone();
            self.check_request = self.check_request.saturating_add(1);
            self.check_task = None;
            self.pr_check_guard = None;
            self.checking = false;
            self.refreshing = true;
            self.check_error = None;
            self.github_viewed_error = None;
            self.github_viewed_status = Some("Refreshing pull request…".into());
            self.refresh_task = Some(cx.spawn_in(window, async move |this, cx| {
                let result = async {
                    let reference = PullRequestReference::parse(
                        &metadata.context("PR metadata missing")?.url(),
                        None,
                    )?;
                    let (details, viewer) = cx
                        .background_spawn(async move {
                            let client = GitHubClient::default();
                            Ok::<_, anyhow::Error>((
                                client.pull_request(&reference)?,
                                client.viewer()?,
                            ))
                        })
                        .await?;
                    ensure!(
                        viewer.node_id.as_deref() == Some(identity.viewer_node_id.as_str()),
                        "GitHub account changed; reconnect and reopen PR"
                    );
                    weak.update_in(cx, |workspace, window, cx| {
                        Self::open_pull_request_impl(workspace, details, viewer, true, window, cx)
                    })?
                    .await
                }
                .await;
                this.update_in(cx, |this, window, cx| {
                    this.refreshing = false;
                    match result {
                        Ok(()) => {
                            this.check_error = None;
                            if let Some(panel) = &this.github_panel {
                                panel.update(cx, |panel, cx| panel.reload(window, cx));
                            }
                            if !this.drain_requested_full_refresh(window, cx)
                                && this.data.session.selector.supports_github_viewed()
                            {
                                this.sync_github_viewed(window, cx);
                            }
                        }
                        Err(error) => {
                            this.check_error = Some(format!("Could not refresh: {error:#}"));
                            this.drain_requested_full_refresh(window, cx);
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }));
            cx.notify();
            return;
        }
        if self.busy
            || self.outdated()
            || matches!(
                self.data.session.selector,
                ComparisonSelector::Commits { .. } | ComparisonSelector::Range { .. }
            )
        {
            return;
        }
        let (base, head, merge_base) = match selector_refs(&self.data.session.selector) {
            Ok(refs) => refs,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.busy = true;
        self.check_request = self.check_request.saturating_add(1);
        self.check_task = None;
        self.checking = false;
        let previous = self.data.session.clone();
        let db = self.db.clone();
        let task = self
            .repository
            .read(cx)
            .review_comparison(base, head, merge_base, cx);
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let mut snapshot = manifest(task.await?)?;
                snapshot.generation = previous
                    .snapshot
                    .generation
                    .checked_add(1)
                    .context("review generation exhausted")?;
                cx.background_spawn(async move {
                    if snapshot.groups != previous.snapshot.groups {
                        db.activate_snapshot(previous.id.clone(), previous.revision, snapshot)
                            .await?;
                    }
                    read_data(&db, db.load_session(&previous.id)?)
                })
                .await
            }
            .await;
            this.update_in(cx, |this, window, cx| {
                this.finish(result, window, cx);
                this.check_for_updates(window, cx);
            })
            .log_err();
        }));
        cx.notify();
    }

    fn load_selected_patches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.data.session.presentation.selected_path.clone() else {
            self.busy = false;
            return;
        };
        let patches = self
            .data
            .session
            .snapshot
            .groups
            .iter()
            .filter_map(|group| {
                group
                    .files
                    .iter()
                    .find(|file| file.file_key().ok() == Some(path.as_str()))
                    .map(|file| {
                        let fingerprint = format!(
                            "{}:{}:{}:{}",
                            self.data.session.id,
                            group.endpoints.group,
                            path,
                            file.fingerprint().unwrap_or_default()
                        );
                        let cached = self
                            .prepared_files
                            .iter()
                            .find(|cached| cached.fingerprint == fingerprint)
                            .cloned();
                        (
                            group.endpoints.group.clone(),
                            group.endpoints.right.hex.clone(),
                            file.clone(),
                            fingerprint,
                            cached,
                        )
                    })
            })
            .collect::<Vec<_>>();
        self.file_request = self.file_request.saturating_add(1);
        let request = self.file_request;
        let repository = self.repository.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let split = self.data.session.presentation.split_view;
        self.busy = true;
        self.error = None;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let registry = project.read_with(cx, |project, _| project.languages().clone());
                let objects = patches.iter().filter(|(_, _, _, _, cached)| cached.is_none()).flat_map(|(_, _, file, _, _)| file.old.iter().chain(&file.new))
                    .filter(|side| side.mode != 0o160000).map(|side| side.object.hex.parse()).collect::<Result<Vec<_>, _>>()?;
                let sizes = repository.read_with(cx, |repository, cx| repository.review_blob_sizes(objects, cx)).await?;
                let size = sizes.into_iter().try_fold(0u64, |sum, value| sum.checked_add(value).context("Selected patch size overflow"))?;
                ensure!(size <= 64 * 1024 * 1024, "Selected patches for this file exceed the 64 MiB preview budget. Narrow the commit selection to inspect these immutable patches.");
                let mut prepared = Vec::new();
                for (group, commit, file, fingerprint, cached) in patches {
                    if let Some(cached) = cached {
                        prepared.push((group, file, cached.retained.as_ref().clone(), cached.buffer, cached.diff));
                        continue;
                    }
                    let mut contents = Vec::new();
                    for side in [&file.old, &file.new] {
                        contents.push(if let Some(side) = side.as_ref().filter(|side| side.mode != 0o160000) {
                            Some(repository.read_with(cx, |repository, cx| repository.review_blob(side.object.hex.parse().expect("validated object"), cx)).await?)
                        } else { None });
                    }
                    let mut contents = contents.into_iter();
                    let retained = RetainedContents { old: contents.next().flatten(), new: contents.next().flatten() };
                    file.validate_retained(&retained, REVIEW_PREVIEW_BUDGET as usize)?;
                    let binary = file.old.iter().chain(&file.new).any(|side| side.mode == 0o160000)
                        || retained.old.iter().chain(&retained.new).any(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err());
                    let (old, new) = if binary { (Some("Binary or submodule change".into()), "Binary or submodule change".into()) }
                        else { (retained.old.clone().map(String::from_utf8).transpose()?, retained.new.clone().map(String::from_utf8).transpose()?.unwrap_or_default()) };
                    if !binary {
                        cx.background_spawn({ let old = old.clone(); let new = new.clone(); async move {
                            if let Some(old) = old { crate::commit_view::validate_review_text(&old)?; }
                            crate::commit_view::validate_review_text(&new)
                        }}).await?;
                    }
                    let blob = cx.update(|_, cx| {
                        let repo_path = RepoPath::from_rel_path(RelPath::from_unix_str(&path)?);
                        let worktree_id = worktree_id_for_repo_path(repository.read(cx), project.read(cx), &repo_path, cx).context("Repository worktree unavailable")?;
                        Ok::<_, anyhow::Error>(Arc::new(GitBlob { path: repo_path, worktree_id, is_deleted: file.new.is_none(), is_binary: binary,
                            display_name: format!("{} · {}", &commit[..commit.len().min(8)], path) }) as Arc<dyn File>)
                    })??;
                    let buffer = build_buffer(new, blob, &registry, cx).await?;
                    let diff = build_buffer_diff(old, &buffer, &registry, cx).await?;
                    let cost = retained.old.as_ref().map_or(0, Vec::len).saturating_add(retained.new.as_ref().map_or(0, Vec::len)).saturating_mul(4).max(16 * 1024);
                    let cached = PreparedReviewFile { fingerprint, buffer: buffer.clone(), diff: diff.clone(), retained: Arc::new(retained.clone()), cost, binary };
                    this.update(cx, |this, _| {
                        const BUDGET: usize = 64 * 1024 * 1024;
                        if cost <= BUDGET {
                            while this.prepared_bytes.saturating_add(cost) > BUDGET || this.prepared_files.len() >= 128 {
                                let Some(evicted) = this.prepared_files.pop_front() else { break; };
                                this.prepared_bytes = this.prepared_bytes.saturating_sub(evicted.cost);
                            }
                            this.prepared_bytes += cost;
                            this.prepared_files.push_back(cached);
                        }
                    })?;
                    prepared.push((group, file, retained, buffer, diff));
                }
                this.update_in(cx, |this, window, cx| {
                    if this.file_request != request { return Ok::<(), anyhow::Error>(()); }
                    let workspace = workspace.upgrade().context("Workspace closed")?;
                    let buffer = cx.new(|cx| { let mut buffer = MultiBuffer::new(Capability::ReadOnly); buffer.set_all_diff_hunks_expanded(cx); buffer });
                    let style = split.map(|split| if split { DiffViewStyle::Split } else { DiffViewStyle::Unified })
                        .unwrap_or(editor::EditorSettings::get_global(cx).diff_view_style);
                    let editor = cx.new(|cx| SplittableEditor::new(style, buffer, project, workspace, window, cx));
                    let review = cx.weak_entity();
                    editor.update(cx, |editor, cx| {
                        editor.set_follow_global_diff_view_style(false);
                        editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                        editor.rhs_editor().update(cx, |editor, _| editor.register_addon(ReviewFileHeaderAddon { review }));
                    });
                    let mut retained = Vec::new();
                    for (index, (group, file, contents, buffer, diff)) in prepared.into_iter().enumerate() {
                        let snapshot = buffer.read(cx).snapshot();
                        let mut ranges = diff.read(cx).snapshot(cx).hunks(&snapshot).map(|hunk| hunk.buffer_range.to_point(&snapshot)).collect::<Vec<_>>();
                        if ranges.is_empty() { ranges.push(language::Point::zero()..snapshot.max_point()); }
                        let key = PathKey::with_sort_prefix(index as u64 + 1, snapshot.file().context("Historical file missing")?.path().clone());
                        editor.update(cx, |editor, cx| editor.update_excerpts_for_path(key, buffer, ranges, multibuffer_context_lines(cx), diff, cx));
                        retained.push((group, file, contents));
                    }
                    if style == DiffViewStyle::Split { editor.update(cx, |editor, cx| editor.split(window, cx)); }
                    this.github_blocks.clear(); this.github_block_keys.clear(); this.clear_selection_affordance(cx);
                    this.github_buffers.clear(); this.github_capture = None; this.github_composer_capture = None;
                    this.github_editor_subscription = None; this.github_layout_subscription = None;
                    this.editor = Some(editor); this.install_review_context_menus(cx); this.selected_patch_contents = retained;
                    this.retained = None; this.multi_loaded = false; this.busy = false;
                    this.layout_note = Some(format!("{} selected commit patches · immutable first-parent diffs", this.selected_patch_contents.len()));
                    cx.notify(); Ok(())
                })??;
                Ok(())
            }.await;
            if let Err(error) = result { this.update(cx, |this, cx| { if this.file_request == request { this.busy = false; this.error = Some(format!("Unable to load selected patches: {error:#}")); cx.notify(); } }).log_err(); }
        }));
        cx.notify();
    }

    fn load_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prewarm_task = None;
        let desired_key = self
            .data
            .session
            .presentation
            .selected_path
            .as_deref()
            .and_then(|path| {
                self.data
                    .session
                    .snapshot
                    .file(active_group(&self.data), path)
                    .ok()
                    .and_then(|file| file.fingerprint().ok())
                    .map(|fingerprint| format!("{}:{}:{}", self.data.session.id, path, fingerprint))
            });
        self.loading_files
            .retain(|key, _| Some(key) == desired_key.as_ref());
        let scroll_driven = std::mem::take(&mut self.rolling_scroll);
        self.rolling_file_task = None;
        self.rolling_file_load = false;
        if matches!(
            self.data.session.selector,
            ComparisonSelector::PullRequestScope { .. }
        ) && self.data.session.snapshot.groups.len() > 1
        {
            self.load_selected_patches(window, cx);
            return;
        }
        if self.data.session.presentation.since_review {
            self.load_since_review_selected(window, cx);
            return;
        }
        self.rolling_file_load = scroll_driven;
        if let Some(panel) = &self.github_panel {
            panel.update(cx, |panel, cx| panel.set_incremental_mode(false, cx));
        }
        let started = Instant::now();
        // The previous editor and its layout remain visible until replacement.
        // Do not temporarily advertise Single file while a scrolling view prepares.
        if !scroll_driven && self.focus.contains_focused(window, cx) {
            self.focus.focus(window, cx);
        }
        self.file_request = self.file_request.saturating_add(1);
        let request = self.file_request;
        self.ensure_github_panel(window, cx);
        self.error = None;
        let Some(path) = self.data.session.presentation.selected_path.clone() else {
            self.editor = None;
            self.retained = None;
            self.github_buffers.clear();
            self.github_blocks.clear();
            self.github_block_keys.clear();
            self.clear_selection_affordance(cx);
            self.busy = false;
            self.rolling_file_load = false;
            cx.notify();
            return;
        };
        let comparison = match self
            .data
            .session
            .snapshot
            .file(active_group(&self.data), &path)
        {
            Ok(file) => file.clone(),
            Err(error) => {
                self.error = Some(error.to_string());
                self.busy = false;
                self.rolling_file_load = false;
                cx.notify();
                return;
            }
        };
        self.busy = !scroll_driven;
        let fingerprint = format!(
            "{}:{}:{}",
            self.data.session.id,
            path,
            comparison.fingerprint().unwrap_or_default()
        );
        let cached_prepared = self
            .prepared_files
            .iter()
            .position(|cached| cached.fingerprint == fingerprint)
            .and_then(|index| self.prepared_files.remove(index));
        if let Some(cached) = &cached_prepared {
            self.prepared_bytes = self.prepared_bytes.saturating_sub(cached.cost);
            self.prepared_files.push_back(cached.clone());
            self.prepared_bytes += cached.cost;
        }
        let group = active_group(&self.data).to_owned();
        let session = self.data.session.clone();
        let repository = self.repository.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let preparation = self.prepare_file(comparison.clone(), group.clone(), window, cx);
        let prepared_cache = self.prepared_files.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let mut cache_hits = usize::from(cached_prepared.is_some());
                let mut cache_misses = usize::from(cached_prepared.is_none());
                let cached = preparation.await.map_err(|error| anyhow::anyhow!("{error:#}"))?;
                let retained = cached.retained.as_ref().clone();
                let binary = cached.binary;
                let buffer = cached.buffer.clone();
                let diff = cached.diff.clone();
                let mut prepared = vec![(buffer, diff, comparison.clone(), binary)];
                let group_files = session
                    .snapshot
                    .groups
                    .iter()
                    .find(|entry| entry.endpoints.group == group)
                    .context("Comparison group unavailable")?
                    .files
                    .clone();
                let want_multi = session.presentation.prefer_scrolling(group_files.len())
                    && session.selector.supports_github_viewed();
                let total_group_files = group_files.len();
                let center = group_files.iter().position(|file| file.file_key().ok() == Some(path.as_str())).unwrap_or(0);
                let window_range = review_file_window(total_group_files, center);
                let mut group_files = if want_multi { group_files[window_range].to_vec() } else { vec![comparison.clone()] };
                let mut layout_note = None;
                if want_multi {
                    // Probe sizes before loading neighboring contents. Large neighbors
                    // remain available through explicit tree selection, without forcing
                    // every other file into single-file mode.
                    let mut window_bytes = retained.old.iter().chain(&retained.new).map(|bytes| bytes.len() as u64).sum::<u64>();
                    let mut bounded_files = Vec::new();
                    for file in group_files {
                        if file.file_key()? == path { bounded_files.push(file); continue; }
                        let fingerprint = format!("{}:{}:{}", session.id, file.file_key()?, file.fingerprint()?);
                        if let Some(cached) = prepared_cache.iter().find(|cached| cached.fingerprint == fingerprint) {
                            let bytes = cached.retained.old.iter().chain(&cached.retained.new).map(|bytes| bytes.len() as u64).sum::<u64>();
                            if bytes <= 1024 * 1024 && window_bytes.saturating_add(bytes) <= 16 * 1024 * 1024 {
                                window_bytes += bytes; bounded_files.push(file);
                            } else { layout_note = Some("Large neighboring files load when selected in the file tree".into()); }
                            continue;
                        }
                        let objects = file.old.iter().chain(&file.new).filter(|side| side.mode != 0o160000)
                            .map(|side| side.object.hex.parse::<git::Oid>()).collect::<Result<Vec<_>, _>>()?;
                        let expected = objects.len();
                        let sizes = if objects.is_empty() { Vec::new() } else { repository.read_with(cx, |repository, cx| repository.review_blob_sizes(objects, cx)).await? };
                        ensure!(sizes.len() == expected, "Historical file size response is incomplete");
                        let bytes = sizes.into_iter().try_fold(0u64, |sum, size| sum.checked_add(size).context("File size overflow"))?;
                        if bytes <= 1024 * 1024 && window_bytes.saturating_add(bytes) <= 16 * 1024 * 1024 {
                            window_bytes += bytes; bounded_files.push(file);
                        } else {
                            layout_note = Some("Large neighboring files load when selected in the file tree".into());
                        }
                    }
                    group_files = bounded_files;
                    let mut known_bytes = retained.old.iter().chain(&retained.new)
                        .map(|bytes| bytes.len() as u64).sum::<u64>();
                    let mut objects = Vec::new();
                    for file in &group_files {
                        let other_path = file.file_key()?;
                        if other_path == path { continue; }
                        let fingerprint = format!("{}:{}:{}", session.id, other_path, file.fingerprint()?);
                        if let Some(cached) = prepared_cache.iter().find(|entry| entry.fingerprint == fingerprint) {
                            known_bytes = cached.retained.old.iter().chain(&cached.retained.new)
                                .try_fold(known_bytes, |sum, bytes| sum.checked_add(bytes.len() as u64).context("File size budget overflow"))?;
                        } else {
                            objects.extend(file.old.iter().chain(&file.new)
                                .filter(|side| side.mode != 0o160000)
                                .map(|side| side.object.hex.parse::<git::Oid>()).collect::<Result<Vec<_>, _>>()?);
                        }
                    }
                    let expected_sizes = objects.len();
                    let sizes = if objects.is_empty() { Vec::new() } else {
                        repository.read_with(cx, |repository, cx| repository.review_blob_sizes(objects, cx)).await?
                    };
                    ensure!(sizes.len() == expected_sizes, "Historical file size response is incomplete");
                    let byte_count = sizes.into_iter().try_fold(known_bytes, |sum, size| {
                        sum.checked_add(size).context("File size budget overflow")
                    })?;
                    if byte_count <= 16 * 1024 * 1024 {
                        for comparison in &group_files {
                            let other_path = comparison.file_key()?.to_string();
                            if other_path == path {
                                continue;
                            }
                            let fingerprint = format!("{}:{}:{}", session.id, other_path, comparison.fingerprint()?);
                            if let Some(cached) = prepared_cache.iter().find(|entry| entry.fingerprint == fingerprint) {
                                let binary = comparison.old.iter().chain(&comparison.new).any(|side| side.mode == 0o160000)
                                    || cached.retained.old.iter().chain(&cached.retained.new)
                                        .any(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err());
                                prepared.push((cached.buffer.clone(), cached.diff.clone(), comparison.clone(), binary));
                                cache_hits += 1;
                                continue;
                            }
                            cache_misses += 1;
                            let preparation = this.update_in(cx, |view, window, cx| view.prepare_file(comparison.clone(), group.clone(), window, cx))?;
                            match preparation.await {
                                Ok(cached) => prepared.push((cached.buffer.clone(), cached.diff.clone(), comparison.clone(), cached.binary)),
                                Err(error) => { layout_note = Some(format!("A neighboring file loads on selection: {error:#}")); }
                            }

                        }
                    } else {
                        layout_note =
                            Some("Single-file view: combined contents exceed the bounded preview budget".to_string());
                    }
                }
                let multi_loaded = want_multi && prepared.len() > 1;
                let rendered_paths = prepared.iter().filter_map(|(_, _, comparison, _)| comparison.file_key().ok().map(str::to_owned)).collect::<BTreeSet<_>>();
                this.update_in(cx, |this, window, cx| {
                    if this.file_request != request {
                        return Ok(());
                    }
                    // Keep the previous native editor alive throughout preparation.
                    // Only replace its selection subscriptions and inline ownership
                    // when all immutable buffers/diffs are ready to install.
                    for (owner, id) in this.github_blocks.drain(..) {
                        owner.update(cx, |editor, cx| editor.remove_blocks([id].into_iter().collect(), None, cx));
                    }
                    this.github_block_keys.clear();
                    this.clear_selection_affordance(cx);
                    this.github_buffers.clear();
                    this.github_capture = None;
                    this.github_composer_capture = None;
                    this.github_editor_subscription = None;
                    this.github_layout_subscription = None;
                    let Some(workspace) = workspace.upgrade() else {
                        anyhow::bail!("workspace closed");
                    };
                    let multibuffer = cx.new(|cx| {
                        let mut buffer = MultiBuffer::new(Capability::ReadOnly);
                        buffer.set_all_diff_hunks_expanded(cx);
                        buffer
                    });
                    let style = session
                        .presentation
                        .split_view
                        .map(|split| {
                            if split {
                                DiffViewStyle::Split
                            } else {
                                DiffViewStyle::Unified
                            }
                        })
                        .unwrap_or(editor::EditorSettings::get_global(cx).diff_view_style);
                    let scope = (session.snapshot.generation, group.clone(), style == DiffViewStyle::Split);
                    let reused_editor = this.retained.is_some() && this.editor.is_some() && this.loaded_window_scope.as_ref() == Some(&scope);
                    let editor = if reused_editor {
                        this.editor.clone().context("Scrolling editor disappeared")?
                    } else {
                        cx.new(|cx| SplittableEditor::new(style, multibuffer, project, workspace, window, cx))
                    };
                    let existing_prepared = {
                        let rhs = editor.read(cx).rhs_editor();
                        let multibuffer = rhs.read(cx).buffer().read(cx);
                        multibuffer.all_buffers_iter().filter_map(|buffer| {
                            let path = buffer.read(cx).file()?.path().to_string();
                            let diff = multibuffer.diff_for(buffer.read(cx).remote_id())?;
                            Some((path, (buffer, diff)))
                        }).collect::<BTreeMap<_, _>>()
                    };
                    let existing_paths = existing_prepared.keys().cloned().collect::<BTreeSet<_>>();
                    if reused_editor {
                        for old_path in existing_paths.difference(&rendered_paths) {
                            let repo_path = RepoPath::from_rel_path(RelPath::from_unix_str(old_path)?);
                            editor.update(cx, |editor, cx| editor.remove_excerpts_for_path(PathKey::with_sort_prefix(1, repo_path.as_ref().clone()), cx));
                        }
                    }
                    let review_header = cx.weak_entity();
                    editor.update(cx, |editor, cx| {
                        editor.rhs_editor().update(cx, |editor, _| editor.register_addon(ReviewFileHeaderAddon { review: review_header.clone() }));
                        editor.set_follow_global_diff_view_style(false);
                        editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                        editor.rhs_editor().update(cx, |editor, cx| {
                            editor.set_show_bookmarks(false, cx);
                            editor.set_show_breakpoints(false, cx);
                            editor.set_show_diff_review_button(false, cx);
                        });
                    });
                    let mut selected_anchor = None;
                    for (index, (mut buffer, mut diff, comparison, binary)) in
                        prepared.into_iter().enumerate()
                    {
                        // An active native excerpt can outlive the LRU cache.
                        // Rebuild canonical comment anchors from its actual buffer,
                        // never from a newly prepared same-path replacement.
                        if reused_editor {
                            if let Some((existing_buffer, existing_diff)) = existing_prepared.get(comparison.file_key()?) {
                                buffer = existing_buffer.clone();
                                diff = existing_diff.clone();
                            }
                        }
                        let snapshot = buffer.read(cx).snapshot();
                        let diff_snapshot = diff.read(cx).snapshot(cx);
                        if session.selector.supports_github_viewed() && !binary {
                            match review_comment_anchor::canonical_comment_buffers(
                                &comparison,
                                &snapshot,
                                &diff_snapshot,
                                binary,
                            ) {
                                Ok(buffers) => this.github_buffers.extend(buffers),
                                Err(error) => log::debug!(
                                    "File is not eligible for GitHub inline comments: {error}"
                                ),
                            }
                        }
                        let mut ranges = diff_snapshot
                            .hunks(&snapshot)
                            .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                            .collect::<Vec<_>>();
                        if ranges.is_empty() {
                            ranges.push(language::Point::zero()..snapshot.max_point());
                        }
                        if index == 0 {
                            selected_anchor = ranges
                                .first()
                                .map(|range| snapshot.anchor_before(range.start));
                        }
                        let key = PathKey::with_sort_prefix(
                            1,
                            snapshot
                                .file()
                                .context("Historical file missing")?
                                .path()
                                .clone(),
                        );
                        if !reused_editor || !existing_paths.contains(comparison.file_key()?) {
                        editor.update(cx, |editor, cx| {
                            editor.update_excerpts_for_path(
                                key,
                                buffer,
                                ranges,
                                multibuffer_context_lines(cx),
                                diff,
                                cx,
                            );
                        });
                        }
                    }
                    editor.update(cx, |editor, cx| {
                        if style == DiffViewStyle::Split && !reused_editor {
                            editor.split(window, cx);
                        }
                        if reused_editor {
                            // Rolling eviction can remove the excerpt holding either
                            // cursor. Re-anchor only after the complete new window is
                            // installed, before folding or selection observers run.
                            // This preserves resolved positions without autoscroll.
                            editor.rhs_editor().update(cx, |editor, cx| {
                                editor.refresh_selection_anchors(cx);
                            });
                            if let Some(lhs) = editor.lhs_editor() {
                                lhs.update(cx, |editor, cx| {
                                    editor.refresh_selection_anchors(cx);
                                });
                            }
                        }
                        editor.rhs_editor().update(cx, |editor, cx| {
                            if let Some(anchor) = selected_anchor.filter(|_| !scroll_driven).and_then(|anchor| {
                                editor
                                    .buffer()
                                    .read(cx)
                                    .snapshot(cx)
                                    .anchor_in_excerpt(anchor)
                            }) {
                                editor.change_selections(
                                    Default::default(),
                                    window,
                                    cx,
                                    |selections| {
                                        selections.select_ranges([anchor..anchor]);
                                    },
                                );
                                editor.request_autoscroll(editor::scroll::Autoscroll::fit(), cx);
                            }
                        });
                    });
                    if !scroll_driven && this.focus.contains_focused(window, cx) {
                        editor.focus_handle(cx).focus(window, cx);
                    }
                    this.github_editor_subscription = Some(cx.subscribe_in(
                        &editor,
                        window,
                        |this, _, event: &EditorEvent, window, cx| {
                            if matches!(event, EditorEvent::ScrollPositionChanged { local: true, autoscroll: false }) {
                                this.extend_scrolling_window(window, cx);
                            }
                            if matches!(event, EditorEvent::SelectionsChanged { local: true }) {
                                this.capture_github_selection(cx);
                                this.update_selection_affordance(cx);
                            }
                        },
                    ));
                    let mut lhs_id = editor.read(cx).lhs_editor().map(Entity::entity_id);
                    this.github_layout_subscription = Some(cx.observe_in(
                        &editor,
                        window,
                        move |this, editor, window, cx| {
                            let current_lhs = editor.read(cx).lhs_editor().map(Entity::entity_id);
                            if current_lhs != lhs_id {
                                lhs_id = current_lhs;
                                this.install_review_context_menus(cx);
                                this.rebuild_github_blocks(window, cx);
                            }
                        },
                    ));
                    this.editor = Some(editor);
                    this.install_review_context_menus(cx);
                    if !scroll_driven {
                        this.busy = false;
                    }
                    this.capture_github_selection(cx);
                    this.rebuild_github_blocks(window, cx);
                    this.rolling_file_load = false;
                    this.retained = Some(retained);
                    this.multi_loaded = multi_loaded;
                    this.loaded_window_scope = Some(scope);
                    if total_group_files > 50 && want_multi && layout_note.is_none() {
                        layout_note = Some("Scrolling nearby files · large files load on selection".into());
                    }
                    this.apply_review_file_folds(cx);
                    this.layout_note = layout_note;
                    if !scroll_driven {
                        this.busy = false;
                    }
                    #[cfg(not(test))]
                    this.drain_github_viewed_sync(window, cx);
                    this.start_prewarming(window, cx);
                    log::info!("review preparation cache: {cache_hits} hits, {cache_misses} misses");
                    log::info!(
                        "pull request review review file ready: {} files rendered in {} ms (excludes paint)",
                        if multi_loaded { group_files.len() } else { 1 },
                        started.elapsed().as_millis()
                    );
                    cx.notify();
                    // This callback runs at the next frame boundary before draw.
                    // Keep scheduling latency distinct from layout/paint and OS
                    // presentation; neither is measured by this milestone.
                    window.on_next_frame(move |_, _| {
                        log::info!(
                            "review next-frame scheduling: {} ms (excludes layout, paint and OS presentation)",
                            started.elapsed().as_millis()
                        );
                    });
                    anyhow::Ok(())
                })??;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    if this.file_request != request {
                        return;
                    }
                    this.rolling_file_load = false;
                    if !scroll_driven {
                        this.busy = false;
                    }
                    this.error = Some(format!("Unable to load snapshot: {error:#}"));
                    cx.notify();
                })
                .log_err();
            }
        });
        if scroll_driven {
            self.rolling_file_task = Some(task);
        } else {
            self.task = Some(task);
        }
        cx.notify();
    }

    fn extend_scrolling_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.rolling_file_load || !self.multi_loaded || self.data.files.len() <= 50
        {
            return;
        }
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let rhs = editor.read(cx).rhs_editor().clone();
        let (visible_path, loaded_paths) = rhs
            .update(cx, |editor, cx| {
                let display = editor.display_snapshot(cx);
                let anchor = editor.scroll_manager.native_anchor(&display, cx);
                let snapshot = display.buffer_snapshot();
                let (buffer_anchor, _) = snapshot.anchor_to_buffer_anchor(anchor.anchor)?;
                let visible_path = snapshot
                    .buffer_for_id(buffer_anchor.buffer_id)
                    .and_then(|buffer| buffer.file())
                    .map(|file| file.path().to_string());
                let loaded_paths = editor
                    .buffer()
                    .read(cx)
                    .all_buffers_iter()
                    .filter_map(|buffer| buffer.read(cx).file().map(|file| file.path().to_string()))
                    .collect::<BTreeSet<_>>();
                Some((visible_path, loaded_paths))
            })
            .unwrap_or_default();
        let Some(path) = visible_path else {
            return;
        };
        let Some(center) = self
            .data
            .files
            .iter()
            .position(|candidate| candidate == &path)
        else {
            return;
        };
        let range = review_file_window(self.data.files.len(), center);
        // Prefetch before reaching either edge; do not restart work on every pixel.
        let missing_nearby = self.data.files[range]
            .iter()
            .any(|candidate| !loaded_paths.contains(candidate));
        let current = self.data.session.presentation.selected_path.as_deref();
        if missing_nearby && current != Some(path.as_str()) {
            self.data.session.presentation.selected_path = Some(path);
            self.rolling_scroll = true;
            self.load_selected(window, cx);
        }
    }

    /// Move within an already prepared scrolling editor without replacing immutable
    /// buffers, inline blocks, or the viewport while navigation is persisted.
    fn reveal_loaded_scrolling_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(path) = self.data.session.presentation.selected_path.as_deref() else {
            return false;
        };
        let Some(editor) = self.editor.clone() else {
            return false;
        };
        let rhs = editor.read(cx).rhs_editor().clone();
        let target = {
            let multibuffer = rhs.read(cx).buffer().read(cx);
            let buffer = multibuffer.all_buffers_iter().find(|buffer| {
                buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| file.path().to_string() == path)
            });
            buffer.and_then(|buffer| {
                let snapshot = multibuffer.snapshot(cx);
                let anchor = snapshot
                    .excerpts_for_buffer(buffer.read(cx).remote_id())
                    .next()
                    .map(|excerpt| excerpt.context.start)?;
                snapshot.anchor_in_excerpt(anchor)
            })
        };
        let Some(anchor) = target else {
            return false;
        };
        self.clear_selection_affordance(cx);
        self.github_capture = None;
        self.github_composer_capture = None;
        rhs.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([anchor..anchor]);
            });
            editor.request_autoscroll(editor::scroll::Autoscroll::fit(), cx);
        });
        self.capture_github_selection(cx);
        log::info!("PR scrolling file activation: reusing loaded editor and buffers");
        true
    }

    fn change_presentation(
        &mut self,
        presentation: ReviewPresentation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Navigation uses the immutable snapshot already displayed. A newer
        // durable revision blocks writes, not reading, selection or expansion.
        let persist_navigation = !self.outdated();
        let before = self.data.session.presentation.clone();
        let reload = before.selected_path != presentation.selected_path
            || before.selected_group != presentation.selected_group
            || before.split_view != presentation.split_view
            || before.multi_file != presentation.multi_file
            || before.since_review != presentation.since_review;
        if reload && self.rolling_file_load {
            self.rolling_file_task = None;
            self.rolling_file_load = false;
            self.file_request = self.file_request.saturating_add(1);
        }
        if before.selected_path != presentation.selected_path
            || before.unreviewed_only != presentation.unreviewed_only
            || before.selected_group != presentation.selected_group
        {
            self.data.pinned_path = None;
            self.local_viewed_pin = None;
        }
        self.data.session.presentation = presentation.clone();
        if before.selected_group != presentation.selected_group
            && self.data.session.selector.pull_request_identity().is_none()
        {
            match read_data(&self.db, self.data.session.clone()) {
                Ok(data) => self.data = data,
                Err(error) => {
                    self.error = Some(format!("Could not load comparison progress: {error:#}"));
                    cx.notify();
                    return;
                }
            }
        }
        let mut effective = self.effective_data();
        choose_visible(&mut effective);
        self.data.session.presentation = effective.session.presentation;
        if self
            .github_conversation_anchor
            .as_ref()
            .is_some_and(|anchor| {
                self.data.session.presentation.selected_path.as_ref() != Some(&anchor.path)
            })
        {
            self.github_conversation_anchor = None;
        }
        let selected_changed = before.selected_path != self.data.session.presentation.selected_path;
        let db = self.db.clone();
        let id = self.data.session.id.clone();
        let generation = self.data.session.snapshot.generation;
        self.navigation_pending = persist_navigation;
        self.navigation_sequence = self.navigation_sequence.saturating_add(1);
        let sequence = self.navigation_sequence;
        self.presentation_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let result = cx
                .background_spawn(async move {
                    if persist_navigation {
                        db.save_navigation(id, generation, presentation).await
                    } else {
                        Ok(())
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if sequence == this.navigation_sequence {
                    this.navigation_pending = false;
                }
                match result {
                    Ok(()) => {}
                    Err(error) => {
                        this.check_error =
                            Some(format!("Could not save review position: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        if reload || selected_changed {
            self.reveal_selected_tree_row();
            let same_contents = !self.busy
                && !before.since_review
                && !self.data.session.presentation.since_review
                && before.selected_group == self.data.session.presentation.selected_group
                && before.multi_file == self.data.session.presentation.multi_file;
            let reused_scrolling = same_contents
                && self.multi_loaded
                && self.data.session.selector.supports_github_viewed()
                && before.split_view == self.data.session.presentation.split_view
                && self.reveal_loaded_scrolling_file(window, cx);
            let reused_layout = !reused_scrolling
                && same_contents
                && !selected_changed
                && before.split_view != self.data.session.presentation.split_view
                && self.editor.as_ref().is_some_and(|editor| {
                    let desired = self.data.session.presentation.split_view.unwrap_or(
                        editor::EditorSettings::get_global(cx).diff_view_style
                            == DiffViewStyle::Split,
                    );
                    editor.update(cx, |editor, cx| {
                        if (editor.diff_view_style() == DiffViewStyle::Split) != desired {
                            editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
                        }
                    });
                    true
                });
            if !reused_scrolling && !reused_layout {
                self.load_selected(window, cx);
            }
        }
        cx.notify();
    }

    fn finish(&mut self, result: Result<ReviewData>, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = false;
        match result {
            Ok(mut data) => {
                self.cached_unverified = false;
                if data.session.revision < self.data.session.revision
                    || data.session.revision < self.observed_revision
                {
                    self.error = Some(
                        "Review changed in another window. Reload review before continuing.".into(),
                    );
                    cx.notify();
                    return;
                }
                self.error = None;
                // Pending/accepting click overlays remain independent. Only the
                // durable map is replaced with the freshly loaded session data.
                self.github_viewed_intents = data.durable_viewed_intents.clone();
                self.tree_width_preview = None;
                if self.refreshing {
                    data.session.presentation = self.data.session.presentation.clone();
                }
                let changed_generation =
                    data.session.snapshot.generation != self.data.session.snapshot.generation;
                let metadata_changed = data.pull_request != self.data.pull_request;
                if changed_generation || metadata_changed {
                    self.github_viewed_initial_pending =
                        data.session.selector.supports_github_viewed();
                    self.history_request += 1;
                    self.history_loading = false;
                    self.history_error = None;
                    self.history_task = None;
                    self.check_request += 1;
                    self.check_task = None;
                    self.pr_check_guard = None;
                    self.checking = false;
                    self.update_available = false;
                }
                if data.session.presentation.path_filter
                    != self.data.session.presentation.path_filter
                {
                    self.path_input.update(cx, |input, cx| {
                        input.set_text(&data.session.presentation.path_filter, window, cx)
                    });
                }
                if let Some(path) = &self.data.session.presentation.selected_path {
                    if data.viewed.contains(path) && !self.data.viewed.contains(path) {
                        self.collapsed_review_files.insert(path.clone());
                    } else if !data.viewed.contains(path) {
                        self.collapsed_review_files.remove(path);
                    }
                }
                self.observed_revision = data.session.revision;
                data.pinned_path = self
                    .local_viewed_pin
                    .as_ref()
                    .filter(|path| {
                        data.session.selector.supports_github_viewed()
                            && self.data.session.presentation.selected_path.as_ref() == Some(*path)
                            && self
                                .github_viewed_intents
                                .get(*path)
                                .copied()
                                .unwrap_or_else(|| data.viewed.contains(*path))
                    })
                    .cloned();
                if data.pinned_path.is_some() {
                    data.session.presentation.selected_path = data.pinned_path.clone();
                }
                choose_visible(&mut data);
                let reload_editor = data.session.presentation.since_review
                    != self.data.session.presentation.since_review
                    || changed_generation
                    || self.editor.is_none()
                    || data.session.presentation.selected_path
                        != self.data.session.presentation.selected_path
                    || data.session.presentation.selected_group
                        != self.data.session.presentation.selected_group
                    || data.session.presentation.split_view
                        != self.data.session.presentation.split_view
                    || data.session.presentation.multi_file
                        != self.data.session.presentation.multi_file;
                let reveal_selected = changed_generation
                    || data.session.presentation.selected_path
                        != self.data.session.presentation.selected_path
                    || data.session.presentation.selected_group
                        != self.data.session.presentation.selected_group;
                self.data = data;
                self.ensure_github_panel(window, cx);
                if reveal_selected {
                    self.reveal_selected_tree_row();
                }
                cx.emit(ItemEvent::Edit);
                if reload_editor {
                    self.load_selected(window, cx);
                } else {
                    self.apply_review_file_folds(cx);
                    cx.notify();
                }
                self.changes.update(cx, |_, cx| {
                    cx.emit(ReviewChanged {
                        session: self.data.session.id.clone(),
                        revision: self.data.session.revision,
                    })
                });
                if changed_generation || metadata_changed {
                    self.check_for_updates(window, cx);
                }
            }
            Err(error) => {
                self.error = Some(format!("Review operation failed: {error:#}"));
                cx.notify();
            }
        }
    }

    fn apply_review_file_folds(&self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        editor.update(cx, |editor, cx| {
            let buffers = editor
                .rhs_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers_iter()
                .collect::<Vec<_>>();
            for buffer in buffers {
                let (buffer_id, path) = {
                    let buffer = buffer.read(cx);
                    let Some(file) = buffer.file() else {
                        continue;
                    };
                    (buffer.remote_id(), file.path().to_string())
                };
                editor.rhs_editor().update(cx, |editor, cx| {
                    if self.collapsed_review_files.contains(&path) {
                        editor.fold_buffer(buffer_id, cx);
                    } else {
                        editor.unfold_buffer(buffer_id, cx);
                    }
                });
            }
        });
    }

    fn select(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let mut presentation = self.data.session.presentation.clone();
        if matches!(
            self.data.session.selector,
            ComparisonSelector::PullRequestScope { .. }
        ) {
            presentation.selected_group = self
                .data
                .session
                .snapshot
                .groups
                .iter()
                .find(|group| {
                    group
                        .files
                        .iter()
                        .any(|file| file.file_key().ok() == Some(path.as_str()))
                })
                .map(|group| group.endpoints.group.clone());
        }
        presentation.selected_path = Some(path);
        self.change_presentation(presentation, window, cx);
    }

    fn toggle_filter(
        &mut self,
        _: &ToggleUnreviewedFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut presentation = self.data.session.presentation.clone();
        presentation.unreviewed_only = !presentation.unreviewed_only;
        self.change_presentation(presentation, window, cx);
    }

    fn toggle_viewed(
        &mut self,
        _: &ToggleReviewViewed,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recovery_running
            || self.cached_unverified
            || self.busy
            || self.outdated()
            || self.error.is_some()
        {
            return;
        }
        let Some(path) = self.data.session.presentation.selected_path.clone() else {
            return;
        };
        if self.data.session.presentation.since_review {
            self.github_viewed_error =
                Some("Open Full PR comparison before marking the complete file Viewed.".into());
            cx.notify();
            return;
        }
        if self.data.session.selector.supports_github_viewed() {
            self.set_github_paths_viewed(
                vec![path.clone()],
                !self.effective_github_viewed(&path),
                None,
                window,
                cx,
            );
            return;
        }
        if matches!(
            self.data.session.selector,
            ComparisonSelector::PullRequestScope { .. }
        ) {
            let reviewed = !self.data.viewed.contains(&path);
            self.review_commit_paths(vec![path], reviewed, true, window, cx);
            return;
        }
        if matches!(
            self.data.session.selector,
            ComparisonSelector::PullRequestScope { .. }
        ) && self.data.session.snapshot.groups.len() > 1
        {
            let patches = self.selected_patch_contents.clone();
            if patches.is_empty() {
                return;
            }
            let unview = self.data.viewed.contains(&path);
            let db = self.db.clone();
            let session = self.data.session.clone();
            self.busy = true;
            self.task = Some(cx.spawn_in(window, async move |this, cx| {
                let result = cx
                    .background_spawn(async move {
                        let mut revision = session.revision;
                        for (group, file, contents) in patches {
                            let target = ViewedTarget {
                                session: session.id.clone(),
                                expected_revision: revision,
                                displayed_generation: session.snapshot.generation,
                                group,
                                path: file.file_key()?.to_owned(),
                                fingerprint: file.fingerprint()?,
                            };
                            revision = if unview {
                                db.mark_unviewed(target).await?
                            } else {
                                db.mark_viewed(target, contents, REVIEW_PREVIEW_BUDGET as usize)
                                    .await?
                            };
                        }
                        read_data(&db, db.load_session(&session.id)?)
                    })
                    .await;
                this.update_in(cx, |this, window, cx| this.finish(result, window, cx))
                    .log_err();
            }));
            cx.notify();
            return;
        }
        let Some(contents) = self.retained.clone() else {
            return;
        };
        let Ok(file) = self
            .data
            .session
            .snapshot
            .file(active_group(&self.data), &path)
        else {
            return;
        };
        let fingerprint = match file.fingerprint() {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let target = ViewedTarget {
            session: self.data.session.id.clone(),
            expected_revision: self.data.session.revision,
            displayed_generation: self.data.session.snapshot.generation,
            group: active_group(&self.data).to_owned(),
            path: path.clone(),
            fingerprint,
        };
        let unview = self.data.viewed.contains(&path);
        let db = self.db.clone();
        self.busy = true;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let id = target.session.clone();
                    if unview {
                        db.mark_unviewed(target).await?;
                    } else {
                        let budget = contents
                            .old
                            .as_ref()
                            .map_or(0, Vec::len)
                            .checked_add(contents.new.as_ref().map_or(0, Vec::len))
                            .context("retention budget overflow")?;
                        db.mark_viewed(target, contents, budget).await?;
                    }
                    read_data(&db, db.load_session(&id)?)
                })
                .await;
            this.update_in(cx, |this, window, cx| this.finish(result, window, cx))
                .log_err();
        }));
        cx.notify();
    }

    fn current_tree_width(&self) -> f32 {
        self.tree_scroll
            .0
            .borrow()
            .last_item_size
            .map(|size| f32::from(size.item.width) + 1.0)
            .filter(|width| *width > 1.0)
            .unwrap_or(
                self.tree_width_preview
                    .or(self.data.session.presentation.tree_width)
                    .unwrap_or(320) as f32,
            )
    }

    fn set_tree_width(&mut self, width: Option<f32>, window: &mut Window, cx: &mut Context<Self>) {
        let mut presentation = self.data.session.presentation.clone();
        presentation.tree_width =
            width.map(|width| bounded_tree_width(width, f32::from(window.viewport_size().width)));
        self.tree_width_preview = presentation.tree_width;
        self.change_presentation(presentation, window, cx);
    }

    fn finish_tree_resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tree_resizing {
            return;
        }
        self.tree_resizing = false;
        if let Some(width) = self.tree_width_preview {
            log::info!(
                "pull request review review tree resize committed: {} px",
                width
            );
            self.set_tree_width(Some(width as f32), window, cx);
        }
    }

    fn change_group(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.data.session.snapshot.groups.get(index) {
            let mut presentation = self.data.session.presentation.clone();
            presentation.selected_group = Some(group.endpoints.group.clone());
            presentation.selected_path = None;
            self.change_presentation(presentation, window, cx);
        }
    }

    fn open_all_pull_request_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(context) = self.pull_request_context() else {
            return;
        };
        if self.busy {
            return;
        }
        let db = self.db.clone();
        let binding = self.data.session.repository.id.clone();
        let project = self.project.clone();
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        self.busy = true;
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let data = cx.background_spawn({ let db = db.clone(); async move {
                    let session = db.find_pull_request_session(&binding, &context.identity)?.context("Full PR comparison is unavailable. Open its overview and reload Files.")?;
                    db.apply_pull_request_session(session.id.clone()).await?;
                    read_data(&db, session)
                }}).await?;
                workspace.update_in(cx, |workspace, window, cx| Self::show_data(workspace, project, repository, db, data, window, cx))?;
                anyhow::Ok(())
            }.await;
            this.update(cx, |this, cx| { this.busy = false; if let Err(error) = result { this.error = Some(error.to_string()); } cx.notify(); }).log_err();
        }));
        cx.notify();
    }

    fn reveal_selected_tree_row(&self) {
        if let Some(index) = review_tree(&self.data).iter().position(|row| {
            !row.directory
                && Some(&row.path) == self.data.session.presentation.selected_path.as_ref()
        }) {
            self.tree_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    fn navigate_file(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut data = self.effective_data();
        // Deliberate Next/Previous skips locally reviewed/pinned entries.
        data.pinned_path = None;
        if let Some(path) = tree_navigation_target(&data, forward) {
            self.select(path, window, cx);
        }
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let db = self.db.clone();
        let id = self.data.session.id.clone();
        self.busy = true;
        cx.notify();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { read_data(&db, db.load_session(&id)?) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.finish(result, window, cx);
                this.check_for_updates(window, cx);
            })
            .log_err();
        }));
    }

    #[cfg(test)]
    fn toggle_split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let mut presentation = self.data.session.presentation.clone();
        presentation.split_view = Some(editor.read(cx).diff_view_style() != DiffViewStyle::Split);
        self.change_presentation(presentation, window, cx);
    }

    pub(crate) fn show_commit_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_pull_request_commits(window, cx);
    }

    fn open_pr_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(feature = "review-test-support")]
        if github_viewed::fixture::matches(&self.data.session.selector) {
            return;
        }
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    crate::pull_request_view::PullRequestView::open_list(workspace, window, cx)
                })
                .log_err();
        });
    }

    fn open_overview(
        &mut self,
        reference: PullRequestReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_pr_page(reference, false, window, cx);
    }

    fn open_commits(
        &mut self,
        reference: PullRequestReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_pr_page(reference, true, window, cx);
    }

    fn open_pr_page(
        &mut self,
        reference: PullRequestReference,
        commits: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(feature = "review-test-support")]
        if github_viewed::fixture::matches(&self.data.session.selector) {
            return;
        }
        let workspace = self.workspace.clone();
        remember_review_scope(
            workspace.entity_id(),
            self.repository.entity_id(),
            &self.data.session,
            cx,
        );
        let origin = cx.weak_entity();
        let session = self.data.session.id.clone();
        // Activating another item deactivates this view and must happen after its listener lease ends.
        window.defer(cx, move |window, cx| {
            let current = origin.read_with(cx, |view, _| {
                view.data.session.id == session
                    && view
                        .data
                        .pull_request
                        .as_ref()
                        .is_some_and(|metadata| metadata.url() == reference.url())
            });
            if !matches!(current, Ok(true)) {
                return;
            }
            let result = workspace.update(cx, |workspace, cx| {
                if workspace
                    .active_item_as::<Self>(cx)
                    .is_some_and(|view| view.entity_id() == origin.entity_id())
                {
                    if commits {
                        crate::pull_request_view::PullRequestView::open_commits(
                            workspace, reference, window, cx,
                        );
                    } else {
                        crate::pull_request_view::PullRequestView::open_overview(
                            workspace, reference, window, cx,
                        );
                    }
                }
            });
            if let Err(error) = result {
                origin
                    .update(cx, |view, cx| {
                        view.error = Some(error.to_string());
                        cx.notify();
                    })
                    .log_err();
            }
        });
    }

    fn open_current_file(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| -> Result<()> {
            let path = RepoPath::from_rel_path(RelPath::from_unix_str(path)?);
            let path = self
                .repository
                .read(cx)
                .repo_path_to_project_path(&path, cx)
                .context("file not available in the project")?;
            ensure!(
                self.project.read(cx).entry_for_path(&path, cx).is_some(),
                "File is absent from the current checkout. Choose Open File: Reviewed revision in the view menu to inspect its read-only version."
            );
            let workspace = self.workspace.clone();
            let origin = cx.weak_entity();
            let session = self.data.session.id.clone();
            window.defer(cx, move |window, cx| {
                if !matches!(
                    origin.read_with(cx, |view, _| view.data.session.id == session),
                    Ok(true)
                ) {
                    return;
                }
                let result = workspace.update(cx, |workspace, cx| {
                    if workspace
                        .active_item_as::<Self>(cx)
                        .is_some_and(|view| view.entity_id() == origin.entity_id())
                    {
                        workspace
                            .open_path_preview(path, None, false, false, true, window, cx)
                            .detach_and_log_err(cx);
                    }
                });
                if let Err(error) = result {
                    origin
                        .update(cx, |view, cx| {
                            view.check_error = Some(error.to_string());
                            cx.notify();
                        })
                        .log_err();
                }
            });
            Ok(())
        })();
        if let Err(error) = result {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(error.to_string(), cx)
                })
                .log_err();
        }
    }
}

impl EventEmitter<ItemEvent> for ReviewView {}
impl Focusable for ReviewView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor
            .as_ref()
            .map(|editor| editor.focus_handle(cx))
            .unwrap_or_else(|| self.focus.clone())
    }
}
impl Item for ReviewView {
    type Event = ItemEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.data
            .pull_request
            .as_ref()
            .map(|metadata| {
                format!(
                    "#{} · {} · {}",
                    metadata.number,
                    metadata.title,
                    match &self.data.session.selector {
                        ComparisonSelector::PullRequestScope { scope, .. } if scope.is_range() =>
                            "Commit range",
                        ComparisonSelector::PullRequestScope { .. } => "Selected commits",
                        _ => "Files",
                    }
                )
                .into()
            })
            .unwrap_or_else(|| "Code Review".into())
    }
    fn to_item_events(event: &ItemEvent, callback: &mut dyn FnMut(ItemEvent)) {
        callback(*event);
    }
    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        this: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(this.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            self.editor
                .as_ref()
                .map(|editor| editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }
    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        self.editor
            .as_ref()
            .map(|editor| Box::new(editor.clone()) as Box<dyn SearchableItemHandle>)
    }
}

impl SerializableItem for ReviewView {
    fn serialized_item_kind() -> &'static str {
        "CodeReview"
    }
    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }
    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = ReviewDb::global(cx);
        window.spawn(cx, async move |cx| {
            let data = cx
                .background_spawn({
                    let db = db.clone();
                    async move { read_data(&db, db.restored_item_session(workspace_id, item_id)?) }
                })
                .await?;
            project
                .read_with(cx, |project, cx| project.wait_for_initial_scan(cx))
                .await;
            let candidates = project.read_with(cx, |project, cx| {
                project
                    .git_store()
                    .read(cx)
                    .repositories()
                    .values()
                    .map(|repo| {
                        (
                            repo.clone(),
                            repo.read(cx)
                                .snapshot()
                                .repository_dir_abs_path
                                .to_path_buf(),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            let paths = candidates
                .iter()
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>();
            let binding = data.session.repository.worktree_directory.clone();
            let index = cx
                .background_spawn(async move {
                    for (index, path) in paths.iter().enumerate() {
                        if path.canonicalize()? == binding {
                            return Ok::<_, anyhow::Error>(index);
                        }
                    }
                    anyhow::bail!("review repository is not in this workspace")
                })
                .await?;
            let repository = candidates
                .get(index)
                .context("repository disappeared")?
                .0
                .clone();
            cx.update(|window, cx| {
                let cached_pr = data.session.selector.pull_request_identity().is_some();
                let restore_key = (workspace.entity_id(), data.session.id.clone());
                if cached_pr {
                    if let Some(existing) = cx
                        .try_global::<RestoredReviewViews>()
                        .and_then(|views| views.0.get(&restore_key))
                        .and_then(WeakEntity::upgrade)
                    {
                        return existing;
                    }
                    if cx.try_global::<RestoredReviewViews>().is_none() {
                        cx.set_global(RestoredReviewViews::default());
                    }
                    remember_review_scope(
                        workspace.entity_id(),
                        repository.entity_id(),
                        &data.session,
                        cx,
                    );
                }
                let view = cx.new(|cx| {
                    let mut view = Self::new(project, repository, workspace, db, data, window, cx);
                    if cached_pr {
                        view.cached_unverified = true;
                        view.github_panel = None;
                        view.github_subscriptions.clear();
                        view.check_error =
                            Some("Cached PR snapshot · verifying account and revision".into());
                    }
                    view
                });
                if cached_pr {
                    cx.global_mut::<RestoredReviewViews>()
                        .0
                        .insert(restore_key, view.downgrade());
                }
                view
            })
        })
    }
    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item: workspace::ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace = workspace.database_id()?;
        let db = self.db.clone();
        let session = self.data.session.id.clone();
        Some(cx.background_spawn(async move { db.save_item(workspace, item, session).await }))
    }
    fn should_serialize(&self, _: &ItemEvent) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "review_view_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "review_checkbox_tests.rs"]
mod checkbox_tests;
