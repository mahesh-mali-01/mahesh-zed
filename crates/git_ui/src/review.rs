use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectId {
    pub format: ObjectFormat,
    pub hex: String,
}

impl ObjectId {
    pub fn validate(&self) -> Result<()> {
        let length = match self.format {
            ObjectFormat::Sha1 => 40,
            ObjectFormat::Sha256 => 64,
        };
        ensure!(self.hex.len() == length, "invalid object ID length");
        ensure!(
            self.hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && self.hex.bytes().any(|byte| byte != b'0'),
            "invalid object ID"
        );
        Ok(())
    }

    pub fn for_blob(format: ObjectFormat, bytes: &[u8]) -> Self {
        let header = format!("blob {}\0", bytes.len());
        let hex = match format {
            ObjectFormat::Sha1 => {
                let mut hash = Sha1::new();
                hash.update(header.as_bytes());
                hash.update(bytes);
                format!("{:x}", hash.finalize())
            }
            ObjectFormat::Sha256 => {
                let mut hash = Sha256::new();
                hash.update(header.as_bytes());
                hash.update(bytes);
                format!("{:x}", hash.finalize())
            }
        };
        Self { format, hex }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryBinding {
    pub id: String,
    pub common_directory: PathBuf,
    pub worktree_directory: PathBuf,
    pub format: ObjectFormat,
}

/// Immutable server/account identity. Branch names, revisions and presentation
/// are deliberately excluded so reopening a changed PR resumes its checkpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestIdentity {
    pub host: String,
    pub repository_node_id: String,
    pub pull_request_node_id: String,
    pub viewer_node_id: String,
}
impl PullRequestIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.host == "github.com", "unsupported PR host");
        for value in [
            &self.repository_node_id,
            &self.pull_request_node_id,
            &self.viewer_node_id,
        ] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 512
                    && !value.chars().any(char::is_whitespace)
                    && !value.chars().any(char::is_control),
                "invalid PR/account identity"
            );
        }
        Ok(())
    }
}
/// Mutable restore/display metadata, never part of the session lookup key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestMetadata {
    pub owner: String,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    /// Verified base tip, distinct from the diff's merge-base endpoint.
    #[serde(default)]
    pub base_ref_oid: String,
    /// Verified head tip for this metadata/snapshot generation.
    #[serde(default)]
    pub head_ref_oid: String,
    /// Server-reported total for reconciling complete pinned Git membership.
    #[serde(default)]
    pub commit_count: Option<usize>,
}
impl PullRequestMetadata {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.number > 0 && self.number <= i32::MAX as u64,
            "invalid PR number"
        );
        for value in [&self.owner, &self.repository] {
            ensure!(
                !value.is_empty()
                    && value != "."
                    && value != ".."
                    && value
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
                "invalid PR repository metadata"
            );
        }
        ensure!(
            self.base_ref_oid.is_empty() == self.head_ref_oid.is_empty(),
            "incomplete PR pinned revision metadata"
        );
        for hex in [&self.base_ref_oid, &self.head_ref_oid]
            .into_iter()
            .filter(|hex| !hex.is_empty())
        {
            let format = match hex.len() {
                40 => ObjectFormat::Sha1,
                64 => ObjectFormat::Sha256,
                _ => anyhow::bail!("invalid pinned PR object ID"),
            };
            ObjectId {
                format,
                hex: hex.clone(),
            }
            .validate()?;
        }
        for value in [&self.title, &self.base_ref_name, &self.head_ref_name] {
            ensure!(
                !value.is_empty() && !value.contains('\0'),
                "invalid PR display metadata"
            );
        }
        Ok(())
    }
    pub fn validate_pinned_tips(&self, format: ObjectFormat) -> Result<()> {
        self.validate()?;
        for hex in [&self.base_ref_oid, &self.head_ref_oid] {
            ObjectId {
                format,
                hex: hex.clone(),
            }
            .validate()
            .context("PR refresh requires verified base and head tips")?;
        }
        Ok(())
    }
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repository, self.number
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComparisonSelector {
    PullRequest {
        identity: PullRequestIdentity,
    },
    /// A historical subset remains tied to its PR/account and original complete
    /// membership context, independently of later full-PR refreshes.
    PullRequestScope {
        identity: PullRequestIdentity,
        base_tip: ObjectId,
        head_tip: ObjectId,
        scope: PullRequestCommitScope,
    },
    Branch {
        base_ref: String,
        head_ref: String,
    },
    Direct {
        left_ref: String,
        right_ref: String,
    },
    Commits {
        selections: Vec<CommitSelection>,
    },
    Range {
        selections: Vec<CommitSelection>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSelection {
    pub commit: ObjectId,
    pub parent: Option<ObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PullRequestCommitScope {
    Commits { selections: Vec<CommitSelection> },
    Range { selections: Vec<CommitSelection> },
}
impl PullRequestCommitScope {
    pub fn selections(&self) -> &[CommitSelection] {
        match self {
            Self::Commits { selections } | Self::Range { selections } => selections,
        }
    }
    pub fn is_range(&self) -> bool {
        matches!(self, Self::Range { .. })
    }
    pub fn validate(&self, format: ObjectFormat) -> Result<()> {
        validate_commit_selections(self.selections(), format)?;
        if self.is_range() {
            for pair in self.selections().windows(2) {
                ensure!(
                    pair[1].parent.as_ref() == Some(&pair[0].commit),
                    "PR range must contain every commit in oldest-to-newest parent order"
                );
            }
        }
        Ok(())
    }
    pub fn validate_snapshot(&self, snapshot: &ReviewSnapshot) -> Result<()> {
        let selections = self.selections();
        ensure!(!selections.is_empty(), "empty PR scope selection");
        let expected = if self.is_range() { 1 } else { selections.len() };
        ensure!(
            snapshot.groups.len() == expected,
            "PR scope manifest group count differs from selected commits"
        );
        for (index, group) in snapshot.groups.iter().enumerate() {
            let first = if self.is_range() {
                &selections[0]
            } else {
                &selections[index]
            };
            let last = if self.is_range() {
                selections.last().context("empty PR range")?
            } else {
                first
            };
            ensure!(
                group.endpoints.right == last.commit,
                "PR scope manifest does not match the selected commit"
            );
            if let Some(parent) = &first.parent {
                ensure!(
                    group.endpoints.left == *parent,
                    "PR scope manifest does not match the selected parent"
                );
            }
        }
        Ok(())
    }
}
fn validate_commit_selections(selections: &[CommitSelection], format: ObjectFormat) -> Result<()> {
    ensure!(!selections.is_empty(), "empty commit selection");
    let mut seen = BTreeSet::new();
    for selection in selections {
        selection.commit.validate()?;
        ensure!(selection.commit.format == format, "object format mismatch");
        ensure!(
            seen.insert(&selection.commit.hex),
            "duplicate selected commit"
        );
        if let Some(parent) = &selection.parent {
            parent.validate()?;
            ensure!(parent.format == format, "parent object format mismatch");
        }
    }
    Ok(())
}

impl ComparisonSelector {
    pub fn pull_request_identity(&self) -> Option<&PullRequestIdentity> {
        match self {
            Self::PullRequest { identity } | Self::PullRequestScope { identity, .. } => {
                Some(identity)
            }
            _ => None,
        }
    }
    /// Direct GitHub Viewed writes require a complete PR comparison. Selected
    /// commit receipts must first prove coverage against that canonical snapshot.
    pub fn supports_github_viewed(&self) -> bool {
        matches!(self, Self::PullRequest { .. })
    }
    pub fn validate(&self, format: ObjectFormat) -> Result<()> {
        match self {
            Self::PullRequest { identity } => identity.validate()?,
            Self::Branch { base_ref, head_ref }
            | Self::Direct {
                left_ref: base_ref,
                right_ref: head_ref,
            } => {
                for name in [base_ref, head_ref] {
                    ensure!(
                        !name.is_empty() && !name.contains('\0'),
                        "invalid comparison selector"
                    );
                }
            }
            Self::Commits { selections } | Self::Range { selections } => {
                validate_commit_selections(selections, format)?
            }
            Self::PullRequestScope {
                identity,
                base_tip,
                head_tip,
                scope,
            } => {
                identity.validate()?;
                for tip in [base_tip, head_tip] {
                    tip.validate()?;
                    ensure!(tip.format == format, "PR membership object format mismatch");
                }
                scope.validate(format)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonKind {
    Full,
    SinceReview,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSide {
    pub path: String,
    pub object: ObjectId,
    pub mode: u32,
}

impl FileSide {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.path)?;
        self.object.validate()?;
        ensure!(
            matches!(self.mode, 0o100644 | 0o100755 | 0o120000 | 0o160000),
            "unsupported Git mode"
        );
        Ok(())
    }
}

fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains('\0')
            && path.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "invalid repository path"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileComparison {
    pub kind: ComparisonKind,
    pub old: Option<FileSide>,
    pub new: Option<FileSide>,
}

impl FileComparison {
    pub fn validate(&self, format: ObjectFormat) -> Result<()> {
        ensure!(
            self.old.is_some() || self.new.is_some(),
            "comparison has no sides"
        );
        ensure!(self.old != self.new, "comparison contains no change");
        for side in [&self.old, &self.new].into_iter().flatten() {
            side.validate()?;
            ensure!(side.object.format == format, "file object format mismatch");
        }
        Ok(())
    }

    pub fn file_key(&self) -> Result<&str> {
        self.new
            .as_ref()
            .or(self.old.as_ref())
            .map(|side| side.path.as_str())
            .context("comparison has no file identity")
    }

    pub fn fingerprint(&self) -> Result<String> {
        let format = self
            .new
            .as_ref()
            .or(self.old.as_ref())
            .context("missing comparison sides")?
            .object
            .format;
        self.validate(format)?;
        let mut hash = Sha256::new();
        fn field(hash: &mut Sha256, bytes: &[u8]) {
            hash.update((bytes.len() as u64).to_be_bytes());
            hash.update(bytes);
        }
        field(&mut hash, b"zed-review-file-v1");
        field(
            &mut hash,
            match self.kind {
                ComparisonKind::Full => b"full",
                ComparisonKind::SinceReview => b"since",
            },
        );
        field(
            &mut hash,
            match format {
                ObjectFormat::Sha1 => b"sha1",
                ObjectFormat::Sha256 => b"sha256",
            },
        );
        for side in [&self.old, &self.new] {
            match side {
                None => field(&mut hash, b"absent"),
                Some(side) => {
                    field(&mut hash, b"present");
                    field(&mut hash, side.path.as_bytes());
                    field(&mut hash, side.object.hex.as_bytes());
                    field(&mut hash, &side.mode.to_be_bytes());
                }
            }
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    pub fn validate_retained(&self, contents: &RetainedContents, budget: usize) -> Result<()> {
        ensure!(
            self.kind == ComparisonKind::Full,
            "incremental view cannot approve a full comparison"
        );
        self.validate_contents(contents, budget)
    }

    /// Validate displayed raw Git contents, including incremental comparisons.
    /// Approval and retained full-review checkpoints must use validate_retained.
    pub fn validate_contents(&self, contents: &RetainedContents, budget: usize) -> Result<()> {
        let format = self
            .new
            .as_ref()
            .or(self.old.as_ref())
            .context("missing comparison")?
            .object
            .format;
        self.validate(format)?;
        let mut total = 0usize;
        for (side, bytes) in [(&self.old, &contents.old), (&self.new, &contents.new)] {
            match (side, bytes) {
                (None, None) => {}
                (Some(side), None) if side.mode == 0o160000 => {}
                (Some(side), Some(bytes)) if side.mode != 0o160000 => {
                    ensure!(
                        ObjectId::for_blob(side.object.format, bytes) == side.object,
                        "retained bytes do not match displayed Git object"
                    );
                    total = total
                        .checked_add(bytes.len())
                        .context("retention size overflow")?;
                }
                _ => anyhow::bail!("missing or unexpected retained content"),
            }
        }
        ensure!(total <= budget, "review retention budget exceeded");
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedContents {
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedEndpoints {
    pub group: String,
    pub left: ObjectId,
    pub right: ObjectId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonGroup {
    pub endpoints: ResolvedEndpoints,
    pub files: Vec<FileComparison>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSnapshot {
    pub generation: u64,
    pub groups: Vec<ComparisonGroup>,
    pub incomplete_reason: Option<String>,
}

impl ReviewSnapshot {
    pub fn validate(&self, format: ObjectFormat) -> Result<()> {
        ensure!(
            self.generation > 0 && self.generation <= i64::MAX as u64,
            "invalid snapshot generation"
        );
        ensure!(!self.groups.is_empty(), "snapshot has no comparison groups");
        let mut groups = BTreeSet::new();
        for group in &self.groups {
            ensure!(
                !group.endpoints.group.is_empty() && groups.insert(&group.endpoints.group),
                "missing or duplicate group identity"
            );
            for object in [&group.endpoints.left, &group.endpoints.right] {
                object.validate()?;
                ensure!(object.format == format, "endpoint object format mismatch");
            }
            let mut paths = BTreeSet::new();
            for file in &group.files {
                file.validate(format)?;
                ensure!(
                    file.kind == ComparisonKind::Full,
                    "snapshot must contain full comparisons"
                );
                ensure!(paths.insert(file.file_key()?), "duplicate file comparison");
            }
        }
        Ok(())
    }

    pub fn file(&self, group: &str, path: &str) -> Result<&FileComparison> {
        self.groups
            .iter()
            .find(|entry| entry.endpoints.group == group)
            .and_then(|entry| {
                entry
                    .files
                    .iter()
                    .find(|file| file.file_key().is_ok_and(|key| key == path))
            })
            .context("file is absent from displayed snapshot")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPresentation {
    #[serde(default)]
    pub since_review: bool,
    #[serde(default)]
    pub tree_width: Option<u32>,
    #[serde(default)]
    pub collapsed_directories: BTreeSet<String>,
    #[serde(default)]
    pub multi_file: Option<bool>,
    #[serde(default)]
    pub path_filter: String,
    pub unreviewed_only: bool,
    pub split_view: Option<bool>,
    pub selected_group: Option<String>,
    pub selected_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSession {
    pub id: String,
    pub repository: RepositoryBinding,
    pub selector: ComparisonSelector,
    pub revision: u64,
    pub snapshot: ReviewSnapshot,
    pub presentation: ReviewPresentation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedCheckpoint {
    pub group: String,
    pub comparison: FileComparison,
    pub generation: u64,
    pub reviewed_at_unix_seconds: u64,
}

impl ReviewedCheckpoint {
    pub fn matches(&self, group: &str, file: &FileComparison) -> Result<bool> {
        Ok(self.group == group && self.comparison.fingerprint()? == file.fingerprint()?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadToken {
    pub session: String,
    pub request: u64,
    pub snapshot_generation: u64,
    pub endpoints: Vec<ResolvedEndpoints>,
}

#[derive(Default)]
pub struct RefreshGate {
    next_request: u64,
    current: Option<LoadToken>,
}

impl RefreshGate {
    pub fn begin(
        &mut self,
        session: String,
        snapshot_generation: u64,
        endpoints: Vec<ResolvedEndpoints>,
    ) -> Result<LoadToken> {
        ensure!(
            snapshot_generation > 0 && snapshot_generation <= i64::MAX as u64,
            "invalid requested snapshot generation"
        );
        self.next_request = self
            .next_request
            .checked_add(1)
            .context("request generation exhausted")?;
        let token = LoadToken {
            session,
            request: self.next_request,
            snapshot_generation,
            endpoints,
        };
        self.current = Some(token.clone());
        Ok(token)
    }

    pub fn accepts(&self, token: &LoadToken, snapshot: &ReviewSnapshot) -> bool {
        self.current.as_ref() == Some(token)
            && token.snapshot_generation == snapshot.generation
            && token.endpoints
                == snapshot
                    .groups
                    .iter()
                    .map(|group| group.endpoints.clone())
                    .collect::<Vec<_>>()
    }

    pub fn cancel(&mut self) {
        self.current = None;
    }
}

#[path = "review_outbox.rs"]
mod outbox;
pub use outbox::*;

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
