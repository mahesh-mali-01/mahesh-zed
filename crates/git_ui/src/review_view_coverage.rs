//! Accumulated selected-commit review and promotion to canonical PR Viewed.
use super::*;
use crate::github_review::ReviewContext;
use crate::review_outbox_runtime::Delivery;

fn scope_targets(session: &ReviewSession, paths: &[String]) -> Result<Vec<ViewedTarget>> {
    ensure!(
        matches!(
            session.selector,
            ComparisonSelector::PullRequestScope { .. }
        ),
        "Select PR commits before marking them reviewed"
    );
    ensure!(
        session.snapshot.incomplete_reason.is_none(),
        "Incomplete comparisons cannot be reviewed"
    );
    let paths = paths.iter().collect::<BTreeSet<_>>();
    for path in &paths {
        ensure!(
            session.snapshot.groups.iter().any(|group| group
                .files
                .iter()
                .any(|file| file.file_key().ok() == Some(path.as_str()))),
            "Selected file is missing from this comparison"
        );
    }
    session
        .snapshot
        .groups
        .iter()
        .flat_map(|group| {
            group.files.iter().filter_map(|file| match file.file_key() {
                Ok(path) if paths.contains(&path.to_owned()) => {
                    Some(file.fingerprint().map(|fingerprint| ViewedTarget {
                        session: session.id.clone(),
                        expected_revision: session.revision,
                        displayed_generation: session.snapshot.generation,
                        group: group.endpoints.group.clone(),
                        path: path.to_owned(),
                        fingerprint,
                    }))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
        })
        .collect()
}

impl ReviewView {
    pub(super) fn mark_selected_commits_reviewed(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review_commit_paths(self.data.files.clone(), true, false, window, cx);
    }

    fn set_coverage_status(
        &mut self,
        origin: &ReviewSession,
        message: String,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.data.session.id != origin.id
            || self.data.session.snapshot.generation != origin.snapshot.generation
        {
            return;
        }
        self.github_viewed_status = Some(message);
        self.github_viewed_error = error;
        if let Some(panel) = &self.github_panel {
            panel.update(cx, |panel, cx| panel.reload_local_operations(cx));
        }
        cx.notify();
    }

    pub(super) fn review_commit_paths(
        &mut self,
        paths: Vec<String>,
        reviewed: bool,
        retain_contents: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy || self.refreshing || self.cached_unverified || self.outdated() {
            return;
        }
        let targets = match scope_targets(&self.data.session, &paths) {
            Ok(targets) if !targets.is_empty() => targets,
            Ok(_) => return,
            Err(error) => {
                self.github_viewed_error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let db = self.db.clone();
        let origin = self.data.session.clone();
        let repository = self.repository.clone();
        let changes = self.changes.clone();
        let recovery = crate::review_outbox_runtime::ready(db.clone(), cx);
        self.busy = true;
        self.set_coverage_status(
            &origin,
            format!("Saving review of {} file comparisons…", targets.len()),
            None,
            cx,
        );
        // Navigation owns `self.task`; it must not cancel durable review or delivery.
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<ReviewData> = async {
                let mut contents = BTreeMap::new();
                if retain_contents && reviewed {
                    let mut remaining = 256 * 1024 * 1024;
                    for target in &targets {
                        let comparison = origin.snapshot.file(&target.group, &target.path)?;
                        let bytes = github_viewed::retained_comparison(&repository, comparison, (REVIEW_PREVIEW_BUDGET as usize).min(remaining), cx).await?;
                        remaining -= bytes.old.as_ref().map_or(0, Vec::len) + bytes.new.as_ref().map_or(0, Vec::len);
                        contents.insert((target.group.clone(), target.path.clone()), bytes);
                    }
                }
                cx.background_spawn({
                    let db = db.clone(); let id = origin.id.clone();
                    async move {
                        if retain_contents {
                            db.set_review_coverage_with_contents(targets, reviewed, contents, REVIEW_PREVIEW_BUDGET as usize).await?;
                        } else { db.set_review_coverage(targets, reviewed).await?; }
                        read_data(&db, db.load_session(&id)?)
                    }
                }).await
            }.await;
            let accepted = result.is_ok();
            this.update_in(cx, |this, window, cx| {
                if this.data.session.id == origin.id && this.data.session.snapshot.generation == origin.snapshot.generation { this.finish(result, window, cx); }
            }).log_err();
            if !accepted { return; }
            if let Err(error) = recovery.await {
                this.update_in(cx, |this, _, cx| this.set_coverage_status(&origin, "Review saved locally · recovery needs attention.".into(), Some(error.to_string()), cx)).log_err();
                return;
            }
            this.update_in(cx, |this, _, cx| this.set_coverage_status(&origin, "Review saved locally · checking complete-file coverage…".into(), None, cx)).log_err();
            match promote_complete_files(&db, &repository, &origin, paths, &changes, cx).await {
                Ok((count, deliveries)) => {
                    this.update_in(cx, |this, _, cx| this.set_coverage_status(&origin, if count == 0 { "Selected review saved · full-file status unchanged.".into() } else { format!("{count} file statuses saved locally · syncing with GitHub…") }, None, cx)).log_err();
                    let mut failure = None;
                    for delivery in deliveries {
                        match delivery.await {
                            Ok(operation) if operation.state == ReviewOperationState::Sent => {},
                            Ok(operation) => failure = Some(operation.last_error.unwrap_or_else(|| "Viewed sync needs attention; use Pending actions to reconcile.".into())),
                            Err(error) => failure = Some(error.to_string()),
                        }
                    }
                    this.update_in(cx, |this, _, cx| {
                        let message = if failure.is_some() { "Review saved locally · GitHub sync needs attention.".into() }
                            else if count > 0 { format!("{count} file statuses synchronized with GitHub.") }
                            else { "Selected review saved · full-file status unchanged.".into() };
                        this.set_coverage_status(&origin, message, failure, cx);
                    }).log_err();
                }
                Err(error) => { this.update_in(cx, |this, _, cx| this.set_coverage_status(&origin, "Review saved locally · synchronization needs attention.".into(), Some(format!("Coverage synchronization paused; earlier saved batches continue: {error:#}")), cx)).log_err(); }
            }
        }).detach();
        cx.notify();
    }
}

async fn promote_complete_files(
    db: &ReviewDb,
    repository: &Entity<Repository>,
    scope: &ReviewSession,
    paths: Vec<String>,
    changes: &Entity<ReviewChangeBus>,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<(usize, Vec<Delivery>)> {
    let (mut canonical, metadata) = cx.background_spawn({
        let db = db.clone(); let scope = scope.clone();
        async move {
            let identity = scope.selector.pull_request_identity().context("Missing PR identity")?;
            let canonical = db.find_pull_request_session(&scope.repository.id, identity)?.context("Open All PR changes and Refresh before synchronizing selected-commit reviews")?;
            let metadata = db.pull_request_metadata(&scope.id)?.context("Missing PR metadata")?;
            ensure!(db.pull_request_metadata(&canonical.id)?.as_ref() == Some(&metadata), "All PR changes has a different revision; refresh it before synchronizing coverage");
            anyhow::Ok((canonical, metadata))
        }
    }).await?;
    let base = metadata.base_ref_oid.parse()?;
    let head = metadata.head_ref_oid.parse()?;
    let history = repository
        .read_with(cx, |repository, cx| {
            repository.review_history(Some(base), head, cx)
        })
        .await?;
    if let Some(expected) = metadata.commit_count {
        ensure!(history.len() == expected, "PR history is incomplete");
    }
    let mut verified = BTreeSet::new();
    for commit in history {
        verified.insert(commit.commit.to_string());
        verified.extend(commit.parents.iter().map(ToString::to_string));
    }
    for group in &canonical.snapshot.groups {
        verified.insert(group.endpoints.left.hex.clone());
    }
    let (covered, retract, coverage_revision) = cx
        .background_spawn({
            let db = db.clone();
            let canonical = canonical.clone();
            async move {
                let canonical_paths = canonical.snapshot.groups[0]
                    .files
                    .iter()
                    .map(|file| file.file_key().map(str::to_owned))
                    .collect::<Result<BTreeSet<_>>>()?;
                let paths = paths
                    .into_iter()
                    .filter(|path| canonical_paths.contains(path))
                    .collect();
                let coverage_revision = db.review_coverage_revision(&canonical)?;
                let covered = db.reviewed_files_coverage(&canonical, &paths, &verified)?;
                let promoted = db.coverage_promoted_paths(&canonical)?;
                let mut effective = read_data(&db, canonical.clone())?.viewed;
                for (path, viewed) in db.github_viewed_local_intents(&canonical)? {
                    if viewed {
                        effective.insert(path);
                    } else {
                        effective.remove(&path);
                    }
                }
                let retract = promoted
                    .intersection(&paths)
                    .filter(|path| !covered.contains(*path) && effective.contains(*path))
                    .cloned()
                    .collect::<Vec<_>>();
                let exact = db.reviewed_checkpoint_metadata(
                    &canonical.id,
                    &canonical.snapshot.groups[0].endpoints.group,
                )?;
                let covered = covered
                    .into_iter()
                    .filter(|path| {
                        !effective.contains(path)
                            || !exact.get(path).is_some_and(|checkpoint| {
                                canonical
                                    .snapshot
                                    .file(&canonical.snapshot.groups[0].endpoints.group, path)
                                    .is_ok_and(|file| {
                                        checkpoint
                                            .matches(
                                                &canonical.snapshot.groups[0].endpoints.group,
                                                file,
                                            )
                                            .unwrap_or(false)
                                    })
                            })
                    })
                    .collect::<Vec<_>>();
                anyhow::Ok((covered, retract, coverage_revision))
            }
        })
        .await?;
    let context = ReviewOperationContext {
        selector: canonical.selector.clone(),
        metadata: metadata.clone(),
    };
    context.validate()?;
    let remote = ReviewContext {
        reference: PullRequestReference::parse(&metadata.url(), None)?,
        identity: scope
            .selector
            .pull_request_identity()
            .context("Missing PR identity")?
            .clone(),
        head_oid: metadata.head_ref_oid.clone(),
        base_tip_oid: metadata.base_ref_oid.clone(),
    };
    let mut deliveries = Vec::new();
    let count = covered.len() + retract.len();
    for (viewed, paths) in [(true, covered), (false, retract)] {
        for batch in paths.chunks(20) {
            let mut contents = BTreeMap::new();
            let mut remaining = 256 * 1024 * 1024;
            if viewed {
                for path in batch {
                    let comparison = canonical
                        .snapshot
                        .file(&canonical.snapshot.groups[0].endpoints.group, path)?;
                    let bytes = github_viewed::retained_comparison(
                        repository,
                        comparison,
                        (REVIEW_PREVIEW_BUDGET as usize).min(remaining),
                        cx,
                    )
                    .await?;
                    remaining -= bytes.old.as_ref().map_or(0, Vec::len)
                        + bytes.new.as_ref().map_or(0, Vec::len);
                    contents.insert(path.clone(), bytes);
                }
            }
            let intent = github_viewed::viewed_intent(&canonical, batch.to_vec(), viewed)?;
            let (operation, revision) = db
                .accept_covered_github_viewed_intent(
                    github_viewed::sync_target(&canonical, canonical.revision),
                    context.clone(),
                    intent,
                    contents,
                    None,
                    REVIEW_PREVIEW_BUDGET as usize,
                    coverage_revision,
                )
                .await?;
            canonical.revision = revision;
            // The app retains delivery before the next batch can fail or this tab can close.
            let delivery = cx.update(|_, cx| {
                let delivery = github_viewed::enqueue_viewed_delivery(
                    db.clone(),
                    operation,
                    remote.clone(),
                    canonical.clone(),
                    cx,
                );
                changes.update(cx, |_, cx| {
                    cx.emit(ReviewChanged {
                        session: canonical.id.clone(),
                        revision,
                    })
                });
                delivery
            })?;
            deliveries.push(delivery);
        }
    }
    Ok((count, deliveries))
}
