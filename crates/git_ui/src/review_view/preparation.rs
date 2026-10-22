use super::*;
use futures::FutureExt as _;

const PREPARED_BUDGET: usize = 64 * 1024 * 1024;
const PREPARED_ENTRIES: usize = 128;
const PREWARM_BUDGET: usize = 48 * 1024 * 1024;

impl ReviewView {
    pub(super) fn prepare_file(
        &mut self,
        comparison: FileComparison,
        group: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> futures::future::Shared<Task<Result<PreparedReviewFile, Arc<anyhow::Error>>>> {
        let path = match comparison.file_key() {
            Ok(path) => path.to_owned(),
            Err(error) => return Task::ready(Err(Arc::new(error))).shared(),
        };
        let fingerprint = match comparison.fingerprint() {
            Ok(value) => format!("{}:{}:{}", self.data.session.id, path, value),
            Err(error) => return Task::ready(Err(Arc::new(error))).shared(),
        };
        if let Some(index) = self
            .prepared_files
            .iter()
            .position(|entry| entry.fingerprint == fingerprint)
        {
            if let Some(prepared) = self.prepared_files.remove(index) {
                self.prepared_files.push_back(prepared.clone());
                return Task::ready(Ok(prepared)).shared();
            }
        }
        if let Some(task) = self.loading_files.get(&fingerprint) {
            return task.clone();
        }
        let repository = self.repository.clone();
        let project = self.project.clone();
        let db = self.db.clone();
        let session_id = self.data.session.id.clone();
        let key = fingerprint.clone();
        let map_key = key.clone();
        let admission = self.preparation_lock.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let _permit = admission.lock().await;
            let result: Result<PreparedReviewFile> = async {
                let cached = cx.background_spawn({
                    let db = db.clone(); let id = session_id.clone(); let path = path.clone();
                    let comparison = comparison.clone(); let group = group.clone();
                    async move { db.matching_checkpoint_contents(&id, &group, &path, &comparison, REVIEW_PREVIEW_BUDGET as usize) }
                }).await?;
                let retained = if let Some(retained) = cached {
                    retained
                } else {
                    // Only cache misses need the original Git objects. Probe
                    // their lengths before materializing either side.
                    let objects = comparison.old.iter().chain(&comparison.new)
                        .filter(|side| side.mode != 0o160000)
                        .map(|side| side.object.hex.parse())
                        .collect::<Result<Vec<_>, _>>()?;
                    let expected_sizes = objects.len();
                    let sizes = repository.read_with(cx, |repository, cx| {
                        repository.review_blob_sizes(objects, cx)
                    }).await?;
                    ensure!(sizes.len() == expected_sizes, "Historical file size response is incomplete");
                    let preview_bytes = sizes.into_iter().try_fold(0u64, |total, size| {
                        total.checked_add(size).context("Historical file size overflow")
                    })?;
                    ensure!(preview_bytes <= REVIEW_PREVIEW_BUDGET,
                        "Preview unavailable: this file's old and new contents exceed the {} MiB preview limit. No contents were loaded or marked reviewed. Choose another file to continue reviewing.",
                        REVIEW_PREVIEW_BUDGET / (1024 * 1024));
                    let mut sides = Vec::new();
                    for side in [&comparison.old, &comparison.new] {
                        sides.push(
                            if let Some(side) = side.as_ref().filter(|side| side.mode != 0o160000) {
                                let object = side.object.hex.parse()?;
                                Some(
                                    repository
                                        .read_with(cx, |repo, cx| repo.review_blob(object, cx))
                                        .await?,
                                )
                            } else {
                                None
                            },
                        );
                    }
                    let mut sides = sides.into_iter();
                    RetainedContents {
                        old: sides.next().flatten(),
                        new: sides.next().flatten(),
                    }
                };
                let (retained, old_text, new_text, binary) = cx
                    .background_spawn({
                        let comparison = comparison.clone();
                        async move {
                            comparison.validate_retained(&retained, REVIEW_PREVIEW_BUDGET as usize)?;
                            let gitlink = comparison
                                .old
                                .iter()
                                .chain(&comparison.new)
                                .any(|side| side.mode == 0o160000);
                            let binary = gitlink
                                || [&retained.old, &retained.new].into_iter().flatten().any(
                                    |bytes| {
                                        bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
                                    },
                                );
                            let (old_text, new_text) = if binary {
                                let label = if gitlink {
                                    "Submodule identity changed (see object IDs above)"
                                } else {
                                    "Binary content changed (preview unavailable)"
                                }
                                .to_owned();
                                (Some(label.clone()), label)
                            } else {
                                (
                                    retained
                                        .old
                                        .as_ref()
                                        .map(|bytes| String::from_utf8(bytes.clone()))
                                        .transpose()?,
                                    retained
                                        .new
                                        .as_ref()
                                        .map(|bytes| String::from_utf8(bytes.clone()))
                                        .transpose()?
                                        .unwrap_or_default(),
                                )
                            };
                            if !binary {
                                if let Some(old_text) = &old_text { crate::commit_view::validate_review_text(old_text)?; }
                                crate::commit_view::validate_review_text(&new_text)?;
                            }
                            Ok::<_, anyhow::Error>((retained, old_text, new_text, binary))
                        }
                    })
                    .await?;
                let (registry, file) = cx.update(|_, cx| {
                    let repo_path = RepoPath::from_rel_path(RelPath::from_unix_str(&path)?);
                    let worktree_id = worktree_id_for_repo_path(
                        repository.read(cx),
                        project.read(cx),
                        &repo_path,
                        cx,
                    )
                    .context("repository worktree unavailable")?;
                    let file = Arc::new(GitBlob {
                        path: repo_path,
                        worktree_id,
                        is_deleted: comparison.new.is_none(),
                        is_binary: binary,
                        display_name: path.clone(),
                    }) as Arc<dyn File>;
                    Ok::<_, anyhow::Error>((project.read(cx).languages().clone(), file))
                })??;
                let buffer = build_buffer(new_text, file, &registry, cx).await?;
                let diff = build_buffer_diff(old_text, &buffer, &registry, cx).await?;
                let cost = retained.old.as_ref().map_or(0, Vec::len)
                    .saturating_add(retained.new.as_ref().map_or(0, Vec::len)).saturating_mul(4).max(16 * 1024);
                Ok(PreparedReviewFile { fingerprint, buffer, diff, retained: Arc::new(retained), cost, binary })

            }.await;
            let result = result.map_err(Arc::new);
            this.update(cx, |view, _| {
                view.loading_files.remove(&key);
                if let Ok(prepared) = &result {
                    if prepared.cost <= PREPARED_BUDGET && !view.prepared_files.iter().any(|entry| entry.fingerprint == key) {
                        while view.prepared_bytes.saturating_add(prepared.cost) > PREPARED_BUDGET || view.prepared_files.len() >= PREPARED_ENTRIES {
                            let Some(evicted) = view.prepared_files.pop_front() else { break; };
                            view.prepared_bytes = view.prepared_bytes.saturating_sub(evicted.cost);
                        }
                        view.prepared_bytes += prepared.cost;
                        view.prepared_files.push_back(prepared.clone());
                    }
                }
            }).log_err();
            result
        }).shared();
        self.loading_files.insert(map_key, task.clone());
        task
    }

    pub(super) fn start_prewarming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prewarm_task = None;
        if self.data.session.presentation.since_review
            || !self.data.session.selector.supports_github_viewed()
        {
            return;
        }
        let group = active_group(&self.data).to_owned();
        let Some(comparison_group) = self
            .data
            .session
            .snapshot
            .groups
            .iter()
            .find(|entry| entry.endpoints.group == group)
        else {
            return;
        };
        let center = comparison_group
            .files
            .iter()
            .position(|file| {
                file.file_key().ok() == self.data.session.presentation.selected_path.as_deref()
            })
            .unwrap_or(0);
        let mut files = comparison_group
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (index.abs_diff(center), file.clone()))
            .collect::<Vec<_>>();
        files.sort_by_key(|(distance, _)| *distance);
        let generation = self.data.session.snapshot.generation;
        let session_id = self.data.session.id.clone();
        self.prewarm_task = Some(cx.spawn_in(window, async move |this, cx| {
            for (distance, file) in files {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(20))
                    .await;
                let proceed = this
                    .read_with(cx, |view, _| {
                        view.data.session.id == session_id
                            && view.data.session.snapshot.generation == generation
                            && !view.busy
                            && (distance <= 5
                                || (view.prepared_bytes < PREWARM_BUDGET
                                    && view.prepared_files.len() < PREPARED_ENTRIES))
                    })
                    .unwrap_or(false);
                if !proceed {
                    break;
                }
                let already_prepared = this
                    .read_with(cx, |view, _| {
                        let key = format!(
                            "{}:{}:{}",
                            view.data.session.id,
                            file.file_key().unwrap_or_default(),
                            file.fingerprint().unwrap_or_default()
                        );
                        view.prepared_files
                            .iter()
                            .any(|entry| entry.fingerprint == key)
                    })
                    .unwrap_or(true);
                if already_prepared {
                    continue;
                }
                // Large previews stay demand-loaded; speculative work cannot monopolize the cache.
                let objects = match file
                    .old
                    .iter()
                    .chain(&file.new)
                    .filter(|side| side.mode != 0o160000)
                    .map(|side| side.object.hex.parse())
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(objects) => objects,
                    Err(error) => {
                        log::debug!("Review prewarm skipped invalid object: {error}");
                        continue;
                    }
                };
                let sizes = match this.read_with(cx, |view, cx| {
                    view.repository.read(cx).review_blob_sizes(objects, cx)
                }) {
                    Ok(task) => task.await,
                    Err(_) => break,
                };
                match sizes {
                    Ok(sizes)
                        if sizes
                            .iter()
                            .try_fold(0u64, |sum, size| sum.checked_add(*size))
                            .is_some_and(|size| size <= 1024 * 1024) => {}
                    Ok(_) => continue,
                    Err(error) => {
                        log::debug!("Review prewarm size probe failed: {error:#}");
                        continue;
                    }
                }
                let task = match this.update_in(cx, |view, window, cx| {
                    if view.data.session.id != session_id
                        || view.data.session.snapshot.generation != generation
                        || view.busy
                    {
                        return None;
                    }
                    Some(view.prepare_file(file, group.clone(), window, cx))
                }) {
                    Ok(Some(task)) => task,
                    Ok(None) | Err(_) => break,
                };
                if let Err(error) = task.await {
                    log::debug!("Review background preparation deferred: {error:#}");
                }
            }
        }));
    }
}
