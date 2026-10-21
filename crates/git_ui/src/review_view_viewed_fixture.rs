//! Explicit offline native acceptance harness. Only the sentinel identity can
//! route here, and a marked disposable checkout is required. No fixture request
//! may fall back to GitHub, including after restart or a missing environment.
use super::*;
use crate::github_review::ReviewTransport;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

const MARKER: &str = "zed-offline-viewed-v1";
const REPO: &str = "offline_viewed_repository";
const PR: &str = "offline_viewed_pull_request";
const VIEWER: &str = "offline_viewed_viewer";
static IO: Mutex<()> = Mutex::new(());

pub(in crate::review_view) fn matches(selector: &ComparisonSelector) -> bool {
    selector.pull_request_identity().is_some_and(|identity| {
        identity.repository_node_id == REPO
            && identity.pull_request_node_id == PR
            && identity.viewer_node_id == VIEWER
    })
}
fn root() -> Result<PathBuf> {
    let root = PathBuf::from(
        std::env::var_os("ZED_REVIEW_VIEWED_FIXTURE_ROOT")
            .context("Set ZED_REVIEW_VIEWED_FIXTURE_ROOT to the marked disposable checkout")?,
    )
    .canonicalize()?;
    ensure!(
        std::fs::read_to_string(root.join(".zed-review-viewed-fixture"))?.trim() == MARKER,
        "Offline fixture marker is missing or invalid"
    );
    ensure!(
        root.join(".git").is_dir(),
        "Offline fixture must be an ordinary disposable Git checkout"
    );
    Ok(root)
}
fn paths() -> Vec<String> {
    (0..21)
        .map(|index| format!("docs/item-{index:02}.md"))
        .collect()
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    viewed: BTreeSet<String>,
    #[serde(default)]
    mutation_paths: Vec<Vec<String>>,
    #[serde(default)]
    reads: usize,
    #[serde(default)]
    delay_ms: u64,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Healthy,
    FailBeforeDispatch,
    LoseLastReceipt,
    LoseFirstReceipt,
}
fn read(root: &Path) -> Result<State> {
    match std::fs::read(root.join("transport.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error.into()),
    }
}
fn write(root: &Path, state: &State) -> Result<()> {
    let temp = root.join("transport.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(temp, root.join("transport.json"))?;
    Ok(())
}
pub(super) struct FixtureTransport {
    root: PathBuf,
    context: ReviewContext,
}
impl FixtureTransport {
    pub(super) fn for_context(context: &ReviewContext) -> Result<Option<Self>> {
        if context.identity.repository_node_id != REPO
            && context.identity.pull_request_node_id != PR
            && context.identity.viewer_node_id != VIEWER
        {
            return Ok(None);
        }
        ensure!(
            context.identity.repository_node_id == REPO
                && context.identity.pull_request_node_id == PR
                && context.identity.viewer_node_id == VIEWER
                && context.reference.repository.full_name() == "example/offline-viewed"
                && context.reference.number == 1,
            "Offline fixture identity is incomplete; refusing network fallback"
        );
        Ok(Some(Self {
            root: root()?,
            context: context.clone(),
        }))
    }
    fn guarded(&self) -> Value {
        json!({"viewer":{"id":VIEWER},"node":{"id":PR,"number":1,"headRefOid":self.context.head_oid,"baseRef":{"target":{"oid":self.context.base_tip_oid}},"repository":{"id":REPO,"nameWithOwner":"example/offline-viewed"},"author":{"nodeId":"offline_author"}}})
    }
}
impl ReviewTransport for FixtureTransport {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let _lock = IO
            .lock()
            .map_err(|_| anyhow::anyhow!("Fixture state lock failed"))?;
        let mut state = read(&self.root)?;
        if query.starts_with("mutation") {
            ensure!(
                query.contains("FileAsViewed"),
                "Unsupported offline mutation"
            );
            let mut submitted = Vec::new();
            let mut response = serde_json::Map::new();
            for (key, input) in variables
                .as_object()
                .context("Expected fixture variables")?
            {
                ensure!(
                    key.starts_with("file") && input["pullRequestId"] == PR,
                    "Unexpected offline mutation identity"
                );
                let path = input["path"]
                    .as_str()
                    .context("Missing fixture path")?
                    .to_owned();
                ensure!(paths().contains(&path), "Path is outside offline fixture");
                if query.contains("UnmarkFileAsViewedInput") {
                    state.viewed.remove(&path);
                } else {
                    state.viewed.insert(path.clone());
                }
                submitted.push(path);
                response.insert(key.clone(), json!({"pullRequest":{"id":PR}}));
            }
            let lose = matches!(state.mode, Mode::LoseFirstReceipt)
                || matches!(state.mode, Mode::LoseLastReceipt)
                    && submitted.iter().any(|path| path.ends_with("item-20.md"));
            if state.delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(state.delay_ms.min(5000)));
            }
            state.mutation_paths.push(submitted);
            if lose {
                state.mode = Mode::Healthy;
            }
            write(&self.root, &state)?;
            ensure!(
                !lose,
                "Offline fixture: response lost after server accepted this chunk"
            );
            return Ok(Value::Object(response));
        }
        ensure!(
            variables["pr"] == PR && query.contains("viewer"),
            "Unsupported offline query"
        );
        if matches!(state.mode, Mode::FailBeforeDispatch) {
            anyhow::bail!("Offline fixture: verified failure before dispatch");
        }
        state.reads += 1;
        write(&self.root, &state)?;
        let mut response = self.guarded();
        if query.contains("files(first:") {
            response["node"]["files"] = json!({"totalCount":21,"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":paths().into_iter().map(|path| { let viewed = state.viewed.contains(&path); json!({"path":path,"viewerViewedState":if viewed {"VIEWED"} else {"UNVIEWED"}}) }).collect::<Vec<_>>()});
        }
        Ok(response)
    }
    fn rest_json(&self, _: &str, _: &str, _: Option<Value>) -> Result<Option<Value>> {
        anyhow::bail!("REST is forbidden for the offline Viewed fixture")
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "This explicitly enabled offline fixture performs read-only Git manifest queries only from its background manifest worker, never a UI callback"
)]
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    ensure!(output.status.success(), "Offline fixture Git read failed");
    Ok(output.stdout)
}
fn manifest(root: &Path) -> Result<(PullRequestMetadata, ReviewSnapshot)> {
    let oid = |revision| -> Result<ObjectId> {
        Ok(ObjectId {
            format: ObjectFormat::Sha1,
            hex: String::from_utf8(git(root, &["rev-parse", revision])?)?
                .trim()
                .into(),
        })
    };
    let base = oid("HEAD~1")?;
    let head = oid("HEAD")?;
    base.validate()?;
    head.validate()?;
    let files = paths()
        .into_iter()
        .map(|path| {
            let old = git(root, &["show", &format!("HEAD~1:{path}")])?;
            let new = git(root, &["show", &format!("HEAD:{path}")])?;
            Ok(FileComparison {
                kind: ComparisonKind::Full,
                old: Some(FileSide {
                    path: path.clone(),
                    object: ObjectId::for_blob(ObjectFormat::Sha1, &old),
                    mode: 0o100644,
                }),
                new: Some(FileSide {
                    path,
                    object: ObjectId::for_blob(ObjectFormat::Sha1, &new),
                    mode: 0o100644,
                }),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((
        PullRequestMetadata {
            owner: "example".into(),
            repository: "offline-viewed".into(),
            number: 1,
            title: "Offline Viewed acceptance fixture".into(),
            base_ref_name: "main".into(),
            head_ref_name: "fixture".into(),
            base_ref_oid: base.hex.clone(),
            head_ref_oid: head.hex.clone(),
            commit_count: Some(1),
        },
        ReviewSnapshot {
            generation: 1,
            groups: vec![ComparisonGroup {
                endpoints: ResolvedEndpoints {
                    group: GROUP.into(),
                    left: base,
                    right: head,
                },
                files,
            }],
            incomplete_reason: None,
        },
    ))
}

pub(in crate::review_view) fn open(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
        return;
    };
    let project = workspace.project().clone();
    let active_root = repository.read(cx).work_directory_abs_path.to_path_buf();
    let weak = workspace.weak_handle();
    let db = ReviewDb::global(cx);
    cx.spawn_in(window, async move |_, cx| {
        let result: Result<()> = async {
            let (root, metadata, snapshot) = cx
                .background_spawn(async move {
                    let root = root()?;
                    ensure!(
                        active_root.canonicalize()? == root,
                        "Open the marked disposable fixture checkout before using this command"
                    );
                    let (metadata, snapshot) = manifest(&root)?;
                    Ok::<_, anyhow::Error>((root, metadata, snapshot))
                })
                .await?;
            let binding = db
                .bind_repository(root.join(".git"), root.join(".git"), ObjectFormat::Sha1)
                .await?;
            let identity = PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: REPO.into(),
                pull_request_node_id: PR.into(),
                viewer_node_id: VIEWER.into(),
            };
            let selector = ComparisonSelector::PullRequest {
                identity: identity.clone(),
            };
            let previous = db.find_session(&binding.id, &selector)?;
            let session = db
                .deploy_pull_request(
                    binding.id,
                    identity,
                    metadata,
                    snapshot,
                    previous.map(|session| session.revision),
                )
                .await?;
            let data = read_data(&db, session)?;
            weak.update_in(cx, |workspace, window, cx| {
                let existing = workspace
                    .items_of_type::<ReviewView>(cx)
                    .find(|item| item.read(cx).data.session.id == data.session.id);
                if let Some(existing) = existing {
                    existing.update(cx, |view, cx| {
                        view.finish(Ok(data), window, cx);
                        verify_restored(view, window, cx);
                    });
                    workspace.activate_item(&existing, true, true, window, cx);
                } else {
                    let view = cx.new(|cx| {
                        ReviewView::new(
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
            Ok(())
        }
        .await;
        if let Err(error) = result {
            weak.update_in(cx, |_workspace, _, cx| {
                git_ui_core::notifications::show_error_toast(
                    cx.entity(),
                    "Offline fixture unavailable",
                    error,
                    cx,
                )
            })
            .log_err();
        }
    })
    .detach();
}

/// Cold restore uses the same local immutable-manifest authority as explicit
/// fixture opening. A marker alone never enables a cached snapshot's writes.
pub(in crate::review_view) fn verify_restored(
    view: &mut ReviewView,
    window: &mut Window,
    cx: &mut Context<ReviewView>,
) {
    if view.checking {
        return;
    }
    let session = view.data.session.clone();
    let metadata = view.data.pull_request.clone();
    let active_root = view
        .repository
        .read(cx)
        .work_directory_abs_path
        .to_path_buf();
    view.checking = true;
    view.check_task = Some(cx.spawn_in(window, async move |this, cx| {
        let source = session.clone();
        let verified = cx.background_spawn(async move {
            let root = root()?;
            ensure!(active_root.canonicalize()? == root && source.repository.worktree_directory.canonicalize()? == root.join(".git").canonicalize()? && source.repository.common_directory.canonicalize()? == root.join(".git").canonicalize()?, "Offline fixture repository binding changed");
            let (current_metadata, snapshot) = manifest(&root)?;
            ensure!(metadata.as_ref() == Some(&current_metadata) && source.snapshot.groups == snapshot.groups, "Offline fixture commits or manifest changed; reopen the explicit fixture command");
            let identity = source.selector.pull_request_identity().context("Offline fixture identity missing")?.clone();
            let context = ReviewContext { reference:PullRequestReference::parse(&current_metadata.url(), None)?, identity, head_oid:current_metadata.head_ref_oid, base_tip_oid:current_metadata.base_ref_oid };
            let transport = FixtureTransport::for_context(&context)?.context("Offline fixture transport missing")?;
            crate::github_review::ReviewService::new(&transport).verify_context(&context)?;
            Ok::<_, anyhow::Error>(())
        }).await;
        this.update(cx, |view, cx| {
            if view.data.session.id != session.id || view.data.session.snapshot.generation != session.snapshot.generation || view.data.session.selector != session.selector { return; }
            view.checking = false;
            match verified {
                Ok(()) => { view.cached_unverified = false; view.check_error = None; }
                Err(error) => { view.cached_unverified = true; view.check_error = Some(format!("Offline fixture verification failed: {error:#}")); }
            }
            cx.notify();
        }).log_err();
    }));
    cx.notify();
}

pub(in crate::review_view) fn toolbar(
    view: &ReviewView,
    cx: &mut Context<ReviewView>,
) -> gpui::AnyElement {
    let operations = view
        .data
        .session
        .selector
        .pull_request_identity()
        .and_then(|identity| view.db.operations(identity).ok())
        .unwrap_or_default();
    let failed = operations
        .iter()
        .rev()
        .find(|operation| {
            operation.kind == ReviewOperationKind::SetFileViewed
                && operation.state == ReviewOperationState::Failed
        })
        .cloned();
    let unknown = operations
        .iter()
        .rev()
        .find(|operation| {
            operation.kind == ReviewOperationKind::SetFileViewed
                && operation.state == ReviewOperationState::DeliveryUnknown
        })
        .cloned();
    let summary = format!(
        "Offline fixture · {} locally viewed · {} pending · {} failed · {} unknown",
        view.data.viewed.len(),
        view.github_viewed_intents.len(),
        operations
            .iter()
            .filter(|op| op.state == ReviewOperationState::Failed)
            .count(),
        operations
            .iter()
            .filter(|op| op.state == ReviewOperationState::DeliveryUnknown)
            .count()
    );
    h_flex()
        .px_3()
        .py_2()
        .gap_2()
        .flex_wrap()
        .child(Label::new(summary).size(LabelSize::Small))
        .child(
            Button::new("fixture-retry-failed", "Retry failed")
                .style(ButtonStyle::Outlined)
                .disabled(failed.is_none() || view.github_viewed_busy)
                .on_click(cx.listener(move |view, _, window, cx| {
                    if let Some(operation) = failed.clone() {
                        view.handle_github_viewed_operation(operation, false, window, cx);
                    }
                })),
        )
        .child(
            Button::new("fixture-reconcile", "Reconcile unknown")
                .style(ButtonStyle::Outlined)
                .disabled(unknown.is_none() || view.github_viewed_busy)
                .on_click(cx.listener(move |view, _, window, cx| {
                    if let Some(operation) = unknown.clone() {
                        view.handle_github_viewed_operation(operation, true, window, cx);
                    }
                })),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_pull_requests::RepositoryName;
    use crate::github_review::{ReviewService, ReviewWriteError, ViewedState};

    fn transport(root: &Path) -> FixtureTransport {
        FixtureTransport {
            root: root.to_owned(),
            context: ReviewContext {
                reference: PullRequestReference {
                    repository: RepositoryName::parse("example/offline-viewed").unwrap(),
                    number: 1,
                },
                identity: PullRequestIdentity {
                    host: "github.com".into(),
                    repository_node_id: REPO.into(),
                    pull_request_node_id: PR.into(),
                    viewer_node_id: VIEWER.into(),
                },
                head_oid: "a".repeat(40),
                base_tip_oid: "b".repeat(40),
            },
        }
    }
    #[test]
    fn offline_identity_never_falls_back_to_network_when_incomplete() {
        let directory = tempfile::tempdir().unwrap();
        let mut context = transport(directory.path()).context;
        context.identity.viewer_node_id = "another_viewer".into();
        assert!(FixtureTransport::for_context(&context).is_err());
        context.identity.repository_node_id = "ordinary_repository".into();
        context.identity.pull_request_node_id = "ordinary_pull_request".into();
        assert!(FixtureTransport::for_context(&context).unwrap().is_none());
    }

    #[test]
    fn offline_failure_before_dispatch_is_definite_and_retry_skips_confirmed_successes() {
        let directory = tempfile::tempdir().unwrap();
        let transport = transport(directory.path());
        let requested = paths();
        let mut state = State {
            mode: Mode::FailBeforeDispatch,
            viewed: requested[..20].iter().cloned().collect(),
            ..Default::default()
        };
        write(directory.path(), &state).unwrap();
        assert!(matches!(
            ReviewService::new(&transport).set_viewed_batch(
                &transport.context,
                "fixture-op",
                &requested,
                true
            ),
            Err(ReviewWriteError::NotSent(_))
        ));
        assert!(read(directory.path()).unwrap().mutation_paths.is_empty());
        state.mode = Mode::Healthy;
        write(directory.path(), &state).unwrap();
        let receipt = ReviewService::new(&transport)
            .set_viewed_batch(&transport.context, "fixture-op", &requested, true)
            .unwrap();
        assert_eq!(receipt.remote.len(), 21);
        assert_eq!(
            read(directory.path()).unwrap().mutation_paths,
            vec![vec!["docs/item-20.md".to_owned()]]
        );
    }
    #[test]
    fn offline_partial_delivery_remains_unknown_and_independent_read_survives_restart() {
        let directory = tempfile::tempdir().unwrap();
        let transport = transport(directory.path());
        write(
            directory.path(),
            &State {
                mode: Mode::LoseFirstReceipt,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            ReviewService::new(&transport).set_viewed_batch(
                &transport.context,
                "fixture-op",
                &paths(),
                true
            ),
            Err(ReviewWriteError::DeliveryUnknown { .. })
        ));
        let persisted = read(directory.path()).unwrap();
        assert_eq!(persisted.viewed.len(), 20);
        assert_eq!(persisted.mutation_paths.len(), 1);
        let reopened = self::transport(directory.path());
        let observed = ReviewService::new(&reopened)
            .load_viewed(&reopened.context)
            .unwrap();
        assert_eq!(
            observed
                .iter()
                .filter(|file| file.viewer_viewed_state == ViewedState::Viewed)
                .count(),
            20
        );
        assert_eq!(
            read(directory.path()).unwrap().mutation_paths.len(),
            1,
            "Reconciliation is read-only; it cannot replay an uncertain chunk"
        );
    }
    #[test]
    fn offline_lost_final_receipt_is_fully_observable_without_replaying_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let transport = transport(directory.path());
        write(
            directory.path(),
            &State {
                mode: Mode::LoseLastReceipt,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            ReviewService::new(&transport).set_viewed_batch(
                &transport.context,
                "fixture-op",
                &paths(),
                true
            ),
            Err(ReviewWriteError::DeliveryUnknown { .. })
        ));
        assert_eq!(read(directory.path()).unwrap().mutation_paths.len(), 2);
        let reopened = self::transport(directory.path());
        let observed = ReviewService::new(&reopened)
            .load_viewed(&reopened.context)
            .unwrap();
        let reconciliation =
            crate::review_github_viewed::reconcile_folder_viewed(&paths(), true, Some(&observed))
                .unwrap();
        assert_eq!(reconciliation.confirmed.len(), 21);
        assert!(reconciliation.not_applied.is_empty());
        assert_eq!(read(directory.path()).unwrap().mutation_paths.len(), 2);
    }
}
