use crate::commit::{CommitDiffObject, CommitDiffObjectKind, parse_git_diff_raw};
use crate::stash::GitStash;
use crate::status::{
    DiffTreeType, FileStatus, GitStatus, StatusCode, TrackedStatus, TreeDiff, TreeDiffStatus,
};
use crate::{Oid, RunHook, SHA256_HEX_LENGTH, SHORT_SHA_LENGTH};
use anyhow::{Context as _, Result, anyhow, bail};
use async_channel::Sender;
use collections::HashMap;
use file_content::{ByteContent, decode_byte_header};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::io::BufWriter;
use futures::{AsyncWriteExt, FutureExt as _, select_biased};
use gpui::{AppContext as _, AsyncApp, BackgroundExecutor, SharedString, Task};
use parking_lot::Mutex;
use rope::Rope;
use schemars::JsonSchema;
use serde::Deserialize;
use smallvec::SmallVec;
use smol::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use text::LineEnding;

use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::sync::atomic::AtomicBool;

use std::process::{ExitStatus, Output};
use std::str::FromStr;
use std::time::SystemTime;
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    sync::Arc,
};
use sum_tree::MapSeekTarget;
use thiserror::Error;
use util::command::{Stdio, new_command};
use util::paths::PathStyle;
use util::rel_path::RelPath;
use util::{ResultExt, paths};
use uuid::Uuid;

pub use askpass::{AskPassDelegate, AskPassResult, AskPassSession};

pub const REMOTE_CANCELLED_BY_USER: &str = "Operation cancelled by user";

/// Format string used in graph log to get initial data for the git graph
/// %H - Full commit hash
/// %P - Parent hashes
/// %D - Ref names
/// %x00 - Null byte separator, used to split up commit data
static GRAPH_COMMIT_FORMAT: &str = "--format=%H%x00%P%x00%D";

/// Used to get commits that match with a search
/// %H - Full commit hash
static SEARCH_COMMIT_FORMAT: &str = "--format=%H";

/// Number of commits to load per chunk for the git graph.
pub const GRAPH_CHUNK_SIZE: usize = 1000;

/// Default value for the `git.worktree_directory` setting.
pub const DEFAULT_WORKTREE_DIRECTORY: &str = "../worktrees";

/// Given the git common directory (from `commondir()`), derive the original
/// repository's working directory.
///
/// For a standard checkout, `common_dir` is `<work_dir>/.git`, so the parent
/// is the working directory. For a git worktree, `common_dir` is the **main**
/// repo's `.git` directory, so the parent is the original repo's working directory.
///
/// Returns `None` if `common_dir` doesn't end with `.git` (e.g. bare repos),
/// because there is no working-tree root to resolve to in that case.
pub fn original_repo_path_from_common_dir(common_dir: &Path) -> Option<PathBuf> {
    if common_dir.file_name() == Some(OsStr::new(".git")) {
        common_dir.parent().map(|p| p.to_path_buf())
    } else {
        None
    }
}

fn linked_worktree_git_dir(worktree_path: &Path) -> Result<PathBuf> {
    let dot_git_path = worktree_path.join(".git");
    let git_file = std::fs::read_to_string(&dot_git_path)
        .with_context(|| format!("failed to read {}", dot_git_path.display()))?;
    let git_dir = git_file
        .strip_prefix("gitdir:")
        .context("worktree .git file missing gitdir pointer")?
        .trim();
    Ok(worktree_path.join(git_dir))
}

fn normalize_git_metadata_path(path: PathBuf) -> Result<PathBuf> {
    paths::normalize_lexically(&path)
        .map_err(|_| anyhow!("git metadata path escapes its filesystem root: {path:?}"))
}

/// Commit data needed for the git graph visualization.
#[derive(Debug, Clone)]
pub struct CommitData {
    pub sha: Oid,
    /// Most commits have a single parent, so we use a SmallVec to avoid allocations.
    pub parents: SmallVec<[Oid; 1]>,
    pub author_name: SharedString,
    pub author_email: SharedString,
    pub commit_timestamp: i64,
    pub subject: SharedString,
    pub message: SharedString,
}

#[derive(Debug)]
pub struct InitialGraphCommitData {
    pub sha: Oid,
    pub parents: SmallVec<[Oid; 1]>,
    pub ref_names: Vec<SharedString>,
}

impl InitialGraphCommitData {
    pub fn tag_names(&self) -> Vec<&str> {
        self.ref_names
            .iter()
            .filter_map(|ref_name| {
                let tag_name = ref_name.strip_prefix("tag: ")?;

                if tag_name.is_empty() {
                    return None;
                }

                Some(tag_name)
            })
            .collect()
    }
}

struct CommitDataRequest {
    sha: Oid,
    response_tx: oneshot::Sender<Result<CommitData>>,
}

pub struct CommitDataReader {
    request_tx: async_channel::Sender<CommitDataRequest>,
    _task: Task<()>,
}

impl CommitDataReader {
    pub async fn read(&self, sha: Oid) -> Result<CommitData> {
        let (response_tx, response_rx) = oneshot::channel();
        self.request_tx
            .send(CommitDataRequest { sha, response_tx })
            .await
            .map_err(|_| anyhow!("commit data reader task closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow!("commit data reader task dropped response"))?
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        executor: BackgroundExecutor,
        resolve: impl 'static + Send + Sync + Fn(Oid) -> Result<CommitData>,
    ) -> Self {
        let (request_tx, request_rx) = smol::channel::bounded::<CommitDataRequest>(64);
        let resolve = Arc::new(resolve);
        let delay_executor = executor.clone();
        let task = executor.spawn(async move {
            while let Ok(CommitDataRequest { sha, response_tx }) = request_rx.recv().await {
                delay_executor.simulate_random_delay().await;
                response_tx.send(resolve(sha)).ok();
            }
        });

        Self {
            request_tx,
            _task: task,
        }
    }
}

fn parse_cat_file_commit(sha: Oid, content: &str) -> Option<CommitData> {
    let mut parents = SmallVec::new();
    let mut author_name = SharedString::default();
    let mut author_email = SharedString::default();
    let mut commit_timestamp = 0i64;
    let mut in_headers = true;
    let mut subject = None;
    let mut message_lines = Vec::new();

    for line in content.lines() {
        if in_headers {
            if line.is_empty() {
                in_headers = false;
                continue;
            }

            if let Some(parent_sha) = line.strip_prefix("parent ") {
                if let Ok(oid) = Oid::from_str(parent_sha.trim()) {
                    parents.push(oid);
                }
            } else if let Some(author_line) = line.strip_prefix("author ") {
                if let Some((name_email, _timestamp_tz)) = author_line.rsplit_once(' ') {
                    if let Some((name_email, timestamp_str)) = name_email.rsplit_once(' ') {
                        if let Ok(ts) = timestamp_str.parse::<i64>() {
                            commit_timestamp = ts;
                        }
                        if let Some((name, email)) = name_email.rsplit_once(" <") {
                            author_name = SharedString::from(name.to_string());
                            author_email =
                                SharedString::from(email.trim_end_matches('>').to_string());
                        }
                    }
                }
            }
        } else {
            if subject.is_none() {
                subject = Some(SharedString::from(line.to_string()));
            }
            message_lines.push(line);
        }
    }

    Some(CommitData {
        sha,
        parents,
        author_name,
        author_email,
        commit_timestamp,
        subject: subject.unwrap_or_default(),
        message: SharedString::from(message_lines.join("\n")),
    })
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Branch {
    pub is_head: bool,
    pub ref_name: SharedString,
    pub upstream: Option<Upstream>,
    pub most_recent_commit: Option<CommitSummary>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchesScanResult {
    pub branches: Vec<Branch>,
    pub error: Option<SharedString>,
}

impl From<Vec<Branch>> for BranchesScanResult {
    fn from(branches: Vec<Branch>) -> Self {
        Self {
            branches,
            error: None,
        }
    }
}

impl Branch {
    pub fn name(&self) -> &str {
        self.ref_name
            .as_ref()
            .strip_prefix("refs/heads/")
            .or_else(|| self.ref_name.as_ref().strip_prefix("refs/remotes/"))
            .unwrap_or(self.ref_name.as_ref())
    }

    pub fn is_remote(&self) -> bool {
        self.ref_name.starts_with("refs/remotes/")
    }

    pub fn remote_name(&self) -> Option<&str> {
        self.ref_name
            .strip_prefix("refs/remotes/")
            .and_then(|stripped| stripped.split("/").next())
    }

    pub fn tracking_status(&self) -> Option<UpstreamTrackingStatus> {
        self.upstream
            .as_ref()
            .and_then(|upstream| upstream.tracking.status())
    }

    pub fn priority_key(&self) -> (bool, Option<i64>) {
        (
            self.is_head,
            self.most_recent_commit
                .as_ref()
                .map(|commit| commit.commit_timestamp),
        )
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub ref_name: Option<SharedString>,
    // todo(git_worktree) This type should be a Oid
    pub sha: SharedString,
    pub is_main: bool,
    pub is_bare: bool,
}

/// Describes how a new worktree should choose or create its checked-out HEAD.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum CreateWorktreeTarget {
    /// Check out an existing local branch in the new worktree.
    ExistingBranch {
        /// The existing local branch to check out.
        branch_name: String,
    },
    /// Create a new local branch for the new worktree.
    NewBranch {
        /// The new local branch to create and check out.
        branch_name: String,
        /// The commit or ref to create the branch from. Uses `HEAD` when `None`.
        base_sha: Option<String>,
    },
    /// Check out a commit or ref in detached HEAD state.
    Detached {
        /// The commit or ref to check out. Uses `HEAD` when `None`.
        base_sha: Option<String>,
    },
}

impl CreateWorktreeTarget {
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::ExistingBranch { branch_name } | Self::NewBranch { branch_name, .. } => {
                Some(branch_name)
            }
            Self::Detached { .. } => None,
        }
    }
}

impl Worktree {
    /// Returns the branch name if the worktree is attached to a branch.
    pub fn branch_name(&self) -> Option<&str> {
        self.ref_name.as_ref().map(|ref_name| {
            ref_name
                .strip_prefix("refs/heads/")
                .or_else(|| ref_name.strip_prefix("refs/remotes/"))
                .unwrap_or(ref_name)
        })
    }

    /// Returns a display name for the worktree, suitable for use in the UI.
    ///
    /// If the worktree is attached to a branch, returns the branch name.
    /// Otherwise, returns the short SHA of the worktree's HEAD commit.
    pub fn display_name(&self) -> &str {
        self.branch_name()
            .unwrap_or(&self.sha[..self.sha.len().min(SHORT_SHA_LENGTH)])
    }

    pub fn directory_name(&self, name_anchor_path: Option<&Path>) -> String {
        if self.is_main {
            return "main worktree".to_string();
        }

        let dir_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(self.display_name());

        if let Some(name_anchor_path) = name_anchor_path {
            let name_anchor_dir = name_anchor_path.file_name().and_then(|name| name.to_str());
            if name_anchor_dir == Some(dir_name) {
                if let Some(parent_name) = self
                    .path
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                {
                    return parent_name.to_string();
                }
            }
        }

        dir_name.to_string()
    }
}

pub fn parse_worktrees_from_str<T: AsRef<str>>(
    raw_worktrees: T,
    main_worktree_path: Option<&Path>,
) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let normalized = raw_worktrees.as_ref().replace("\r\n", "\n");
    let entries = normalized.split("\n\n");
    for entry in entries {
        let mut path = None;
        let mut sha = None;
        let mut ref_name = None;

        let mut is_bare = false;

        for line in entry.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("worktree ") {
                path = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("HEAD ") {
                sha = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("branch ") {
                ref_name = Some(rest.to_string());
            } else if line == "bare" {
                is_bare = true;
            }
            // Ignore other lines: detached, locked, prunable, etc.
        }

        if let (Some(path), Some(sha)) = (path, sha) {
            let path = PathBuf::from(path);
            let is_main =
                main_worktree_path.is_some_and(|main_worktree_path| path == main_worktree_path);
            worktrees.push(Worktree {
                path,
                ref_name: ref_name.map(Into::into),
                sha: sha.into(),
                is_main,
                is_bare,
            });
        }
    }

    worktrees
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Upstream {
    pub ref_name: SharedString,
    pub tracking: UpstreamTracking,
}

impl Upstream {
    pub fn is_remote(&self) -> bool {
        self.remote_name().is_some()
    }

    pub fn remote_name(&self) -> Option<&str> {
        self.ref_name
            .strip_prefix("refs/remotes/")
            .and_then(|stripped| stripped.split("/").next())
    }

    pub fn stripped_ref_name(&self) -> Option<&str> {
        self.ref_name.strip_prefix("refs/remotes/")
    }

    pub fn branch_name(&self) -> Option<&str> {
        self.ref_name
            .strip_prefix("refs/remotes/")
            .and_then(|stripped| stripped.split_once('/').map(|(_, name)| name))
    }
}

#[derive(Clone, Copy, Default)]
pub struct CommitOptions {
    pub amend: bool,
    pub signoff: bool,
    pub allow_empty: bool,
    pub no_verify: bool,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum UpstreamTracking {
    /// Remote ref not present in local repository.
    Gone,
    /// Remote ref present in local repository (fetched from remote).
    Tracked(UpstreamTrackingStatus),
}

impl From<UpstreamTrackingStatus> for UpstreamTracking {
    fn from(status: UpstreamTrackingStatus) -> Self {
        UpstreamTracking::Tracked(status)
    }
}

impl UpstreamTracking {
    pub fn is_gone(&self) -> bool {
        matches!(self, UpstreamTracking::Gone)
    }

    pub fn status(&self) -> Option<UpstreamTrackingStatus> {
        match self {
            UpstreamTracking::Gone => None,
            UpstreamTracking::Tracked(status) => Some(*status),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteCommandOutput {
    pub stdout: String,
    pub stderr: String,
}

impl RemoteCommandOutput {
    pub fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct UpstreamTrackingStatus {
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CommitSummary {
    pub sha: SharedString,
    pub subject: SharedString,
    /// This is a unix timestamp
    pub commit_timestamp: i64,
    pub author_name: SharedString,
    pub has_parent: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct CommitDetails {
    pub sha: SharedString,
    pub message: SharedString,
    pub commit_timestamp: i64,
    pub author_email: SharedString,
    pub author_name: SharedString,
}

#[derive(Debug)]
pub struct CommitDiff {
    pub files: Vec<CommitFile>,
    pub is_shallow_boundary: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileHistoryChangedFileSets {
    pub file_sets: Vec<Vec<RepoPath>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommitFileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug)]
pub struct CommitFile {
    pub path: RepoPath,
    pub old_content: Option<Vec<u8>>,
    pub new_content: Option<Vec<u8>>,
    pub is_binary: bool,
}

impl CommitFile {
    pub fn status(&self) -> CommitFileStatus {
        match (&self.old_content, &self.new_content) {
            (None, Some(_)) => CommitFileStatus::Added,
            (Some(_), None) => CommitFileStatus::Deleted,
            _ => CommitFileStatus::Modified,
        }
    }
}

impl CommitDetails {
    pub fn short_sha(&self) -> SharedString {
        self.sha[..SHORT_SHA_LENGTH].to_string().into()
    }
}

pub fn is_binary_content(content: &[u8]) -> bool {
    let check_len = content.len().min(8000);
    content[..check_len].contains(&0)
}

struct LoadedCommitObject {
    content: Vec<u8>,
    is_binary: bool,
}

async fn read_commit_blob<R: smol::io::AsyncBufRead + Unpin>(
    stdout: &mut R,
    info_line: &mut String,
    newline: &mut [u8; 1],
) -> Result<LoadedCommitObject> {
    info_line.clear();
    stdout.read_line(info_line).await?;

    let len = info_line
        .trim_end()
        .parse()
        .with_context(|| format!("invalid object size output from cat-file {info_line}"))?;

    let mut bytes = vec![0; len];
    stdout.read_exact(&mut bytes).await?;
    stdout.read_exact(newline).await?;

    let is_binary =
        is_binary_content(&bytes) || decode_byte_header(&bytes).1 == ByteContent::Binary;
    Ok(LoadedCommitObject {
        content: bytes,
        is_binary,
    })
}

async fn load_commit_object<R: smol::io::AsyncBufRead + Unpin>(
    object: Option<CommitDiffObject<'_>>,
    stdout: &mut R,
    info_line: &mut String,
    newline: &mut [u8; 1],
) -> Result<Option<LoadedCommitObject>> {
    match object {
        Some(object) if object.kind == CommitDiffObjectKind::Gitlink => {
            Ok(Some(LoadedCommitObject {
                content: format!("Subproject commit {}\n", object.oid).into_bytes(),
                is_binary: false,
            }))
        }
        Some(_) => Ok(Some(read_commit_blob(stdout, info_line, newline).await?)),
        None => Ok(None),
    }
}

async fn read_shallow_file(shallow_file_path: &Path) -> Result<Option<String>> {
    match smol::fs::read_to_string(shallow_file_path).await {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("reading shallow file"),
    }
}

async fn is_shallow_boundary_commit(
    git: &GitBinary,
    shallow_file_path: &Path,
    commit: &str,
) -> Result<bool> {
    let Some(shallow_contents) = read_shallow_file(shallow_file_path).await? else {
        return Ok(false);
    };

    let oid = git
        .run(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")])
        .await
        .context("resolving commit for shallow boundary check")?;
    Ok(shallow_contents
        .lines()
        .any(|line| line.trim() == oid.trim()))
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Remote {
    pub name: SharedString,
}

pub enum ResetMode {
    /// Reset the branch pointer, leave index and worktree unchanged (this will make it look like things that were
    /// committed are now staged).
    Soft,
    /// Reset the branch pointer and index, leave worktree unchanged (this makes it look as though things that were
    /// committed are now unstaged).
    Mixed,
}

/// A pinned GitHub PR fetch. Remote identity and revisions come from verified API metadata.
#[derive(Clone, Debug)]
pub struct ReviewPrFetch {
    pub owner: String,
    pub repository: String,
    pub base_branch: String,
    pub number: u64,
    pub expected_base: Oid,
    pub expected_head: Oid,
}

#[derive(Clone, Debug)]
pub struct VerifiedReviewPr {
    pub base: Oid,
    pub head: Oid,
    pub base_ref: String,
    pub head_ref: String,
}

impl ReviewPrFetch {
    fn validate(&self) -> Result<()> {
        for component in [&self.owner, &self.repository] {
            anyhow::ensure!(
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && component
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte)),
                "Invalid GitHub repository identity"
            );
        }
        anyhow::ensure!(
            self.number > 0 && self.number <= i32::MAX as u64,
            "Invalid PR number"
        );
        anyhow::ensure!(
            !self.base_branch.is_empty() && !self.base_branch.contains('\0'),
            "Invalid base branch"
        );
        anyhow::ensure!(
            !self.expected_base.is_zero() && !self.expected_head.is_zero(),
            "Invalid PR revision"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum FetchOptions {
    All,
    Unshallow,
    Remote(Remote),
}

impl FetchOptions {
    pub fn to_proto(&self) -> Option<String> {
        match self {
            FetchOptions::All | FetchOptions::Unshallow => None,
            FetchOptions::Remote(remote) => Some(remote.clone().name.into()),
        }
    }

    pub fn from_proto(remote_name: Option<String>, unshallow: bool) -> Self {
        if unshallow {
            return FetchOptions::Unshallow;
        }
        match remote_name {
            Some(name) => FetchOptions::Remote(Remote { name: name.into() }),
            None => FetchOptions::All,
        }
    }

    pub fn name(&self) -> SharedString {
        match self {
            Self::All => "Fetch all remotes".into(),
            Self::Unshallow => "Fetch missing history".into(),
            Self::Remote(remote) => remote.name.clone(),
        }
    }
}

impl std::fmt::Display for FetchOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchOptions::All => write!(f, "--all"),
            FetchOptions::Unshallow => write!(f, "--unshallow"),
            FetchOptions::Remote(remote) => write!(f, "{}", remote.name),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Copy)]
pub enum LogOrder {
    #[default]
    DateOrder,
    TopoOrder,
    AuthorDateOrder,
    ReverseChronological,
}

impl LogOrder {
    pub fn as_arg(&self) -> &'static str {
        match self {
            LogOrder::DateOrder => "--date-order",
            LogOrder::TopoOrder => "--topo-order",
            LogOrder::AuthorDateOrder => "--author-date-order",
            LogOrder::ReverseChronological => "--reverse",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum LogSource {
    #[default]
    All,
    Branch(SharedString),
    Sha(Oid),
    Path(RepoPath),
}

impl LogSource {
    fn get_args(&self) -> Vec<Cow<'_, str>> {
        let mut args = match self {
            LogSource::All => vec![
                Cow::Borrowed("--ignore-missing"), // needed in case of unborn HEAD
                Cow::Borrowed("--branches"),
                Cow::Borrowed("--remotes"),
                Cow::Borrowed("--tags"),
                Cow::Borrowed("HEAD"),
            ],
            LogSource::Branch(branch) => vec![Cow::Borrowed(branch.as_str())],
            LogSource::Sha(oid) => vec![Cow::Owned(oid.to_string())],
            LogSource::Path(path) => vec![
                Cow::Borrowed("--follow"),
                Cow::Borrowed("--"),
                Cow::Borrowed(path.as_unix_str()),
            ],
        };
        // Without a terminator git cannot tell a branch named `docs/rewrite` from a
        // `docs/rewrite` directory in the working tree, and refuses the argument as
        // ambiguous. `Path` states its own separator before the path it passes.
        if !matches!(self, LogSource::Path(_)) {
            args.push(Cow::Borrowed("--"));
        }
        args
    }
}

pub struct SearchCommitArgs {
    pub query: SharedString,
    pub case_sensitive: bool,
}

pub fn commit_hash_search_query(query: &str) -> Option<&str> {
    let query = query.trim();
    (SHORT_SHA_LENGTH..=SHA256_HEX_LENGTH)
        .contains(&query.len())
        .then_some(query)
        .filter(|query| query.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub fn delete_branch_flag(is_remote_tracking_ref: bool, force: bool) -> &'static str {
    match (is_remote_tracking_ref, force) {
        (true, true) => "-Dr",
        (true, false) => "-dr",
        (false, true) => "-D",
        (false, false) => "-d",
    }
}

pub trait GitRepository: Send + Sync {
    /// Returns the contents of an entry in the repository's index, or None if there is no entry for the given path.
    ///
    /// Also returns `None` for symlinks.
    fn load_index_text(&self, path: RepoPath) -> BoxFuture<'_, Option<Vec<u8>>> {
        let future = self.load_revisions(vec![format!(":{}", path.as_unix_str())]);
        async move { future.await.ok()?.pop()? }.boxed()
    }

    /// Returns the contents of an entry in the repository's HEAD, or None if HEAD does not exist or has no entry for the given path.
    ///
    /// Also returns `None` for symlinks.
    fn load_committed_text(&self, path: RepoPath) -> BoxFuture<'_, Option<Vec<u8>>> {
        let future = self.load_revisions(vec![format!("HEAD:{}", path.as_unix_str())]);
        async move { future.await.ok()?.pop()? }.boxed()
    }
    fn load_blob_content(&self, oid: Oid) -> BoxFuture<'_, Result<Vec<u8>>>;

    fn set_index_text(
        &self,
        path: RepoPath,
        content: Option<Vec<u8>>,
        env: Arc<HashMap<String, String>>,
        is_executable: bool,
    ) -> BoxFuture<'_, anyhow::Result<()>>;

    /// Returns the URL of the remote with the given name.
    fn remote_url(&self, name: &str) -> BoxFuture<'_, Option<String>> {
        let name = name.to_string();
        let fut = self.remote_urls();
        async move { fut.await.remove(&name) }.boxed()
    }

    /// Returns the URL of all remotes.
    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>>;

    /// Resolve a list of refs to SHAs.
    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>>;

    fn load_revisions(&self, revisions: Vec<String>)
    -> BoxFuture<'_, Result<Vec<Option<Vec<u8>>>>>;

    fn head_sha(&self) -> BoxFuture<'_, Option<String>> {
        async move {
            self.revparse_batch(vec!["HEAD".into()])
                .await
                .unwrap_or_default()
                .into_iter()
                .next()
                .flatten()
        }
        .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>>;

    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>>;
    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>>;

    fn review_blob_sizes(&self, _objects: Vec<Oid>) -> BoxFuture<'_, Result<Vec<u64>>> {
        async { bail!("Review blob sizes are unsupported by this Git backend") }.boxed()
    }

    fn review_history(
        &self,
        _base: Option<Oid>,
        _head: Oid,
    ) -> BoxFuture<'_, Result<Vec<crate::review::ReviewCommit>>> {
        async { anyhow::bail!("commit review is unsupported by this Git backend") }.boxed()
    }

    fn review_commits(
        &self,
        _selections: Vec<crate::review::ReviewCommitSelection>,
        _range: bool,
    ) -> BoxFuture<'_, Result<Vec<crate::review::ReviewComparison>>> {
        async { anyhow::bail!("commit review is unsupported by this Git backend") }.boxed()
    }

    fn review_comparison(
        &self,
        _base: String,
        _head: String,
        _merge_base: bool,
    ) -> BoxFuture<'_, Result<crate::review::ReviewComparison>> {
        async { anyhow::bail!("immutable review comparisons are unsupported by this Git backend") }
            .boxed()
    }

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>>;

    fn check_access(&self) -> BoxFuture<'_, Result<()>> {
        async move { Ok(()) }.boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>>;

    fn change_branch(&self, name: String) -> BoxFuture<'_, Result<()>>;
    fn create_branch(&self, name: String, base_branch: Option<String>)
    -> BoxFuture<'_, Result<()>>;
    fn rename_branch(&self, branch: String, new_name: String) -> BoxFuture<'_, Result<()>>;

    fn delete_branch(
        &self,
        is_remote: bool,
        name: String,
        force: bool,
    ) -> BoxFuture<'_, Result<()>>;

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>>;

    /// Returns the creation time of a linked worktree's git metadata
    /// directory (`.git/worktrees/<name>/`), resolved via the worktree's
    /// `.git` file.
    ///
    /// The metadata directory is created by `git worktree add` and removed
    /// by `git worktree remove`, so its creation time identifies a
    /// particular incarnation of the worktree: if the worktree is removed
    /// and recreated at the same path, the creation time changes.
    ///
    /// Returns `Ok(None)` when the worktree directory does not exist at
    /// all, and an error when the directory exists but the time cannot be
    /// determined (e.g. on filesystems without birthtime support); callers
    /// should fail safe in the error case.
    fn worktree_created_at(
        &self,
        worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>>;

    fn create_worktree(
        &self,
        target: CreateWorktreeTarget,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<()>>;

    fn checkout_branch_in_worktree(
        &self,
        branch_name: String,
        worktree_path: PathBuf,
        create: bool,
    ) -> BoxFuture<'_, Result<()>>;

    fn remove_worktree(&self, path: PathBuf, force: bool) -> BoxFuture<'_, Result<()>>;

    fn rename_worktree(&self, old_path: PathBuf, new_path: PathBuf) -> BoxFuture<'_, Result<()>>;

    fn reset(
        &self,
        commit: String,
        mode: ResetMode,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    /// Restores `paths` in the working tree.
    ///
    /// With `Some(commit)`, both the index and the working tree are reset to the
    /// commit's version, discarding staged and unstaged changes. With `None`, only
    /// the working tree is restored from the index, so staged changes are kept.
    fn checkout_files(
        &self,
        commit: Option<String>,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>>;

    fn load_commit(
        &self,
        commit: String,
        ignore_shallow_boundary: bool,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<CommitDiff>>;
    fn blame(
        &self,
        path: RepoPath,
        content: Rope,
        line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>>;

    fn blame_at_revision(
        &self,
        path: RepoPath,
        revision: Oid,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>>;

    /// Returns the absolute path to the repository. For worktrees, this will be the path to the
    /// worktree's gitdir within the main repository (typically `.git/worktrees/<name>`).
    fn path(&self) -> PathBuf;

    fn main_repository_path(&self) -> PathBuf;

    /// Updates the index to match the worktree at the given paths.
    ///
    /// If any of the paths have been deleted from the worktree, they will be removed from the index if found there.
    fn stage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;
    /// Updates the index to match HEAD at the given paths.
    ///
    /// If any of the paths were previously staged but do not exist in HEAD, they will be removed from the index.
    fn unstage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    /// Only used to serve `proto::RunGitHook` requests from older remote clients;
    /// new code lets `git commit` run hooks itself.
    ///
    /// TODO: remove together with `proto::RunGitHook` (see the deprecation note in git.proto).
    fn run_hook(
        &self,
        hook: RunHook,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn commit(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_paths(
        &self,
        paths: Vec<RepoPath>,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_staged(
        &self,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_pop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_apply(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_drop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn push(
        &self,
        branch_name: String,
        remote_branch_name: String,
        upstream_name: String,
        options: Option<PushOptions>,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        // This method takes an AsyncApp to ensure it's invoked on the main thread,
        // otherwise git-credentials-manager won't work.
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>>;

    fn pull(
        &self,
        branch_name: Option<String>,
        upstream_name: String,
        rebase: bool,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        // This method takes an AsyncApp to ensure it's invoked on the main thread,
        // otherwise git-credentials-manager won't work.
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>>;

    fn fetch(
        &self,
        fetch_options: FetchOptions,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        // This method takes an AsyncApp to ensure it's invoked on the main thread,
        // otherwise git-credentials-manager won't work.
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>>;

    /// Local-only PR-object fetch; remote transports must opt in explicitly.
    fn fetch_review_pr(
        &self,
        _request: ReviewPrFetch,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<VerifiedReviewPr>> {
        async { bail!("PR fetching is unsupported by this Git backend") }.boxed()
    }

    fn get_push_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>>;

    fn get_branch_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>>;

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>>;

    fn remove_remote(&self, name: String) -> BoxFuture<'_, Result<()>>;

    fn create_remote(&self, name: String, url: String) -> BoxFuture<'_, Result<()>>;

    /// returns a list of remote branches that contain HEAD
    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>>;

    /// Run git diff
    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>>;

    fn diff_stat(
        &self,
        diff: DiffStatType,
        path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<crate::status::GitDiffStat>>;

    /// Creates a checkpoint for the repository.
    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>>;

    /// Resets to a previously-created checkpoint.
    fn restore_checkpoint(&self, checkpoint: GitRepositoryCheckpoint) -> BoxFuture<'_, Result<()>>;

    /// Creates two detached commits capturing the current staged and unstaged
    /// state without moving any branch. Returns (staged_sha, unstaged_sha).
    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>>;

    /// Restores the working directory and index from archive checkpoint SHAs.
    /// Assumes HEAD is already at the correct commit (original_commit_hash).
    /// Restores the index to match staged_sha's tree, and the working
    /// directory to match unstaged_sha's tree.
    fn restore_archive_checkpoint(
        &self,
        staged_sha: String,
        unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>>;

    /// Compares two checkpoints, returning true if they are equal
    fn compare_checkpoints(
        &self,
        left: GitRepositoryCheckpoint,
        right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>>;

    /// Computes a diff between two checkpoints.
    fn diff_checkpoints(
        &self,
        base_checkpoint: GitRepositoryCheckpoint,
        target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>>;

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>>;

    fn default_branch(
        &self,
        include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>>;

    /// Runs `git rev-list --parents` to get the commit graph structure.
    /// Returns commit SHAs and their parent SHAs for building the graph visualization.
    fn initial_graph_data(
        &self,
        log_source: LogSource,
        log_order: LogOrder,
        request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn search_commits(
        &self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>>;

    fn file_history_changed_files(
        &self,
        paths: Vec<RepoPath>,
        commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>>;

    fn commit_data_reader(&self) -> Result<CommitDataReader>;

    fn create_ref(&self, ref_name: String, commit: String) -> BoxFuture<'_, Result<()>>;
    fn update_ref(&self, ref_name: String, commit: String) -> BoxFuture<'_, Result<()>>;

    fn delete_ref(&self, ref_name: String) -> BoxFuture<'_, Result<()>>;

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>>;

    fn set_trusted(&self, trusted: bool);
    fn is_trusted(&self) -> bool;
}

pub enum DiffType {
    HeadToIndex,
    HeadToWorktree,
    MergeBase { base_ref: SharedString },
}

#[derive(Clone, Copy)]
pub enum DiffStatType {
    HeadToIndex,
    HeadToWorktree,
    IndexToWorktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum PushOptions {
    SetUpstream,
    Force,
}

impl std::fmt::Debug for dyn GitRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("dyn GitRepository<...>").finish()
    }
}

pub struct RealGitRepository {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    /// `None` only for bare repositories, which do not have a working directory.
    pub working_directory: Option<PathBuf>,
    pub system_git_binary_path: Option<PathBuf>,
    pub any_git_binary_path: PathBuf,
    any_git_binary_help_output: Arc<Mutex<Option<SharedString>>>,
    executor: BackgroundExecutor,
    is_trusted: Arc<AtomicBool>,
}

#[derive(Debug)]
pub enum RefEdit {
    Create { ref_name: String, commit: String },
    Update { ref_name: String, commit: String },
    Delete { ref_name: String },
}

impl RefEdit {
    fn into_args(self) -> Vec<OsString> {
        match self {
            Self::Create { ref_name, commit } => {
                vec![
                    "update-ref".into(),
                    ref_name.into(),
                    commit.into(),
                    "".into(),
                ]
            }
            Self::Update { ref_name, commit } => {
                vec!["update-ref".into(), ref_name.into(), commit.into()]
            }
            Self::Delete { ref_name } => {
                vec!["update-ref".into(), "-d".into(), ref_name.into()]
            }
        }
    }
}

impl RealGitRepository {
    pub fn new(
        dotgit_path: &Path,
        bundled_git_binary_path: Option<PathBuf>,
        system_git_binary_path: Option<PathBuf>,
        executor: BackgroundExecutor,
    ) -> Result<Self> {
        let any_git_binary_path = system_git_binary_path
            .clone()
            .or(bundled_git_binary_path)
            .context("no git binary available")?;
        log::info!(
            "opening git repository at {dotgit_path:?} using git binary {any_git_binary_path:?}"
        );
        let dotgit_parent = dotgit_path.parent().context(".git has no parent")?;
        let has_working_directory =
            dotgit_path.is_file() || dotgit_path.file_name() == Some(OsStr::new(".git"));
        let working_directory = if has_working_directory {
            Some(normalize_git_metadata_path(dotgit_parent.to_path_buf())?)
        } else {
            None
        };

        let git_dir = if dotgit_path.is_file() {
            let content =
                std::fs::read_to_string(dotgit_path).context("reading .git worktree file")?;
            let path_str = content
                .strip_prefix("gitdir: ")
                .context("expected .git file to start with 'gitdir: '")?
                .trim();
            let resolved = PathBuf::from(path_str);
            let resolved = if resolved.is_absolute() {
                resolved
            } else {
                dotgit_parent.join(resolved)
            };
            normalize_git_metadata_path(resolved)?
        } else {
            normalize_git_metadata_path(dotgit_path.to_path_buf())?
        };

        let common_dir = {
            let commondir_file = git_dir.join("commondir");
            if commondir_file.is_file() {
                let content =
                    std::fs::read_to_string(&commondir_file).context("reading commondir file")?;
                let path_str = content.trim();
                let resolved = PathBuf::from(path_str);
                let resolved = if resolved.is_absolute() {
                    resolved
                } else {
                    git_dir.join(resolved)
                };
                normalize_git_metadata_path(resolved)?
            } else {
                git_dir.clone()
            }
        };

        Ok(Self {
            git_dir,
            common_dir,
            working_directory,
            system_git_binary_path,
            any_git_binary_path,
            executor,
            any_git_binary_help_output: Arc::new(Mutex::new(None)),
            is_trusted: Arc::new(AtomicBool::new(false)),
        })
    }

    fn working_directory(&self) -> Result<PathBuf> {
        self.working_directory
            .clone()
            .context("bare repositories do not have a working directory")
    }

    fn command_directory(&self) -> PathBuf {
        self.working_directory
            .clone()
            .unwrap_or_else(|| self.git_dir.clone())
    }

    fn git_binary_in_worktree(&self) -> Result<GitBinary> {
        Ok(GitBinary::new(
            self.any_git_binary_path.clone(),
            self.working_directory()?,
            self.path(),
            self.executor.clone(),
            self.is_trusted(),
        ))
    }

    fn git_binary(&self) -> GitBinary {
        GitBinary::new(
            self.any_git_binary_path.clone(),
            self.command_directory(),
            self.path(),
            self.executor.clone(),
            self.is_trusted(),
        )
    }

    fn edit_ref(&self, edit: RefEdit) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary();
        self.executor
            .spawn(async move {
                let git = git_binary;
                let args = edit.into_args();
                git.run(&args).await?;
                Ok(())
            })
            .boxed()
    }

    async fn any_git_binary_help_output(&self) -> SharedString {
        if let Some(output) = self.any_git_binary_help_output.lock().clone() {
            return output;
        }
        let git = self.git_binary();
        let output: SharedString = self
            .executor
            .spawn(async move { git.run(&["help", "-a"]).await })
            .await
            .unwrap_or_default()
            .into();
        *self.any_git_binary_help_output.lock() = Some(output.clone());
        output
    }
}

#[derive(Clone, Debug)]
pub struct GitRepositoryCheckpoint {
    pub commit_sha: Oid,
}

#[derive(Debug)]
pub struct GitCommitter {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GitCommitTemplate {
    pub template: String,
}

pub async fn get_git_committer(cx: &AsyncApp) -> GitCommitter {
    if cfg!(any(feature = "test-support", test)) {
        return GitCommitter {
            name: None,
            email: None,
        };
    }

    let git_binary_path =
        if cfg!(target_os = "macos") && option_env!("ZED_BUNDLE").as_deref() == Some("true") {
            cx.update(|cx| {
                cx.path_for_auxiliary_executable("git")
                    .context("could not find git binary path")
                    .log_err()
            })
        } else {
            None
        };

    let git = GitBinary::new(
        git_binary_path.unwrap_or(PathBuf::from("git")),
        paths::home_dir().clone(),
        paths::home_dir().join(".git"),
        cx.background_executor().clone(),
        true,
    );

    cx.background_spawn(async move {
        let name = git
            .run(&["config", "--global", "user.name"])
            .await
            .log_err();
        let email = git
            .run(&["config", "--global", "user.email"])
            .await
            .log_err();
        GitCommitter { name, email }
    })
    .await
}

fn parse_remote_urls(stdout: &str) -> HashMap<String, String> {
    let mut urls = HashMap::default();
    for line in stdout.lines() {
        if let Some((line, suffix)) = line.rsplit_once(" (fetch)")
            && (suffix.is_empty() || suffix.starts_with(" [") && suffix.ends_with(']'))
            && let Some((name, url)) = line.split_once(char::is_whitespace)
        {
            urls.insert(name.to_string(), url.trim_start().to_string());
        }
    }
    urls
}

impl GitRepository for RealGitRepository {
    fn review_blob_sizes(&self, objects: Vec<Oid>) -> BoxFuture<'_, Result<Vec<u64>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                anyhow::ensure!(objects.len() <= 128, "Too many review size probes");
                let mut sizes = Vec::with_capacity(objects.len());
                for object in objects {
                    anyhow::ensure!(!object.is_zero(), "Invalid review object");
                    let size = git.run(&["cat-file", "-s", &object.to_string()]).await?;
                    sizes.push(size.parse().context("Invalid Git object size")?);
                }
                Ok(sizes)
            })
            .boxed()
    }

    fn review_history(
        &self,
        base: Option<Oid>,
        head: Oid,
    ) -> BoxFuture<'_, Result<Vec<crate::review::ReviewCommit>>> {
        self.executor
            .spawn(crate::review::history(self.git_binary(), base, head))
            .boxed()
    }

    fn review_commits(
        &self,
        selections: Vec<crate::review::ReviewCommitSelection>,
        range: bool,
    ) -> BoxFuture<'_, Result<Vec<crate::review::ReviewComparison>>> {
        self.executor
            .spawn(crate::review::commits(self.git_binary(), selections, range))
            .boxed()
    }

    fn review_comparison(
        &self,
        base: String,
        head: String,
        merge_base: bool,
    ) -> BoxFuture<'_, Result<crate::review::ReviewComparison>> {
        self.executor
            .spawn(crate::review::load(
                self.git_binary(),
                base,
                head,
                merge_base,
            ))
            .boxed()
    }

    fn path(&self) -> PathBuf {
        self.git_dir.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.common_dir.clone()
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git
                    .build_command(&[
                        "show",
                        "--no-patch",
                        "--format=%H%x00%B%x00%at%x00%ae%x00%an%x00",
                        &commit,
                        // `commit` reaches here as whatever the user typed, so it can name
                        // a branch that also names a path in the working tree.
                        "--",
                    ])
                    .output()
                    .await?;
                let output = std::str::from_utf8(&output.stdout)?;
                let fields = output.split('\0').collect::<Vec<_>>();
                if fields.len() != 6 {
                    bail!("unexpected git-show output for {commit:?}: {output:?}")
                }
                let sha = fields[0].to_string().into();
                let message = fields[1].to_string().into();
                let commit_timestamp = fields[2].parse()?;
                let author_email = fields[3].to_string().into();
                let author_name = fields[4].to_string().into();
                Ok(CommitDetails {
                    sha,
                    message,
                    commit_timestamp,
                    author_email,
                    author_name,
                })
            })
            .boxed()
    }

    fn load_commit(
        &self,
        commit: String,
        ignore_shallow_boundary: bool,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<CommitDiff>> {
        let git = self.git_binary();
        let shallow_file_path = self.common_dir.join("shallow");
        cx.background_spawn(async move {
            if !ignore_shallow_boundary
                && is_shallow_boundary_commit(&git, &shallow_file_path, &commit).await?
            {
                return Ok(CommitDiff {
                    files: Vec::new(),
                    is_shallow_boundary: true,
                });
            }

            let show_output = git
                .build_command(&[
                    "show",
                    "--format=",
                    "-z",
                    "--no-renames",
                    "--raw",
                    "--no-abbrev",
                    "--first-parent",
                ])
                .arg(&commit)
                .arg("--")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .context("starting git show process")?;
            anyhow::ensure!(
                show_output.status.success(),
                "git show failed: {}",
                String::from_utf8_lossy(&show_output.stderr)
            );

            let show_stdout = String::from_utf8_lossy(&show_output.stdout);
            let changes = parse_git_diff_raw(&show_stdout);

            let mut cat_file_process = git
                .build_command(&["cat-file", "--batch=%(objectsize)"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("starting git cat-file process")?;

            let mut files = Vec::<CommitFile>::new();
            let stdin = cat_file_process
                .stdin
                .take()
                .context("git cat-file process has no stdin")?;
            let stdout = cat_file_process
                .stdout
                .take()
                .context("git cat-file process has no stdout")?;
            let mut stdin = BufWriter::with_capacity(512, stdin);
            let mut stdout = BufReader::new(stdout);
            let mut info_line = String::new();
            let mut newline = [b'\0'];
            for change in changes {
                let change = change?;
                let path = change.path;
                // git-show outputs `/`-delimited paths even on Windows.
                let Some(rel_path) = RelPath::from_unix_str(path).log_err() else {
                    continue;
                };

                let objects = [change.new_object, change.old_object];
                let mut has_blobs = false;
                for object in objects.iter().flatten() {
                    if object.kind == CommitDiffObjectKind::Blob {
                        stdin.write_all(object.oid.as_bytes()).await?;
                        stdin.write_all(b"\n").await?;
                        has_blobs = true;
                    }
                }
                if has_blobs {
                    stdin.flush().await?;
                }

                let [new_object, old_object] = objects;
                let new_object =
                    load_commit_object(new_object, &mut stdout, &mut info_line, &mut newline)
                        .await?;
                let old_object =
                    load_commit_object(old_object, &mut stdout, &mut info_line, &mut newline)
                        .await?;
                let is_binary = new_object.as_ref().is_some_and(|object| object.is_binary)
                    || old_object.as_ref().is_some_and(|object| object.is_binary);
                let new_content = new_object.map(|object| object.content);
                let old_content = old_object.map(|object| object.content);

                files.push(CommitFile {
                    path: RepoPath(Arc::from(rel_path)),
                    old_content,
                    new_content,
                    is_binary,
                })
            }

            Ok(CommitDiff {
                files,
                is_shallow_boundary: false,
            })
        })
        .boxed()
    }

    fn reset(
        &self,
        commit: String,
        mode: ResetMode,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        async move {
            let git = git?;
            let mode_flag = match mode {
                ResetMode::Mixed => "--mixed",
                ResetMode::Soft => "--soft",
            };

            let output = git
                .build_command(&["reset", mode_flag, &commit])
                .envs(env.iter())
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success(),
                "Failed to reset:\n{}",
                String::from_utf8_lossy(&output.stderr),
            );
            Ok(())
        }
        .boxed()
    }

    fn checkout_files(
        &self,
        commit: Option<String>,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        async move {
            let git = git?;
            if paths.is_empty() {
                return Ok(());
            }

            let mut args = vec!["checkout"];
            args.extend(commit.as_deref());
            args.push("--");

            let output = git
                .build_command(&args)
                .envs(env.iter())
                .args(paths.iter().map(|path| path.as_unix_str()))
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success(),
                "Failed to checkout files:\n{}",
                String::from_utf8_lossy(&output.stderr),
            );
            Ok(())
        }
        .boxed()
    }

    fn load_blob_content(&self, oid: Oid) -> BoxFuture<'_, Result<Vec<u8>>> {
        let git_binary = self.git_binary();
        let oid_str = oid.to_string();
        self.executor
            .spawn(async move {
                let mut command = git_binary.build_command(&["cat-file", "blob", &oid_str]);
                let output = command.output().await?;
                anyhow::ensure!(
                    output.status.success(),
                    GitBinaryCommandError {
                        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                        status: output.status,
                    }
                );
                Ok(output.stdout)
            })
            .boxed()
    }

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>> {
        let working_directory = self.working_directory();
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let working_directory = working_directory?;
                let git_binary = git_binary?;
                let output = git_binary
                    .build_command(&["config", "--get", "commit.template"])
                    .output()
                    .await
                    .context("failed to run git config --get commit.template")?;

                let raw_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !output.status.success() || raw_path.is_empty() {
                    return Ok(None);
                }

                let path = PathBuf::from(&raw_path);
                let path = if let Some(path) = raw_path.strip_prefix("~/") {
                    paths::home_dir().join(path)
                } else if path.is_relative() {
                    working_directory.join(path)
                } else {
                    path
                };

                let template = match std::fs::read_to_string(&path) {
                    Ok(s) if !s.trim().is_empty() => Some(s),
                    Err(err) => {
                        log::warn!("failed to read commit template {}: {}", path.display(), err);
                        None
                    }
                    _ => None,
                };

                Ok(template.map(|template| GitCommitTemplate { template }))
            })
            .boxed()
    }

    fn set_index_text(
        &self,
        path: RepoPath,
        content: Option<Vec<u8>>,
        env: Arc<HashMap<String, String>>,
        is_executable: bool,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let mode = if is_executable { "100755" } else { "100644" };

                if let Some(content) = content {
                    let mut child = git
                        .build_command(&[
                            "hash-object",
                            "-w",
                            "--stdin",
                            "--path",
                            path.as_unix_str(),
                        ])
                        .envs(env.iter())
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()?;
                    let mut stdin = child.stdin.take().context("hash-object has no stdin")?;
                    stdin.write_all(&content).await?;
                    stdin.flush().await?;
                    drop(stdin);
                    let output = child.output().await?;
                    anyhow::ensure!(
                        output.status.success(),
                        GitBinaryCommandError {
                            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                            status: output.status,
                        }
                    );
                    let sha = str::from_utf8(&output.stdout)?.trim();

                    log::debug!("indexing SHA: {sha}, path {path:?}");

                    let output = git
                        .build_command(&["update-index", "--add", "--cacheinfo", mode, sha])
                        .envs(env.iter())
                        .arg(path.as_unix_str())
                        .output()
                        .await?;

                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to stage:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                } else {
                    log::debug!("removing path {path:?} from the index");
                    let output = git
                        .build_command(&["update-index", "--force-remove", "--"])
                        .envs(env.iter())
                        .arg(path.as_unix_str())
                        .output()
                        .await?;
                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to unstage:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }

                Ok(())
            })
            .boxed()
    }

    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                if let Ok(stdout) = git.run(&["remote", "-v"]).await {
                    parse_remote_urls(&stdout)
                } else {
                    HashMap::default()
                }
            })
            .boxed()
    }

    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let mut process = git
                    .build_command(&["cat-file", "--batch-check=%(objectname)"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;

                let stdin = process
                    .stdin
                    .take()
                    .context("no stdin for git cat-file subprocess")?;
                let mut stdin = BufWriter::new(stdin);
                for rev in &revs {
                    stdin.write_all(rev.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                }
                stdin.flush().await?;
                drop(stdin);

                let output = process.output().await?;
                let output = std::str::from_utf8(&output.stdout)?;
                let shas = output
                    .lines()
                    .map(|line| {
                        if line.ends_with("missing") {
                            None
                        } else {
                            Some(line.to_string())
                        }
                    })
                    .collect::<Vec<_>>();

                if shas.len() != revs.len() {
                    // In an octopus merge, git cat-file still only outputs the first sha from MERGE_HEAD.
                    bail!("unexpected number of shas")
                }

                Ok(shas)
            })
            .boxed()
    }

    fn load_revisions(
        &self,
        revisions: Vec<String>,
    ) -> BoxFuture<'_, Result<Vec<Option<Vec<u8>>>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                if revisions.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(revision) = revisions.iter().find(|revision| revision.contains('\n')) {
                    anyhow::bail!(
                        "revision spec {revision:?} contains a newline and cannot be passed to git cat-file --batch"
                    );
                }

                let mut process = git
                    .build_command(&["cat-file", "--batch"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()?;

                let mut stdin = BufWriter::new(process.stdin.take().context("no stdin")?);
                let mut stdout = BufReader::new(process.stdout.take().context("no stdout")?);
                let mut newline = [0u8; 1];

                let mut header_bytes = Vec::new();
                let mut results = Vec::with_capacity(revisions.len());
                for rev in &revisions {
                    stdin.write_all(rev.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;

                    header_bytes.clear();
                    stdout.read_until(b'\n', &mut header_bytes).await?;
                    let header_line = String::from_utf8_lossy(&header_bytes);

                    let parts: Vec<&str> = header_line.trim().split(' ').collect();
                    match parts[..] {
                        [.., "missing"] => {
                            results.push(None);
                        }
                        [_, object_type, size_str] => {
                            let size: usize = size_str
                                .parse()
                                .with_context(|| format!("invalid object size: {size_str}"))?;

                            let mut content = vec![0u8; size];
                            stdout.read_exact(&mut content).await?;
                            stdout.read_exact(&mut newline).await?;

                            if object_type == "blob" {
                                results.push(Some(content));
                            } else {
                                results.push(None);
                            }
                        }
                        _ => bail!("invalid cat-file header: {header_line}"),
                    }
                }

                drop(stdin);
                process.output().await?;
                Ok(results)
            })
            .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        let path = self.path().join("MERGE_MSG");
        self.executor
            .spawn(async move { std::fs::read_to_string(&path).ok() })
            .boxed()
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        let git = self.git_binary_in_worktree();
        let args = git_status_args(path_prefixes);
        log::debug!("Checking for git status in {path_prefixes:?}");
        self.executor.spawn(async move {
            let git = git?;
            let output = git.build_command(&args).output().await?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.parse()
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("git status failed: {stderr}");
            }
        })
    }

    fn check_access(&self) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                git?.run(&["rev-parse"]).await?;
                Ok(())
            })
            .boxed()
    }

    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        let git = self.git_binary_in_worktree();
        let working_directory = self.working_directory.clone();
        let merge_base_ref = match &request {
            DiffTreeType::MergeBaseWithWorktree { base } => Some(base.clone()),
            DiffTreeType::MergeBase { .. } | DiffTreeType::Since { .. } => None,
        };

        let args = match request {
            DiffTreeType::MergeBase { base, head } => [
                "diff-tree",
                "-r",
                "-z",
                "--abbrev=64",
                "--no-renames",
                "--merge-base",
                base.as_str(),
                head.as_str(),
                "--",
            ]
            .map(OsString::from)
            .to_vec(),
            DiffTreeType::MergeBaseWithWorktree { base } => [
                "diff",
                "--raw",
                "-z",
                "--abbrev=64",
                "--no-renames",
                "--merge-base",
                base.as_str(),
                "--",
            ]
            .map(OsString::from)
            .to_vec(),
            DiffTreeType::Since { base, head } => [
                "diff-tree",
                "-r",
                "-z",
                "--abbrev=64",
                "--no-renames",
                base.as_str(),
                head.as_str(),
                "--",
            ]
            .map(OsString::from)
            .to_vec(),
        };

        self.executor
            .spawn(async move {
                let git = git?;
                let output = git.build_command(&args).output().await?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git diff-tree failed: {stderr}");
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut tree_diff = stdout.parse::<TreeDiff>()?;
                let Some(merge_base_ref) = merge_base_ref else {
                    return Ok(tree_diff);
                };
                let Some(working_directory) = working_directory else {
                    return Ok(tree_diff);
                };
                if !tree_diff
                    .entries
                    .values()
                    .any(|status| matches!(status, TreeDiffStatus::Deleted { .. }))
                {
                    return Ok(tree_diff);
                }

                let status_output = git.build_command(&git_status_args(&[])).output().await?;
                if !status_output.status.success() {
                    let stderr = String::from_utf8_lossy(&status_output.stderr);
                    anyhow::bail!("git status failed: {stderr}");
                }
                let status = String::from_utf8_lossy(&status_output.stdout).parse::<GitStatus>()?;
                // Files the diff reports as deleted but that exist on disk
                // (deleted from the index or from a commit, then recreated).
                // `git diff` compares them against the index, so compare their
                // disk contents against the merge base ourselves.
                let recreated: Vec<(RepoPath, Oid)> = status
                    .entries
                    .iter()
                    .filter(|(_, status)| {
                        matches!(
                            *status,
                            FileStatus::Untracked
                                | FileStatus::Tracked(TrackedStatus {
                                    index_status: StatusCode::Deleted,
                                    worktree_status: StatusCode::Added,
                                })
                        )
                    })
                    .filter_map(|(path, _)| match tree_diff.entries.get(path) {
                        Some(TreeDiffStatus::Deleted { old }) => Some((path.clone(), *old)),
                        _ => None,
                    })
                    .collect();
                if recreated.is_empty() {
                    return Ok(tree_diff);
                }

                let merge_base_output = git
                    .build_command(&["merge-base", merge_base_ref.as_ref(), "HEAD"])
                    .output()
                    .await?;
                if !merge_base_output.status.success() {
                    let stderr = String::from_utf8_lossy(&merge_base_output.stderr);
                    anyhow::bail!("git merge-base failed: {stderr}");
                }
                let merge_base = String::from_utf8_lossy(&merge_base_output.stdout);
                let merge_base = merge_base.trim();

                for (path, old) in recreated {
                    let full_path = working_directory.join(path.as_std_path());
                    let metadata = match smol::fs::symlink_metadata(&full_path).await {
                        Ok(metadata) => metadata,
                        Err(_) => continue,
                    };
                    let base_entry = git
                        .build_command(
                            &["ls-tree", merge_base, "--", path.as_unix_str()].map(OsString::from),
                        )
                        .output()
                        .await?;
                    if !base_entry.status.success() {
                        continue;
                    }
                    let base_mode = String::from_utf8_lossy(&base_entry.stdout);
                    let Some(base_mode) = base_mode.split_ascii_whitespace().next() else {
                        continue;
                    };
                    let current_mode = if metadata.file_type().is_symlink() {
                        "120000"
                    } else {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt as _;
                            if metadata.permissions().mode() & 0o111 == 0 {
                                "100644"
                            } else {
                                "100755"
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            "100644"
                        }
                    };
                    if current_mode != base_mode {
                        tree_diff
                            .entries
                            .insert(path.clone(), TreeDiffStatus::Modified { old });
                        continue;
                    }

                    let hash_output = if metadata.file_type().is_symlink() {
                        let target = smol::fs::read_link(&full_path).await?;
                        let mut child = git
                            .build_command(&["hash-object", "--stdin"])
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .spawn()?;
                        let mut stdin = child.stdin.take().context("hash-object has no stdin")?;
                        stdin
                            .write_all(target.as_os_str().as_encoded_bytes())
                            .await?;
                        stdin.flush().await?;
                        drop(stdin);
                        child.output().await?
                    } else {
                        git.build_command(&[
                            OsString::from("hash-object"),
                            OsString::from(format!("--path={}", path.as_unix_str())),
                            OsString::from("--"),
                            full_path.into_os_string(),
                        ])
                        .output()
                        .await?
                    };
                    if !hash_output.status.success() {
                        continue;
                    }
                    let worktree_oid = String::from_utf8_lossy(&hash_output.stdout);
                    if worktree_oid.trim() == old.to_string() {
                        tree_diff.entries.remove(&path);
                    } else {
                        tree_diff
                            .entries
                            .insert(path.clone(), TreeDiffStatus::Modified { old });
                    }
                }
                Ok(tree_diff)
            })
            .boxed()
    }

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let output = git
                    .build_command(&["stash", "list", "--pretty=format:%gd%x00%H%x00%ct%x00%s"])
                    .output()
                    .await?;
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    stdout.parse()
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git status failed: {stderr}");
                }
            })
            .boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let fields = [
                    "%(HEAD)",
                    "%(objectname)",
                    "%(parent)",
                    "%(refname)",
                    "%(upstream)",
                    "%(upstream:track)",
                    "%(committerdate:unix)",
                    "%(authorname)",
                    "%(contents:subject)",
                ]
                .join("%00");
                let args = vec![
                    "for-each-ref",
                    "refs/heads/**/*",
                    "refs/remotes/**/*",
                    "--format",
                    &fields,
                ];
                let output = git.build_command(&args).output().await?;

                let error = if output.status.success() {
                    None
                } else {
                    let error = format_branch_scan_error(&output);
                    log::warn!("failed to get git branches with commit metadata: {error}");
                    Some(error.into())
                };

                let input = String::from_utf8_lossy(&output.stdout);
                let mut branches = parse_branch_input(&input)?;
                if branches.is_empty() {
                    let args = vec!["symbolic-ref", "--quiet", "HEAD"];

                    let output = git.build_command(&args).output().await?;

                    // git symbolic-ref returns a non-0 exit code if HEAD points
                    // to something other than a branch
                    if output.status.success() {
                        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();

                        branches.push(Branch {
                            ref_name: name.into(),
                            is_head: true,
                            upstream: None,
                            most_recent_commit: None,
                        });
                    }
                }

                Ok(BranchesScanResult { branches, error })
            })
            .boxed()
    }

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>> {
        let git = self.git_binary();
        let main_worktree_path = original_repo_path_from_common_dir(&self.common_dir);
        self.executor
            .spawn(async move {
                let output = git
                    .build_command(&["worktree", "list", "--porcelain"])
                    .output()
                    .await?;
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    Ok(parse_worktrees_from_str(
                        &stdout,
                        main_worktree_path.as_deref(),
                    ))
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git worktree list failed: {stderr}");
                }
            })
            .boxed()
    }

    fn worktree_created_at(
        &self,
        worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>> {
        self.executor
            .spawn(async move {
                match std::fs::metadata(&worktree_path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(None);
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to stat {}", worktree_path.display())
                        });
                    }
                    Ok(_) => {}
                }
                let git_dir = linked_worktree_git_dir(&worktree_path)?;
                let metadata = std::fs::metadata(&git_dir)
                    .with_context(|| format!("failed to stat {}", git_dir.display()))?;
                let created_at = metadata.created().with_context(|| {
                    format!("creation time unavailable for {}", git_dir.display())
                })?;
                Ok(Some(created_at))
            })
            .boxed()
    }

    fn create_worktree(
        &self,
        target: CreateWorktreeTarget,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();
        let mut args = vec![OsString::from("worktree"), OsString::from("add")];

        match &target {
            CreateWorktreeTarget::ExistingBranch { branch_name } => {
                args.push(OsString::from("--"));
                args.push(OsString::from(path.as_os_str()));
                args.push(OsString::from(branch_name));
            }
            CreateWorktreeTarget::NewBranch {
                branch_name,
                base_sha: start_point,
            } => {
                args.push(OsString::from("-b"));
                args.push(OsString::from(branch_name));
                args.push(OsString::from("--"));
                args.push(OsString::from(path.as_os_str()));
                args.push(OsString::from(start_point.as_deref().unwrap_or("HEAD")));
            }
            CreateWorktreeTarget::Detached {
                base_sha: start_point,
            } => {
                args.push(OsString::from("--detach"));
                args.push(OsString::from("--"));
                args.push(OsString::from(path.as_os_str()));
                args.push(OsString::from(start_point.as_deref().unwrap_or("HEAD")));
            }
        }

        self.executor
            .spawn(async move {
                std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
                let output = git.build_command(&args).output().await?;
                if output.status.success() {
                    Ok(())
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git worktree add failed: {stderr}");
                }
            })
            .boxed()
    }

    fn remove_worktree(&self, path: PathBuf, force: bool) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        self.executor
            .spawn(async move {
                let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
                if force {
                    args.push("--force".into());
                }
                args.push("--".into());
                args.push(path.as_os_str().into());
                git.run(&args).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn rename_worktree(&self, old_path: PathBuf, new_path: PathBuf) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        self.executor
            .spawn(async move {
                let args: Vec<OsString> = vec![
                    "worktree".into(),
                    "move".into(),
                    "--".into(),
                    old_path.as_os_str().into(),
                    new_path.as_os_str().into(),
                ];
                git.run(&args).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn checkout_branch_in_worktree(
        &self,
        branch_name: String,
        worktree_path: PathBuf,
        create: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = GitBinary::new(
            self.any_git_binary_path.clone(),
            worktree_path,
            self.path(),
            self.executor.clone(),
            self.is_trusted(),
        );

        self.executor
            .spawn(async move {
                if create {
                    git_binary.run(&["checkout", "-b", &branch_name]).await?;
                } else {
                    git_binary.run(&["checkout", &branch_name]).await?;
                }
                anyhow::Ok(())
            })
            .boxed()
    }

    fn change_branch(&self, name: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                let local_ref = format!("refs/heads/{name}");
                if git_binary
                    .run(&["show-ref", "--verify", "--quiet", &local_ref])
                    .await
                    .is_ok()
                {
                    git_binary.run(&["checkout", &name]).await?;
                    return anyhow::Ok(());
                }

                let remote_ref = format!("refs/remotes/{name}");
                if git_binary
                    .run(&["show-ref", "--verify", "--quiet", &remote_ref])
                    .await
                    .is_ok()
                {
                    let name = match git_binary.run(&["symbolic-ref", &remote_ref]).await {
                        Ok(resolved) => resolved
                            .strip_prefix("refs/remotes/")
                            .map(str::to_owned)
                            .unwrap_or(name),
                        Err(_) => name,
                    };
                    let (_, branch_name) =
                        name.split_once('/').context("Unexpected branch format")?;
                    let local_branch_ref = format!("refs/heads/{branch_name}");
                    if git_binary
                        .run(&["show-ref", "--verify", "--quiet", &local_branch_ref])
                        .await
                        .is_ok()
                    {
                        git_binary
                            .run(&["branch", "--set-upstream-to", &name, branch_name])
                            .await?;
                    } else {
                        git_binary
                            .run(&["branch", "--track", branch_name, &name])
                            .await?;
                    }

                    git_binary.run(&["checkout", branch_name]).await?;
                    return anyhow::Ok(());
                }

                anyhow::bail!("Branch '{}' not found", name);
            })
            .boxed()
    }

    fn create_branch(
        &self,
        name: String,
        base_branch: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                let mut args = vec!["switch", "-c", &name];
                let base_branch_str;
                if let Some(ref base) = base_branch {
                    base_branch_str = base.clone();
                    args.push(&base_branch_str);
                }

                git_binary.run(&args).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn rename_branch(&self, branch: String, new_name: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                git_binary
                    .run(&["branch", "-m", &branch, &new_name])
                    .await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn delete_branch(
        &self,
        is_remote: bool,
        name: String,
        force: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                let flag = delete_branch_flag(is_remote, force);
                git_binary.run(&["branch", flag, &name]).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn blame(
        &self,
        path: RepoPath,
        content: Rope,
        line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>> {
        let git = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git = git?;
                crate::blame::Blame::for_path(&git, &path, &content, line_ending).await
            })
            .boxed()
    }

    fn blame_at_revision(
        &self,
        path: RepoPath,
        revision: Oid,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>> {
        let git = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git = git?;
                crate::blame::Blame::for_path_at_revision(&git, &path, revision).await
            })
            .boxed()
    }

    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let output = match diff {
                    DiffType::HeadToIndex => {
                        git.build_command(&["diff", "--staged"]).output().await?
                    }
                    DiffType::HeadToWorktree => git.build_command(&["diff"]).output().await?,
                    DiffType::MergeBase { base_ref } => {
                        git.build_command(&["diff", "--merge-base", base_ref.as_ref(), "--"])
                            .output()
                            .await?
                    }
                };

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to run git diff:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            })
            .boxed()
    }

    fn diff_stat(
        &self,
        diff: DiffStatType,
        path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<crate::status::GitDiffStat>> {
        let path_prefixes = path_prefixes.to_vec();
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                let mut args: Vec<String> =
                    vec!["diff".into(), "--numstat".into(), "--no-renames".into()];
                match diff {
                    DiffStatType::HeadToIndex => args.extend(["--cached".into(), "HEAD".into()]),
                    DiffStatType::HeadToWorktree => args.push("HEAD".into()),
                    DiffStatType::IndexToWorktree => {}
                }
                if !path_prefixes.is_empty() {
                    args.push("--".into());
                    args.extend(
                        path_prefixes
                            .iter()
                            .map(|p| p.as_std_path().to_string_lossy().into_owned()),
                    );
                }
                let output = git_binary.run(&args).await?;
                Ok(crate::status::parse_numstat(&output))
            })
            .boxed()
    }

    fn stage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                if !paths.is_empty() {
                    let output = git
                        .build_command(&["update-index", "--add", "--remove", "--"])
                        .envs(env.iter())
                        .args(paths.iter().map(|p| p.as_unix_str()))
                        .output()
                        .await?;
                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to stage paths:\n{}",
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
                Ok(())
            })
            .boxed()
    }

    fn unstage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git = git?;
                if !paths.is_empty() {
                    let output = git
                        .build_command(&["reset", "--quiet", "--"])
                        .envs(env.iter())
                        .args(paths.iter().map(|p| p.as_std_path()))
                        .output()
                        .await?;

                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to unstage:\n{}",
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
                Ok(())
            })
            .boxed()
    }

    fn stash_paths(
        &self,
        paths: Vec<RepoPath>,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let mut args = vec!["stash", "push", "--quiet", "--include-untracked"];
                if let Some(message) = message.as_deref() {
                    args.extend_from_slice(&["--message", message]);
                }
                args.push("--");
                let output = git
                    .build_command(&args)
                    .envs(env.iter())
                    .args(paths.iter().map(|p| p.as_unix_str()))
                    .output()
                    .await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_staged(
        &self,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                // `--staged` cannot be expressed as a pathspec: a partially staged
                // file would otherwise have its unstaged hunks stashed too.
                let mut args = vec!["stash", "push", "--quiet", "--staged"];
                if let Some(message) = message.as_deref() {
                    args.extend_from_slice(&["--message", message]);
                }
                let output = git.build_command(&args).envs(env.iter()).output().await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash staged changes (requires git 2.35 or newer):\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_pop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let mut args = vec!["stash".to_string(), "pop".to_string()];
                if let Some(index) = index {
                    args.push(format!("stash@{{{}}}", index));
                }
                let output = git.build_command(&args).envs(env.iter()).output().await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash pop:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_apply(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let mut args = vec!["stash".to_string(), "apply".to_string()];
                if let Some(index) = index {
                    args.push(format!("stash@{{{}}}", index));
                }
                let output = git.build_command(&args).envs(env.iter()).output().await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to apply stash:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_drop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let mut args = vec!["stash".to_string(), "drop".to_string()];
                if let Some(index) = index {
                    args.push(format!("stash@{{{}}}", index));
                }
                let output = git.build_command(&args).envs(env.iter()).output().await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash drop:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn commit(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        let executor = self.executor.clone();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git = git?;
            let mut cmd = git.build_command(&["commit", "--quiet", "-m"]);
            cmd.envs(env.iter())
                .arg(&message.to_string())
                .arg("--cleanup=strip")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            if options.amend {
                cmd.arg("--amend");
            }

            if options.signoff {
                cmd.arg("--signoff");
            }

            if options.allow_empty {
                cmd.arg("--allow-empty");
            }

            if options.no_verify {
                cmd.arg("--no-verify");
            }

            if let Some((name, email)) = name_and_email {
                cmd.arg("--author").arg(&format!("{name} <{email}>"));
            }

            run_git_command(env, ask_pass, cmd, executor).await?;

            Ok(())
        }
        .boxed()
    }

    fn create_ref(&self, ref_name: String, commit: String) -> BoxFuture<'_, Result<()>> {
        self.edit_ref(RefEdit::Create { ref_name, commit })
    }

    fn update_ref(&self, ref_name: String, commit: String) -> BoxFuture<'_, Result<()>> {
        self.edit_ref(RefEdit::Update { ref_name, commit })
    }

    fn delete_ref(&self, ref_name: String) -> BoxFuture<'_, Result<()>> {
        self.edit_ref(RefEdit::Delete { ref_name })
    }

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let args: Vec<OsString> = vec!["worktree".into(), "repair".into()];
                git.run(&args).await?;
                Ok(())
            })
            .boxed()
    }

    fn push(
        &self,
        branch_name: String,
        remote_branch_name: String,
        remote_name: String,
        options: Option<PushOptions>,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let working_directory = self.command_directory();
        let git_directory = self.path();
        let executor = cx.background_executor().clone();
        let git_binary_path = self.system_git_binary_path.clone();
        let is_trusted = self.is_trusted();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git_binary_path = git_binary_path.context("git not found on $PATH, can't push")?;
            let git = GitBinary::new(
                git_binary_path,
                working_directory,
                git_directory,
                executor.clone(),
                is_trusted,
            );
            let mut command = git.build_command(&["push"]);
            command
                .envs(env.iter())
                .args(options.map(|option| match option {
                    PushOptions::SetUpstream => "--set-upstream",
                    PushOptions::Force => "--force-with-lease",
                }))
                .arg(remote_name)
                .arg(format!("{}:{}", branch_name, remote_branch_name))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            run_git_command(env, ask_pass, command, executor).await
        }
        .boxed()
    }

    fn pull(
        &self,
        branch_name: Option<String>,
        remote_name: String,
        rebase: bool,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let working_directory = self.command_directory();
        let git_directory = self.path();
        let executor = cx.background_executor().clone();
        let git_binary_path = self.system_git_binary_path.clone();
        let is_trusted = self.is_trusted();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git_binary_path = git_binary_path.context("git not found on $PATH, can't pull")?;
            let git = GitBinary::new(
                git_binary_path,
                working_directory,
                git_directory,
                executor.clone(),
                is_trusted,
            );
            let mut command = git.build_command(&["pull"]);
            command.envs(env.iter());

            if rebase {
                command.arg("--rebase");
            }

            command
                .arg(remote_name)
                .args(branch_name)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            run_git_command(env, ask_pass, command, executor).await
        }
        .boxed()
    }

    fn fetch(
        &self,
        fetch_options: FetchOptions,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let working_directory = self.command_directory();
        let git_directory = self.path();
        let remote_name = format!("{}", fetch_options);
        let git_binary_path = self.system_git_binary_path.clone();
        let executor = cx.background_executor().clone();
        let is_trusted = self.is_trusted();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git_binary_path = git_binary_path.context("git not found on $PATH, can't fetch")?;
            let git = GitBinary::new(
                git_binary_path,
                working_directory,
                git_directory,
                executor.clone(),
                is_trusted,
            );
            let mut command = git.build_command(&["fetch", &remote_name]);
            command
                .envs(env.iter())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            run_git_command(env, ask_pass, command, executor).await
        }
        .boxed()
    }

    fn fetch_review_pr(
        &self,
        request: ReviewPrFetch,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<VerifiedReviewPr>> {
        let git = self.git_binary();
        let executor = cx.background_executor().clone();
        async move {
            request.validate()?;
            let source = format!("https://github.com/{}/{}.git", request.owner, request.repository);
            let (mut command, refs) = review_pr_fetch_command(&git, &request, &source, true).await?;
            command.envs(env.iter()).env_remove("GH_DEBUG").kill_on_drop(true);
            let mut attempt = ReviewPrAttempt::new(git.clone(), refs.clone());
            {
            let operation = run_git_command_inner(env, askpass, command, executor.clone(), Some(&mut attempt)).fuse();
            let timeout = executor.timer(std::time::Duration::from_secs(300)).fuse();
            futures::pin_mut!(operation, timeout);
            select_biased! {
                result = operation => { result.map_err(|_| anyhow!("Could not fetch PR objects; check GitHub access and connectivity"))?; }
                _ = timeout => bail!("PR object fetch timed out"),
            }
            }
            let verified = verify_review_pr_fetch(&git, &request, refs).await?;
            attempt.accepted = true;
            Ok(verified)
        }.boxed()
    }

    fn get_push_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git
                    .build_command(&["rev-parse", "--abbrev-ref"])
                    .arg(format!("{branch}@{{push}}"))
                    .output()
                    .await?;
                if !output.status.success() {
                    return Ok(None);
                }
                let remote_name = String::from_utf8_lossy(&output.stdout)
                    .split('/')
                    .next()
                    .map(|name| Remote {
                        name: name.trim().to_string().into(),
                    });

                Ok(remote_name)
            })
            .boxed()
    }

    fn get_branch_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git
                    .build_command(&["config", "--get"])
                    .arg(format!("branch.{branch}.remote"))
                    .output()
                    .await?;
                if !output.status.success() {
                    return Ok(None);
                }

                let remote_name = String::from_utf8_lossy(&output.stdout);
                return Ok(Some(Remote {
                    name: remote_name.trim().to_string().into(),
                }));
            })
            .boxed()
    }

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git.build_command(&["remote", "-v"]).output().await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to get all remotes:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let remote_names: HashSet<Remote> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .filter_map(|line| {
                        let mut split_line = line.split_whitespace();
                        let remote_name = split_line.next()?;

                        Some(Remote {
                            name: remote_name.trim().to_string().into(),
                        })
                    })
                    .collect();

                Ok(remote_names.into_iter().collect())
            })
            .boxed()
    }

    fn remove_remote(&self, name: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary();
        self.executor
            .spawn(async move {
                git_binary.run(&["remote", "remove", &name]).await?;
                Ok(())
            })
            .boxed()
    }

    fn create_remote(&self, name: String, url: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary();
        self.executor
            .spawn(async move {
                git_binary.run(&["remote", "add", &name, &url]).await?;
                Ok(())
            })
            .boxed()
    }

    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                // This command outputs a list of remote tracking refs, e.g.:
                // refs/remotes/origin/HEAD
                // refs/remotes/origin/main
                let Ok(output) = git?
                    .run(&[
                        "for-each-ref",
                        "--format=%(refname)",
                        "--contains",
                        "HEAD",
                        "refs/remotes/",
                    ])
                    .await
                else {
                    return Ok(Vec::new());
                };

                Ok(output
                    .lines()
                    .map(|line| line.trim())
                    .filter(|line| !line.ends_with("/HEAD"))
                    .filter_map(|line| line.strip_prefix("refs/remotes/"))
                    .map(SharedString::from)
                    .collect())
            })
            .boxed()
    }

    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let mut git = git?.envs(checkpoint_author_envs());
                git.with_temp_index(async |git| {
                    let head_sha = git.run(&["rev-parse", "HEAD"]).await.ok();

                    git.run(&["add", "--update"]).await?;
                    let untracked_files = untracked_files_for_checkpoint(git).await?;
                    add_files_to_index(git, &untracked_files).await?;
                    let tree = git.run(&["write-tree"]).await?;
                    let checkpoint_sha = if let Some(head_sha) = head_sha.as_deref() {
                        git.run(&["commit-tree", &tree, "-p", head_sha, "-m", "Checkpoint"])
                            .await?
                    } else {
                        git.run(&["commit-tree", &tree, "-m", "Checkpoint"]).await?
                    };

                    Ok(GitRepositoryCheckpoint {
                        commit_sha: checkpoint_sha.parse()?,
                    })
                })
                .await
            })
            .boxed()
    }

    fn restore_checkpoint(&self, checkpoint: GitRepositoryCheckpoint) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                git.run(&[
                    "restore",
                    "--source",
                    &checkpoint.commit_sha.to_string(),
                    "--worktree",
                    ".",
                ])
                .await?;

                // TODO: We don't track binary and large files anymore,
                //       so the following call would delete them.
                //       Implement an alternative way to track files added by agent.
                //
                // git.with_temp_index(async move |git| {
                //     git.run(&["read-tree", &checkpoint.commit_sha.to_string()])
                //         .await?;
                //     git.run(&["clean", "-d", "--force"]).await
                // })
                // .await?;

                Ok(())
            })
            .boxed()
    }

    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let mut git = git?.envs(checkpoint_author_envs());
                let head_sha = git
                    .run(&["rev-parse", "HEAD"])
                    .await
                    .context("failed to read HEAD")?;

                // Capture the staged state: write-tree reads the current index
                let staged_tree = git
                    .run(&["write-tree"])
                    .await
                    .context("failed to write staged tree")?;
                let staged_sha = git
                    .run(&[
                        "commit-tree",
                        &staged_tree,
                        "-p",
                        &head_sha,
                        "-m",
                        "WIP staged",
                    ])
                    .await
                    .context("failed to create staged commit")?;

                // Capture the full state (staged + unstaged + untracked) using
                // a temporary index so we don't disturb the real one.
                let unstaged_sha = git
                    .with_temp_index(async |git| {
                        git.run(&["add", "--all"]).await?;
                        let full_tree = git.run(&["write-tree"]).await?;
                        let sha = git
                            .run(&[
                                "commit-tree",
                                &full_tree,
                                "-p",
                                &staged_sha,
                                "-m",
                                "WIP unstaged",
                            ])
                            .await?;
                        Ok(sha)
                    })
                    .await
                    .context("failed to create unstaged commit")?;

                Ok((staged_sha, unstaged_sha))
            })
            .boxed()
    }

    fn restore_archive_checkpoint(
        &self,
        staged_sha: String,
        unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                // First, set the index AND working tree to match the unstaged
                // tree. --reset -u computes a tree-level diff between the
                // current index and unstaged_sha's tree and applies additions,
                // modifications, and deletions to the working directory.
                git.run(&["read-tree", "--reset", "-u", &unstaged_sha])
                    .await
                    .context("failed to restore working directory from unstaged commit")?;

                // Then replace just the index with the staged tree. Without -u
                // this doesn't touch the working directory, so the result is:
                // working tree = unstaged state, index = staged state.
                git.run(&["read-tree", &staged_sha])
                    .await
                    .context("failed to restore index from staged commit")?;

                Ok(())
            })
            .boxed()
    }

    fn compare_checkpoints(
        &self,
        left: GitRepositoryCheckpoint,
        right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let result = git
                    .run(&[
                        "diff-tree",
                        "--quiet",
                        &left.commit_sha.to_string(),
                        &right.commit_sha.to_string(),
                    ])
                    .await;
                match result {
                    Ok(_) => Ok(true),
                    Err(error) => {
                        if let Some(GitBinaryCommandError { status, .. }) =
                            error.downcast_ref::<GitBinaryCommandError>()
                            && status.code() == Some(1)
                        {
                            return Ok(false);
                        }

                        Err(error)
                    }
                }
            })
            .boxed()
    }

    fn diff_checkpoints(
        &self,
        base_checkpoint: GitRepositoryCheckpoint,
        target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                git.run(&[
                    "diff",
                    "--find-renames",
                    "--patch",
                    &base_checkpoint.commit_sha.to_string(),
                    &target_checkpoint.commit_sha.to_string(),
                ])
                .await
            })
            .boxed()
    }

    fn default_branch(
        &self,
        include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git
                    .run(&[
                        "for-each-ref",
                        "--format=%(refname)\t%(symref)",
                        "refs/remotes/upstream/HEAD",
                        "refs/remotes/origin/HEAD",
                        "refs/heads/",
                    ])
                    .await
                    .unwrap_or_default();
                let refs: HashMap<&str, &str> = output
                    .lines()
                    .filter_map(|line| line.split_once('\t'))
                    .collect();

                if let Some(target) = refs.get("refs/remotes/upstream/HEAD") {
                    let strip_prefix = if include_remote_name {
                        "refs/remotes/"
                    } else {
                        "refs/remotes/upstream/"
                    };
                    if let Some(branch) = target.strip_prefix(strip_prefix) {
                        return Ok(Some(branch.into()));
                    }
                }

                if let Some(target) = refs.get("refs/remotes/origin/HEAD") {
                    let strip_prefix = if include_remote_name {
                        "refs/remotes/"
                    } else {
                        "refs/remotes/origin/"
                    };
                    if let Some(branch) = target.strip_prefix(strip_prefix) {
                        return Ok(Some(branch.into()));
                    }
                }

                let local_branch_exists =
                    |branch: &str| refs.contains_key(format!("refs/heads/{branch}").as_str());

                if let Ok(default_branch) = git.run(&["config", "init.defaultBranch"]).await {
                    if local_branch_exists(&default_branch) {
                        return Ok(Some(default_branch.into()));
                    }
                }

                if local_branch_exists("main") {
                    return Ok(Some("main".into()));
                }

                if local_branch_exists("master") {
                    return Ok(Some("master".into()));
                }

                Ok(None)
            })
            .boxed()
    }

    fn run_hook(
        &self,
        hook: RunHook,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();
        let git_dir = self.git_dir.clone();
        let help_output = self.any_git_binary_help_output();

        // Note: Do not spawn these commands on the background thread, as this causes some git hooks to hang.
        async move {
            let git_binary = git_binary?;
            let working_directory = git_binary.working_directory.clone();
            if !help_output
                .await
                .lines()
                .any(|line| line.trim().starts_with("hook "))
            {
                let hook_abs_path = git_dir.join("hooks").join(hook.as_str());
                if hook_abs_path.is_file() && git_binary.is_trusted {
                    #[allow(clippy::disallowed_methods)]
                    let output = new_command(&hook_abs_path)
                        .envs(env.iter())
                        .current_dir(&working_directory)
                        .output()
                        .await?;

                    if !output.status.success() {
                        return Err(GitBinaryCommandError {
                            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                            status: output.status,
                        }
                        .into());
                    }
                }

                return Ok(());
            }

            if git_binary.is_trusted {
                let git_binary = git_binary.envs(HashMap::clone(&env));
                git_binary
                    .run(&["hook", "run", "--ignore-missing", hook.as_str()])
                    .await?;
            }
            Ok(())
        }
        .boxed()
    }

    fn initial_graph_data(
        &self,
        log_source: LogSource,
        log_order: LogOrder,
        request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        async move {
            let log_source_args = log_source.get_args();
            let mut git_log_command = vec!["log", GRAPH_COMMIT_FORMAT, log_order.as_arg()];
            git_log_command.extend(log_source_args.iter().map(|arg| arg.as_ref()));
            let mut command = git.build_command(&git_log_command);
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());

            let mut child = command.spawn()?;
            let stdout = child.stdout.take().context("failed to get stdout")?;
            let stderr = child.stderr.take().context("failed to get stderr")?;
            let mut reader = BufReader::new(stdout);

            let mut line_buffer = String::new();
            let mut lines: Vec<String> = Vec::with_capacity(GRAPH_CHUNK_SIZE);

            loop {
                line_buffer.clear();
                let bytes_read = reader.read_line(&mut line_buffer).await?;

                if bytes_read == 0 {
                    if !lines.is_empty() {
                        let commits = parse_initial_graph_output(lines.iter().map(|s| s.as_str()));
                        if request_tx.send(commits).await.is_err() {
                            log::warn!(
                                "initial_graph_data: receiver dropped while sending commits"
                            );
                        }
                    }
                    break;
                }

                let line = line_buffer.trim_end_matches('\n').to_string();
                lines.push(line);

                if lines.len() >= GRAPH_CHUNK_SIZE {
                    let commits = parse_initial_graph_output(lines.iter().map(|s| s.as_str()));
                    if request_tx.send(commits).await.is_err() {
                        log::warn!("initial_graph_data: receiver dropped while streaming commits");
                        break;
                    }
                    lines.clear();
                }
            }

            let status = child.status().await?;
            if !status.success() {
                let mut stderr_output = String::new();
                BufReader::new(stderr)
                    .read_to_string(&mut stderr_output)
                    .await
                    .log_err();

                if stderr_output.is_empty() {
                    anyhow::bail!("git log command failed with {}", status);
                } else {
                    anyhow::bail!("git log command failed with {}: {}", status, stderr_output);
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn search_commits(
        &self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        async move {
            let log_source_args = log_source.get_args();
            let mut args = vec!["log", SEARCH_COMMIT_FORMAT];
            let hash_query = commit_hash_search_query(search_args.query.as_str())
                .map(|query| query.to_ascii_lowercase());

            if hash_query.is_none() {
                args.push("--fixed-strings");

                if !search_args.case_sensitive {
                    args.push("--regexp-ignore-case");
                }

                args.push("--grep");
                args.push(search_args.query.as_str());
            }

            args.extend(log_source_args.iter().map(|arg| arg.as_ref()));
            let mut command = git.build_command(&args);
            command.stdout(Stdio::piped());
            command.stderr(Stdio::null());

            let mut child = command.spawn()?;
            let stdout = child.stdout.take().context("failed to get stdout")?;
            let mut reader = BufReader::new(stdout);

            let mut line_buffer = String::new();

            loop {
                line_buffer.clear();
                let bytes_read = reader.read_line(&mut line_buffer).await?;

                if bytes_read == 0 {
                    break;
                }

                let sha = line_buffer.trim_end_matches('\n');
                if let Some(hash_query) = hash_query.as_ref()
                    && !sha.to_ascii_lowercase().starts_with(hash_query)
                {
                    continue;
                }

                if let Ok(oid) = Oid::from_str(sha)
                    && request_tx.send(oid).await.is_err()
                {
                    break;
                }
            }

            child.status().await?;
            Ok(())
        }
        .boxed()
    }

    fn file_history_changed_files(
        &self,
        paths: Vec<RepoPath>,
        commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>> {
        let git = self.git_binary();
        let shallow_file_path = self.common_dir.join("shallow");

        async move {
            if paths.is_empty() {
                return Ok(Vec::new());
            }

            if commit_limit == 0 {
                return Ok(vec![FileHistoryChangedFileSets::default(); paths.len()]);
            }

            let max_count_arg = format!("--max-count={commit_limit}");
            let mut args = [
                "log",
                max_count_arg.as_str(),
                "--full-diff",
                "--no-renames",
                "--name-only",
                "-z",
                "--format=%x1e%H",
                "--",
            ]
            .map(OsString::from)
            .to_vec();
            args.extend(paths.iter().map(|path| OsString::from(path.as_unix_str())));

            let output = git.build_command(&args).output().await?;
            anyhow::ensure!(
                output.status.success(),
                "git log failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );

            let shallow_boundary_oids = read_shallow_file(&shallow_file_path)
                .await?
                .map(|contents| {
                    contents
                        .lines()
                        .map(|line| line.trim().to_string())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();

            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(parse_file_history_changed_files_output(
                &stdout,
                &paths,
                &shallow_boundary_oids,
            ))
        }
        .boxed()
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        let git_binary = self.git_binary();

        let (request_tx, request_rx) = async_channel::bounded::<CommitDataRequest>(64);

        let task = self.executor.spawn(async move {
            if let Err(error) = run_commit_data_reader(git_binary, request_rx).await {
                log::error!("commit data reader failed: {error:?}");
            }
        });

        Ok(CommitDataReader {
            request_tx,
            _task: task,
        })
    }

    fn set_trusted(&self, trusted: bool) {
        self.is_trusted
            .store(trusted, std::sync::atomic::Ordering::Release);
    }

    fn is_trusted(&self) -> bool {
        self.is_trusted.load(std::sync::atomic::Ordering::Acquire)
    }
}

async fn run_commit_data_reader(
    git: GitBinary,
    request_rx: async_channel::Receiver<CommitDataRequest>,
) -> Result<()> {
    let mut process = git
        .build_command(&["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting git cat-file --batch process")?;

    let mut stdin = BufWriter::new(process.stdin.take().context("no stdin")?);
    let mut stdout = BufReader::new(process.stdout.take().context("no stdout")?);

    const MAX_BATCH_SIZE: usize = 64;

    while let Ok(first_request) = request_rx.recv().await {
        let mut pending_requests = vec![first_request];

        while pending_requests.len() < MAX_BATCH_SIZE {
            match request_rx.try_recv() {
                Ok(request) => pending_requests.push(request),
                Err(_) => break,
            }
        }

        for request in &pending_requests {
            stdin.write_all(request.sha.to_string().as_bytes()).await?;
            stdin.write_all(b"\n").await?;
        }
        stdin.flush().await?;

        for request in pending_requests {
            let result = read_single_commit_response(&mut stdout, &request.sha).await;
            request.response_tx.send(result).ok();
        }
    }

    drop(stdin);
    process.kill().ok();

    Ok(())
}

async fn read_single_commit_response<R: smol::io::AsyncBufRead + Unpin>(
    stdout: &mut R,
    sha: &Oid,
) -> Result<CommitData> {
    let mut header_bytes = Vec::new();
    stdout.read_until(b'\n', &mut header_bytes).await?;
    let header_line = String::from_utf8_lossy(&header_bytes);

    let parts: Vec<&str> = header_line.trim().split(' ').collect();
    if parts.len() < 3 {
        bail!("invalid cat-file header: {header_line}");
    }

    let object_type = parts[1];
    if object_type == "missing" {
        bail!("object not found: {}", sha);
    }

    if object_type != "commit" {
        bail!("expected commit object, got {object_type}");
    }

    let size: usize = parts[2]
        .parse()
        .with_context(|| format!("invalid object size: {}", parts[2]))?;

    let mut content = vec![0u8; size];
    stdout.read_exact(&mut content).await?;

    let mut newline = [0u8; 1];
    stdout.read_exact(&mut newline).await?;

    let content_str = String::from_utf8_lossy(&content);
    parse_cat_file_commit(*sha, &content_str)
        .ok_or_else(|| anyhow!("failed to parse commit {}", sha))
}

fn parse_file_history_changed_files_output(
    output: &str,
    queried_paths: &[RepoPath],
    shallow_boundary_oids: &HashSet<String>,
) -> Vec<FileHistoryChangedFileSets> {
    let mut histories = vec![FileHistoryChangedFileSets::default(); queried_paths.len()];

    for record in output.split('\x1e') {
        let mut fields = record.split('\0');
        let sha = fields.next().unwrap_or_default().trim();
        if shallow_boundary_oids.contains(sha) {
            continue;
        }
        let changed_files = fields
            .filter_map(|field| {
                let path = field.trim_start_matches('\n');
                if path.is_empty() {
                    return None;
                }
                RepoPath::new(path).ok()
            })
            .collect::<std::collections::BTreeSet<_>>();

        if changed_files.is_empty() {
            continue;
        }

        let file_set = changed_files.iter().cloned().collect::<Vec<_>>();
        for (index, queried_path) in queried_paths.iter().enumerate() {
            if changed_files.contains(queried_path) {
                histories[index].file_sets.push(file_set.clone());
            }
        }
    }

    histories
}

fn parse_initial_graph_output<'a>(
    lines: impl Iterator<Item = &'a str>,
) -> Vec<Arc<InitialGraphCommitData>> {
    lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            // Format: "SHA\x00PARENT1 PARENT2...\x00REF1, REF2, ..."
            let mut parts = line.split('\x00');

            let sha = Oid::from_str(parts.next()?).ok()?;
            let parents_str = parts.next()?;
            let parents = parents_str
                .split_whitespace()
                .filter_map(|p| Oid::from_str(p).ok())
                .collect();

            let ref_names_str = parts.next().unwrap_or("");
            let ref_names = if ref_names_str.is_empty() {
                Vec::new()
            } else {
                ref_names_str
                    .split(", ")
                    .filter(|decoration| *decoration != "grafted" && *decoration != "replaced")
                    .map(|s| SharedString::from(s.to_string()))
                    .collect()
            };

            Some(Arc::new(InitialGraphCommitData {
                sha,
                parents,
                ref_names,
            }))
        })
        .collect()
}

fn git_status_args(path_prefixes: &[RepoPath]) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("status"),
        OsString::from("--porcelain=v1"),
        OsString::from("--untracked-files=all"),
        OsString::from("--no-renames"),
        OsString::from("-z"),
        OsString::from("--"),
    ];
    args.extend(path_prefixes.iter().map(|path_prefix| {
        if path_prefix.is_empty() {
            Path::new(".").into()
        } else {
            path_prefix.as_std_path().into()
        }
    }));
    args
}

/// Lists untracked files that should be included in a checkpoint, skipping
/// commonly ignored file types and files over 2MB.
async fn untracked_files_for_checkpoint(git: &GitBinary) -> Result<Vec<String>> {
    const MAX_SIZE: u64 = 2 * 1024 * 1024; // 2 MB

    // The extra checkpoint excludes are passed ad hoc via --exclude-from
    // rather than by mutating .git/info/exclude, whose writes would trigger a
    // rescan of the repository. The scratch file is placed directly in the
    // .git directory with a .tmp extension so that the worktree scanner
    // filters out the events it generates.
    let excludes_file_path = git
        .git_directory
        .join(format!("checkpoint-excludes-{}.tmp", Uuid::new_v4()));

    let delete_excludes_file = util::defer({
        let excludes_file_path = excludes_file_path.clone();
        let executor = git.executor.clone();
        move || {
            executor
                .spawn(async move {
                    smol::fs::remove_file(excludes_file_path).await.log_err();
                })
                .detach();
        }
    });

    smol::fs::write(&excludes_file_path, include_str!("./checkpoint.gitignore")).await?;

    let mut exclude_from_arg = OsString::from("--exclude-from=");
    exclude_from_arg.push(&excludes_file_path);
    let output = git
        .run(&[
            OsStr::new("ls-files"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
            exclude_from_arg.as_os_str(),
        ])
        .await;

    smol::fs::remove_file(&excludes_file_path).await.ok();
    delete_excludes_file.abort();
    let output = output?;

    let working_directory = git.working_directory.clone();
    let size_checks = output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| {
            let full_path = working_directory.join(path);
            let path = path.to_string();
            smol::spawn(async move {
                match smol::fs::metadata(&full_path).await {
                    Ok(metadata) if metadata.is_file() && metadata.len() >= MAX_SIZE => None,
                    _ => Some(path),
                }
            })
        })
        .collect::<Vec<_>>();

    let untracked_files = futures::future::join_all(size_checks)
        .await
        .into_iter()
        .flatten()
        .collect();
    Ok(untracked_files)
}

async fn add_files_to_index(git: &GitBinary, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let mut process = git
        .build_command(&["update-index", "--add", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = BufWriter::new(
        process
            .stdin
            .take()
            .context("no stdin for git update-index subprocess")?,
    );
    for file in files {
        stdin.write_all(file.as_bytes()).await?;
        stdin.write_all(b"\0").await?;
    }
    stdin.flush().await?;
    drop(stdin);

    let output = process.output().await?;
    anyhow::ensure!(
        output.status.success(),
        GitBinaryCommandError {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            status: output.status,
        }
    );
    Ok(())
}

#[derive(Clone)]
pub(crate) struct GitBinary {
    git_binary_path: PathBuf,
    working_directory: PathBuf,
    git_directory: PathBuf,
    executor: BackgroundExecutor,
    index_file_path: Option<PathBuf>,
    envs: HashMap<String, String>,
    is_trusted: bool,
}

impl GitBinary {
    pub(crate) fn new(
        git_binary_path: PathBuf,
        working_directory: PathBuf,
        git_directory: PathBuf,
        executor: BackgroundExecutor,
        is_trusted: bool,
    ) -> Self {
        Self {
            git_binary_path,
            working_directory,
            git_directory,
            executor,
            index_file_path: None,
            envs: HashMap::default(),
            is_trusted,
        }
    }

    fn envs(mut self, envs: HashMap<String, String>) -> Self {
        self.envs = envs;
        self
    }

    pub async fn with_temp_index<R>(
        &mut self,
        f: impl AsyncFnOnce(&Self) -> Result<R>,
    ) -> Result<R> {
        let index_file_path = self.path_for_index_id(Uuid::new_v4());

        let delete_temp_index = util::defer({
            let index_file_path = index_file_path.clone();
            let executor = self.executor.clone();
            move || {
                executor
                    .spawn(async move {
                        smol::fs::remove_file(index_file_path).await.log_err();
                    })
                    .detach();
            }
        });

        // Copy the default index file so that Git doesn't have to rebuild the
        // whole index from scratch. This might fail if this is an empty repository.
        smol::fs::copy(self.git_directory.join("index"), &index_file_path)
            .await
            .ok();

        self.index_file_path = Some(index_file_path.clone());
        let result = f(self).await;
        self.index_file_path = None;
        let result = result?;

        smol::fs::remove_file(index_file_path).await.ok();
        delete_temp_index.abort();

        Ok(result)
    }

    fn path_for_index_id(&self, id: Uuid) -> PathBuf {
        self.git_directory.join(format!("index-{}.tmp", id))
    }

    pub async fn run<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut stdout = self.run_raw(args).await?;
        if stdout.chars().last() == Some('\n') {
            stdout.pop();
        }
        Ok(stdout)
    }

    /// Returns the result of the command without trimming the trailing newline.
    pub async fn run_raw<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut command = self.build_command(args);
        let output = command.output().await?;
        anyhow::ensure!(
            output.status.success(),
            GitBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    #[allow(clippy::disallowed_methods)]
    pub(crate) fn build_command<S>(&self, args: &[S]) -> util::command::Command
    where
        S: AsRef<OsStr>,
    {
        let mut command = new_command(&self.git_binary_path);
        command.current_dir(&self.working_directory);
        // Disabled to stop malicious actors from running arbitrary commands via fsmonitor hooks
        command.args(["-c", "core.fsmonitor=false"]);
        // Prepended signature lines would corrupt our --format parsers.
        command.args(["-c", "log.showSignature=false"]);
        command.arg("--no-optional-locks");
        // Internal commands must be non-interactive so background tasks never block on user input.
        command.arg("--no-pager");

        if !self.is_trusted {
            command.args(["-c", "core.hooksPath=/dev/null"]);
            command.args(["-c", "core.sshCommand=ssh"]);
            command.args(["-c", "credential.helper="]);
            command.args(["-c", "protocol.ext.allow=never"]);
            command.args(["-c", "diff.external="]);
        }
        // If the `diff` command is being used, we'll want to add the
        // `--no-ext-diff` flag when working on an untrusted repository,
        // preventing any external diff programs from being invoked. It goes
        // directly after the subcommand: callers end their revisions with `--`,
        // and anything after that separator is read as a pathspec rather than as
        // an option.
        let mut args = args.iter();
        if let Some(subcommand) = args.next() {
            let is_diff = subcommand.as_ref() == "diff";
            command.arg(subcommand);
            if !self.is_trusted && is_diff {
                command.arg("--no-ext-diff");
            }
            command.args(args);
        }

        if let Some(index_file_path) = self.index_file_path.as_ref() {
            command.env("GIT_INDEX_FILE", index_file_path);
        }
        command.envs(&self.envs);
        command
    }
}

#[derive(Error, Debug)]
#[error("Git command failed:\n{stdout}{stderr}\n")]
struct GitBinaryCommandError {
    stdout: String,
    stderr: String,
    status: ExitStatus,
}

// URL injection is impossible in production: callers supply the canonical URL
// constructed from validated components. The separate source supports local-Git fixtures.
async fn review_pr_fetch_command(
    git: &GitBinary,
    request: &ReviewPrFetch,
    source: &str,
    github_credentials: bool,
) -> Result<(util::command::Command, VerifiedReviewPr)> {
    request.validate()?;
    git.run(&[
        "check-ref-format",
        &format!("refs/heads/{}", request.base_branch),
    ])
    .await?;
    let namespace = format!(
        "refs/zed/review/pr/{}/{}",
        request.number,
        Uuid::new_v4().simple()
    );
    let refs = VerifiedReviewPr {
        base: request.expected_base,
        head: request.expected_head,
        base_ref: format!("{namespace}/base"),
        head_ref: format!("{namespace}/head"),
    };
    let mut args = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "maintenance.auto=false",
        "-c",
        "fetch.writeCommitGraph=false",
    ];
    if github_credentials {
        args.extend([
            "-c",
            "credential.helper=",
            "-c",
            "credential.helper=!gh auth git-credential",
        ]);
    }
    args.extend([
        "fetch",
        "--atomic",
        "--no-tags",
        "--no-recurse-submodules",
        "--no-write-fetch-head",
        "--refmap=",
        "--",
    ]);
    let mut command = git.build_command(&args);
    command
        .arg(source)
        .arg(format!(
            "refs/heads/{}:{}",
            request.base_branch, refs.base_ref
        ))
        .arg(format!(
            "refs/pull/{}/head:{}",
            request.number, refs.head_ref
        ))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env_remove("GH_DEBUG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok((command, refs))
}

async fn verify_review_pr_fetch(
    git: &GitBinary,
    request: &ReviewPrFetch,
    refs: VerifiedReviewPr,
) -> Result<VerifiedReviewPr> {
    for (reference, expected) in [
        (&refs.base_ref, request.expected_base),
        (&refs.head_ref, request.expected_head),
    ] {
        let resolved = git
            .run(&[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ])
            .await?;
        anyhow::ensure!(
            Oid::from_str(&resolved)? == expected,
            "PR revisions changed during fetch; refresh the PR before opening files"
        );
    }
    Ok(refs)
}

// Cleanup waits for the killed child to be reaped, so a cancelled fetch cannot
// install its refs after cleanup. Every deletion uses the observed old OID CAS.
struct ReviewPrAttempt {
    git: Option<GitBinary>,
    refs: VerifiedReviewPr,
    completion: Option<
        futures::future::Shared<BoxFuture<'static, std::result::Result<ExitStatus, String>>>,
    >,
    child: Option<Arc<Mutex<util::command::Child>>>,
    accepted: bool,
}
impl ReviewPrAttempt {
    fn new(git: GitBinary, refs: VerifiedReviewPr) -> Self {
        Self {
            git: Some(git),
            refs,
            completion: None,
            child: None,
            accepted: false,
        }
    }
}
impl Drop for ReviewPrAttempt {
    fn drop(&mut self) {
        if self.accepted {
            return;
        }
        if let Some(child) = self.child.take() {
            let mut child = child.lock();
            if !matches!(child.try_status(), Ok(Some(_))) {
                child.kill().log_err();
            }
        }
        if let Some(git) = self.git.take() {
            let refs = self.refs.clone();
            let completion = self.completion.take();
            git.executor
                .clone()
                .spawn(async move {
                    if let Some(completion) = completion {
                        completion.await.log_err();
                    }
                    cleanup_review_pr_refs(&git, &refs).await.log_err();
                })
                .detach();
        }
    }
}
async fn cleanup_review_pr_refs(git: &GitBinary, refs: &VerifiedReviewPr) -> Result<()> {
    for reference in [&refs.base_ref, &refs.head_ref] {
        let output = git
            .build_command(&["rev-parse", "--verify", "--quiet", reference])
            .output()
            .await?;
        if output.status.success() {
            let old = Oid::from_str(std::str::from_utf8(&output.stdout)?.trim())?;
            // Disable hooks for cleanup as well as creation of private refs.
            git.run(&[
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "-d",
                reference,
                &old.to_string(),
            ])
            .await?;
        }
    }
    Ok(())
}

async fn review_pr_child_output(
    mut child: util::command::Child,
    attempt: &mut ReviewPrAttempt,
) -> Result<Output> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let completion = child
        .status()
        .map(|result| result.map_err(|_| "Could not wait for PR fetch".to_string()))
        .boxed()
        .shared();
    attempt.completion = Some(completion.clone());
    attempt.child = Some(Arc::new(Mutex::new(child)));
    let stdout = async move {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = stdout {
            pipe.read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    };
    let stderr = async move {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = stderr {
            pipe.read_to_end(&mut bytes).await?;
        }
        Ok::<_, std::io::Error>(bytes)
    };
    let (stdout, stderr) = futures::try_join!(stdout, stderr)?;
    let status = completion.await.map_err(anyhow::Error::msg)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn run_git_command(
    env: Arc<HashMap<String, String>>,
    ask_pass: AskPassDelegate,
    command: util::command::Command,
    executor: BackgroundExecutor,
) -> Result<RemoteCommandOutput> {
    run_git_command_inner(env, ask_pass, command, executor, None).await
}

async fn run_git_command_inner(
    env: Arc<HashMap<String, String>>,
    ask_pass: AskPassDelegate,
    mut command: util::command::Command,
    executor: BackgroundExecutor,
    mut attempt: Option<&mut ReviewPrAttempt>,
) -> Result<RemoteCommandOutput> {
    if env.contains_key("GIT_ASKPASS") {
        let git_process = command.spawn()?;
        let output = if let Some(attempt) = attempt.as_deref_mut() {
            review_pr_child_output(git_process, attempt).await?
        } else {
            git_process.output().await?
        };
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(RemoteCommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    } else {
        let ask_pass = AskPassSession::new(executor, ask_pass).await?;
        command
            .env("GIT_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS_REQUIRE", "force");

        if !env.contains_key("GIT_CONFIG_COUNT")
            && let Some(gpg_wrapper) = ask_pass.gpg_wrapper_path()
        {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "gpg.program")
                .env("GIT_CONFIG_VALUE_0", gpg_wrapper);
        }

        #[cfg(target_os = "windows")]
        command.env("ZED_ASKPASS_SOCKET", ask_pass.socket_path());
        let git_process = command.spawn()?;
        run_askpass_command(ask_pass, git_process, attempt).await
    }
}

async fn run_askpass_command(
    mut ask_pass: AskPassSession,
    git_process: util::command::Child,
    attempt: Option<&mut ReviewPrAttempt>,
) -> anyhow::Result<RemoteCommandOutput> {
    select_biased! {
        // Git can legitimately run long without prompting (e.g. large fetches,
        // hooks), so completion is determined by the process itself.
        result = ask_pass.run(None).fuse() => {
            match result {
                AskPassResult::CancelledByUser => {
                    Err(anyhow!(REMOTE_CANCELLED_BY_USER))?
                }
                AskPassResult::Timedout => {
                    // Unreachable since no timeout is passed to run()
                    Err(anyhow!("Connecting to host timed out"))?
                }
            }
        }
        output = async move {
            if let Some(attempt) = attempt {
                review_pr_child_output(git_process, attempt).await
            } else {
                git_process.output().await.map_err(anyhow::Error::from)
            }
        }.fuse() => {
            let output = output?;
            anyhow::ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(RemoteCommandOutput {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            })
        }
    }
}

#[derive(Clone, Ord, Hash, PartialOrd, Eq, PartialEq)]
pub struct RepoPath(Arc<RelPath>);

impl std::fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl RepoPath {
    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> Result<Self> {
        let rel_path = RelPath::from_unix_str(s.as_ref())?;
        Ok(Self::from_rel_path(rel_path))
    }

    pub fn from_std_path(path: &Path, path_style: PathStyle) -> Result<Self> {
        let rel_path = RelPath::new(path, path_style)?;
        Ok(Self::from_rel_path(&rel_path))
    }

    pub fn from_proto(proto: &str) -> Result<Self> {
        let rel_path = RelPath::from_unix_str(proto)?.into();
        Ok(Self(rel_path))
    }

    pub fn from_rel_path(path: &RelPath) -> RepoPath {
        Self(Arc::from(path))
    }

    pub fn as_std_path(&self) -> &Path {
        if self.is_empty() {
            Path::new(".")
        } else {
            self.0.as_std_path()
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn repo_path<S: AsRef<str> + ?Sized>(s: &S) -> RepoPath {
    RepoPath(RelPath::from_unix_str(s.as_ref()).unwrap().into())
}

impl AsRef<Arc<RelPath>> for RepoPath {
    fn as_ref(&self) -> &Arc<RelPath> {
        &self.0
    }
}

impl std::ops::Deref for RepoPath {
    type Target = RelPath;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct RepoPathDescendants<'a>(pub &'a RepoPath);

impl MapSeekTarget<RepoPath> for RepoPathDescendants<'_> {
    fn cmp_cursor(&self, key: &RepoPath) -> Ordering {
        if key.starts_with(self.0) {
            Ordering::Greater
        } else {
            self.0.cmp(key)
        }
    }
}

fn parse_branch_input(input: &str) -> Result<Vec<Branch>> {
    let mut branches = Vec::new();
    for line in input.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\x00');
        let Some(head) = fields.next() else {
            continue;
        };
        let Some(head_sha) = fields.next().map(|f| f.to_string().into()) else {
            continue;
        };
        let Some(parent_sha) = fields.next().map(|f| f.to_string()) else {
            continue;
        };
        let Some(ref_name) = fields.next().map(|f| f.to_string().into()) else {
            continue;
        };
        let Some(upstream_name) = fields.next().map(|f| f.to_string()) else {
            continue;
        };
        let Some(upstream_tracking) = fields.next().and_then(|f| parse_upstream_track(f).ok())
        else {
            continue;
        };
        let Some(commiterdate) = fields.next().and_then(|f| f.parse::<i64>().ok()) else {
            continue;
        };
        let Some(author_name) = fields.next().map(|f| f.to_string().into()) else {
            continue;
        };
        let Some(subject) = fields.next().map(|f| f.to_string().into()) else {
            continue;
        };

        branches.push(Branch {
            is_head: head == "*",
            ref_name,
            most_recent_commit: Some(CommitSummary {
                sha: head_sha,
                subject,
                commit_timestamp: commiterdate,
                author_name: author_name,
                has_parent: !parent_sha.is_empty(),
            }),
            upstream: if upstream_name.is_empty() {
                None
            } else {
                Some(Upstream {
                    ref_name: upstream_name.into(),
                    tracking: upstream_tracking,
                })
            },
        })
    }

    Ok(branches)
}

fn format_branch_scan_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim()
        .replace('\n', " ");
    if stderr.is_empty() {
        format!("git for-each-ref exited with {}", output.status)
    } else {
        stderr
    }
}

fn parse_upstream_track(upstream_track: &str) -> Result<UpstreamTracking> {
    if upstream_track.is_empty() {
        return Ok(UpstreamTracking::Tracked(UpstreamTrackingStatus {
            ahead: 0,
            behind: 0,
        }));
    }

    let upstream_track = upstream_track.strip_prefix("[").context("missing [")?;
    let upstream_track = upstream_track.strip_suffix("]").context("missing [")?;
    let mut ahead: u32 = 0;
    let mut behind: u32 = 0;
    for component in upstream_track.split(", ") {
        if component == "gone" {
            return Ok(UpstreamTracking::Gone);
        }
        if let Some(ahead_num) = component.strip_prefix("ahead ") {
            ahead = ahead_num.parse::<u32>()?;
        }
        if let Some(behind_num) = component.strip_prefix("behind ") {
            behind = behind_num.parse::<u32>()?;
        }
    }
    Ok(UpstreamTracking::Tracked(UpstreamTrackingStatus {
        ahead,
        behind,
    }))
}

fn checkpoint_author_envs() -> HashMap<String, String> {
    HashMap::from_iter([
        ("GIT_AUTHOR_NAME".to_string(), "Zed".to_string()),
        ("GIT_AUTHOR_EMAIL".to_string(), "hi@zed.dev".to_string()),
        ("GIT_COMMITTER_NAME".to_string(), "Zed".to_string()),
        ("GIT_COMMITTER_EMAIL".to_string(), "hi@zed.dev".to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        fs,
    };

    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn test_create_ref_requires_ref_to_not_exist() {
        assert_eq!(
            RefEdit::Create {
                ref_name: "refs/tags/v1.0.0".to_string(),
                commit: "abcdef".to_string(),
            }
            .into_args(),
            ["update-ref", "refs/tags/v1.0.0", "abcdef", ""].map(OsString::from)
        );
    }

    #[test]
    fn test_commit_hash_search_query_accepts_sha1_and_sha256_hashes() {
        assert_eq!(
            commit_hash_search_query("0123456789abcdef0123456789abcdef01234567"),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(
            commit_hash_search_query(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            commit_hash_search_query(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0"
            ),
            None
        );
    }

    fn disable_git_global_config() {
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "");
            std::env::set_var("GIT_CONFIG_SYSTEM", "");
        }
    }

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn git_command_output<I, S>(working_directory: &Path, arguments: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(working_directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@zed.dev")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@zed.dev")
            .output()
            .expect("failed to run git command");
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git command output was not valid UTF-8")
            .trim()
            .to_string()
    }

    #[track_caller]
    fn git_command<I, S>(working_directory: &Path, arguments: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        git_command_output(working_directory, arguments);
    }

    fn git_init_repo(path: &Path) {
        fs::create_dir_all(path).expect("failed to create repo directory");
        git_command(path, ["init", "-b", "main"]);
    }

    fn clone_remote_repository_with_main_and_feature(temp_dir: &Path) -> (PathBuf, PathBuf) {
        let remote_directory = temp_dir.join("remote.git");
        let seed_directory = temp_dir.join("seed");
        let clone_directory = temp_dir.join("clone");

        git_command(
            temp_dir,
            [
                OsString::from("init"),
                OsString::from("--bare"),
                OsString::from("-b"),
                OsString::from("main"),
                remote_directory.as_os_str().into(),
            ],
        );
        git_init_repo(&seed_directory);
        fs::write(seed_directory.join("file.txt"), "main").unwrap();
        git_command(&seed_directory, ["add", "file.txt"]);
        git_command(&seed_directory, ["commit", "-m", "initial"]);
        git_command(&seed_directory, ["switch", "-c", "feature"]);
        fs::write(seed_directory.join("feature.txt"), "feature").unwrap();
        git_command(&seed_directory, ["add", "feature.txt"]);
        git_command(&seed_directory, ["commit", "-m", "feature"]);
        git_command(
            &seed_directory,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote_directory.as_os_str().into(),
            ],
        );
        git_command(&seed_directory, ["push", "-u", "origin", "main"]);
        git_command(&seed_directory, ["push", "-u", "origin", "feature"]);
        git_command(
            temp_dir,
            [
                OsString::from("clone"),
                remote_directory.as_os_str().into(),
                clone_directory.as_os_str().into(),
            ],
        );

        (remote_directory, clone_directory)
    }

    fn test_commit_envs() -> HashMap<String, String> {
        let mut env = checkpoint_author_envs();
        env.insert("GIT_ASKPASS".to_string(), "false".to_string());
        env
    }

    #[track_caller]
    fn assert_same_path(left: impl AsRef<Path>, right: impl AsRef<Path>) {
        assert_eq!(
            fs::canonicalize(left.as_ref()).unwrap(),
            fs::canonicalize(right.as_ref()).unwrap()
        );
    }

    #[gpui::test]
    async fn test_review_commit_scopes_match_git_and_reject_missing_history(
        cx: &mut TestAppContext,
    ) {
        use crate::review::ReviewCommitSelection;
        disable_git_global_config();
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git_init_repo(path);
        fs::write(path.join("auth.ts"), "base\n").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "root"]);
        let root: Oid = git_command_output(path, ["rev-parse", "HEAD"])
            .parse()
            .unwrap();
        let mut chain = vec![root];
        for (file, contents) in [
            ("auth.ts", "first\n"),
            ("billing.ts", "billing\n"),
            ("auth.ts", "third\n"),
        ] {
            fs::write(path.join(file), contents).unwrap();
            git_command(path, ["add", "."]);
            git_command(path, ["commit", "-m", file]);
            chain.push(
                git_command_output(path, ["rev-parse", "HEAD"])
                    .parse::<Oid>()
                    .unwrap(),
            );
        }
        let repository =
            RealGitRepository::new(&path.join(".git"), None, Some("git".into()), cx.executor())
                .unwrap();
        let history = repository
            .review_history(Some(root), chain[3])
            .await
            .unwrap();
        assert_eq!(
            history
                .iter()
                .map(|commit| commit.commit)
                .collect::<Vec<_>>(),
            vec![chain[3], chain[2], chain[1]]
        );
        assert_eq!(history[0].parents, vec![chain[2]]);
        let select = |index: usize| ReviewCommitSelection {
            commit: chain[index],
            parent: index.checked_sub(1).map(|parent| chain[parent]),
        };
        fs::write(path.join("auth.ts"), "staged\n").unwrap();
        git_command(path, ["add", "auth.ts"]);
        fs::write(path.join("auth.ts"), "disk-only\n").unwrap();
        let index = fs::read(path.join(".git/index")).unwrap();
        let objects_before = git_command_output(path, ["count-objects", "-v"]);
        let root_diff = repository
            .review_commits(vec![select(0)], false)
            .await
            .unwrap();
        assert_eq!(root_diff[0].files.len(), 1);
        assert!(root_diff[0].files[0].old.is_none());
        assert_eq!(root_diff[0].right, root);
        let groups = repository
            .review_commits(vec![select(1), select(3)], false)
            .await
            .unwrap();
        assert_eq!(groups.len(), 2);
        for (group, selected) in groups.iter().zip([select(1), select(3)]) {
            assert_eq!(group.left, selected.parent.unwrap());
            assert_eq!(group.right, selected.commit);
            assert_eq!(group.files.len(), 1);
            assert_eq!(group.files[0].path.as_unix_str(), "auth.ts");
            let expected =
                git_command_output(path, ["rev-parse", &format!("{}:auth.ts", selected.commit)]);
            assert_eq!(
                group.files[0].new.as_ref().unwrap().object.to_string(),
                expected
            );
        }
        let range = repository
            .review_commits(vec![select(1), select(2), select(3)], true)
            .await
            .unwrap();
        assert_eq!(range.len(), 1);
        assert_eq!((range[0].left, range[0].right), (root, chain[3]));
        assert_eq!(range[0].files.len(), 2);
        let root_range = repository
            .review_commits(vec![select(0), select(1)], true)
            .await
            .unwrap();
        assert!(root_range[0].files[0].old.is_none());
        assert_eq!(root_range[0].right, chain[1]);
        assert!(
            repository
                .review_commits(vec![select(1), select(3)], true)
                .await
                .is_err()
        );
        assert!(
            repository
                .review_commits(vec![select(2), select(1)], true)
                .await
                .is_err()
        );
        assert!(
            repository
                .review_commits(vec![select(1), select(1)], false)
                .await
                .is_err()
        );
        assert!(repository.review_commits(vec![], false).await.is_err());
        assert!(
            repository
                .review_commits(
                    vec![ReviewCommitSelection {
                        commit: chain[3],
                        parent: Some(root)
                    }],
                    false
                )
                .await
                .is_err()
        );
        assert!(
            repository
                .review_commits(
                    vec![ReviewCommitSelection {
                        commit: chain[3],
                        parent: None
                    }],
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(
            git_command_output(path, ["rev-parse", "HEAD"]),
            chain[3].to_string()
        );
        assert_eq!(fs::read(path.join(".git/index")).unwrap(), index);
        assert_eq!(
            fs::read_to_string(path.join("auth.ts")).unwrap(),
            "disk-only\n"
        );
        assert_eq!(
            git_command_output(path, ["count-objects", "-v"]),
            objects_before
        );

        let clone_dir = tempfile::tempdir().unwrap();
        let shallow = clone_dir.path().join("shallow");
        git_command(
            clone_dir.path(),
            [
                "clone",
                "--depth=1",
                &format!("file://{}", path.display()),
                shallow.to_str().unwrap(),
            ],
        );
        let shallow_repo = RealGitRepository::new(
            &shallow.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let shallow_history = shallow_repo
            .review_history(None, chain[3])
            .await
            .unwrap_err();
        assert!(shallow_history.to_string().contains("missing"));
        assert!(
            shallow_repo
                .review_commits(vec![select(3)], false)
                .await
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
        assert!(
            shallow_repo
                .review_commits(
                    vec![ReviewCommitSelection {
                        commit: chain[3],
                        parent: None
                    }],
                    false
                )
                .await
                .is_err()
        );
    }

    #[gpui::test]
    async fn test_review_history_batches_more_than_2000_commits_and_preserves_merge_membership(
        cx: &mut TestAppContext,
    ) {
        use crate::review::ReviewCommitSelection;
        disable_git_global_config();
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git_init_repo(path);
        // One fast-import process generates a large real object graph without
        // thousands of commit/checkout subprocesses or unrelated file changes.
        const LAST_LINEAR: usize = 2052;
        let mut input = String::new();
        for mark in 1..=LAST_LINEAR {
            let message = format!("history {mark}");
            input.push_str(&format!("commit refs/heads/fixture\nmark :{mark}\ncommitter Review Fixture <fixture@example.invalid> 1700000000 +0000\ndata {}\n{message}\n",message.len()));
            if mark > 1 {
                input.push_str(&format!("from :{}\n", mark - 1));
            }
            input.push('\n');
        }
        input.push_str(&format!("commit refs/heads/side\nmark :{}\ncommitter Review Fixture <fixture@example.invalid> 1700000000 +0000\ndata 4\nside\nfrom :1\n\n",LAST_LINEAR+1));
        input.push_str(&format!("commit refs/heads/merged\nmark :{}\ncommitter Review Fixture <fixture@example.invalid> 1700000000 +0000\ndata 5\nmerge\nfrom :{LAST_LINEAR}\nmerge :{}\n\ndone\n",LAST_LINEAR+2,LAST_LINEAR+1));
        let binary = GitBinary::new(
            "git".into(),
            path.to_path_buf(),
            path.join(".git"),
            cx.executor(),
            true,
        );
        let mut child = binary
            .build_command(&["fast-import", "--quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
        let imported = child.output().await.unwrap();
        assert!(
            imported.status.success(),
            "{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        let oid = |revision: &str| {
            git_command_output(path, ["rev-parse", revision])
                .parse::<Oid>()
                .unwrap()
        };
        let head = oid("fixture");
        let root = oid(&format!("fixture~{}", LAST_LINEAR - 1));
        let first = oid(&format!("fixture~{}", LAST_LINEAR - 2));
        let side = oid("side");
        let merged = oid("merged");
        let repository =
            RealGitRepository::new(&path.join(".git"), None, Some("git".into()), cx.executor())
                .unwrap();
        fs::write(path.join("untouched-sentinel"), "local-only").unwrap();
        let refs_before = git_command_output(path, ["show-ref"]);
        let index_before = fs::read(path.join(".git/index")).ok();
        let objects_before = git_command_output(path, ["count-objects", "-v"]);
        let history = repository.review_history(Some(root), head).await.unwrap();
        assert_eq!(history.len(), LAST_LINEAR - 1);
        assert_eq!(history.first().unwrap().commit, head);
        assert_eq!(history.last().unwrap().commit, first);
        assert_eq!(history.last().unwrap().parents, vec![root]);
        assert_eq!(
            history.first().unwrap().subject,
            format!("history {LAST_LINEAR}")
        );
        let selections = history
            .iter()
            .rev()
            .map(|commit| ReviewCommitSelection {
                commit: commit.commit,
                parent: commit.parents.first().copied(),
            })
            .collect();
        let range = repository.review_commits(selections, true).await.unwrap();
        assert_eq!(range.len(), 1);
        assert_eq!((range[0].left, range[0].right), (root, head));
        let merged_history = repository.review_history(Some(root), merged).await.unwrap();
        assert_eq!(merged_history.len(), LAST_LINEAR + 1);
        assert_eq!(merged_history[0].commit, merged);
        assert_eq!(merged_history[0].parents, vec![head, side]);
        let ids = merged_history
            .iter()
            .map(|commit| commit.commit)
            .collect::<std::collections::HashSet<_>>();
        assert!(ids.contains(&side));
        assert!(!ids.contains(&root));
        assert!(history.iter().all(|commit| ids.contains(&commit.commit)));
        let excluded_side = repository.review_history(Some(side), merged).await.unwrap();
        assert_eq!(excluded_side.len(), LAST_LINEAR);
        assert!(
            !excluded_side
                .iter()
                .any(|commit| commit.commit == side || commit.commit == root)
        );
        assert_eq!(git_command_output(path, ["show-ref"]), refs_before);
        assert_eq!(fs::read(path.join(".git/index")).ok(), index_before);
        assert_eq!(
            git_command_output(path, ["count-objects", "-v"]),
            objects_before
        );
        assert_eq!(
            fs::read_to_string(path.join("untouched-sentinel")).unwrap(),
            "local-only"
        );
        // An unrelated shallow boundary does not invalidate complete selected
        // history, but a reachable boundary cannot silently truncate the list.
        fs::write(path.join(".git/shallow"), format!("{side}\n")).unwrap();
        assert_eq!(
            repository
                .review_history(Some(root), head)
                .await
                .unwrap()
                .len(),
            LAST_LINEAR - 1
        );
        fs::write(path.join(".git/shallow"), format!("{first}\n")).unwrap();
        assert!(
            repository
                .review_history(Some(root), head)
                .await
                .unwrap_err()
                .to_string()
                .contains("shallow")
        );
    }

    #[gpui::test]
    async fn test_review_merge_requires_real_chosen_parent(cx: &mut TestAppContext) {
        use crate::review::ReviewCommitSelection;
        disable_git_global_config();
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git_init_repo(path);
        fs::write(path.join("base"), "base").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "base"]);
        git_command(path, ["switch", "-c", "side"]);
        fs::write(path.join("side"), "side").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "side"]);
        let side: Oid = git_command_output(path, ["rev-parse", "HEAD"])
            .parse()
            .unwrap();
        git_command(path, ["switch", "main"]);
        fs::write(path.join("main"), "main").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "main"]);
        let main: Oid = git_command_output(path, ["rev-parse", "HEAD"])
            .parse()
            .unwrap();
        git_command(path, ["merge", "--no-ff", "side", "-m", "merge"]);
        let merge: Oid = git_command_output(path, ["rev-parse", "HEAD"])
            .parse()
            .unwrap();
        let repository =
            RealGitRepository::new(&path.join(".git"), None, Some("git".into()), cx.executor())
                .unwrap();
        assert_eq!(
            repository.review_history(None, merge).await.unwrap()[0].parents,
            vec![main, side]
        );
        assert!(
            repository
                .review_commits(
                    vec![ReviewCommitSelection {
                        commit: merge,
                        parent: None
                    }],
                    false
                )
                .await
                .is_err()
        );
        for (parent, file) in [(main, "side"), (side, "main")] {
            let result = repository
                .review_commits(
                    vec![ReviewCommitSelection {
                        commit: merge,
                        parent: Some(parent),
                    }],
                    false,
                )
                .await
                .unwrap();
            assert_eq!(result[0].left, parent);
            assert_eq!(result[0].files.len(), 1);
            assert_eq!(result[0].files[0].path.as_unix_str(), file);
            assert!(
                repository
                    .review_commits(
                        vec![ReviewCommitSelection {
                            commit: merge,
                            parent: Some(parent)
                        }],
                        true
                    )
                    .await
                    .is_err()
            );
        }
    }

    #[gpui::test]
    async fn test_review_comparison_is_committed_rich_and_read_only(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git_init_repo(path);
        fs::write(path.join("auth.ts"), "base\n").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "base"]);
        let base = git_command_output(path, ["rev-parse", "HEAD"]);
        git_command(path, ["switch", "-c", "feature"]);
        fs::write(path.join("auth.ts"), "committed\n").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "feature"]);
        let head = git_command_output(path, ["rev-parse", "HEAD"]);
        git_command(path, ["switch", "main"]);
        fs::write(path.join("base-only.txt"), "base-only\n").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "advance base"]);
        git_command(path, ["switch", "feature"]);
        fs::write(path.join("auth.ts"), "staged\n").unwrap();
        git_command(path, ["add", "."]);
        fs::write(path.join("auth.ts"), "disk\n").unwrap();
        let index = fs::read(path.join(".git/index")).unwrap();
        let repository =
            RealGitRepository::new(&path.join(".git"), None, Some("git".into()), cx.executor())
                .unwrap();
        let comparison = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        assert_eq!(comparison.left.to_string(), base);
        assert_eq!(comparison.right.to_string(), head);
        assert_eq!(comparison.files.len(), 1);
        let file = comparison.files.first().unwrap();
        assert_eq!(file.path.as_unix_str(), "auth.ts");
        assert_eq!(file.old.as_ref().unwrap().mode, 0o100644);
        assert_eq!(
            repository
                .load_blob_content(file.new.as_ref().unwrap().object)
                .await
                .unwrap(),
            b"committed\n"
        );
        let direct = repository
            .review_comparison("main".into(), "feature".into(), false)
            .await
            .unwrap();
        assert_eq!(direct.files.len(), 2);
        assert!(
            repository
                .review_comparison("HEAD~1".into(), "feature".into(), true)
                .await
                .is_err()
        );
        assert!(
            repository
                .review_comparison("missing-ref".into(), "feature".into(), true)
                .await
                .is_err()
        );
        assert_eq!(fs::read(path.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read(path.join("auth.ts")).unwrap(), b"disk\n");
        assert_eq!(git_command_output(path, ["rev-parse", "HEAD"]), head);
    }

    #[gpui::test]
    async fn test_review_metadata_across_rewrites_modes_renames_and_base_changes(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        git_init_repo(path);
        for name in ["auth.ts", "billing.ts", "remove.ts"] {
            fs::write(path.join(name), "base\n").unwrap();
        }
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "base"]);
        let base = git_command_output(path, ["rev-parse", "HEAD"]);
        git_command(path, ["switch", "-c", "feature"]);
        for name in ["auth.ts", "billing.ts"] {
            fs::write(path.join(name), "reviewed\n").unwrap();
        }
        git_command(path, ["commit", "-am", "reviewed"]);
        let first_head = git_command_output(path, ["rev-parse", "HEAD"]);
        let repository =
            RealGitRepository::new(&path.join(".git"), None, Some("git".into()), cx.executor())
                .unwrap();
        let initial = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        fs::write(path.join("extra.ts"), "new\n").unwrap();
        git_command(path, ["add", "."]);
        git_command(path, ["commit", "-m", "unrelated"]);
        let unrelated = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        for file in &initial.files {
            assert!(unrelated.files.contains(file));
        }
        fs::write(path.join("auth.ts"), "changed again\n").unwrap();
        git_command(path, ["commit", "-am", "changed auth"]);
        let changed = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        let find = |comparison: &crate::review::ReviewComparison, name: &str| {
            comparison
                .files
                .iter()
                .find(|file| file.path.as_unix_str() == name)
                .cloned()
                .unwrap()
        };
        assert_ne!(find(&initial, "auth.ts"), find(&changed, "auth.ts"));
        assert_eq!(find(&initial, "billing.ts"), find(&changed, "billing.ts"));
        git_command(path, ["update-index", "--chmod=+x", "billing.ts"]);
        git_command(path, ["rm", "remove.ts"]);
        git_command(path, ["mv", "auth.ts", "renamed.ts"]);
        git_command(path, ["commit", "-m", "mode rename delete"]);
        let metadata = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        assert_eq!(find(&metadata, "billing.ts").new.unwrap().mode, 0o100755);
        assert!(find(&metadata, "auth.ts").new.is_none());
        assert!(find(&metadata, "renamed.ts").old.is_none());
        assert!(find(&metadata, "remove.ts").new.is_none());
        git_command(path, ["update-ref", "refs/heads/feature", &first_head]);
        let rewind = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        assert_eq!(rewind.files, initial.files);
        let tree = git_command_output(path, ["rev-parse", &format!("{first_head}^{{tree}}")]);
        let rewritten =
            git_command_output(path, ["commit-tree", &tree, "-p", &base, "-m", "rewritten"]);
        git_command(path, ["update-ref", "refs/heads/feature", &rewritten]);
        let rewrite = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        assert_ne!(rewrite.right, initial.right);
        assert_eq!(rewrite.files, initial.files);
        git_command(path, ["update-ref", "refs/heads/main", &first_head]);
        let based_on_advanced = git_command_output(
            path,
            ["commit-tree", &tree, "-p", &first_head, "-m", "rebased"],
        );
        git_command(
            path,
            ["update-ref", "refs/heads/feature", &based_on_advanced],
        );
        let index = fs::read(path.join(".git/index")).unwrap();
        let disk = fs::read(path.join("renamed.ts")).unwrap();
        let rebased = repository
            .review_comparison("main".into(), "feature".into(), true)
            .await
            .unwrap();
        assert_eq!(rebased.left.to_string(), first_head);
        assert!(rebased.files.is_empty());
        assert_eq!(fs::read(path.join(".git/index")).unwrap(), index);
        assert_eq!(fs::read(path.join("renamed.ts")).unwrap(), disk);
        assert_eq!(
            git_command_output(path, ["rev-parse", "HEAD"]),
            based_on_advanced
        );
    }

    #[gpui::test]
    async fn test_real_git_repository_new_resolves_normal_repository_paths(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        assert_same_path(&repository.git_dir, repo_dir.path().join(".git"));
        assert_same_path(&repository.common_dir, repo_dir.path().join(".git"));
        assert_same_path(
            repository.working_directory.as_ref().unwrap(),
            repo_dir.path(),
        );
        assert_same_path(
            original_repo_path_from_common_dir(&repository.common_dir).unwrap(),
            repo_dir.path(),
        );
    }

    #[gpui::test]
    async fn test_merge_base_worktree_diff_handles_recreated_index_deletion(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        fs::write(&file_path, "base\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "base"]);
        let base_oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD:file.txt"])
            .parse()
            .unwrap();

        fs::write(&file_path, "head\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "head"]);
        git_command(repo_dir.path(), ["rm", "--cached", "file.txt"]);
        fs::write(&file_path, "base\n").unwrap();

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::default(),
            }
        );

        fs::write(&file_path, "worktree\n").unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("file.txt").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    #[gpui::test]
    async fn test_checkout_files_from_index_keeps_staged_changes(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        fs::write(&file_path, "head\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "head"]);
        fs::write(&file_path, "staged\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        fs::write(&file_path, "unstaged\n").unwrap();

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let paths = vec![RepoPath::new("file.txt").unwrap()];

        repository
            .checkout_files(None, paths.clone(), Arc::default())
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "staged\n");
        assert_eq!(
            git_command_output(repo_dir.path(), ["show", ":file.txt"]),
            "staged"
        );

        repository
            .checkout_files(Some("HEAD".into()), paths, Arc::default())
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "head\n");
        assert_eq!(
            git_command_output(repo_dir.path(), ["show", ":file.txt"]),
            "head"
        );
    }

    #[gpui::test]
    async fn test_merge_base_worktree_diff_handles_committed_deletion_recreated_on_disk(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        fs::write(&file_path, "base\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "base"]);
        let base_oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD:file.txt"])
            .parse()
            .unwrap();

        git_command(repo_dir.path(), ["rm", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "delete"]);
        fs::write(&file_path, "base\n").unwrap();

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::default(),
            }
        );

        fs::write(&file_path, "worktree\n").unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("file.txt").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_merge_base_worktree_diff_handles_recreated_symlink(cx: &mut TestAppContext) {
        use std::os::unix::fs::symlink;

        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        symlink("base-target", &file_path).unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "base"]);
        let base_oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD:file.txt"])
            .parse()
            .unwrap();

        fs::remove_file(&file_path).unwrap();
        symlink("head-target", &file_path).unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "head"]);
        git_command(repo_dir.path(), ["rm", "--cached", "file.txt"]);
        fs::remove_file(&file_path).unwrap();
        symlink("base-target", &file_path).unwrap();

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::default(),
            }
        );

        fs::remove_file(&file_path).unwrap();
        fs::write(&file_path, "base-target").unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("file.txt").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    #[test]
    fn test_read_commit_blob_binary_detection() {
        let mut pickle = b"\x80\x02X\0\x04\0\0".to_vec();
        pickle.extend_from_slice(&b"A\0".repeat(512));
        pickle.extend_from_slice(b"q\0.");
        let mut cases = vec![
            (pickle, true),
            (b"hello\n".to_vec(), false),
            (b"\xFF\xFEH\0e\0l\0l\0o\0\n\0".to_vec(), true),
            (b"\xFE\xFF\0H\0e\0l\0l\0o\0\n".to_vec(), true),
            (b"H\0e\0l\0l\0o\0\n\0".to_vec(), true),
            (b"\0H\0e\0l\0l\0o\0\n".to_vec(), true),
            (b"\xEF\xBB\xBFhello\0".to_vec(), true),
            (b"\x89PNG\r\n\x1a\n".to_vec(), true),
        ];
        for offset in [4096, 7999, 8000] {
            let mut bytes = vec![b'a'; offset];
            bytes.push(0);
            cases.push((bytes, offset < 8000));
        }

        for (bytes, expected_binary) in cases {
            let mut output = format!("{}\n", bytes.len()).into_bytes();
            output.extend_from_slice(&bytes);
            output.push(b'\n');
            let object = smol::block_on(read_commit_blob(
                &mut smol::io::Cursor::new(output),
                &mut String::new(),
                &mut [0],
            ))
            .unwrap();

            assert_eq!(object.is_binary, expected_binary);
            assert_eq!(object.content, bytes);
        }
    }

    #[gpui::test]
    async fn test_load_commit_with_type_changed_file(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().expect("failed to create temporary repository");
        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("file.txt"), "regular contents\n")
            .expect("failed to write regular file");
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "initial"]);

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open repository");
        fs::write(repo_dir.path().join("file.txt"), "target")
            .expect("failed to write symlink target");

        let symlink_blob = repository
            .git_binary()
            .run(&["hash-object", "-w", "file.txt"])
            .await
            .expect("failed to write symlink blob");
        git_command(
            repo_dir.path(),
            [
                OsString::from("update-index"),
                OsString::from("--cacheinfo"),
                OsString::from("120000"),
                OsString::from(symlink_blob),
                OsString::from("file.txt"),
            ],
        );
        git_command(repo_dir.path(), ["commit", "-m", "type change"]);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load type-changed commit");
        assert_eq!(commit_diff.files.len(), 1);

        let file = commit_diff
            .files
            .first()
            .expect("type-changed file should be present");
        assert_eq!(file.path.as_unix_str(), "file.txt");
        assert_eq!(
            file.old_content.as_deref(),
            Some(b"regular contents\n".as_slice())
        );
        assert_eq!(file.new_content.as_deref(), Some(b"target".as_slice()));
        assert_eq!(file.status(), CommitFileStatus::Modified);
    }

    #[gpui::test]
    async fn test_load_commit_with_gitlink_changes(cx: &mut TestAppContext) {
        const FIRST_SUBMODULE_COMMIT: &str = "1111111111111111111111111111111111111111";
        const SECOND_SUBMODULE_COMMIT: &str = "2222222222222222222222222222222222222222";

        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().expect("failed to create temporary repository");
        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("README.md"), "parent repository\n")
            .expect("failed to write regular file");
        git_command(repo_dir.path(), ["add", "README.md"]);
        git_command(
            repo_dir.path(),
            [
                "update-index",
                "--add",
                "--cacheinfo",
                crate::commit::GITLINK_MODE,
                FIRST_SUBMODULE_COMMIT,
                "modules/example",
            ],
        );
        git_command(repo_dir.path(), ["commit", "-m", "add submodule"]);

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open repository");

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load commit that adds a gitlink");
        assert_eq!(commit_diff.files.len(), 2);
        let gitlink = commit_diff
            .files
            .iter()
            .find(|file| file.path.as_unix_str() == "modules/example")
            .expect("gitlink should be present alongside the regular file");
        assert_eq!(gitlink.status(), CommitFileStatus::Added);
        assert_eq!(gitlink.old_content, None);
        assert_eq!(
            gitlink.new_content.as_deref(),
            Some(b"Subproject commit 1111111111111111111111111111111111111111\n".as_slice())
        );
        assert!(!gitlink.is_binary);

        git_command(
            repo_dir.path(),
            [
                "update-index",
                "--cacheinfo",
                crate::commit::GITLINK_MODE,
                SECOND_SUBMODULE_COMMIT,
                "modules/example",
            ],
        );
        git_command(repo_dir.path(), ["commit", "-m", "update submodule"]);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load commit that updates a gitlink");
        let [gitlink] = commit_diff.files.as_slice() else {
            panic!("expected one updated gitlink");
        };
        assert_eq!(gitlink.status(), CommitFileStatus::Modified);
        assert_eq!(
            gitlink.old_content.as_deref(),
            Some(b"Subproject commit 1111111111111111111111111111111111111111\n".as_slice())
        );
        assert_eq!(
            gitlink.new_content.as_deref(),
            Some(b"Subproject commit 2222222222222222222222222222222222222222\n".as_slice())
        );
        assert!(!gitlink.is_binary);

        git_command(repo_dir.path(), ["rm", "--cached", "modules/example"]);
        git_command(repo_dir.path(), ["commit", "-m", "remove submodule"]);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load commit that deletes a gitlink");
        let [gitlink] = commit_diff.files.as_slice() else {
            panic!("expected one deleted gitlink");
        };
        assert_eq!(gitlink.status(), CommitFileStatus::Deleted);
        assert_eq!(
            gitlink.old_content.as_deref(),
            Some(b"Subproject commit 2222222222222222222222222222222222222222\n".as_slice())
        );
        assert_eq!(gitlink.new_content, None);
        assert!(!gitlink.is_binary);
    }

    #[gpui::test]
    async fn test_load_commit_shallow_boundary(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let source_dir = tempfile::tempdir().expect("failed to create source repository");
        git_init_repo(source_dir.path());
        fs::write(source_dir.path().join("a.txt"), "one\n").expect("failed to write a.txt");
        git_command(source_dir.path(), ["add", "a.txt"]);
        git_command(source_dir.path(), ["commit", "-m", "first"]);
        fs::write(source_dir.path().join("a.txt"), "two\n").expect("failed to update a.txt");
        fs::write(source_dir.path().join("b.txt"), "new\n").expect("failed to write b.txt");
        git_command(source_dir.path(), ["add", "a.txt", "b.txt"]);
        git_command(source_dir.path(), ["commit", "-m", "second"]);

        let clone_dir = tempfile::tempdir().expect("failed to create clone directory");
        git_command(
            clone_dir.path(),
            [
                "clone".to_string(),
                "--depth=1".to_string(),
                format!("file://{}", source_dir.path().display()),
                "shallow".to_string(),
            ],
        );

        let repository = RealGitRepository::new(
            &clone_dir.path().join("shallow").join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open shallow repository");

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load boundary commit");
        assert!(commit_diff.is_shallow_boundary);
        assert_eq!(commit_diff.files.len(), 0);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), true, cx.to_async())
            .await
            .expect("failed to load boundary commit snapshot");
        assert!(!commit_diff.is_shallow_boundary);
        let files = commit_diff
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_unix_str().to_owned(),
                    file.old_content.clone(),
                    file.status(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            vec![
                ("a.txt".to_string(), None, CommitFileStatus::Added),
                ("b.txt".to_string(), None, CommitFileStatus::Added),
            ]
        );

        let source_repository = RealGitRepository::new(
            &source_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open source repository");
        let commit_diff = source_repository
            .load_commit("HEAD~1".to_string(), false, cx.to_async())
            .await
            .expect("failed to load root commit");
        assert!(!commit_diff.is_shallow_boundary);
        assert_eq!(commit_diff.files.len(), 1);
    }

    #[gpui::test]
    async fn test_check_access(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        assert!(repository.check_access().await.is_err());
        git_init_repo(repo_dir.path());
        assert!(repository.check_access().await.is_ok());
    }

    #[gpui::test]
    async fn test_real_git_repository_new_resolves_linked_worktree_paths(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktree_dir = temp_dir.path().join("worktree");
        git_init_repo(&repo_dir);
        fs::write(repo_dir.join("file.txt"), "initial").unwrap();
        git_command(&repo_dir, ["add", "file.txt"]);
        git_command(&repo_dir, ["commit", "-m", "initial"]);
        git_command(
            &repo_dir,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree_dir.as_os_str().into(),
            ],
        );

        let repository = RealGitRepository::new(
            &worktree_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        assert_same_path(
            repository.working_directory.as_ref().unwrap(),
            &worktree_dir,
        );
        assert_same_path(&repository.common_dir, repo_dir.join(".git"));
        assert_same_path(
            original_repo_path_from_common_dir(&repository.common_dir).unwrap(),
            repo_dir,
        );
    }

    #[gpui::test]
    async fn test_real_git_repository_new_supports_bare_repositories(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo.git");
        git_command(
            temp_dir.path(),
            [
                OsString::from("init"),
                OsString::from("--bare"),
                repo_dir.as_os_str().into(),
            ],
        );

        let repository =
            RealGitRepository::new(&repo_dir, None, Some("git".into()), cx.executor()).unwrap();

        assert_same_path(&repository.git_dir, &repo_dir);
        assert_same_path(&repository.common_dir, &repo_dir);
        assert_eq!(repository.working_directory, None);
        assert_same_path(repository.main_repository_path(), &repo_dir);
        assert_eq!(
            repository
                .git_binary()
                .run(&["rev-parse", "--is-bare-repository"])
                .await
                .unwrap(),
            "true"
        );
    }

    #[gpui::test]
    async fn test_change_branch_creates_local_tracking_branch_from_remote(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());

        let repository = RealGitRepository::new(
            &clone_directory.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let git = repository.git_binary_in_worktree().unwrap();
        assert!(
            git.run(&[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/remotes/origin/feature"
            ])
            .await
            .is_ok()
        );
        assert!(
            git.run(&["show-ref", "--verify", "--quiet", "refs/heads/feature"])
                .await
                .is_err()
        );

        repository
            .change_branch("origin/feature".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["branch", "--show-current"]).await.unwrap(),
            "feature"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "origin/feature"
        );

        git.run(&["checkout", "main"]).await.unwrap();
        git.run(&["branch", "--unset-upstream", "feature"])
            .await
            .unwrap();

        repository
            .change_branch("origin/feature".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["branch", "--show-current"]).await.unwrap(),
            "feature"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "origin/feature"
        );
    }

    #[gpui::test]
    async fn test_change_branch_resolves_remote_head_to_tracking_branch(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());

        let repository = RealGitRepository::new(
            &clone_directory.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let git = repository.git_binary_in_worktree().unwrap();
        git.run(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/feature",
        ])
        .await
        .unwrap();
        assert_eq!(
            git.run(&["symbolic-ref", "refs/remotes/origin/HEAD"])
                .await
                .unwrap(),
            "refs/remotes/origin/feature"
        );
        assert!(
            git.run(&["show-ref", "--verify", "--quiet", "refs/heads/feature"])
                .await
                .is_err()
        );

        repository
            .change_branch("origin/HEAD".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["branch", "--show-current"]).await.unwrap(),
            "feature"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "origin/feature"
        );
    }

    #[gpui::test]
    async fn test_change_branch_resolves_non_origin_remote_head(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());

        git_command(
            &clone_directory,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("upstream"),
                remote_directory.as_os_str().into(),
            ],
        );
        git_command(&clone_directory, ["fetch", "upstream"]);

        let repository = RealGitRepository::new(
            &clone_directory.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let git = repository.git_binary_in_worktree().unwrap();
        git.run(&[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ])
        .await
        .unwrap();
        git.run(&["checkout", "-b", "scratch"]).await.unwrap();

        repository
            .change_branch("upstream/HEAD".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["branch", "--show-current"]).await.unwrap(),
            "main"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "upstream/main"
        );
    }

    #[gpui::test]
    fn test_real_git_repository_new_rejects_malformed_git_file(cx: &mut TestAppContext) {
        disable_git_global_config();

        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_dir = temp_dir.path().join("worktree");
        fs::create_dir_all(&worktree_dir).unwrap();
        fs::write(worktree_dir.join(".git"), "not a gitdir file\n").unwrap();

        let error = match RealGitRepository::new(
            &worktree_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        ) {
            Ok(_) => panic!("malformed .git file should be rejected"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("expected .git file to start with 'gitdir: '"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn test_initial_graph_commit_data_tag_names() {
        let commit = InitialGraphCommitData {
            sha: Oid::from_bytes(&[0; 20]).unwrap(),
            parents: SmallVec::new(),
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
                SharedString::from("tag: v1.1.0"),
                SharedString::from("tag: "),
                SharedString::from("refs/heads/feature"),
            ],
        };

        assert_eq!(commit.tag_names(), ["v1.0.0", "v1.1.0"]);
    }

    #[test]
    fn test_parse_file_history_changed_files_output() {
        let queried_paths = vec![
            RepoPath::new("src/a.rs").unwrap(),
            RepoPath::new("src/b.rs").unwrap(),
        ];
        let output = concat!(
            "\x1e1111111111111111111111111111111111111111\0\nsrc/a.rs\0src/shared.rs\0",
            "\x1e2222222222222222222222222222222222222222\0\nsrc/b.rs\0src/shared.rs\0",
            "\x1e3333333333333333333333333333333333333333\0\nsrc/a.rs\0src/b.rs\0src/shared.rs\0",
        );

        let histories =
            parse_file_history_changed_files_output(output, &queried_paths, &HashSet::default());

        assert_eq!(histories.len(), 2);
        assert_eq!(
            histories[0].file_sets,
            vec![
                vec![
                    RepoPath::new("src/a.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
                vec![
                    RepoPath::new("src/a.rs").unwrap(),
                    RepoPath::new("src/b.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
            ]
        );
        assert_eq!(
            histories[1].file_sets,
            vec![
                vec![
                    RepoPath::new("src/b.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
                vec![
                    RepoPath::new("src/a.rs").unwrap(),
                    RepoPath::new("src/b.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
            ]
        );
    }

    #[test]
    fn test_parse_file_history_changed_files_output_skips_shallow_boundary() {
        let queried_paths = vec![RepoPath::new("src/a.rs").unwrap()];
        let output = concat!(
            "\x1e1111111111111111111111111111111111111111\0\nsrc/a.rs\0src/shared.rs\0",
            "\x1e2222222222222222222222222222222222222222\0\nsrc/a.rs\0src/b.rs\0src/shared.rs\0",
        );
        let shallow_boundary_oids =
            HashSet::from_iter(["2222222222222222222222222222222222222222".to_string()]);

        let histories =
            parse_file_history_changed_files_output(output, &queried_paths, &shallow_boundary_oids);

        assert_eq!(histories.len(), 1);
        assert_eq!(
            histories[0].file_sets,
            vec![vec![
                RepoPath::new("src/a.rs").unwrap(),
                RepoPath::new("src/shared.rs").unwrap(),
            ]]
        );
    }

    #[test]
    fn test_parse_initial_graph_output_filters_graft_decorations() {
        let line = "0f36a166633a057bf7dd660508d237cad2606cab\x00\x00grafted, HEAD -> refs/heads/main, refs/remotes/origin/main";
        let commits = parse_initial_graph_output([line].into_iter());
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].parents.len(), 0);
        assert_eq!(
            commits[0].ref_names,
            vec![
                SharedString::from("HEAD -> refs/heads/main"),
                SharedString::from("refs/remotes/origin/main"),
            ]
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_accepts_sha_log_source(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("file"), "initial").unwrap();
        git_command(repo_dir.path(), ["add", "file"]);
        git_command(repo_dir.path(), ["commit", "-m", "Initial commit"]);

        let commit_sha: Oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"])
            .parse()
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let (request_tx, request_rx) = async_channel::unbounded();

        repo.initial_graph_data(LogSource::Sha(commit_sha), LogOrder::DateOrder, request_tx)
            .await
            .unwrap();

        let graph_data = request_rx.recv().await.unwrap();
        assert_eq!(graph_data.len(), 1);
        assert_eq!(graph_data[0].sha, commit_sha);
    }

    /// A branch whose name also names a path in the working tree - `docs/rewrite` in a
    /// repository that also has a `docs/rewrite` directory - made git reject the revision
    /// as ambiguous, so the git panel's History tab reported "Failed to load commit
    /// history" and commit search silently returned nothing.
    #[gpui::test]
    async fn test_initial_graph_data_with_branch_named_after_a_path(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::create_dir_all(repo_dir.path().join("docs/rewrite")).unwrap();
        fs::write(repo_dir.path().join("docs/rewrite/notes.md"), "notes").unwrap();
        git_command(repo_dir.path(), ["add", "."]);
        git_command(repo_dir.path(), ["commit", "-m", "Add rewrite notes"]);
        git_command(repo_dir.path(), ["checkout", "-b", "docs/rewrite"]);

        let commit_sha: Oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"])
            .parse()
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let (request_tx, request_rx) = async_channel::unbounded();

        repo.initial_graph_data(
            LogSource::Branch("docs/rewrite".into()),
            LogOrder::DateOrder,
            request_tx,
        )
        .await
        .unwrap();

        let graph_data = request_rx.recv().await.unwrap();
        assert_eq!(graph_data.len(), 1);
        assert_eq!(graph_data[0].sha, commit_sha);
    }

    /// The branch diff passes the base ref straight to `git diff`, so a branch named after
    /// a path in the working tree broke it the same way. `git diff` takes the same `--`
    /// terminator, so the base ref is terminated like the log sources are.
    #[gpui::test]
    async fn test_merge_base_worktree_diff_with_branch_named_after_a_path(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::create_dir_all(repo_dir.path().join("docs/rewrite")).unwrap();
        let notes_path = repo_dir.path().join("docs/rewrite/notes.md");
        fs::write(&notes_path, "notes\n").unwrap();
        git_command(repo_dir.path(), ["add", "."]);
        git_command(repo_dir.path(), ["commit", "-m", "Add rewrite notes"]);
        git_command(repo_dir.path(), ["checkout", "-b", "docs/rewrite"]);

        let base_oid: Oid =
            git_command_output(repo_dir.path(), ["rev-parse", "HEAD:docs/rewrite/notes.md"])
                .parse()
                .unwrap();

        fs::write(&notes_path, "edited\n").unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let diff = repo
            .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                base: "docs/rewrite".into(),
            })
            .await
            .unwrap();

        assert_eq!(
            diff,
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("docs/rewrite/notes.md").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    /// `show` takes the revision the user typed in the "open commit by ref" input, so a
    /// branch named after a path in the working tree reaches git unqualified there too.
    #[gpui::test]
    async fn test_show_with_branch_named_after_a_path(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::create_dir_all(repo_dir.path().join("docs/rewrite")).unwrap();
        fs::write(repo_dir.path().join("docs/rewrite/notes.md"), "notes").unwrap();
        git_command(repo_dir.path(), ["add", "."]);
        git_command(repo_dir.path(), ["commit", "-m", "Add rewrite notes"]);
        git_command(repo_dir.path(), ["checkout", "-b", "docs/rewrite"]);

        let commit_sha: Oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"])
            .parse()
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let details = repo.show("docs/rewrite".to_string()).await.unwrap();
        assert_eq!(details.sha.as_ref(), commit_sha.to_string());
    }

    #[test]
    fn test_log_source_terminates_revisions() {
        // Every revision-taking source has to end the revision list, or git cannot tell a
        // branch name from an identically named path. `Path` is the exception: it states
        // the separator itself, before the path it is passing.
        for source in [
            LogSource::All,
            LogSource::Branch("docs/rewrite".into()),
            LogSource::Sha(Oid::from_str("0000000000000000000000000000000000000000").unwrap()),
        ] {
            let args = source.get_args();
            assert_eq!(
                args.last().map(|arg| arg.as_ref()),
                Some("--"),
                "{source:?} must terminate its revisions"
            );
        }

        let path_source = LogSource::Path(RepoPath::new("docs/rewrite").unwrap());
        let path_args = path_source.get_args();
        assert_eq!(
            path_args.iter().filter(|arg| arg.as_ref() == "--").count(),
            1,
            "Path states the separator itself and must not gain a second one"
        );
    }

    #[gpui::test]
    async fn test_build_command_untrusted_includes_both_safety_args(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["version"])
            .output()
            .await
            .expect("git version should succeed");
        assert!(output.status.success());

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["config", "--get", "core.fsmonitor"])
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "fsmonitor should be disabled for untrusted repos"
        );

        git_init_repo(dir.path());
        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["config", "--get", "core.hooksPath"])
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "/dev/null",
            "hooksPath should be /dev/null for untrusted repos"
        );
    }

    #[gpui::test]
    async fn test_build_command_trusted_only_disables_fsmonitor(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            true,
        );
        let output = git
            .build_command(&["config", "--get", "core.fsmonitor"])
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "fsmonitor should be disabled even for trusted repos"
        );

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            true,
        );
        let output = git
            .build_command(&["config", "--get", "core.hooksPath"])
            .output()
            .await
            .expect("git config should run");
        assert!(
            !output.status.success(),
            "hooksPath should NOT be overridden for trusted repos"
        );
    }

    #[gpui::test]
    async fn test_build_command_disables_log_show_signature(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            true,
        );
        let output = git
            .build_command(&["config", "--get", "log.showSignature"])
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "log.showSignature should be disabled for trusted repos"
        );

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["config", "--get", "log.showSignature"])
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "log.showSignature should be disabled for untrusted repos"
        );
    }

    #[gpui::test]
    async fn test_path_for_index_id_uses_real_git_directory(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let working_directory = PathBuf::from("/code/worktree");
        let git_directory = PathBuf::from("/code/repo/.git/modules/worktree");
        let git = GitBinary::new(
            PathBuf::from("git"),
            working_directory,
            git_directory.clone(),
            cx.executor(),
            false,
        );

        let path = git.path_for_index_id(Uuid::nil());

        assert_eq!(
            path,
            git_directory.join(format!("index-{}.tmp", Uuid::nil()))
        );
    }

    #[gpui::test]
    async fn test_checkpoint_basic(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file");
        smol::fs::write(&file_path, "initial").await.unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        smol::fs::write(&file_path, "modified before checkpoint")
            .await
            .unwrap();
        smol::fs::write(repo_dir.path().join("new_file_before_checkpoint"), "1")
            .await
            .unwrap();
        let checkpoint = repo.checkpoint().await.unwrap();

        // Ensure the user can't see any branches after creating a checkpoint.
        assert_eq!(repo.branches().await.unwrap().branches.len(), 1);

        smol::fs::write(&file_path, "modified after checkpoint")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Commit after checkpoint".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        smol::fs::remove_file(repo_dir.path().join("new_file_before_checkpoint"))
            .await
            .unwrap();
        smol::fs::write(repo_dir.path().join("new_file_after_checkpoint"), "2")
            .await
            .unwrap();

        // Ensure checkpoint stays alive even after a Git GC.
        repo.gc().await.unwrap();
        repo.restore_checkpoint(checkpoint.clone()).await.unwrap();

        assert_eq!(
            smol::fs::read_to_string(&file_path).await.unwrap(),
            "modified before checkpoint"
        );
        assert_eq!(
            smol::fs::read_to_string(repo_dir.path().join("new_file_before_checkpoint"))
                .await
                .unwrap(),
            "1"
        );
        // See TODO above
        // assert_eq!(
        //     smol::fs::read_to_string(repo_dir.path().join("new_file_after_checkpoint"))
        //         .await
        //         .ok(),
        //     None
        // );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_commit_runs_git_hooks(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let hooks_dir = repo_dir.path().join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let write_hook = |name: &str, contents: &str| {
            let path = hooks_dir.join(name);
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };

        write_hook("pre-commit", "#!/bin/sh\nexit 1\n");

        fs::write(repo_dir.path().join("file"), "one").unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        // Hooks must not run for untrusted repositories.
        repo.commit(
            "Commit in untrusted repo".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .expect("failing pre-commit hook should be skipped in untrusted repos");

        repo.set_trusted(true);

        fs::write(repo_dir.path().join("file"), "two").unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        repo.commit(
            "Commit blocked by hook".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .expect_err("failing pre-commit hook should abort the commit");

        write_hook("pre-commit", "#!/bin/sh\nexit 0\n");
        write_hook(
            "commit-msg",
            "#!/bin/sh\necho 'rewritten by commit-msg hook' > \"$1\"\n",
        );

        repo.commit(
            "Original message".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        let message = git_command_output(repo_dir.path(), ["log", "-1", "--pretty=%B"]);
        assert_eq!(message, "rewritten by commit-msg hook");

        write_hook("pre-commit", "#!/bin/sh\nexit 1\n");
        fs::write(repo_dir.path().join("file"), "three").unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        repo.commit(
            "Commit without verification".into(),
            None,
            CommitOptions {
                no_verify: true,
                ..Default::default()
            },
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .expect("--no-verify should skip pre-commit and commit-msg hooks");

        let message = git_command_output(repo_dir.path(), ["log", "-1", "--pretty=%B"]);
        assert_eq!(message, "Commit without verification");
    }

    #[gpui::test]
    async fn test_load_revisions(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let file1_path = repo_dir.path().join("file1");
        let file2_path = repo_dir.path().join("file2");
        let space_file_path = repo_dir.path().join("file with spaces");

        smol::fs::write(&file1_path, "file1 committed contents")
            .await
            .unwrap();
        smol::fs::write(&file2_path, "file2 committed contents")
            .await
            .unwrap();
        smol::fs::write(&space_file_path, "space file committed contents")
            .await
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        // Stage files and commit
        repo.stage_paths(
            vec![
                repo_path("file1"),
                repo_path("file2"),
                repo_path("file with spaces"),
            ],
            Arc::new(HashMap::default()),
        )
        .await
        .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // Now modify files in index but not yet committed
        smol::fs::write(&file1_path, "file1 index contents")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file1")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        // Write working tree contents (not indexed, not committed)
        smol::fs::write(&file1_path, "file1 worktree contents")
            .await
            .unwrap();

        // Now test load_revisions
        let results = repo
            .load_revisions(
                [
                    "HEAD:file1",
                    ":file1",
                    "HEAD:file2",
                    ":file2",
                    "HEAD:nonexistent",
                    "HEAD:file with spaces",
                    "HEAD:nonexistent file with spaces",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
            )
            .await
            .unwrap();

        assert_eq!(
            results,
            vec![
                Some(b"file1 committed contents".to_vec()),
                Some(b"file1 index contents".to_vec()),
                Some(b"file2 committed contents".to_vec()),
                Some(b"file2 committed contents".to_vec()), // untouched in index, should match HEAD
                None,
                Some(b"space file committed contents".to_vec()),
                None,
            ]
        );
    }

    #[gpui::test]
    async fn test_blame_at_revision(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let file_name = "ürlich file1";
        fs::write(repo_dir.path().join(file_name), "line one\n").unwrap();
        git_command(repo_dir.path(), ["add", "-A"]);
        git_command(repo_dir.path(), ["commit", "-m", "First commit"]);
        let first_sha = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);

        fs::write(repo_dir.path().join(file_name), "line one\nline two\n").unwrap();
        git_command(repo_dir.path(), ["add", "-A"]);
        git_command(repo_dir.path(), ["commit", "-m", "Second commit"]);
        let second_sha = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);

        let blame_at_head = repo
            .blame_at_revision(repo_path(file_name), second_sha.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            blame_at_head
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.sha.to_string(),
                        entry.range.clone(),
                        entry.filename.clone(),
                        entry.previous.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (first_sha.clone(), 0..1, file_name.to_owned(), None),
                (
                    second_sha.clone(),
                    1..2,
                    file_name.to_owned(),
                    Some(format!("{first_sha} {file_name}"))
                ),
            ]
        );

        let blame_at_first = repo
            .blame_at_revision(repo_path(file_name), first_sha.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            blame_at_first
                .entries
                .iter()
                .map(|entry| (entry.sha.to_string(), entry.range.clone()))
                .collect::<Vec<_>>(),
            vec![(first_sha.clone(), 0..1)]
        );
    }

    #[gpui::test]
    async fn test_checkpoint_empty_repo(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        smol::fs::write(repo_dir.path().join("foo"), "foo")
            .await
            .unwrap();
        let checkpoint_sha = repo.checkpoint().await.unwrap();

        // Ensure the user can't see any branches after creating a checkpoint.
        assert_eq!(repo.branches().await.unwrap().branches.len(), 1);

        smol::fs::write(repo_dir.path().join("foo"), "bar")
            .await
            .unwrap();
        smol::fs::write(repo_dir.path().join("baz"), "qux")
            .await
            .unwrap();
        repo.restore_checkpoint(checkpoint_sha).await.unwrap();
        assert_eq!(
            smol::fs::read_to_string(repo_dir.path().join("foo"))
                .await
                .unwrap(),
            "foo"
        );
        // See TODOs above
        // assert_eq!(
        //     smol::fs::read_to_string(repo_dir.path().join("baz"))
        //         .await
        //         .ok(),
        //     None
        // );
    }

    #[gpui::test]
    async fn test_branches_return_head_when_commit_metadata_cannot_be_read(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        smol::fs::write(repo_dir.path().join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        smol::fs::write(
            repo_dir.path().join(".git").join("refs/heads/broken"),
            "0a103ede22f159c792dc6405e0c8304d9bd4dc29\n",
        )
        .await
        .unwrap();

        let branches_scan = repo.branches().await.unwrap();
        assert!(branches_scan.error.is_some());
        let head_branch = branches_scan
            .branches
            .iter()
            .find(|branch| branch.is_head)
            .expect("branch list should include HEAD");
        assert!(head_branch.ref_name.starts_with("refs/heads/"));

        assert!(
            branches_scan
                .branches
                .iter()
                .all(|branch| branch.ref_name.as_ref() != "refs/heads/broken")
        );
    }

    #[gpui::test]
    async fn test_compare_checkpoints(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        smol::fs::write(repo_dir.path().join("file1"), "content1")
            .await
            .unwrap();
        let checkpoint1 = repo.checkpoint().await.unwrap();

        smol::fs::write(repo_dir.path().join("file2"), "content2")
            .await
            .unwrap();
        let checkpoint2 = repo.checkpoint().await.unwrap();

        assert!(
            !repo
                .compare_checkpoints(checkpoint1, checkpoint2.clone())
                .await
                .unwrap()
        );

        let checkpoint3 = repo.checkpoint().await.unwrap();
        assert!(
            repo.compare_checkpoints(checkpoint2, checkpoint3)
                .await
                .unwrap()
        );
    }

    #[gpui::test]
    async fn test_checkpoint_exclude_binary_files(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        let text_path = repo_dir.path().join("main.rs");
        let bin_path = repo_dir.path().join("binary.o");

        git_init_repo(repo_dir.path());

        smol::fs::write(&text_path, "fn main() {}").await.unwrap();

        smol::fs::write(&bin_path, "some binary file here")
            .await
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        // initial commit
        repo.stage_paths(vec![repo_path("main.rs")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        let checkpoint = repo.checkpoint().await.unwrap();

        smol::fs::write(&text_path, "fn main() { println!(\"Modified\"); }")
            .await
            .unwrap();
        smol::fs::write(&bin_path, "Modified binary file")
            .await
            .unwrap();

        repo.restore_checkpoint(checkpoint).await.unwrap();

        // Text files should be restored to checkpoint state,
        // but binaries should not (they aren't tracked)
        assert_eq!(
            smol::fs::read_to_string(&text_path).await.unwrap(),
            "fn main() {}"
        );

        assert_eq!(
            smol::fs::read_to_string(&bin_path).await.unwrap(),
            "Modified binary file"
        );
    }

    #[test]
    fn test_branches_parsing() {
        // suppress "help: octal escapes are not supported, `\0` is always null"
        #[allow(clippy::octal_escapes)]
        let input = "*\0060964da10574cd9bf06463a53bf6e0769c5c45e\0\0refs/heads/zed-patches\0refs/remotes/origin/zed-patches\0\01733187470\0John Doe\0generated protobuf\n";
        assert_eq!(
            parse_branch_input(input).unwrap(),
            vec![Branch {
                is_head: true,
                ref_name: "refs/heads/zed-patches".into(),
                upstream: Some(Upstream {
                    ref_name: "refs/remotes/origin/zed-patches".into(),
                    tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                        ahead: 0,
                        behind: 0
                    })
                }),
                most_recent_commit: Some(CommitSummary {
                    sha: "060964da10574cd9bf06463a53bf6e0769c5c45e".into(),
                    subject: "generated protobuf".into(),
                    commit_timestamp: 1733187470,
                    author_name: SharedString::new_static("John Doe"),
                    has_parent: false,
                })
            }]
        )
    }

    #[test]
    fn test_branches_parsing_containing_refs_with_missing_fields() {
        #[allow(clippy::octal_escapes)]
        let input = " \090012116c03db04344ab10d50348553aa94f1ea0\0refs/heads/broken\n \0eb0cae33272689bd11030822939dd2701c52f81e\0895951d681e5561478c0acdd6905e8aacdfd2249\0refs/heads/dev\0\0\01762948725\0Zed\0Add feature\n*\0895951d681e5561478c0acdd6905e8aacdfd2249\0\0refs/heads/main\0\0\01762948695\0Zed\0Initial commit\n";

        let branches = parse_branch_input(input).unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(
            branches,
            vec![
                Branch {
                    is_head: false,
                    ref_name: "refs/heads/dev".into(),
                    upstream: None,
                    most_recent_commit: Some(CommitSummary {
                        sha: "eb0cae33272689bd11030822939dd2701c52f81e".into(),
                        subject: "Add feature".into(),
                        commit_timestamp: 1762948725,
                        author_name: SharedString::new_static("Zed"),
                        has_parent: true,
                    })
                },
                Branch {
                    is_head: true,
                    ref_name: "refs/heads/main".into(),
                    upstream: None,
                    most_recent_commit: Some(CommitSummary {
                        sha: "895951d681e5561478c0acdd6905e8aacdfd2249".into(),
                        subject: "Initial commit".into(),
                        commit_timestamp: 1762948695,
                        author_name: SharedString::new_static("Zed"),
                        has_parent: false,
                    })
                }
            ]
        )
    }

    #[test]
    fn test_upstream_branch_name() {
        let upstream = Upstream {
            ref_name: "refs/remotes/origin/feature/branch".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), Some("feature/branch"));

        let upstream = Upstream {
            ref_name: "refs/remotes/upstream/main".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), Some("main"));

        let upstream = Upstream {
            ref_name: "refs/heads/local".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), None);

        // Test case where upstream branch name differs from what might be the local branch name
        let upstream = Upstream {
            ref_name: "refs/remotes/origin/feature/git-pull-request".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), Some("feature/git-pull-request"));
    }

    #[test]
    fn test_parse_worktrees_from_str() {
        // Empty input
        let result = parse_worktrees_from_str("", None);
        assert!(result.is_empty());

        // Single worktree (main)
        let input = "worktree /home/user/project\nHEAD abc123def\nbranch refs/heads/main\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].sha.as_ref(), "abc123def");
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
        assert!(!result[0].is_bare);

        // Multiple worktrees
        let input = "worktree /home/user/project-wt\nHEAD def456\nbranch refs/heads/feature\n\n\
                      worktree /home/user/project\nHEAD abc123\nbranch refs/heads/main\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project-wt"));
        assert_eq!(result[0].ref_name, Some("refs/heads/feature".into()));
        assert!(!result[0].is_main);
        assert!(!result[0].is_bare);
        assert_eq!(result[1].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[1].ref_name, Some("refs/heads/main".into()));
        assert!(result[1].is_main);
        assert!(!result[1].is_bare);

        // Detached HEAD entry (included with ref_name: None)
        let input = "worktree /home/user/project\nHEAD abc123\nbranch refs/heads/main\n\n\
                      worktree /home/user/detached\nHEAD def456\ndetached\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
        assert_eq!(result[1].path, PathBuf::from("/home/user/detached"));
        assert_eq!(result[1].ref_name, None);
        assert_eq!(result[1].sha.as_ref(), "def456");
        assert!(!result[1].is_main);
        assert!(!result[1].is_bare);

        // Bare repo entry with no main worktree.
        let input = "worktree /home/user/bare.git\nHEAD abc123\nbare\n\n\
                      worktree /home/user/project\nHEAD def456\nbranch refs/heads/main\n\n";
        let result = parse_worktrees_from_str(input, None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, PathBuf::from("/home/user/bare.git"));
        assert_eq!(result[0].ref_name, None);
        assert!(!result[0].is_main);
        assert!(result[0].is_bare);
        assert_eq!(result[1].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[1].ref_name, Some("refs/heads/main".into()));
        assert!(!result[1].is_main);
        assert!(!result[1].is_bare);

        // Extra porcelain lines (locked, prunable) should be ignored
        let input = "worktree /home/user/project\nHEAD abc123\nbranch refs/heads/main\n\n\
                      worktree /home/user/locked-wt\nHEAD def456\nbranch refs/heads/locked-branch\nlocked\n\n\
                      worktree /home/user/prunable-wt\nHEAD 789aaa\nbranch refs/heads/prunable-branch\nprunable\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
        assert_eq!(result[1].path, PathBuf::from("/home/user/locked-wt"));
        assert_eq!(result[1].ref_name, Some("refs/heads/locked-branch".into()));
        assert!(!result[1].is_main);
        assert_eq!(result[2].path, PathBuf::from("/home/user/prunable-wt"));
        assert_eq!(
            result[2].ref_name,
            Some("refs/heads/prunable-branch".into())
        );
        assert!(!result[2].is_main);

        // Leading/trailing whitespace on lines should be tolerated
        let input =
            "  worktree /home/user/project  \n  HEAD abc123  \n  branch refs/heads/main  \n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].sha.as_ref(), "abc123");
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);

        // Windows-style line endings should be handled
        let input = "worktree /home/user/project\r\nHEAD abc123\r\nbranch refs/heads/main\r\n\r\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].sha.as_ref(), "abc123");
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
    }

    #[gpui::test]
    async fn test_create_and_list_worktrees(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktrees_dir = temp_dir.path().join("worktrees");

        fs::create_dir_all(&repo_dir).unwrap();
        fs::create_dir_all(&worktrees_dir).unwrap();

        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        // Create an initial commit (required for worktrees)
        smol::fs::write(repo_dir.join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // List worktrees — should have just the main one
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(
            worktrees[0].path.canonicalize().unwrap(),
            repo_dir.canonicalize().unwrap()
        );

        let worktree_path = worktrees_dir.join("some-worktree");

        // Create a new worktree
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "test-branch".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            worktree_path.clone(),
        )
        .await
        .unwrap();

        // List worktrees — should have two
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 2);

        let new_worktree = worktrees
            .iter()
            .find(|w| w.display_name() == "test-branch")
            .expect("should find worktree with test-branch");
        assert_eq!(
            new_worktree.path.canonicalize().unwrap(),
            worktree_path.canonicalize().unwrap(),
        );

        // The new worktree's git metadata directory should report a creation
        // time, resolved via the worktree's `.git` file.
        let created_at = repo
            .worktree_created_at(worktree_path.clone())
            .await
            .unwrap();
        assert!(
            created_at.is_some(),
            "creation time should be available for a freshly created worktree"
        );

        // A path with no worktree at all reports `None`.
        let missing = repo
            .worktree_created_at(worktrees_dir.join("does-not-exist"))
            .await
            .unwrap();
        assert_eq!(missing, None);
    }

    #[gpui::test]
    async fn test_remove_worktree(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktrees_dir = temp_dir.path().join("worktrees");
        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        // Create an initial commit
        smol::fs::write(repo_dir.join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // Create a worktree
        let worktree_path = worktrees_dir.join("worktree-to-remove");
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "to-remove".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            worktree_path.clone(),
        )
        .await
        .unwrap();

        // Remove the worktree
        repo.remove_worktree(worktree_path.clone(), false)
            .await
            .unwrap();

        // Verify the directory is removed
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1);
        assert!(
            worktrees.iter().all(|w| w.display_name() != "to-remove"),
            "removed worktree should not appear in list"
        );
        assert!(!worktree_path.exists());

        // Create a worktree
        let worktree_path = worktrees_dir.join("dirty-wt");
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "dirty-wt".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            worktree_path.clone(),
        )
        .await
        .unwrap();

        assert!(worktree_path.exists());

        // Add uncommitted changes in the worktree
        smol::fs::write(worktree_path.join("dirty-file.txt"), "uncommitted")
            .await
            .unwrap();

        // Non-force removal should fail with dirty worktree
        let result = repo.remove_worktree(worktree_path.clone(), false).await;
        assert!(
            result.is_err(),
            "non-force removal of dirty worktree should fail"
        );

        // Force removal should succeed
        repo.remove_worktree(worktree_path.clone(), true)
            .await
            .unwrap();

        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1);
        assert!(!worktree_path.exists());
    }

    #[gpui::test]
    async fn test_rename_worktree(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktrees_dir = temp_dir.path().join("worktrees");

        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        // Create an initial commit
        smol::fs::write(repo_dir.join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // Create a worktree
        let old_path = worktrees_dir.join("old-worktree-name");
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "old-name".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            old_path.clone(),
        )
        .await
        .unwrap();

        assert!(old_path.exists());

        // Move the worktree to a new path
        let new_path = worktrees_dir.join("new-worktree-name");
        repo.rename_worktree(old_path.clone(), new_path.clone())
            .await
            .unwrap();

        // Verify the old path is gone and new path exists
        assert!(!old_path.exists());
        assert!(new_path.exists());

        // Verify it shows up in worktree list at the new path
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 2);
        let moved_worktree = worktrees
            .iter()
            .find(|w| w.display_name() == "old-name")
            .expect("should find worktree by branch name");
        assert_eq!(
            moved_worktree.path.canonicalize().unwrap(),
            new_path.canonicalize().unwrap()
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_ref_set(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let git = repo.git_binary();

        let graph_commits = async || {
            let (tx, rx) = smol::channel::unbounded();
            repo.initial_graph_data(LogSource::All, LogOrder::DateOrder, tx)
                .await
                .unwrap();
            let mut commits = std::collections::HashSet::new();
            while let Ok(chunk) = rx.try_recv() {
                for commit in chunk {
                    commits.insert(commit.sha);
                }
            }
            commits
        };

        smol::fs::write(repo_dir.path().join("file1"), "1")
            .await
            .unwrap();
        let branch_sha = repo.checkpoint().await.unwrap().commit_sha;
        repo.update_ref("refs/heads/main".into(), branch_sha.to_string())
            .await
            .unwrap();

        smol::fs::write(repo_dir.path().join("file2"), "2")
            .await
            .unwrap();
        let hidden_sha = repo.checkpoint().await.unwrap().commit_sha;
        repo.update_ref("refs/custom/hidden".into(), hidden_sha.to_string())
            .await
            .unwrap();

        let graph = graph_commits().await;
        assert!(graph.contains(&branch_sha));
        assert!(!graph.contains(&hidden_sha));

        git.build_command(&["update-ref", "--no-deref", "HEAD", &hidden_sha.to_string()])
            .output()
            .await
            .unwrap();

        let graph = graph_commits().await;
        assert!(graph.contains(&branch_sha));
        assert!(graph.contains(&hidden_sha));
    }

    #[gpui::test]
    async fn test_check_for_pushed_commit(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        // New repo doesn't have any commits yet
        assert!(repo.check_for_pushed_commit().await.unwrap().is_empty());

        git_command(
            &repo_dir,
            ["commit", "--allow-empty", "-m", "Initial commit"],
        );

        // No remote branches exist yet
        assert!(repo.check_for_pushed_commit().await.unwrap().is_empty());

        // Create simulated remote branches
        git_command(
            &repo_dir,
            ["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git_command(
            &repo_dir,
            ["update-ref", "refs/remotes/origin/other-branch", "HEAD"],
        );
        assert_eq!(
            repo.check_for_pushed_commit().await.unwrap(),
            vec![
                SharedString::from("origin/main"),
                SharedString::from("origin/other-branch")
            ]
        );

        // Switch to a new branch, commit but do not push
        git_command(&repo_dir, ["switch", "-c", "local-feature"]);
        git_command(&repo_dir, ["commit", "--allow-empty", "-m", "Local commit"]);

        // New commit has not been pushed
        assert!(repo.check_for_pushed_commit().await.unwrap().is_empty());
    }

    #[test]
    fn test_original_repo_path_from_common_dir() {
        // Normal repo: common_dir is <work_dir>/.git
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/code/zed5/.git")),
            Some(PathBuf::from("/code/zed5"))
        );

        // Worktree: common_dir is the main repo's .git
        // (same result — that's the point, it always traces back to the original)
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/code/zed5/.git")),
            Some(PathBuf::from("/code/zed5"))
        );

        // Bare repo: no .git suffix, returns None (no working-tree root)
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/code/zed5.git")),
            None
        );

        // Root-level .git directory
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/.git")),
            Some(PathBuf::from("/"))
        );
    }

    #[gpui::test]
    async fn test_default_branch(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        assert_eq!(repo.default_branch(false).await.unwrap(), None);

        git_command(
            repo_dir.path(),
            ["commit", "--allow-empty", "-m", "Initial commit"],
        );

        assert_eq!(
            repo.default_branch(false).await.unwrap(),
            Some("main".into())
        );

        git_command(
            repo_dir.path(),
            ["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git_command(
            repo_dir.path(),
            [
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        assert_eq!(
            repo.default_branch(false).await.unwrap(),
            Some("main".into())
        );
        assert_eq!(
            repo.default_branch(true).await.unwrap(),
            Some("origin/main".into())
        );
    }

    impl RealGitRepository {
        /// Force a Git garbage collection on the repository.
        fn gc(&self) -> BoxFuture<'_, Result<()>> {
            let working_directory = self.command_directory();
            let git_directory = self.path();
            let git_binary_path = self.any_git_binary_path.clone();
            let executor = self.executor.clone();
            self.executor
                .spawn(async move {
                    let git_binary_path = git_binary_path.clone();
                    let git = GitBinary::new(
                        git_binary_path,
                        working_directory,
                        git_directory,
                        executor,
                        true,
                    );
                    git.run(&["gc", "--prune"]).await?;
                    Ok(())
                })
                .boxed()
        }
    }

    #[test]
    fn test_parse_remote_urls() {
        let stdout = concat!(
            "origin\thttps://github.com/zed-industries/zed.git (fetch) [blob:none]\n",
            "origin\thttps://github.com/zed-industries/zed.git (push)\n",
            "upstream\t/Users/user/My Projects/upstream.git (fetch)\n",
            "upstream\t/Users/user/My Projects/upstream.git (push)\n",
            "a\t/x (fetch) dir (fetch)\n",
            "a\t/x (fetch) dir (push)\n",
            "archive\t/tmp/remote [archive].git (fetch)\n",
            "archive\t/tmp/remote [archive].git (push)\n",
        );

        let remote_urls = parse_remote_urls(stdout);
        assert_eq!(remote_urls.len(), 4);
        assert_eq!(
            remote_urls.get("origin").map(String::as_str),
            Some("https://github.com/zed-industries/zed.git")
        );
        assert_eq!(
            remote_urls.get("upstream").map(String::as_str),
            Some("/Users/user/My Projects/upstream.git")
        );
        assert_eq!(
            remote_urls.get("a").map(String::as_str),
            Some("/x (fetch) dir")
        );
        assert_eq!(
            remote_urls.get("archive").map(String::as_str),
            Some("/tmp/remote [archive].git")
        );
    }

    #[gpui::test]
    async fn test_remote_urls(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let git = repo.git_binary();
        git.run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/zed-industries/zed.git",
        ])
        .await
        .unwrap();
        git.run(&[
            "remote",
            "add",
            "upstream",
            "/Users/user/My Projects/upstream.git",
        ])
        .await
        .unwrap();
        git.run(&["config", "remote.origin.promisor", "true"])
            .await
            .unwrap();
        git.run(&["config", "remote.origin.partialclonefilter", "blob:none"])
            .await
            .unwrap();

        let remote_urls = repo.remote_urls().await;
        assert_eq!(remote_urls.len(), 2);
        assert_eq!(
            remote_urls.get("origin").unwrap(),
            "https://github.com/zed-industries/zed.git"
        );
        assert_eq!(
            remote_urls.get("upstream").unwrap(),
            "/Users/user/My Projects/upstream.git"
        );
    }
}

#[cfg(test)]
mod review_pr_fetch_tests {
    use super::*;
    use gpui::TestAppContext;

    async fn git(cx: &TestAppContext, path: &Path, args: &[&str]) -> String {
        let binary = GitBinary::new(
            "git".into(),
            path.to_path_buf(),
            path.join(".git"),
            cx.executor(),
            true,
        );
        let output = binary
            .build_command(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "The fixture executable and process-liveness probe are not Git commands; Git subprocesses still use GitBinary::build_command"
    )]
    fn fixture_process(program: impl AsRef<OsStr>) -> util::command::Command {
        new_command(program)
    }

    #[gpui::test]
    async fn stalled_pr_fetch_process_fixture(cx: &mut TestAppContext) {
        let Some(root) = std::env::var_os("ZED_PR_FETCH_FIXTURE_ROOT") else {
            return;
        };
        cx.executor().allow_parking();
        let root = PathBuf::from(root);
        let base = std::env::var("ZED_PR_FETCH_FIXTURE_BASE").unwrap();
        let head = std::env::var("ZED_PR_FETCH_FIXTURE_HEAD").unwrap();
        let oid = std::env::var("ZED_PR_FETCH_FIXTURE_OID").unwrap();
        git(cx, &root, &["update-ref", &base, &oid]).await;
        git(cx, &root, &["update-ref", &head, &oid]).await;
        std::fs::write(
            std::env::var_os("ZED_PR_FETCH_FIXTURE_MARKER").unwrap(),
            std::process::id().to_string(),
        )
        .unwrap();
        // This dedicated child intentionally remains alive until the parent's
        // cancellation path kills and reaps it; never block the parent executor.
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn cancelled_pr_attempt_reaps_child_cleans_refs_and_allows_status(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let local = tempfile::tempdir().unwrap();
        git(cx, local.path(), &["init", "-b", "main"]).await;
        git(cx, local.path(), &["config", "user.name", "Fixture"]).await;
        git(
            cx,
            local.path(),
            &["config", "user.email", "fixture@example.invalid"],
        )
        .await;
        git(cx, local.path(), &["commit", "--allow-empty", "-m", "base"]).await;
        let oid = Oid::from_str(&git(cx, local.path(), &["rev-parse", "HEAD"]).await).unwrap();
        let repo = RealGitRepository::new(
            &local.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let binary = repo.git_binary();
        let refs = VerifiedReviewPr {
            base: oid,
            head: oid,
            base_ref: "refs/zed/review/pr/42/cancel/base".into(),
            head_ref: "refs/zed/review/pr/42/cancel/head".into(),
        };
        let marker = local.path().join("started");
        let mut command = fixture_process(std::env::current_exe().unwrap());
        let fixture_name = format!(
            "{}::stalled_pr_fetch_process_fixture",
            module_path!().split_once("::").unwrap().1
        );
        command
            .args(["--exact", &fixture_name, "--nocapture"])
            .env("ZED_PR_FETCH_FIXTURE_ROOT", local.path())
            .env("ZED_PR_FETCH_FIXTURE_BASE", &refs.base_ref)
            .env("ZED_PR_FETCH_FIXTURE_HEAD", &refs.head_ref)
            .env("ZED_PR_FETCH_FIXTURE_OID", oid.to_string())
            .env("ZED_PR_FETCH_FIXTURE_MARKER", &marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let task_binary = binary.clone();
        let task_refs = refs.clone();
        let task = cx.spawn(move |mut cx| async move {
            let mut attempt = ReviewPrAttempt::new(task_binary, task_refs);
            let delegate =
                AskPassDelegate::new(&mut cx, |_, _, _| panic!("unexpected credential prompt"));
            let environment = Arc::new(HashMap::from_iter([(
                "GIT_ASKPASS".into(),
                "/dev/null".into(),
            )]));
            run_git_command_inner(
                environment,
                delegate,
                command,
                cx.background_executor().clone(),
                Some(&mut attempt),
            )
            .await
        });
        // The external child uses wall time; pump GPUI while waiting for its marker.
        for _ in 0..200 {
            cx.run_until_parked();
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(marker.exists(), "stalled process never started");
        let pid = std::fs::read_to_string(&marker).unwrap();
        assert!(
            binary.run(&["status", "--porcelain"]).await.is_ok(),
            "ordinary Git must progress while fetch stalls"
        );
        drop(task);
        for _ in 0..200 {
            cx.run_until_parked();
            if git(
                cx,
                local.path(),
                &["for-each-ref", "refs/zed/review/pr/42/cancel"],
            )
            .await
            .is_empty()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            git(
                cx,
                local.path(),
                &["for-each-ref", "refs/zed/review/pr/42/cancel"]
            )
            .await
            .is_empty()
        );
        assert!(
            !fixture_process("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .await
                .unwrap()
                .success()
        );
        assert!(binary.run(&["status", "--porcelain"]).await.is_ok());
    }

    #[gpui::test]
    async fn pinned_pr_fetch_isolated_refs_preserve_checkout_and_reject_races(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        let remote = tempfile::tempdir().unwrap();
        git(cx, remote.path(), &["init", "-b", "main"]).await;
        git(cx, remote.path(), &["config", "user.name", "Fixture"]).await;
        git(
            cx,
            remote.path(),
            &["config", "user.email", "fixture@example.invalid"],
        )
        .await;
        std::fs::write(remote.path().join("file"), "base\n").unwrap();
        git(cx, remote.path(), &["add", "file"]).await;
        git(cx, remote.path(), &["commit", "-m", "base"]).await;
        let base = Oid::from_str(&git(cx, remote.path(), &["rev-parse", "HEAD"]).await).unwrap();
        git(cx, remote.path(), &["switch", "-c", "feature"]).await;
        std::fs::write(remote.path().join("file"), "head\n").unwrap();
        git(cx, remote.path(), &["commit", "-am", "head"]).await;
        let head = Oid::from_str(&git(cx, remote.path(), &["rev-parse", "HEAD"]).await).unwrap();
        git(
            cx,
            remote.path(),
            &["update-ref", "refs/pull/42/head", &head.to_string()],
        )
        .await;
        let local = tempfile::tempdir().unwrap();
        git(
            cx,
            local.path(),
            &["clone", remote.path().to_str().unwrap(), "."],
        )
        .await;
        std::fs::write(local.path().join("file"), "dirty staged\n").unwrap();
        git(cx, local.path(), &["add", "file"]).await;
        std::fs::write(local.path().join("file"), "dirty unstaged\n").unwrap();
        std::fs::write(local.path().join(".git/FETCH_HEAD"), "sentinel\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let hook = local.path().join(".git/hooks/reference-transaction");
            std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let original_head = git(cx, local.path(), &["rev-parse", "HEAD"]).await;
        let original_refs = git(
            cx,
            local.path(),
            &["for-each-ref", "refs/heads", "refs/remotes", "refs/tags"],
        )
        .await;
        let index = std::fs::read(local.path().join(".git/index")).unwrap();
        let config = std::fs::read(local.path().join(".git/config")).unwrap();
        let repo = RealGitRepository::new(
            &local.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let git_binary = repo.git_binary();
        let blob =
            Oid::from_str(&git(cx, local.path(), &["rev-parse", "HEAD:file"]).await).unwrap();
        assert_eq!(repo.review_blob_sizes(vec![blob]).await.unwrap(), vec![5]);
        assert!(repo.review_blob_sizes(vec![blob; 129]).await.is_err());

        let request = ReviewPrFetch {
            owner: "fixture".into(),
            repository: "repository".into(),
            base_branch: "main".into(),
            number: 42,
            expected_base: base,
            expected_head: head,
        };
        let (mut command, refs) = review_pr_fetch_command(
            &git_binary,
            &request,
            remote.path().to_str().unwrap(),
            false,
        )
        .await
        .unwrap();
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let accepted = verify_review_pr_fetch(&git_binary, &request, refs)
            .await
            .unwrap();
        assert!(!local.path().join("hook-ran").exists());
        assert_eq!(accepted.base, base);
        assert_eq!(accepted.head, head);
        assert_eq!(
            git(cx, local.path(), &["rev-parse", "HEAD"]).await,
            original_head
        );
        assert_eq!(
            git(
                cx,
                local.path(),
                &["for-each-ref", "refs/heads", "refs/remotes", "refs/tags"]
            )
            .await,
            original_refs
        );
        assert_eq!(
            std::fs::read(local.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read(local.path().join(".git/config")).unwrap(),
            config
        );
        assert_eq!(
            std::fs::read_to_string(local.path().join("file")).unwrap(),
            "dirty unstaged\n"
        );
        assert_eq!(
            std::fs::read_to_string(local.path().join(".git/FETCH_HEAD")).unwrap(),
            "sentinel\n"
        );
        for raced in [
            ReviewPrFetch {
                expected_base: head,
                ..request.clone()
            },
            ReviewPrFetch {
                expected_head: base,
                ..request.clone()
            },
        ] {
            let (mut command, refs) = review_pr_fetch_command(
                &git_binary,
                &raced,
                remote.path().to_str().unwrap(),
                false,
            )
            .await
            .unwrap();
            assert_ne!(refs.base_ref, accepted.base_ref);
            assert!(command.output().await.unwrap().status.success());
            assert!(
                verify_review_pr_fetch(&git_binary, &raced, refs.clone())
                    .await
                    .is_err()
            );
            cleanup_review_pr_refs(&git_binary, &refs).await.unwrap();
            assert!(
                git_binary
                    .run(&["rev-parse", "--verify", &refs.base_ref])
                    .await
                    .is_err()
            );
            assert!(
                git_binary
                    .run(&["rev-parse", "--verify", &refs.head_ref])
                    .await
                    .is_err()
            );
        }
        verify_review_pr_fetch(&git_binary, &request, accepted)
            .await
            .unwrap();
        let invalid = ReviewPrFetch {
            base_branch: "main:refs/heads/evil".into(),
            ..request.clone()
        };
        assert!(
            review_pr_fetch_command(
                &git_binary,
                &invalid,
                remote.path().to_str().unwrap(),
                false
            )
            .await
            .is_err()
        );
        let missing = ReviewPrFetch {
            number: 43,
            ..request
        };
        let (mut command, refs) = review_pr_fetch_command(
            &git_binary,
            &missing,
            remote.path().to_str().unwrap(),
            false,
        )
        .await
        .unwrap();
        assert!(!command.output().await.unwrap().status.success());
        assert!(
            git_binary
                .run(&["rev-parse", "--verify", &refs.base_ref])
                .await
                .is_err()
        );
    }
}
