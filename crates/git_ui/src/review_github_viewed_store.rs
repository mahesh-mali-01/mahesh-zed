//! Persisted remote approval is independent of retained local review history.
//! Every write uses the full PR's account/clone/session and pinned-generation CAS.
use super::{
    ReviewDb, StaleReview, ViewedTarget, bump_revision, ensure_durable, read_session,
    record_review_coverage_connection, require_revision, retain,
    review_coverage_revision_connection, revoke_review_coverage_connection,
};
use crate::{
    github_review::{RemoteViewedFile, ReviewContext, ViewedState},
    review::*,
    review_github_viewed::{
        GitHubViewedObservation, ViewedFileDecision, ViewedReconciliationInput,
        reconcile_github_viewed,
    },
};
use anyhow::{Context as _, Result, ensure};
use db::sqlez::connection::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitHubViewedSyncTarget {
    pub session: String,
    pub expected_revision: u64,
    pub displayed_generation: u64,
}
#[derive(Clone, Debug)]
pub struct GitHubViewedSyncResult {
    pub revision: u64,
    pub decisions: Vec<ViewedFileDecision>,
}
/// Durable SetFileViewed payload. Call the service once with every requested
/// path; its internal 20-item chunks avoid repeating full-PR pagination.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitHubViewedIntent {
    /// Persisted before dispatch. Recovery never guesses a missing origin.
    #[serde(default)]
    pub origin: Option<GitHubViewedSyncTarget>,
    pub paths: Vec<String>,
    pub viewed: bool,
    pub fingerprints: BTreeMap<String, String>,
}
impl GitHubViewedIntent {
    pub fn validate_for(&self, session: &ReviewSession) -> Result<()> {
        ensure!(
            session.selector.supports_github_viewed()
                && session.snapshot.incomplete_reason.is_none()
                && session.snapshot.groups.len() == 1,
            "Viewed intent requires a complete PR"
        );
        ensure!(!self.paths.is_empty(), "Viewed intent contains no files");
        let group = &session.snapshot.groups[0].endpoints.group;
        let mut seen = BTreeSet::new();
        for path in &self.paths {
            ensure!(seen.insert(path), "Duplicate Viewed intent path");
            ensure!(
                self.fingerprints.get(path)
                    == Some(&session.snapshot.file(group, path)?.fingerprint()?),
                "Viewed intent fingerprint changed; refresh before applying"
            );
        }
        ensure!(
            self.fingerprints.len() == self.paths.len(),
            "Viewed intent has unrequested fingerprints"
        );
        Ok(())
    }
}
struct CachedObservation {
    observation: GitHubViewedObservation,
    generation: u64,
    base_tip_oid: String,
}
fn metadata(connection: &Connection, session: &ReviewSession) -> Result<PullRequestMetadata> {
    let json = connection.select_row_bound::<String, String>(
        "SELECT metadata FROM review_pull_requests WHERE session_id = ?",
    )?(session.id.clone())?
    .context("PR metadata is unavailable")?;
    let metadata: PullRequestMetadata = serde_json::from_str(&json)?;
    metadata.validate_pinned_tips(session.repository.format)?;
    Ok(metadata)
}
fn require_full_context(
    connection: &Connection,
    target: &GitHubViewedSyncTarget,
    context: &ReviewContext,
) -> Result<(ReviewSession, PullRequestMetadata)> {
    let session = read_session(connection, &target.session)?;
    require_revision(
        &session,
        target.expected_revision,
        target.displayed_generation,
    )?;
    context.validate()?;
    ensure!(
        session.selector.supports_github_viewed()
            && session.selector.pull_request_identity() == Some(&context.identity),
        "Viewed response belongs to another PR/account or a subset scope"
    );
    ensure!(
        session.snapshot.incomplete_reason.is_none() && session.snapshot.groups.len() == 1,
        "Viewed synchronization requires a complete PR comparison"
    );
    let metadata = metadata(connection, &session)?;
    ensure!(
        metadata
            .owner
            .eq_ignore_ascii_case(&context.reference.repository.owner)
            && metadata
                .repository
                .eq_ignore_ascii_case(&context.reference.repository.name)
            && metadata.number == context.reference.number,
        "Viewed response repository changed"
    );
    ensure!(
        metadata.base_ref_oid == context.base_tip_oid
            && metadata.head_ref_oid == context.head_oid
            && session.snapshot.groups[0].endpoints.right.hex == context.head_oid,
        "Viewed response revision changed"
    );
    Ok((session, metadata))
}
fn read_cached(
    connection: &Connection,
    session: &ReviewSession,
    group: &str,
) -> Result<BTreeMap<String, CachedObservation>> {
    read_cached_at(connection, session, group, None)
}
fn read_cached_at(
    connection: &Connection,
    session: &ReviewSession,
    group: &str,
    path: Option<&str>,
) -> Result<BTreeMap<String, CachedObservation>> {
    let identity = session
        .selector
        .pull_request_identity()
        .context("Not a PR session")?;
    let rows=connection.select_bound::<(String,String,Option<String>,Option<String>),(String,String,i64,String,String)>("SELECT file_key, identity, generation, base_tip_oid, observation FROM review_github_viewed WHERE session_id = ? AND group_id = ? AND (? IS NULL OR file_key = ?)")?((session.id.clone(),group.to_owned(),path.map(str::to_owned),path.map(str::to_owned)))?;
    let mut observations = BTreeMap::new();
    for (path, serialized_identity, generation, base_tip_oid, json) in rows {
        let observation: GitHubViewedObservation = serde_json::from_str(&json)?;
        ensure!(
            serialized_identity == serde_json::to_string(identity)?
                && observation.identity == *identity
                && observation.group == group
                && observation.path == path
                && generation > 0
                && generation as u64 <= session.snapshot.generation,
            "Viewed observation identity is corrupt"
        );
        ObjectId {
            format: session.repository.format,
            hex: base_tip_oid.clone(),
        }
        .validate()?;
        ObjectId {
            format: session.repository.format,
            hex: observation.head_oid.clone(),
        }
        .validate()?;
        ensure!(
            observation.fingerprint.len() == 64
                && observation
                    .fingerprint
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "Viewed observation fingerprint is corrupt"
        );
        observations.insert(
            path,
            CachedObservation {
                observation,
                generation: generation as u64,
                base_tip_oid,
            },
        );
    }
    Ok(observations)
}
fn save_observation(
    connection: &Connection,
    session: &ReviewSession,
    base_tip: &str,
    observation: &GitHubViewedObservation,
) -> Result<()> {
    connection.exec_bound::<(String,String,String,i64,String,String,String)>("INSERT INTO review_github_viewed(session_id,group_id,file_key,generation,identity,base_tip_oid,observation) VALUES (?,?,?,?,?,?,?) ON CONFLICT(session_id,group_id,file_key) DO UPDATE SET generation=excluded.generation,identity=excluded.identity,base_tip_oid=excluded.base_tip_oid,observation=excluded.observation")?((session.id.clone(),observation.group.clone(),observation.path.clone(),session.snapshot.generation as i64,serde_json::to_string(&observation.identity)?,base_tip.to_owned(),serde_json::to_string(observation)?))?;
    Ok(())
}
fn checkpoint_metadata(
    connection: &Connection,
    session: &ReviewSession,
    group: &str,
) -> Result<BTreeMap<String, ReviewedCheckpoint>> {
    let rows=connection.select_bound::<(String,String),(String,i64,String)>("SELECT file_key,generation,checkpoint FROM review_checkpoints WHERE session_id = ? AND group_id = ?")?((session.id.clone(),group.to_owned()))?;
    let mut checkpoints = BTreeMap::new();
    for (path, generation, json) in rows {
        let checkpoint: ReviewedCheckpoint = serde_json::from_str(&json)?;
        checkpoint.comparison.validate(session.repository.format)?;
        ensure!(
            checkpoint.group == group
                && checkpoint.comparison.file_key()? == path
                && checkpoint.comparison.kind == ComparisonKind::Full
                && generation > 0
                && checkpoint.generation == generation as u64
                && checkpoint.generation <= session.snapshot.generation,
            "Retained review identity is corrupt"
        );
        checkpoints.insert(path, checkpoint);
    }
    // Only explicit old->new canonical comparisons provide rename aliases.
    // Never join unrelated files merely because their blobs happen to match.
    for file in &session
        .snapshot
        .groups
        .iter()
        .find(|entry| entry.endpoints.group == group)
        .context("Unknown comparison group")?
        .files
    {
        let path = file.file_key()?;
        if checkpoints.contains_key(path) {
            continue;
        }
        if let Some(old) = &file.old {
            if let Some(prior) = checkpoints.get(&old.path).cloned() {
                if prior
                    .comparison
                    .new
                    .as_ref()
                    .is_some_and(|side| side.path == old.path)
                {
                    checkpoints.insert(path.to_owned(), prior);
                    continue;
                }
            }
            // Without an explicit new->old mapping, keep this checkpoint under
            // its historical key. Reconciliation withholds approval for this
            // file; an ambiguous lineage must not prevent the whole PR loading.
        }
    }
    Ok(checkpoints)
}
fn unresolved_paths(
    connection: &Connection,
    session: &ReviewSession,
    current_paths: &[String],
) -> Result<BTreeSet<String>> {
    let identity = session
        .selector
        .pull_request_identity()
        .context("Not a PR session")?;
    let ids = connection.select_bound::<String, String>(
        "SELECT id FROM review_operations WHERE identity = ? AND state != 'sent'",
    )?(serde_json::to_string(identity)?)?;
    let mut paths = BTreeSet::new();
    for id in ids {
        let operation = super::outbox::read_operation(connection, &id)?;
        if operation.kind != ReviewOperationKind::SetFileViewed {
            continue;
        }
        if let Ok(intent) = serde_json::from_value::<GitHubViewedIntent>(operation.payload.clone())
        {
            for path in intent.paths {
                let superseded = operation.state == ReviewOperationState::DeliveryUnknown
                    && connection.select_row_bound::<(String,String,String,i64),String>(
                        "SELECT receipt_operation_id FROM review_github_viewed_superseded WHERE session_id = ? AND file_key = ? AND operation_id = ? AND generation = ?"
                    )?((session.id.clone(),path.clone(),operation.operation_id.clone(),session.snapshot.generation as i64))?
                    .is_some();
                if !superseded {
                    paths.insert(path);
                }
            }
        } else if let Some(path) = operation
            .payload
            .get("path")
            .and_then(serde_json::Value::as_str)
        {
            paths.insert(path.to_owned());
        } else {
            // An older or malformed unresolved intent is not evidence that a
            // remote flag can safely overwrite any local file's approval.
            paths.extend(current_paths.iter().cloned());
        }
    }
    Ok(paths)
}
fn has_review_changes(connection: &Connection, session_id: &str, revision: u64) -> Result<bool> {
    Ok(connection.select_row_bound::<(String, i64), i64>(
        "SELECT 1 FROM review_state_revisions WHERE session_id = ? AND revision > ? LIMIT 1",
    )?((session_id.to_owned(), revision as i64))?
    .is_some())
}
fn apply_confirmed_receipt(
    connection: &Connection,
    target: &GitHubViewedSyncTarget,
    operation_id: &str,
    receipts: Vec<RemoteViewedFile>,
) -> Result<u64> {
    let operation = super::outbox::read_operation(connection, &operation_id)?;
    ensure!(
        operation.kind == ReviewOperationKind::SetFileViewed
            && operation.state == ReviewOperationState::Sent,
        "Viewed receipt requires a confirmed sent local operation"
    );
    let context = ReviewContext {
        reference: crate::github_pull_requests::PullRequestReference {
            repository: crate::github_pull_requests::RepositoryName {
                owner: operation.context.metadata.owner.clone(),
                name: operation.context.metadata.repository.clone(),
            },
            number: operation.context.metadata.number,
        },
        identity: operation.context.identity()?.clone(),
        head_oid: operation.context.metadata.head_ref_oid.clone(),
        base_tip_oid: operation.context.metadata.base_ref_oid.clone(),
    };
    let current = read_session(connection, &target.session)?;
    ensure!(
        current.snapshot.generation == target.displayed_generation
            && current.revision >= target.expected_revision,
        "Viewed receipt snapshot changed"
    );
    let target = GitHubViewedSyncTarget {
        session: target.session.clone(),
        expected_revision: current.revision,
        displayed_generation: target.displayed_generation,
    };
    let (session, metadata) = require_full_context(connection, &target, &context)?;
    let already_applied = connection.select_row_bound::<(String,String),i64>("SELECT generation FROM review_github_viewed_receipts WHERE session_id = ? AND operation_id = ?")?((session.id.clone(),operation_id.to_owned()))?.is_some();
    if already_applied {
        return Ok(session.revision);
    }
    ensure!(
        operation.context.selector.supports_github_viewed(),
        "Subset operation cannot grant full-file Viewed approval"
    );
    let intent: GitHubViewedIntent = serde_json::from_value(operation.payload.clone())?;
    intent.validate_for(&session)?;
    let baseline = intent.origin.as_ref().unwrap_or(&target);
    ensure!(
        baseline.session == session.id
            && baseline.displayed_generation == session.snapshot.generation
            && baseline.expected_revision <= session.revision,
        "Viewed intent origin changed"
    );
    ensure!(
        !has_review_changes(connection, &session.id, baseline.expected_revision)?,
        "Newer review state supersedes this receipt; reload before marking again"
    );
    let mut states = BTreeMap::new();
    for receipt in receipts {
        ensure!(
            states
                .insert(receipt.path, receipt.viewer_viewed_state)
                .is_none(),
            "Duplicate Viewed receipt"
        );
    }
    ensure!(
        states.len() == intent.paths.len()
            && intent.paths.iter().all(|path| states
                .get(path)
                .is_some_and(|state| (*state == ViewedState::Viewed) == intent.viewed)),
        "Viewed receipt is partial or differs from the persisted intent"
    );
    let group = &session.snapshot.groups[0].endpoints.group;
    let unknown_ids = connection.select_bound::<String, String>(
        "SELECT id FROM review_operations WHERE identity = ? AND state = 'delivery_unknown'",
    )?(serde_json::to_string(&context.identity)?)?;
    let mut older_unknowns = Vec::new();
    for id in unknown_ids {
        let prior = super::outbox::read_operation(connection, &id)?;
        if prior.kind != ReviewOperationKind::SetFileViewed
            || prior.operation_id >= operation.operation_id
            || !prior.context.selector.supports_github_viewed()
            || prior.context.metadata.head_ref_oid == metadata.head_ref_oid
            || prior.context.metadata.owner != metadata.owner
            || prior.context.metadata.repository != metadata.repository
            || prior.context.metadata.number != metadata.number
        {
            continue;
        }
        if let Ok(prior_intent) = serde_json::from_value::<GitHubViewedIntent>(prior.payload) {
            older_unknowns.push((id, prior_intent.paths.into_iter().collect::<BTreeSet<_>>()));
        }
    }
    for path in &intent.paths {
        let observation = GitHubViewedObservation {
            identity: context.identity.clone(),
            group: group.clone(),
            path: path.clone(),
            head_oid: metadata.head_ref_oid.clone(),
            fingerprint: intent.fingerprints[path].clone(),
            state: states[path],
            trusted_viewed: intent.viewed,
        };
        save_observation(connection, &session, &metadata.base_ref_oid, &observation)?;
        // A later confirmed current-head intent can supersede an older
        // head's uncertain flag for this exact clone/generation/path.
        // The old outbox row stays Unknown and is never replayed.
        for (id, prior_paths) in &older_unknowns {
            if prior_paths.contains(path) {
                connection.exec_bound::<(String,String,String,String,i64)>("INSERT INTO review_github_viewed_superseded(session_id,file_key,operation_id,receipt_operation_id,generation) VALUES (?,?,?,?,?) ON CONFLICT(session_id,file_key,operation_id) DO UPDATE SET receipt_operation_id=excluded.receipt_operation_id,generation=excluded.generation")?((session.id.clone(),path.clone(),id.clone(),operation.operation_id.clone(),session.snapshot.generation as i64))?;
            }
        }
    }
    connection.exec_bound::<(String,String,i64)>("INSERT INTO review_github_viewed_receipts(session_id,operation_id,generation) VALUES (?,?,?)")?((session.id.clone(),operation_id.to_owned(),session.snapshot.generation as i64))?;
    bump_revision(connection, &session)
}
impl ReviewDb {
    /// One durable acceptance transaction: exact retained history, the latest
    /// local checkpoint, and its queued remote intent either all commit or all
    /// roll back. A process crash cannot leave an unqueued approval behind.
    pub async fn accept_github_viewed_intent(
        &self,
        target: GitHubViewedSyncTarget,
        context: ReviewOperationContext,
        intent: GitHubViewedIntent,
        contents: BTreeMap<String, RetainedContents>,
        existing: Option<ReviewOperation>,
        budget: usize,
    ) -> Result<(ReviewOperation, u64)> {
        self.accept_github_viewed_intent_inner(
            target, context, intent, contents, existing, budget, None,
        )
        .await
    }

    pub async fn accept_covered_github_viewed_intent(
        &self,
        target: GitHubViewedSyncTarget,
        context: ReviewOperationContext,
        intent: GitHubViewedIntent,
        contents: BTreeMap<String, RetainedContents>,
        existing: Option<ReviewOperation>,
        budget: usize,
        expected_coverage_revision: u64,
    ) -> Result<(ReviewOperation, u64)> {
        self.accept_github_viewed_intent_inner(
            target,
            context,
            intent,
            contents,
            existing,
            budget,
            Some(expected_coverage_revision),
        )
        .await
    }

    async fn accept_github_viewed_intent_inner(
        &self,
        target: GitHubViewedSyncTarget,
        context: ReviewOperationContext,
        mut intent: GitHubViewedIntent,
        mut contents: BTreeMap<String, RetainedContents>,
        existing: Option<ReviewOperation>,
        budget: usize,
        expected_coverage_revision: Option<u64>,
    ) -> Result<(ReviewOperation, u64)> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_github_viewed_accept", || {
                let session = read_session(connection, &target.session)?;
                require_revision(&session, target.expected_revision, target.displayed_generation)?;
                if let Some(expected) = expected_coverage_revision {
                    if review_coverage_revision_connection(connection, &session)? != expected { anyhow::bail!(StaleReview); }
                }
                let inferred = expected_coverage_revision.is_some();
                context.validate()?;
                ensure!(context.selector == session.selector && context.metadata == metadata(connection, &session)?, "Viewed context changed before local acceptance");
                intent.validate_for(&session)?;
                let group = session.snapshot.groups.first().context("PR comparison missing")?.endpoints.group.clone();
                ensure!(contents.len() == if intent.viewed { intent.paths.len() } else { 0 }, "Viewed contents do not match the requested paths");
                let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                for path in &intent.paths {
                    if intent.viewed {
                        let comparison = session.snapshot.file(&group, path)?.clone();
                        let bytes = contents.remove(path).context("Reviewed contents are missing")?;
                        comparison.validate_retained(&bytes, budget)?;
                        let checkpoint = ReviewedCheckpoint { group:group.clone(), comparison, generation:session.snapshot.generation, reviewed_at_unix_seconds:timestamp };
                        if inferred {
                            connection.exec_bound::<(String,String,String)>("INSERT INTO review_coverage_promotions(session_id,file_key,fingerprint) VALUES (?,?,?) ON CONFLICT(session_id,file_key) DO UPDATE SET fingerprint=excluded.fingerprint")?((session.id.clone(),path.clone(),intent.fingerprints[path].clone()))?;
                        } else {
                            record_review_coverage_connection(connection, &session, &group, path)?;
                        }
                        let old = retain(connection, bytes.old)?;
                        let new = retain(connection, bytes.new)?;
                        connection.exec_bound::<(String,String,String,i64,String,Option<String>,Option<String>)>("INSERT INTO review_checkpoints(session_id,group_id,file_key,generation,checkpoint,old_content,new_content) VALUES (?,?,?,?,?,?,?) ON CONFLICT(session_id,group_id,file_key) DO UPDATE SET generation=excluded.generation,checkpoint=excluded.checkpoint,old_content=excluded.old_content,new_content=excluded.new_content")?((session.id.clone(),group.clone(),path.clone(),session.snapshot.generation as i64,serde_json::to_string(&checkpoint)?,old,new))?;
                    } else {
                        if inferred {
                            let promotion=connection.select_row_bound::<(String,String),String>("SELECT fingerprint FROM review_coverage_promotions WHERE session_id=? AND file_key=?")?((session.id.clone(),path.clone()))?;
                            ensure!(promotion.as_ref()==intent.fingerprints.get(path), "Only coverage-owned Viewed state can be revoked automatically");
                        } else {
                            revoke_review_coverage_connection(connection, &session, &group, path)?;
                        }
                        connection.exec_bound::<(String,String,String)>("DELETE FROM review_checkpoints WHERE session_id = ? AND group_id = ? AND file_key = ?")?((session.id.clone(),group.clone(),path.clone()))?;
                    }
                }
                let revision = bump_revision(connection, &session)?;
                intent.origin = Some(GitHubViewedSyncTarget { session:session.id.clone(), expected_revision:revision, displayed_generation:session.snapshot.generation });
                // Rapid toggles of the same exact file set update only an unsent
                // durable row. A Sending/Unknown row is immutable: a later intent
                // must get its own operation and run after it.
                let existing = if existing.is_some() { existing } else {
                    let ids = connection.select_bound::<String, String>(
                        "SELECT id FROM review_operations WHERE identity = ? AND state = 'queued' ORDER BY id DESC",
                    )?(serde_json::to_string(context.identity()?)?)?;
                    let mut reusable = None;
                    let mut newer_paths = BTreeSet::new();
                    for id in ids {
                        let candidate = super::outbox::read_operation(connection, &id)?;
                        if candidate.kind != ReviewOperationKind::SetFileViewed || candidate.context != context { continue; }
                        let prior: GitHubViewedIntent = serde_json::from_value(candidate.payload.clone())?;
                        if prior.origin.as_ref().is_some_and(|origin| origin.session == session.id && origin.displayed_generation == session.snapshot.generation)
                            && prior.paths.iter().all(|path| !newer_paths.contains(path))
                            && prior.paths.iter().collect::<BTreeSet<_>>() == intent.paths.iter().collect::<BTreeSet<_>>() {
                            reusable = Some(candidate); break;
                        }
                        newer_paths.extend(prior.paths);
                    }
                    reusable
                };
                let payload = serde_json::to_value(intent)?;
                let operation = if let Some(existing) = existing {
                    let mut current = super::outbox::read_operation(connection, &existing.operation_id)?;
                    ensure!(current.revision == existing.revision && current.context == context && current.kind == ReviewOperationKind::SetFileViewed && matches!(current.state, ReviewOperationState::LocalDraft | ReviewOperationState::Queued | ReviewOperationState::Failed), "Only this unchanged unsent Viewed operation can be resumed");
                    current.payload = payload;
                    current.location = ReviewLocationState::NotRequired;
                    current.state = ReviewOperationState::Queued;
                    current.last_error = None;
                    current.ensure_dispatchable()?;
                    let old_revision = current.revision;
                    super::outbox::store_changed(connection, current, old_revision)?
                } else {
                    let operation = ReviewOperation { operation_id:db::uuid::Uuid::now_v7().to_string(), revision:0, context, kind:ReviewOperationKind::SetFileViewed, payload, location:ReviewLocationState::NotRequired, state:ReviewOperationState::Queued, before_server_ids:Vec::new(), server_ids:Vec::new(), last_error:None, created_at_unix_seconds:timestamp, updated_at_unix_seconds:timestamp };
                    operation.ensure_dispatchable()?;
                    connection.exec_bound::<(String,String,i64,String,String)>("INSERT INTO review_operations(id,identity,revision,state,record) VALUES (?,?,?,?,?)")?((operation.operation_id.clone(),serde_json::to_string(operation.context.identity()?)?,0,operation.state.as_str().into(),serde_json::to_string(&operation)?))?;
                    operation
                };
                Ok((operation, revision))
            })
        }).await
    }

    /// The latest durable per-path user intent, scoped to this exact local
    /// session and pinned contents. Terminal older operations cannot overwrite
    /// a newer toggle; a successful later intent supersedes older failures.
    pub fn github_viewed_local_intents(
        &self,
        session: &ReviewSession,
    ) -> Result<BTreeMap<String, bool>> {
        let Some(identity) = session.selector.pull_request_identity() else {
            return Ok(BTreeMap::new());
        };
        let mut latest: BTreeMap<String, ((u64, String), bool, bool)> = BTreeMap::new();
        let group = session
            .snapshot
            .groups
            .first()
            .context("PR comparison missing")?
            .endpoints
            .group
            .clone();
        let confirmed = read_cached(self, session, &group)?;
        for operation in self.operations(identity)? {
            if operation.kind != ReviewOperationKind::SetFileViewed
                || operation.context.selector != session.selector
            {
                continue;
            }
            let intent: GitHubViewedIntent = serde_json::from_value(operation.payload)?;
            if intent.validate_for(session).is_err() {
                continue;
            }
            if intent.origin.as_ref().is_none_or(|origin| {
                origin.session != session.id
                    || origin.displayed_generation != session.snapshot.generation
            }) {
                continue;
            }
            for path in intent.paths {
                let acknowledged = operation.state == ReviewOperationState::Sent
                    && confirmed.get(&path).is_some_and(|cached| {
                        cached.observation.fingerprint == intent.fingerprints[&path]
                            && (cached.observation.state == ViewedState::Viewed
                                && cached.observation.trusted_viewed)
                                == intent.viewed
                    });
                // Explicit retry can reuse an older operation UUID. Acceptance
                // revision, not creation order, determines the user's latest intent.
                let order = (
                    intent
                        .origin
                        .as_ref()
                        .map_or(0, |origin| origin.expected_revision),
                    operation.operation_id.clone(),
                );
                if latest.get(&path).is_none_or(|(prior, _, _)| prior < &order) {
                    latest.insert(path, (order, intent.viewed, !acknowledged));
                }
            }
        }
        Ok(latest
            .into_iter()
            .filter_map(|(path, (_, viewed, pending))| pending.then_some((path, viewed)))
            .collect())
    }

    /// Metadata only: retained old/new blob contents are never loaded here.
    pub fn reviewed_checkpoint_metadata(
        &self,
        session_id: &str,
        group: &str,
    ) -> Result<BTreeMap<String, ReviewedCheckpoint>> {
        ensure_durable(self)?;
        let session = read_session(self, session_id)?;
        ensure!(
            session
                .snapshot
                .groups
                .iter()
                .any(|entry| entry.endpoints.group == group),
            "Unknown comparison group"
        );
        checkpoint_metadata(self, &session, group)
    }
    pub fn github_viewed_observations(
        &self,
        session_id: &str,
        group: &str,
    ) -> Result<BTreeMap<String, GitHubViewedObservation>> {
        ensure_durable(self)?;
        let session = read_session(self, session_id)?;
        ensure!(
            session.selector.supports_github_viewed()
                && session
                    .snapshot
                    .groups
                    .iter()
                    .any(|entry| entry.endpoints.group == group),
            "Viewed cache is unavailable for this scope"
        );
        Ok(read_cached(self, &session, group)?
            .into_iter()
            .map(|(path, cached)| (path, cached.observation))
            .collect())
    }
    /// Apply these overrides AFTER computing local checkpoint matches. False
    /// suppresses approval without deleting bytes used by Since review. Pending
    /// local operations retain local intent instead of applying a remote flag.
    pub fn github_viewed_overrides(
        &self,
        session: &ReviewSession,
        group: &str,
    ) -> Result<BTreeMap<String, bool>> {
        ensure_durable(self)?;
        let current = read_session(self, &session.id)?;
        require_revision(&current, session.revision, session.snapshot.generation)?;
        if !session.selector.supports_github_viewed() {
            return Ok(BTreeMap::new());
        }
        let comparisons = session
            .snapshot
            .groups
            .iter()
            .find(|entry| entry.endpoints.group == group)
            .context("Unknown PR comparison group")?;
        let paths = comparisons
            .files
            .iter()
            .map(|file| Ok(file.file_key()?.to_owned()))
            .collect::<Result<Vec<_>>>()?;
        let pending = unresolved_paths(self, session, &paths)?;
        let observations = read_cached(self, session, group)?;
        let mut overrides = BTreeMap::new();
        for file in &comparisons.files {
            let path = file.file_key()?;
            if pending.contains(path) {
                continue;
            }
            if let Some(cached) = observations.get(path) {
                if cached.observation.fingerprint == file.fingerprint()? {
                    overrides.insert(
                        path.to_owned(),
                        cached.observation.state == ViewedState::Viewed
                            && cached.observation.trusted_viewed,
                    );
                }
            }
        }
        Ok(overrides)
    }
    /// Complete server reads use the conservative fingerprint policy. This
    /// writes observations only; retain blobs later, one bounded file at a time.
    pub async fn sync_github_viewed(
        &self,
        target: GitHubViewedSyncTarget,
        context: ReviewContext,
        remote: Vec<RemoteViewedFile>,
        mut pending_paths: BTreeSet<String>,
    ) -> Result<GitHubViewedSyncResult> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_github_viewed_sync", || {
                let (session, metadata) = require_full_context(connection, &target, &context)?;
                let group = &session.snapshot.groups[0].endpoints.group;
                let previous = read_cached(connection, &session, group)?;
                let observations = previous
                    .iter()
                    .map(|(path, cached)| (path.clone(), cached.observation.clone()))
                    .collect();
                let checkpoints = checkpoint_metadata(connection, &session, group)?;
                let current_paths = session.snapshot.groups[0]
                    .files
                    .iter()
                    .map(|file| Ok(file.file_key()?.to_owned()))
                    .collect::<Result<Vec<_>>>()?;
                pending_paths.extend(unresolved_paths(connection, &session, &current_paths)?);
                let decisions = reconcile_github_viewed(ViewedReconciliationInput {
                    session: &session,
                    metadata: &metadata,
                    context: &context,
                    remote: &remote,
                    checkpoints: &checkpoints,
                    observations: &observations,
                    pending_paths: &pending_paths,
                })?;
                let mut changed = false;
                for decision in &decisions {
                    if let Some(observation) = &decision.observation {
                        let unchanged = previous.get(&decision.path).is_some_and(|cached| {
                            cached.observation == *observation
                                && cached.generation == session.snapshot.generation
                                && cached.base_tip_oid == context.base_tip_oid
                        });
                        if !unchanged {
                            save_observation(
                                connection,
                                &session,
                                &context.base_tip_oid,
                                observation,
                            )?;
                            changed = true;
                        }
                    }
                }
                let revision = if changed {
                    bump_revision(connection, &session)?
                } else {
                    session.revision
                };
                Ok(GitHubViewedSyncResult {
                    revision,
                    decisions,
                })
            })
        })
        .await
    }
    /// Only an independently confirmed Sent local intent can override a changed
    /// prior fingerprint directly. Plain server reads must go through sync above.
    pub async fn record_github_viewed_receipt(
        &self,
        target: GitHubViewedSyncTarget,
        operation_id: String,
        receipts: Vec<RemoteViewedFile>,
    ) -> Result<u64> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_github_viewed_receipt", || {
                apply_confirmed_receipt(connection, &target, &operation_id, receipts)
            })
        })
        .await
    }
    /// Recover the local application of confirmed Sent intents after a crash.
    /// This never sends a GitHub request. Each intent is applied once per clone
    /// and must still match the current account, full scope and pinned contents.
    pub async fn recover_github_viewed_receipts(
        &self,
        target: GitHubViewedSyncTarget,
    ) -> Result<u64> {
        self.write(move |connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_github_viewed_recover", || {
                let session = read_session(connection, &target.session)?;
                ensure!(session.snapshot.generation == target.displayed_generation && session.revision >= target.expected_revision, "Viewed recovery snapshot changed");
                ensure!(session.selector.supports_github_viewed(), "Viewed recovery requires a full PR");
                let identity = session.selector.pull_request_identity().context("Missing PR identity")?;
                let metadata = metadata(connection, &session)?;
                let ids = connection.select_bound::<(String,String),String>("SELECT o.id FROM review_operations o WHERE o.identity = ? AND o.state = 'sent' AND NOT EXISTS (SELECT 1 FROM review_github_viewed_receipts r WHERE r.session_id = ? AND r.operation_id = o.id) ORDER BY o.id DESC")?((serde_json::to_string(identity)?,session.id.clone()))?;
                let mut revision = session.revision;
                for id in ids {
                    let operation = super::outbox::read_operation(connection, &id)?;
                    if operation.kind != ReviewOperationKind::SetFileViewed
                        || operation.context.selector != session.selector
                        || operation.context.metadata != metadata { continue; }
                    let Ok(intent) = serde_json::from_value::<GitHubViewedIntent>(operation.payload) else { continue; };
                    if intent.validate_for(&session).is_err() { continue; }
                    let Some(origin) = &intent.origin else { continue; };
                    if origin.session != session.id || origin.displayed_generation != session.snapshot.generation || origin.expected_revision > session.revision || has_review_changes(connection, &session.id, origin.expected_revision)? { continue; }
                    let receipts = intent.paths.into_iter().map(|path| RemoteViewedFile { path, viewer_viewed_state: if intent.viewed { ViewedState::Viewed } else { ViewedState::Unviewed } }).collect();
                    revision = apply_confirmed_receipt(connection, &GitHubViewedSyncTarget { session:session.id.clone(),expected_revision:revision,displayed_generation:target.displayed_generation }, &id, receipts)?;
                }
                Ok(revision)
            })
        }).await
    }
    /// Call from a background task, streaming one file's current old/new bytes.
    /// This validates exact contents and budget before replacing any checkpoint.
    pub async fn import_github_viewed_checkpoint(
        &self,
        target: ViewedTarget,
        identity: PullRequestIdentity,
        contents: RetainedContents,
        budget: usize,
    ) -> Result<u64> {
        self.write(move|connection| {
            ensure_durable(connection)?;
            connection.with_savepoint("review_github_viewed_import",|| {
                let session=read_session(connection,&target.session)?;
                require_revision(&session,target.expected_revision,target.displayed_generation)?;
                ensure!(session.selector.supports_github_viewed() && session.selector.pull_request_identity()==Some(&identity) && session.snapshot.incomplete_reason.is_none(),"Viewed import requires this account's complete PR");
                let metadata=metadata(connection,&session)?;
                let comparison=session.snapshot.file(&target.group,&target.path)?.clone();
                if comparison.fingerprint()?!=target.fingerprint{anyhow::bail!(StaleReview);}
                let cache=read_cached_at(connection,&session,&target.group,Some(&target.path))?;
                let cached=cache.get(&target.path).context("No verified remote Viewed observation")?;
                ensure!(cached.observation.state==ViewedState::Viewed && cached.observation.trusted_viewed && cached.observation.fingerprint==target.fingerprint && cached.observation.head_oid==metadata.head_ref_oid && cached.base_tip_oid==metadata.base_ref_oid && cached.generation==session.snapshot.generation,"Remote Viewed observation is stale or cannot approve current bytes");
                ensure!(!unresolved_paths(connection,&session,std::slice::from_ref(&target.path))?.contains(&target.path),"A local Viewed operation still needs reconciliation");
                comparison.validate_retained(&contents,budget)?;
                let checkpoint=ReviewedCheckpoint{group:target.group.clone(),comparison,generation:target.displayed_generation,reviewed_at_unix_seconds:SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()};
                let old_content=retain(connection,contents.old)?;let new_content=retain(connection,contents.new)?;
                connection.exec_bound::<(String,String,String,i64,String,Option<String>,Option<String>)>("INSERT INTO review_checkpoints(session_id,group_id,file_key,generation,checkpoint,old_content,new_content) VALUES (?,?,?,?,?,?,?) ON CONFLICT(session_id,group_id,file_key) DO UPDATE SET generation=excluded.generation,checkpoint=excluded.checkpoint,old_content=excluded.old_content,new_content=excluded.new_content")?((target.session,target.group,target.path,checkpoint.generation as i64,serde_json::to_string(&checkpoint)?,old_content,new_content))?;
                bump_revision(connection,&session)
            })
        }).await
    }
}

#[cfg(test)]
#[path = "review_github_viewed_store_tests.rs"]
mod tests;
