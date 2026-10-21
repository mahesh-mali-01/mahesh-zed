use crate::review::{
    ComparisonSelector, FileComparison, ObjectFormat, PullRequestCommitScope, PullRequestIdentity,
    PullRequestMetadata, RepositoryBinding, RetainedContents, ReviewPresentation, ReviewSession,
    ReviewSnapshot, ReviewedCheckpoint,
};
use anyhow::{Context as _, Result, ensure};
use db::sqlez::{
    connection::Connection, domain::Domain, thread_safe_connection::ThreadSafeConnection,
};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct ReviewDb(ThreadSafeConnection);

impl Domain for ReviewDb {
    const NAME: &str = "ReviewDb";
    const MIGRATIONS: &[&str] = &[
        r#"
        CREATE TABLE review_repositories (
            id TEXT PRIMARY KEY NOT NULL,
            common_directory BLOB NOT NULL,
            worktree_directory BLOB NOT NULL,
            object_format TEXT NOT NULL,
            UNIQUE(common_directory, worktree_directory)
        ) STRICT;
        CREATE TABLE review_sessions (
            id TEXT PRIMARY KEY NOT NULL,
            repository_id TEXT NOT NULL REFERENCES review_repositories(id),
            version INTEGER NOT NULL CHECK(version = 1),
            selector TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK(revision >= 0),
            active_generation INTEGER NOT NULL CHECK(active_generation > 0),
            presentation TEXT NOT NULL
        ) STRICT;
        CREATE TABLE review_snapshots (
            session_id TEXT NOT NULL REFERENCES review_sessions(id),
            generation INTEGER NOT NULL CHECK(generation > 0),
            manifest TEXT NOT NULL,
            PRIMARY KEY(session_id, generation)
        ) STRICT;
        CREATE TABLE review_contents (
            digest TEXT PRIMARY KEY NOT NULL,
            bytes BLOB NOT NULL
        ) STRICT;
        CREATE TABLE review_checkpoints (
            session_id TEXT NOT NULL,
            group_id TEXT NOT NULL,
            file_key TEXT NOT NULL,
            generation INTEGER NOT NULL,
            checkpoint TEXT NOT NULL,
            old_content TEXT REFERENCES review_contents(digest),
            new_content TEXT REFERENCES review_contents(digest),
            PRIMARY KEY(session_id, group_id, file_key),
            FOREIGN KEY(session_id, generation) REFERENCES review_snapshots(session_id, generation)
        ) STRICT;
    "#,
        r#"
        CREATE TABLE review_items (
            workspace_id INTEGER NOT NULL,
            item_id INTEGER NOT NULL,
            session_id TEXT NOT NULL REFERENCES review_sessions(id),
            PRIMARY KEY(workspace_id, item_id)
        ) STRICT;
    "#,
        r#"
        CREATE TABLE review_pull_requests (
            session_id TEXT PRIMARY KEY NOT NULL REFERENCES review_sessions(id),
            metadata TEXT NOT NULL
        ) STRICT;
    "#,
        r#"
        CREATE TABLE review_operations (
            id TEXT PRIMARY KEY NOT NULL,
            identity TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK(revision >= 0),
            state TEXT NOT NULL CHECK(state IN ('local_draft','queued','sending','sent','failed','delivery_unknown')),
            record TEXT NOT NULL
        ) STRICT;
        CREATE INDEX review_operations_identity_state ON review_operations(identity, state, id);
    "#,
        r#"
        CREATE TABLE review_github_viewed (
            session_id TEXT NOT NULL,
            group_id TEXT NOT NULL,
            file_key TEXT NOT NULL,
            generation INTEGER NOT NULL CHECK(generation > 0),
            identity TEXT NOT NULL,
            base_tip_oid TEXT NOT NULL,
            observation TEXT NOT NULL,
            PRIMARY KEY(session_id, group_id, file_key),
            FOREIGN KEY(session_id, generation) REFERENCES review_snapshots(session_id, generation)
        ) STRICT;
        CREATE INDEX review_github_viewed_identity ON review_github_viewed(identity, session_id, group_id);
        CREATE TABLE review_state_revisions (
            session_id TEXT NOT NULL REFERENCES review_sessions(id),
            revision INTEGER NOT NULL CHECK(revision > 0),
            PRIMARY KEY(session_id, revision)
        ) STRICT;
        CREATE TABLE review_github_viewed_receipts (
            session_id TEXT NOT NULL,
            operation_id TEXT NOT NULL REFERENCES review_operations(id),
            generation INTEGER NOT NULL CHECK(generation > 0),
            PRIMARY KEY(session_id, operation_id),
            FOREIGN KEY(session_id, generation) REFERENCES review_snapshots(session_id, generation)
        ) STRICT;
        CREATE TABLE review_github_viewed_superseded (
            session_id TEXT NOT NULL,
            file_key TEXT NOT NULL,
            operation_id TEXT NOT NULL REFERENCES review_operations(id),
            receipt_operation_id TEXT NOT NULL REFERENCES review_operations(id),
            generation INTEGER NOT NULL CHECK(generation > 0),
            PRIMARY KEY(session_id, file_key, operation_id),
            FOREIGN KEY(session_id, generation) REFERENCES review_snapshots(session_id, generation)
        ) STRICT;

    "#,
        r#"
        CREATE TABLE review_applied_pr_scopes (
            repository_id TEXT NOT NULL REFERENCES review_repositories(id),
            identity TEXT NOT NULL,
            session_id TEXT NOT NULL REFERENCES review_sessions(id),
            PRIMARY KEY(repository_id, identity)
        ) STRICT;
    "#,
    ];
}

db::static_connection!(ReviewDb, []);

#[derive(Debug)]
pub struct StaleReview;

impl std::fmt::Display for StaleReview {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("review changed; reload before retrying this operation")
    }
}

impl std::error::Error for StaleReview {}

#[derive(Clone, Debug)]
pub struct ViewedTarget {
    pub session: String,
    pub expected_revision: u64,
    pub displayed_generation: u64,
    pub group: String,
    pub path: String,
    pub fingerprint: String,
}

fn ensure_durable(connection: &Connection) -> Result<()> {
    let databases = connection.select::<(i64, String, String)>("PRAGMA database_list")?()?;
    ensure!(
        databases
            .iter()
            .any(|(_, name, path)| name == "main" && !path.is_empty()),
        "review persistence unavailable: database is in memory"
    );
    ensure!(
        connection.select_row::<i64>("PRAGMA foreign_keys")?()? == Some(1),
        "review persistence requires foreign key enforcement"
    );
    Ok(())
}

fn read_binding(connection: &Connection, id: &str) -> Result<RepositoryBinding> {
    let (common_directory, worktree_directory, format) = connection
        .select_row_bound::<String, (PathBuf, PathBuf, String)>(
            "SELECT common_directory, worktree_directory, object_format FROM review_repositories WHERE id = ?",
        )?(id.to_owned())?.context("unknown review repository")?;
    Ok(RepositoryBinding {
        id: id.to_owned(),
        common_directory,
        worktree_directory,
        format: serde_json::from_str(&format)?,
    })
}

fn read_session(connection: &Connection, id: &str) -> Result<ReviewSession> {
    let (repository, version, selector, revision, generation, presentation, manifest) = connection
        .select_row_bound::<String, (String, i64, String, i64, i64, String, String)>(
            "SELECT s.repository_id, s.version, s.selector, s.revision, s.active_generation, s.presentation, m.manifest
            FROM review_sessions s JOIN review_snapshots m
            ON m.session_id = s.id AND m.generation = s.active_generation WHERE s.id = ?",
        )?(id.to_owned())?.context("review session not found or active manifest missing")?;
    ensure!(
        version == 1 && revision >= 0,
        "unsupported review record version/revision"
    );
    let repository = read_binding(connection, &repository)?;
    let selector: ComparisonSelector = serde_json::from_str(&selector)?;
    selector.validate(repository.format)?;
    let snapshot: ReviewSnapshot = serde_json::from_str(&manifest)?;
    snapshot.validate(repository.format)?;
    ensure!(
        snapshot.generation == generation as u64,
        "manifest generation mismatch"
    );
    Ok(ReviewSession {
        id: id.to_owned(),
        repository,
        selector,
        revision: revision as u64,
        snapshot,
        presentation: serde_json::from_str(&presentation)?,
    })
}

fn require_revision(
    session: &ReviewSession,
    expected_revision: u64,
    generation: u64,
) -> Result<()> {
    if session.revision != expected_revision || session.snapshot.generation != generation {
        anyhow::bail!(StaleReview);
    }
    Ok(())
}

fn bump_revision(connection: &Connection, session: &ReviewSession) -> Result<u64> {
    let next = bump_presentation_revision(connection, session)?;
    // The pre-migration fixture exercises old schema methods before upgrading.
    if connection.select_row::<i64>(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='review_state_revisions'",
    )?()?
    .is_some()
    {
        connection.exec_bound::<(String, i64)>(
            "INSERT INTO review_state_revisions(session_id,revision) VALUES (?,?)",
        )?((session.id.clone(), next as i64))?;
    }
    Ok(next)
}
fn bump_presentation_revision(connection: &Connection, session: &ReviewSession) -> Result<u64> {
    let next = session
        .revision
        .checked_add(1)
        .filter(|next| *next <= i64::MAX as u64)
        .context("review revision exhausted")?;
    connection.exec_bound::<(i64, String, i64)>(
        "UPDATE review_sessions SET revision = ? WHERE id = ? AND revision = ?",
    )?((next as i64, session.id.clone(), session.revision as i64))?;
    if connection.select_row::<i64>("SELECT changes()")?()? != Some(1) {
        anyhow::bail!(StaleReview);
    }
    Ok(next)
}

fn write_applied_pr_scope(
    connection: &Connection,
    session: &ReviewSession,
    overwrite: bool,
) -> Result<()> {
    let identity = session
        .selector
        .pull_request_identity()
        .context("Applied scope must belong to a PR")?;
    identity.validate()?;
    let sql = if overwrite {
        "INSERT INTO review_applied_pr_scopes(repository_id,identity,session_id) VALUES (?,?,?) ON CONFLICT(repository_id,identity) DO UPDATE SET session_id=excluded.session_id"
    } else {
        "INSERT INTO review_applied_pr_scopes(repository_id,identity,session_id) VALUES (?,?,?) ON CONFLICT(repository_id,identity) DO NOTHING"
    };
    connection.exec_bound::<(String, String, String)>(sql)?((
        session.repository.id.clone(),
        serde_json::to_string(identity)?,
        session.id.clone(),
    ))?;
    Ok(())
}

fn retain(connection: &Connection, bytes: Option<Vec<u8>>) -> Result<Option<String>> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let digest = format!("{:x}", Sha256::digest(&bytes));
    connection.exec_bound::<(String, Vec<u8>)>(
        "INSERT INTO review_contents(digest, bytes) VALUES (?, ?) ON CONFLICT(digest) DO NOTHING",
    )?((digest.clone(), bytes.clone()))?;
    let existing = connection.select_row_bound::<String, Vec<u8>>(
        "SELECT bytes FROM review_contents WHERE digest = ?",
    )?(digest.clone())?
    .context("retained content missing")?;
    ensure!(existing == bytes, "retained content integrity failure");
    Ok(Some(digest))
}

fn read_content(connection: &Connection, digest: Option<String>) -> Result<Option<Vec<u8>>> {
    let Some(digest) = digest else {
        return Ok(None);
    };
    let bytes = connection.select_row_bound::<String, Vec<u8>>(
        "SELECT bytes FROM review_contents WHERE digest = ?",
    )?(digest.clone())?
    .context("retained content missing")?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == digest,
        "retained content is corrupt"
    );
    Ok(Some(bytes))
}

impl ReviewDb {
    pub fn from_connection(connection: ThreadSafeConnection) -> Result<Self> {
        ensure_durable(&connection)?;
        Ok(Self(connection))
    }

    pub async fn bind_repository(
        &self,
        common_directory: PathBuf,
        worktree_directory: PathBuf,
        format: ObjectFormat,
    ) -> Result<RepositoryBinding> {
        let common_directory = common_directory
            .canonicalize()
            .context("canonicalizing common Git directory")?;
        let worktree_directory = worktree_directory
            .canonicalize()
            .context("canonicalizing worktree Git directory")?;
        ensure!(
            common_directory.is_dir() && worktree_directory.is_dir(),
            "Git binding paths must be directories"
        );
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_bind", || {
                let existing = connection.select_row_bound::<(PathBuf, PathBuf), String>(
                    "SELECT id FROM review_repositories WHERE common_directory = ? AND worktree_directory = ?",
                )?((common_directory.clone(), worktree_directory.clone()))?;
                if let Some(id) = existing {
                    let binding = read_binding(connection, &id)?;
                    ensure!(binding.format == format, "repository object format changed; rebind explicitly");
                    return Ok(binding);
                }
                let id = db::uuid::Uuid::now_v7().to_string();
                connection.exec_bound::<(String, PathBuf, PathBuf, String)>(
                    "INSERT INTO review_repositories(id, common_directory, worktree_directory, object_format) VALUES (?, ?, ?, ?)",
                )?((id.clone(), common_directory, worktree_directory, serde_json::to_string(&format)?))?;
                read_binding(connection, &id)
            })
        }).await
    }

    pub async fn create_session(
        &self,
        repository: String,
        selector: ComparisonSelector,
        snapshot: ReviewSnapshot,
    ) -> Result<ReviewSession> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_create", || {
                let binding = read_binding(connection, &repository)?;
                selector.validate(binding.format)?;
                snapshot.validate(binding.format)?;
                ensure!(snapshot.generation == 1, "new session must start at generation one");
                let id = db::uuid::Uuid::now_v7().to_string();
                connection.exec_bound::<(String, String, String, String)>(
                    "INSERT INTO review_sessions(id, repository_id, version, selector, revision, active_generation, presentation)
                    VALUES (?, ?, 1, ?, 0, 1, ?)",
                )?((id.clone(), repository, serde_json::to_string(&selector)?, serde_json::to_string(&ReviewPresentation::default())?))?;
                connection.exec_bound::<(String, i64, String)>(
                    "INSERT INTO review_snapshots(session_id, generation, manifest) VALUES (?, ?, ?)",
                )?((id.clone(), 1, serde_json::to_string(&snapshot)?))?;
                read_session(connection, &id)
            })
        }).await
    }

    pub fn find_session(
        &self,
        repository: &str,
        selector: &ComparisonSelector,
    ) -> Result<Option<ReviewSession>> {
        ensure_durable(self)?;
        let id = self.select_row_bound::<(String, String), String>(
            "SELECT id FROM review_sessions WHERE repository_id = ? AND selector = ? ORDER BY id DESC LIMIT 1",
        )?((repository.to_owned(), serde_json::to_string(selector)?))?;
        id.map(|id| self.load_session(&id)).transpose()
    }

    pub fn find_pull_request_session(
        &self,
        repository: &str,
        identity: &PullRequestIdentity,
    ) -> Result<Option<ReviewSession>> {
        identity.validate()?;
        self.find_session(
            repository,
            &ComparisonSelector::PullRequest {
                identity: identity.clone(),
            },
        )
    }

    /// The user's applied scope is independent of tab serialization order.
    /// Repository bindings include the exact worktree; identity includes viewer.
    pub fn applied_pull_request_session(
        &self,
        repository: &str,
        identity: &PullRequestIdentity,
    ) -> Result<Option<ReviewSession>> {
        identity.validate()?;
        ensure_durable(self)?;
        let id = self.select_row_bound::<(String, String), String>(
            "SELECT session_id FROM review_applied_pr_scopes WHERE repository_id=? AND identity=?",
        )?((repository.to_owned(), serde_json::to_string(identity)?))?;
        id.map(|id| {
            let session = self.load_session(&id)?;
            ensure!(
                session.repository.id == repository
                    && session.selector.pull_request_identity() == Some(identity),
                "Applied PR scope binding is invalid"
            );
            Ok(session)
        })
        .transpose()
    }

    /// Called by explicit Apply / All commits, never by background serialization.
    pub async fn apply_pull_request_session(&self, id: String) -> Result<()> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            let session = read_session(connection, &id)?;
            write_applied_pr_scope(connection, &session, true)
        })
        .await
    }

    pub fn pull_request_metadata(&self, session: &str) -> Result<Option<PullRequestMetadata>> {
        ensure_durable(self)?;
        let metadata = self.select_row_bound::<String, String>(
            "SELECT metadata FROM review_pull_requests WHERE session_id = ?",
        )?(session.to_owned())?;
        metadata
            .map(|metadata| {
                let metadata: PullRequestMetadata = serde_json::from_str(&metadata)?;
                metadata.validate()?;
                Ok(metadata)
            })
            .transpose()
    }

    /// Atomically opens a new PR or activates its newly fetched snapshot while
    /// retaining checkpoints. Capture expected_revision before fetching: None
    /// expects no session; Some protects against another window's intervening
    /// review/refresh. An existing session never discards the incoming revision.
    /// Local provider bindings remain explicit: the same server identity in a
    /// different checkout cannot silently rebind historical object reads.
    pub async fn deploy_pull_request(
        &self,
        repository: String,
        identity: PullRequestIdentity,
        metadata: PullRequestMetadata,
        mut snapshot: ReviewSnapshot,
        expected_revision: Option<u64>,
    ) -> Result<ReviewSession> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_pull_request_deploy", || {
                identity.validate()?; metadata.validate()?;
                let binding = read_binding(connection,&repository)?;
                metadata.validate_pinned_tips(binding.format)?;
                snapshot.validate(binding.format)?;
                ensure!(snapshot.incomplete_reason.is_none(),"cannot open an incomplete PR comparison");
                let selector = ComparisonSelector::PullRequest {identity};
                let selector_json = serde_json::to_string(&selector)?;
                let id = connection.select_row_bound::<(String,String),String>("SELECT id FROM review_sessions WHERE repository_id = ? AND selector = ? ORDER BY id DESC LIMIT 1")?((repository.clone(),selector_json.clone()))?;
                let id = if let Some(id) = id {
                    let current = read_session(connection,&id)?;
                    if Some(current.revision) != expected_revision { anyhow::bail!(StaleReview); }
                    let old_metadata = connection.select_row_bound::<String,String>("SELECT metadata FROM review_pull_requests WHERE session_id = ?")?(id.clone())?;
                    let metadata_changed = old_metadata.as_deref() != Some(serde_json::to_string(&metadata)?.as_str());
                    let snapshot_changed = current.snapshot.groups != snapshot.groups || current.snapshot.incomplete_reason != snapshot.incomplete_reason;
                    if snapshot_changed {
                        snapshot.generation = current.snapshot.generation.checked_add(1).filter(|generation|*generation <= i64::MAX as u64).context("PR generation exhausted")?;
                        connection.exec_bound::<(String,i64,String)>("INSERT INTO review_snapshots(session_id,generation,manifest) VALUES (?,?,?)")?((id.clone(),snapshot.generation as i64,serde_json::to_string(&snapshot)?))?;
                        connection.exec_bound::<(i64,String)>("UPDATE review_sessions SET active_generation = ? WHERE id = ?")?((snapshot.generation as i64,id.clone()))?;
                    }
                    if snapshot_changed || metadata_changed { bump_revision(connection,&current)?; }
                    id
                } else {
                    if expected_revision.is_some() { anyhow::bail!(StaleReview); }
                    snapshot.generation = 1;
                    let id = db::uuid::Uuid::now_v7().to_string();
                    connection.exec_bound::<(String,String,String,String)>("INSERT INTO review_sessions(id,repository_id,version,selector,revision,active_generation,presentation) VALUES (?,?,1,?,0,1,?)")?((id.clone(),repository,selector_json,serde_json::to_string(&ReviewPresentation::default())?))?;
                    connection.exec_bound::<(String,i64,String)>("INSERT INTO review_snapshots(session_id,generation,manifest) VALUES (?,?,?)")?((id.clone(),1,serde_json::to_string(&snapshot)?))?;
                    id
                };
                connection.exec_bound::<(String,String)>("INSERT INTO review_pull_requests(session_id,metadata) VALUES (?,?) ON CONFLICT(session_id) DO UPDATE SET metadata=excluded.metadata")?((id.clone(),serde_json::to_string(&metadata)?))?;
                let session = read_session(connection,&id)?;
                write_applied_pr_scope(connection, &session, false)?;
                Ok(session)
            })
        }).await
    }

    /// Create/resume an immutable selected-commit PR scope. The complete PR's
    /// identity and pinned membership tips remain available after restart, but
    /// checkpoint rows belong to this scope's independent session ID.
    pub async fn create_pull_request_scope(
        &self,
        repository: String,
        identity: PullRequestIdentity,
        metadata: PullRequestMetadata,
        base_tip: crate::review::ObjectId,
        head_tip: crate::review::ObjectId,
        scope: PullRequestCommitScope,
        snapshot: ReviewSnapshot,
    ) -> Result<ReviewSession> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_pull_request_scope",|| {
                let binding=read_binding(connection,&repository)?;
                let selector=ComparisonSelector::PullRequestScope{identity,base_tip,head_tip,scope};
                selector.validate(binding.format)?;
                metadata.validate_pinned_tips(binding.format)?;
                if let ComparisonSelector::PullRequestScope{base_tip,head_tip,scope,..}=&selector {
                    ensure!(metadata.base_ref_oid==base_tip.hex && metadata.head_ref_oid==head_tip.hex,"PR scope membership tips differ from verified metadata");
                    scope.validate_snapshot(&snapshot)?;
                }
                snapshot.validate(binding.format)?;
                ensure!(snapshot.generation==1 && snapshot.incomplete_reason.is_none(),"new PR scope requires a complete generation-one snapshot");
                let selector_json=serde_json::to_string(&selector)?;
                let existing=connection.select_row_bound::<(String,String),String>("SELECT id FROM review_sessions WHERE repository_id = ? AND selector = ? ORDER BY id DESC LIMIT 1")?((repository.clone(),selector_json.clone()))?;
                if let Some(id)=existing {
                    let session=read_session(connection,&id)?;
                    ensure!(session.snapshot.groups==snapshot.groups,"immutable PR scope manifest changed; reload the pinned selection");
                    write_applied_pr_scope(connection, &session, true)?;
                    return Ok(session);
                }
                let id=db::uuid::Uuid::now_v7().to_string();
                connection.exec_bound::<(String,String,String,String)>("INSERT INTO review_sessions(id,repository_id,version,selector,revision,active_generation,presentation) VALUES (?,?,1,?,0,1,?)")?((id.clone(),repository,selector_json,serde_json::to_string(&ReviewPresentation::default())?))?;
                connection.exec_bound::<(String,i64,String)>("INSERT INTO review_snapshots(session_id,generation,manifest) VALUES (?,?,?)")?((id.clone(),1,serde_json::to_string(&snapshot)?))?;
                connection.exec_bound::<(String,String)>("INSERT INTO review_pull_requests(session_id,metadata) VALUES (?,?)")?((id.clone(),serde_json::to_string(&metadata)?))?;
                let session = read_session(connection,&id)?;
                write_applied_pr_scope(connection, &session, true)?;
                Ok(session)
            })
        }).await
    }

    pub async fn save_item(
        &self,
        workspace: workspace::WorkspaceId,
        item: workspace::ItemId,
        session: String,
    ) -> Result<()> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.exec_bound::<(workspace::WorkspaceId, workspace::ItemId, String)>(
                "INSERT INTO review_items(workspace_id, item_id, session_id) VALUES (?, ?, ?) ON CONFLICT(workspace_id, item_id) DO UPDATE SET session_id = excluded.session_id",
            )?((workspace, item, session))
        }).await
    }

    pub fn restored_item_session(
        &self,
        workspace: workspace::WorkspaceId,
        item: workspace::ItemId,
    ) -> Result<ReviewSession> {
        let saved = self.item_session(workspace, item)?;
        if let Some(identity) = saved.selector.pull_request_identity() {
            if let Some(applied) =
                self.applied_pull_request_session(&saved.repository.id, identity)?
            {
                return Ok(applied);
            }
        }
        Ok(saved)
    }

    pub fn item_session(
        &self,
        workspace: workspace::WorkspaceId,
        item: workspace::ItemId,
    ) -> Result<ReviewSession> {
        ensure_durable(self)?;
        let id = self.select_row_bound::<(workspace::WorkspaceId, workspace::ItemId), String>(
            "SELECT session_id FROM review_items WHERE workspace_id = ? AND item_id = ?",
        )?((workspace, item))?
        .context("review item mapping missing")?;
        self.load_session(&id)
    }

    pub fn load_session(&self, id: &str) -> Result<ReviewSession> {
        ensure_durable(self)?;
        read_session(self, id)
    }

    pub async fn activate_snapshot(
        &self,
        id: String,
        expected_revision: u64,
        snapshot: ReviewSnapshot,
    ) -> Result<u64> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_refresh", || {
                let session = read_session(connection, &id)?;
                if session.revision != expected_revision { anyhow::bail!(StaleReview); }
                snapshot.validate(session.repository.format)?;
                ensure!(snapshot.incomplete_reason.is_none(), "cannot activate an incomplete refresh");
                ensure!(session.snapshot.generation.checked_add(1) == Some(snapshot.generation), "invalid refresh generation");
                connection.exec_bound::<(String, i64, String)>(
                    "INSERT INTO review_snapshots(session_id, generation, manifest) VALUES (?, ?, ?)",
                )?((id.clone(), snapshot.generation as i64, serde_json::to_string(&snapshot)?))?;
                let revision = bump_revision(connection, &session)?;
                connection.exec_bound::<(i64, String)>(
                    "UPDATE review_sessions SET active_generation = ? WHERE id = ?",
                )?((snapshot.generation as i64, id))?;
                Ok(revision)
            })
        }).await
    }

    pub async fn mark_viewed(
        &self,
        target: ViewedTarget,
        contents: RetainedContents,
        budget: usize,
    ) -> Result<u64> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_viewed", || {
                let session = read_session(connection, &target.session)?;
                require_revision(&session, target.expected_revision, target.displayed_generation)?;
                ensure!(session.snapshot.incomplete_reason.is_none(), "incomplete review cannot be marked viewed");
                let comparison = session.snapshot.file(&target.group, &target.path)?.clone();
                if comparison.fingerprint()? != target.fingerprint { anyhow::bail!(StaleReview); }
                comparison.validate_retained(&contents, budget)?;
                let checkpoint = ReviewedCheckpoint { group: target.group.clone(), comparison,
                    generation: target.displayed_generation,
                    reviewed_at_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() };
                let old_content = retain(connection, contents.old)?;
                let new_content = retain(connection, contents.new)?;
                connection.exec_bound::<(String, String, String, i64, String, Option<String>, Option<String>)>(
                    "INSERT INTO review_checkpoints(session_id, group_id, file_key, generation, checkpoint, old_content, new_content)
                    VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(session_id, group_id, file_key) DO UPDATE SET
                    generation = excluded.generation, checkpoint = excluded.checkpoint,
                    old_content = excluded.old_content, new_content = excluded.new_content",
                )?((target.session, target.group, target.path, checkpoint.generation as i64,
                    serde_json::to_string(&checkpoint)?, old_content, new_content))?;
                bump_revision(connection, &session)
            })
        }).await
    }

    /// Reopen an exact retained snapshot without requiring its Git objects to
    /// survive pruning. Length probes precede blob materialization, and content
    /// digests remain immutable and are validated when read.
    pub fn matching_checkpoint_contents(
        &self,
        session: &str,
        group: &str,
        path: &str,
        comparison: &FileComparison,
        budget: usize,
    ) -> Result<Option<RetainedContents>> {
        ensure_durable(self)?;
        let record = self.select_row_bound::<(String, String, String), (String, Option<String>, Option<String>, Option<i64>, Option<i64>)>(
            "SELECT c.checkpoint, c.old_content, c.new_content, length(o.bytes), length(n.bytes) FROM review_checkpoints c LEFT JOIN review_contents o ON o.digest = c.old_content LEFT JOIN review_contents n ON n.digest = c.new_content WHERE c.session_id = ? AND c.group_id = ? AND c.file_key = ?",
        )?((session.to_owned(), group.to_owned(), path.to_owned()))?;
        let Some((checkpoint, old_content, new_content, old_size, new_size)) = record else {
            return Ok(None);
        };
        let checkpoint: ReviewedCheckpoint = serde_json::from_str(&checkpoint)?;
        ensure!(
            checkpoint.group == group && checkpoint.comparison.file_key()? == path,
            "checkpoint identity is corrupt"
        );
        if checkpoint.comparison != *comparison {
            return Ok(None);
        }
        let mut bytes = 0u64;
        for (digest, size) in [(&old_content, old_size), (&new_content, new_size)] {
            ensure!(
                digest.is_some() == size.is_some(),
                "retained content missing"
            );
            if let Some(size) = size {
                ensure!(size >= 0, "retained content length is corrupt");
                bytes = bytes
                    .checked_add(size as u64)
                    .context("retained content length overflow")?;
            }
        }
        ensure!(
            bytes <= budget as u64,
            "Preview unavailable: retained old and new contents exceed the {} MiB preview limit. No contents were loaded or marked reviewed. Choose another file to continue reviewing.",
            budget / (1024 * 1024)
        );
        let contents = RetainedContents {
            old: read_content(self, old_content)?,
            new: read_content(self, new_content)?,
        };
        comparison.validate_retained(&contents, budget)?;
        Ok(Some(contents))
    }

    pub fn checkpoint(
        &self,
        session: &str,
        group: &str,
        path: &str,
    ) -> Result<Option<(ReviewedCheckpoint, RetainedContents)>> {
        ensure_durable(self)?;
        let record = self.select_row_bound::<(String, String, String), (String, Option<String>, Option<String>)>(
            "SELECT checkpoint, old_content, new_content FROM review_checkpoints WHERE session_id = ? AND group_id = ? AND file_key = ?",
        )?((session.to_owned(), group.to_owned(), path.to_owned()))?;
        let Some((checkpoint, old_content, new_content)) = record else {
            return Ok(None);
        };
        let checkpoint: ReviewedCheckpoint = serde_json::from_str(&checkpoint)?;
        ensure!(
            checkpoint.group == group && checkpoint.comparison.file_key()? == path,
            "checkpoint identity is corrupt"
        );
        let contents = RetainedContents {
            old: read_content(self, old_content)?,
            new: read_content(self, new_content)?,
        };
        checkpoint
            .comparison
            .validate_retained(&contents, usize::MAX)?;
        Ok(Some((checkpoint, contents)))
    }

    pub async fn mark_unviewed(&self, target: ViewedTarget) -> Result<u64> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_unviewed", || {
                let session = read_session(connection, &target.session)?;
                require_revision(&session, target.expected_revision, target.displayed_generation)?;
                if session.snapshot.file(&target.group, &target.path)?.fingerprint()? != target.fingerprint {
                    anyhow::bail!(StaleReview);
                }
                connection.exec_bound::<(String, String, String)>(
                    "DELETE FROM review_checkpoints WHERE session_id = ? AND group_id = ? AND file_key = ?",
                )?((target.session, target.group, target.path))?;
                bump_revision(connection, &session)
            })
        }).await
    }

    /// Persist UI-only state without invalidating an in-flight GitHub receipt.
    /// A generation guard prevents stale views from writing into a newer snapshot.
    /// Review/checkpoint mutations retain their stronger revision guards.
    pub async fn save_navigation(
        &self,
        id: String,
        expected_generation: u64,
        presentation: ReviewPresentation,
    ) -> Result<()> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_navigation", || {
                let session = read_session(connection, &id)?;
                ensure!(
                    session.snapshot.generation == expected_generation,
                    "Review snapshot changed; navigation was not saved"
                );
                connection.exec_bound::<(String, String)>(
                    "UPDATE review_sessions SET presentation = ? WHERE id = ?",
                )?((serde_json::to_string(&presentation)?, id))?;
                Ok(())
            })
        })
        .await
    }

    pub async fn save_presentation(
        &self,
        id: String,
        expected_revision: u64,
        presentation: ReviewPresentation,
    ) -> Result<u64> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_presentation", || {
                let session = read_session(connection, &id)?;
                if session.revision != expected_revision {
                    anyhow::bail!(StaleReview);
                }
                let revision = bump_presentation_revision(connection, &session)?;
                connection.exec_bound::<(String, String)>(
                    "UPDATE review_sessions SET presentation = ? WHERE id = ?",
                )?((serde_json::to_string(&presentation)?, id))?;
                Ok(revision)
            })
        })
        .await
    }
}

#[path = "review_github_viewed_store.rs"]
mod github_viewed;
pub use github_viewed::{GitHubViewedIntent, GitHubViewedSyncResult, GitHubViewedSyncTarget};

#[path = "review_outbox_store.rs"]
mod outbox;
#[cfg(test)]
#[path = "review_outbox_tests.rs"]
mod outbox_tests;

#[cfg(test)]
#[path = "review_store_tests.rs"]
mod tests;
