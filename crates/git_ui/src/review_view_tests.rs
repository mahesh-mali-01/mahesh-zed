use super::*;
use db::sqlez::thread_safe_connection::{ThreadSafeConnection, locking_queue};
use gpui::TestAppContext;
use project::FakeFs;
use serde_json::json;
use settings::SettingsStore;
use std::{fs, path::PathBuf};
use workspace::MultiWorkspace;

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    fs: Arc<FakeFs>,
    project: Entity<Project>,
    repository: Entity<Repository>,
    db: ReviewDb,
    session: ReviewSession,
}

fn comparison(path: &str, old: &[u8], new: &[u8]) -> FileComparison {
    FileComparison {
        kind: ComparisonKind::Full,
        old: Some(FileSide {
            path: path.into(),
            object: ObjectId::for_blob(ObjectFormat::Sha1, old),
            mode: 0o100644,
        }),
        new: Some(FileSide {
            path: path.into(),
            object: ObjectId::for_blob(ObjectFormat::Sha1, new),
            mode: 0o100644,
        }),
    }
}

async fn fixture(cx: &mut TestAppContext) -> Result<Fixture> {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init(cx);
        for asset in [
            include_str!("../../../assets/keymaps/default-macos.json"),
            include_str!("../../../assets/keymaps/macos/jetbrains.json"),
            include_str!("../../../assets/keymaps/specific-overrides-macos.json"),
        ] {
            let bindings = match settings::KeymapFile::load(asset, cx) {
                settings::KeymapFileLoadResult::Success { key_bindings }
                | settings::KeymapFileLoadResult::SomeFailedToLoad { key_bindings, .. } => {
                    key_bindings
                }
                settings::KeymapFileLoadResult::JsonParseFailure { error } => panic!("{error}"),
            };
            cx.bind_keys(bindings);
        }
    });
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("project");
    fs::create_dir_all(root.join(".git"))?;
    let connection = ThreadSafeConnection::builder::<db::AppMigrator>(
        directory
            .path()
            .join("app.sqlite")
            .to_str()
            .context("database path")?,
        true,
    )
    .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
    .with_write_queue_constructor(locking_queue())
    .build()
    .await?;
    cx.update(|cx| cx.set_global(db::AppDatabase(connection)));
    let fake_fs = FakeFs::new(cx.executor());
    fake_fs
        .insert_tree(
            &root,
            json!({".git": {}, "auth.ts": "WORKTREE auth\n", "billing.ts": "WORKTREE billing\n"}),
        )
        .await;
    let files = vec![
        comparison("auth.ts", b"base auth\n", b"reviewed auth\n"),
        comparison("billing.ts", b"10\n", b"12\n"),
    ];
    fake_fs
        .with_git_state(&root.join(".git"), true, |state| {
            for (file, old, new) in [
                (
                    &files[0],
                    b"base auth\n".as_slice(),
                    b"reviewed auth\n".as_slice(),
                ),
                (&files[1], b"10\n".as_slice(), b"12\n".as_slice()),
            ] {
                state.oids.insert(
                    file.old.as_ref().unwrap().object.hex.parse().unwrap(),
                    old.to_vec(),
                );
                state.oids.insert(
                    file.new.as_ref().unwrap().object.hex.parse().unwrap(),
                    new.to_vec(),
                );
            }
        })
        .context("fake Git state")?;
    let project = Project::test(fake_fs.clone(), [&root as &std::path::Path], cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    let repository = project
        .read_with(cx, |project, cx| project.active_repository(cx))
        .context("repository")?;
    let db = cx.update(|cx| ReviewDb::global(cx));
    let binding = db
        .bind_repository(root.join(".git"), root.join(".git"), ObjectFormat::Sha1)
        .await?;
    let snapshot = ReviewSnapshot {
        generation: 1,
        groups: vec![ComparisonGroup {
            endpoints: ResolvedEndpoints {
                group: GROUP.into(),
                left: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "1".repeat(40),
                },
                right: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "2".repeat(40),
                },
            },
            files,
        }],
        incomplete_reason: None,
    };
    let session = db
        .create_session(
            binding.id,
            ComparisonSelector::Branch {
                base_ref: "main".into(),
                head_ref: "feature".into(),
            },
            snapshot,
        )
        .await?;
    fake_fs
        .with_git_state(&root.join(".git"), false, |state| {
            state.review_comparisons.insert(
                ("main".into(), "feature".into(), true),
                git_comparison(&session.snapshot),
            );
        })
        .unwrap();
    Ok(Fixture {
        _directory: directory,
        root,
        fs: fake_fs,
        project,
        repository,
        db,
        session,
    })
}

#[gpui::test]
async fn review_controls_are_read_only_persistent_and_restore(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.unwrap();
    let (multi, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(fixture.project.clone(), window, cx)
    });
    let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
    let weak = workspace.downgrade();
    let view = cx.update(|window, cx| {
        cx.new(|cx| {
            ReviewView::new(
                fixture.project.clone(),
                fixture.repository.clone(),
                weak.clone(),
                fixture.db.clone(),
                read_data(&fixture.db, fixture.session.clone()).unwrap(),
                window,
                cx,
            )
        })
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
    });
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && (view.editor.is_some() || view.error.is_some())
    })
    .await;
    view.read_with(cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        let editor = view.editor.as_ref().unwrap().read(cx);
        assert!(editor.rhs_editor().read(cx).read_only(cx));
        assert!(
            editor
                .rhs_editor()
                .read(cx)
                .text(cx)
                .contains("reviewed auth")
        );
        assert!(!editor.rhs_editor().read(cx).text(cx).contains("WORKTREE"));
    });
    cx.focus(&view);
    let before = view.read_with(cx, |view, cx| {
        view.editor
            .as_ref()
            .unwrap()
            .read(cx)
            .rhs_editor()
            .read(cx)
            .text(cx)
    });
    cx.simulate_keystrokes("alt-down");
    cx.condition(&view, |view, _| {
        !view.busy
            && !view.navigation_pending
            && view.data.session.presentation.selected_path.as_deref() == Some("billing.ts")
    })
    .await;
    cx.simulate_keystrokes("alt-up");
    cx.condition(&view, |view, _| {
        !view.busy
            && !view.navigation_pending
            && view.data.session.presentation.selected_path.as_deref() == Some("auth.ts")
    })
    .await;
    cx.simulate_input("should not edit a snapshot");
    view.read_with(cx, |view, cx| {
        assert_eq!(
            before,
            view.editor
                .as_ref()
                .unwrap()
                .read(cx)
                .rhs_editor()
                .read(cx)
                .text(cx)
        )
    });
    cx.simulate_keystrokes("alt-v");
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.data.viewed.contains("auth.ts")
    })
    .await;
    cx.simulate_keystrokes("alt-u");
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.data.session.presentation.unreviewed_only
    })
    .await;
    view.read_with(cx, |view, _| {
        assert!(view.error.is_none());
        assert_eq!(paths(&view.data), vec!["billing.ts"]);
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("billing.ts")
        );
    });
    cx.update(|window, cx| assert!(view.read(cx).focus.contains_focused(window, cx)));
    cx.simulate_keystrokes("alt-v");
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.data.viewed.len() == 2
    })
    .await;
    view.read_with(cx, |view, _| {
        assert!(paths(&view.data).is_empty());
        assert!(view.data.session.presentation.selected_path.is_none());
    });
    cx.update(|window, cx| assert!(view.read(cx).focus.contains_focused(window, cx)));
    cx.simulate_keystrokes("alt-u");
    cx.condition(&view, |view, _| {
        !view.busy
            && !view.navigation_pending
            && !view.data.session.presentation.unreviewed_only
            && view.editor.is_some()
    })
    .await;
    view.update_in(cx, |view, window, cx| view.toggle_split(window, cx));
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.editor.is_some()
    })
    .await;
    view.read_with(cx, |view, cx| {
        let editor = view.editor.as_ref().unwrap().read(cx);
        for editor in std::iter::once(editor.rhs_editor()).chain(editor.lhs_editor()) {
            assert!(editor.read(cx).read_only(cx));
        }
    });
    let chosen_style = view.read_with(cx, |view, cx| {
        view.editor.as_ref().unwrap().read(cx).diff_view_style()
    });
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.editor.diff_view_style = Some(if chosen_style == DiffViewStyle::Split {
                    DiffViewStyle::Unified
                } else {
                    DiffViewStyle::Split
                });
            });
        });
    });
    cx.run_until_parked();
    view.read_with(cx, |view, cx| {
        assert_eq!(
            view.editor.as_ref().unwrap().read(cx).diff_view_style(),
            chosen_style
        );
    });
    let workspace_id = workspace::WorkspaceId::from_i64(101);
    fixture
        .db
        .save_item(workspace_id, 501, fixture.session.id.clone())
        .await
        .unwrap();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.oids.clear()
        })
        .unwrap();
    let restored = cx
        .update(|window, cx| {
            ReviewView::deserialize(fixture.project.clone(), weak, workspace_id, 501, window, cx)
        })
        .await
        .unwrap();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(restored.clone()), None, true, window, cx)
    });
    cx.condition(&restored, |view, _| {
        !view.busy && !view.navigation_pending && (view.editor.is_some() || view.error.is_some())
    })
    .await;
    restored.read_with(cx, |view, _| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert_eq!(view.data.viewed.len(), 2);
    });
}

#[gpui::test]
async fn review_missing_object_is_visible_and_cannot_be_approved(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.unwrap();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.oids.clear()
        })
        .unwrap();
    let (multi, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(fixture.project.clone(), window, cx)
    });
    let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
    let view = workspace.update_in(cx, |workspace, window, cx| {
        let view = cx.new(|cx| {
            ReviewView::new(
                fixture.project.clone(),
                fixture.repository.clone(),
                workspace.weak_handle(),
                fixture.db.clone(),
                read_data(&fixture.db, fixture.session.clone()).unwrap(),
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    });
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.error.is_some()
    })
    .await;
    cx.focus(&view);
    cx.simulate_keystrokes("alt-v");
    view.read_with(cx, |view, _| {
        assert!(view.retained.is_none());
        assert!(view.data.viewed.is_empty());
        assert!(view.error.as_ref().unwrap().contains("snapshot"));
    });
    assert!(
        fixture
            .db
            .checkpoint(&fixture.session.id, GROUP, "auth.ts")
            .unwrap()
            .is_none()
    );
}

#[gpui::test]
async fn review_branch_picker_advances_without_checkout(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.unwrap();
    fixture.repository.update(cx, |repository, cx| {
        repository.set_branch_list_for_test(
            vec![git::repository::Branch {
                is_head: false,
                ref_name: "refs/heads/main".into(),
                upstream: None,
                most_recent_commit: None,
            }],
            cx,
        );
    });
    let before = fixture
        .repository
        .read_with(cx, |repository, _| repository.branch.clone());
    let (multi, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(fixture.project.clone(), window, cx)
    });
    let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        ReviewView::pick(workspace, false, window, cx)
    });
    cx.run_until_parked();
    let base = workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_modal::<branch_picker::BranchList>(cx)
            .unwrap()
    });
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    let head = workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_modal::<branch_picker::BranchList>(cx)
            .expect("head picker must remain open")
    });
    assert_ne!(base.entity_id(), head.entity_id());
    cx.update(|window, cx| {
        assert!(head.focus_handle(cx).is_focused(window));
    });
    fixture
        .repository
        .read_with(cx, |repository, _| assert_eq!(repository.branch, before));
}

fn git_comparison(snapshot: &ReviewSnapshot) -> git::review::ReviewComparison {
    let group = snapshot.groups.first().unwrap();
    git::review::ReviewComparison {
        left: group.endpoints.left.hex.parse().unwrap(),
        right: group.endpoints.right.hex.parse().unwrap(),
        files: group
            .files
            .iter()
            .map(|file| git::review::ReviewFile {
                path: RepoPath::from_rel_path(
                    RelPath::from_unix_str(file.file_key().unwrap()).unwrap(),
                ),
                old: file.old.as_ref().map(|side| git::review::ReviewSide {
                    object: side.object.hex.parse().unwrap(),
                    mode: side.mode,
                }),
                new: file.new.as_ref().map(|side| git::review::ReviewSide {
                    object: side.object.hex.parse().unwrap(),
                    mode: side.mode,
                }),
            })
            .collect(),
    }
}

fn seed_review(fixture: &Fixture, snapshot: &ReviewSnapshot, new_auth: &[u8]) {
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.review_comparisons.insert(
                ("main".into(), "feature".into(), true),
                git_comparison(snapshot),
            );
            state.oids.insert(
                ObjectId::for_blob(ObjectFormat::Sha1, new_auth)
                    .hex
                    .parse()
                    .unwrap(),
                new_auth.to_vec(),
            );
            state.oids.insert(
                ObjectId::for_blob(ObjectFormat::Sha1, b"new file\n")
                    .hex
                    .parse()
                    .unwrap(),
                b"new file\n".to_vec(),
            );
        })
        .unwrap();
}

fn review_window(
    fixture: &Fixture,
    cx: &mut TestAppContext,
    app_state: Option<Arc<workspace::AppState>>,
) -> (Entity<ReviewView>, gpui::VisualTestContext) {
    let window = cx.add_window(|window, cx| {
        if let Some(app_state) = app_state {
            let workspace =
                cx.new(|cx| Workspace::new(None, fixture.project.clone(), app_state, window, cx));
            MultiWorkspace::new(workspace, window, cx)
        } else {
            MultiWorkspace::test_new(fixture.project.clone(), window, cx)
        }
    });
    let multi = window.root(cx).unwrap();
    let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
    let mut window_cx = gpui::VisualTestContext::from_window(window.into(), cx);
    let view = workspace.update_in(&mut window_cx, |workspace, window, cx| {
        let view = cx.new(|cx| {
            ReviewView::new(
                fixture.project.clone(),
                fixture.repository.clone(),
                workspace.weak_handle(),
                fixture.db.clone(),
                read_data(
                    &fixture.db,
                    fixture.db.load_session(&fixture.session.id).unwrap(),
                )
                .unwrap(),
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    });
    (view, window_cx)
}

fn advanced_snapshot(fixture: &Fixture) -> ReviewSnapshot {
    let mut snapshot = fixture.session.snapshot.clone();
    snapshot.groups[0].endpoints.right.hex = "3".repeat(40);
    snapshot.groups[0].files[0] = comparison("auth.ts", b"base auth\n", b"new auth\n");
    snapshot
}

#[gpui::test]
async fn review_refresh_preserves_unchanged_progress_and_exposes_changed_files(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.unwrap();
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && !view.checking
    })
    .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.data.viewed.len() == 1
    })
    .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.select("billing.ts".into(), window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.data.viewed.len() == 2
    })
    .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.toggle_filter(&ToggleUnreviewedFiles, window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let mut snapshot = advanced_snapshot(&fixture);
    snapshot.groups[0].files.push(FileComparison {
        kind: ComparisonKind::Full,
        old: None,
        new: Some(FileSide {
            path: "new.ts".into(),
            object: ObjectId::for_blob(ObjectFormat::Sha1, b"new file\n"),
            mode: 0o100644,
        }),
    });
    seed_review(&fixture, &snapshot, b"new auth\n");
    fixture
        .repository
        .update(&mut cx, |_, cx| cx.emit(RepositoryEvent::BranchListChanged));
    cx.condition(&view, |view, _| !view.checking && view.update_available)
        .await;
    view.read_with(&cx, |view, _| {
        assert_eq!(view.data.session.snapshot.generation, 1);
        assert_eq!(view.data.viewed.len(), 2);
        assert!(paths(&view.data).is_empty());
    });
    cx.focus(&view);
    cx.dispatch_action(RefreshReview);
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.data.session.snapshot.generation == 2
    })
    .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.select("auth.ts".into(), window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, cx| {
        assert_eq!(view.data.viewed, BTreeSet::from(["billing.ts".into()]));
        assert_eq!(view.data.changed, BTreeSet::from(["auth.ts".into()]));
        assert_eq!(paths(&view.data), vec!["auth.ts", "new.ts"]);
        assert!(
            view.editor
                .as_ref()
                .unwrap()
                .read(cx)
                .rhs_editor()
                .read(cx)
                .text(cx)
                .contains("new auth")
        );
        assert!(
            !view
                .editor
                .as_ref()
                .unwrap()
                .read(cx)
                .rhs_editor()
                .read(cx)
                .text(cx)
                .contains("WORKTREE")
        );
    });
    snapshot.groups[0].endpoints.right.hex = "4".repeat(40);
    seed_review(&fixture, &snapshot, b"new auth\n");
    view.update_in(&mut cx, |view, window, cx| view.refresh(window, cx));
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.data.session.snapshot.generation == 3
    })
    .await;
    view.read_with(&cx, |view, _| {
        assert!(view.data.viewed.contains("billing.ts"))
    });
    view.update_in(&mut cx, |view, window, cx| view.refresh(window, cx));
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    assert_eq!(
        view.read_with(&cx, |view, _| view.data.session.snapshot.generation),
        3
    );
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.review_comparisons.clear()
        })
        .unwrap();
    view.update_in(&mut cx, |view, window, cx| view.refresh(window, cx));
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.error.is_some()
    })
    .await;
    assert_eq!(
        fixture
            .db
            .load_session(&fixture.session.id)
            .unwrap()
            .snapshot
            .generation,
        3
    );
    assert!(
        fixture
            .db
            .checkpoint(&fixture.session.id, GROUP, "auth.ts")
            .unwrap()
            .is_some()
    );
}

#[gpui::test]
async fn review_late_update_check_cannot_override_newer_result(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.unwrap();
    let gate = ::fs::FakeBlobReadGate::default();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.review_read_gate = Some(gate.clone())
        })
        .unwrap();
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.run_until_parked();
    assert_eq!(gate.waiting(), 1);
    let old_task = view.update(&mut cx, |view, _| view.check_task.take());
    let snapshot = advanced_snapshot(&fixture);
    seed_review(&fixture, &snapshot, b"new auth\n");
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.review_read_gate = None
        })
        .unwrap();
    fixture
        .repository
        .update(&mut cx, |_, cx| cx.emit(RepositoryEvent::HeadChanged));
    cx.condition(&view, |view, _| !view.checking && view.update_available)
        .await;
    gate.open();
    old_task.unwrap().await;
    view.read_with(&cx, |view, _| {
        assert!(view.update_available);
        assert_eq!(view.data.session.snapshot.generation, 1);
    });
}

#[gpui::test]
async fn review_two_windows_viewed_wins_then_refresh_requires_reload(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.unwrap();
    let (first, mut first_cx) = review_window(&fixture, cx, None);
    let app_state = first.read_with(cx, |view, cx| {
        view.workspace
            .upgrade()
            .unwrap()
            .read(cx)
            .app_state()
            .clone()
    });
    let (second, mut second_cx) = review_window(&fixture, cx, Some(app_state));
    cx.condition(&first, |view, _| !view.busy && !view.navigation_pending)
        .await;
    cx.condition(&second, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let snapshot = advanced_snapshot(&fixture);
    seed_review(&fixture, &snapshot, b"new auth\n");
    let gate = ::fs::FakeBlobReadGate::default();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.review_read_gate = Some(gate.clone())
        })
        .unwrap();
    first.update_in(&mut first_cx, |view, window, cx| view.refresh(window, cx));
    cx.run_until_parked();
    assert!(gate.waiting() > 0);
    second.update_in(&mut second_cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    cx.condition(&second, |view, _| {
        !view.busy && !view.navigation_pending && view.data.viewed.contains("auth.ts")
    })
    .await;
    gate.open();
    cx.condition(&first, |view, _| {
        !view.busy && !view.navigation_pending && view.error.is_some()
    })
    .await;
    let saved = fixture.db.load_session(&fixture.session.id).unwrap();
    assert_eq!(saved.snapshot.generation, 1);
    assert!(
        fixture
            .db
            .checkpoint(&saved.id, GROUP, "auth.ts")
            .unwrap()
            .is_some()
    );
    first.update_in(&mut first_cx, |view, window, cx| view.retry(window, cx));
    cx.condition(&first, |view, _| {
        !view.busy && !view.navigation_pending && !view.outdated()
    })
    .await;
    first.update_in(&mut first_cx, |view, window, cx| view.refresh(window, cx));
    cx.condition(&first, |view, _| {
        !view.busy && !view.navigation_pending && view.data.session.snapshot.generation == 2
    })
    .await;
    cx.condition(&second, |view, _| view.outdated()).await;
    let revision = fixture.db.load_session(&saved.id).unwrap().revision;
    second.update_in(&mut second_cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        fixture.db.load_session(&saved.id).unwrap().revision,
        revision
    );
    assert_eq!(
        second.read_with(cx, |view, _| view.data.session.snapshot.generation),
        1
    );
    second.update_in(&mut second_cx, |view, window, cx| view.retry(window, cx));
    cx.condition(&second, |view, _| {
        !view.busy && !view.navigation_pending && !view.outdated()
    })
    .await;
    second.read_with(cx, |view, _| {
        assert_eq!(view.data.session.snapshot.generation, 2);
        assert!(!view.data.viewed.contains("auth.ts"));
    });
}

#[gpui::test]
async fn review_commit_groups_isolate_same_path_approvals_and_restore(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await.unwrap();
    let mut first = fixture.session.snapshot.groups[0].clone();
    first.endpoints.group = "first-commit".into();
    first.files.truncate(1);
    let mut second = first.clone();
    second.endpoints.group = "second-commit".into();
    second.endpoints.left = first.endpoints.right.clone();
    second.endpoints.right.hex = "3".repeat(40);
    second.files = vec![comparison("auth.ts", b"10\n", b"12\n")];
    let selector = ComparisonSelector::Commits {
        selections: vec![
            CommitSelection {
                commit: first.endpoints.right.clone(),
                parent: Some(first.endpoints.left.clone()),
            },
            CommitSelection {
                commit: second.endpoints.right.clone(),
                parent: Some(second.endpoints.left.clone()),
            },
        ],
    };
    fixture.session = fixture
        .db
        .create_session(
            fixture.session.repository.id.clone(),
            selector,
            ReviewSnapshot {
                generation: 1,
                groups: vec![first, second],
                incomplete_reason: None,
            },
        )
        .await
        .unwrap();
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, _| {
        assert_eq!(active_group(&view.data), "first-commit");
        assert_eq!(view.data.viewed.len(), 1);
        assert_eq!(view.data.files.len(), 1);
        assert_eq!(view.data.group_progress, vec![(1, 1), (0, 1)]);
        assert!(!view.checking);
        assert!(view.check_error.is_none());
    });
    view.update_in(&mut cx, |view, window, cx| {
        let mut presentation = view.data.session.presentation.clone();
        presentation.selected_group = Some("second-commit".into());
        presentation.selected_path = None;
        view.change_presentation(presentation, window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, cx| {
        assert!(view.data.viewed.is_empty());
        assert!(
            view.editor
                .as_ref()
                .unwrap()
                .read(cx)
                .rhs_editor()
                .read(cx)
                .text(cx)
                .contains("12")
        );
    });
    cx.focus(&view);
    cx.simulate_keystrokes("alt-v");
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let stored = fixture.db.load_session(&fixture.session.id).unwrap();
    let revision = stored.revision;
    assert!(
        fixture
            .db
            .checkpoint(&stored.id, "first-commit", "auth.ts")
            .unwrap()
            .is_some()
    );
    assert!(
        fixture
            .db
            .checkpoint(&stored.id, "second-commit", "auth.ts")
            .unwrap()
            .is_some()
    );
    view.update_in(&mut cx, |view, window, cx| view.refresh(window, cx));
    cx.run_until_parked();
    assert_eq!(
        fixture.db.load_session(&stored.id).unwrap().revision,
        revision
    );
    fixture
        .db
        .save_item(
            workspace::WorkspaceId::from_i64(606),
            606,
            stored.id.clone(),
        )
        .await
        .unwrap();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.oids.clear()
        })
        .unwrap();
    let weak = view.read_with(&cx, |view, _| view.workspace.clone());
    let restored = cx
        .update(|window, cx| {
            ReviewView::deserialize(
                fixture.project.clone(),
                weak,
                workspace::WorkspaceId::from_i64(606),
                606,
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.condition(&restored, |view, _| !view.busy && !view.navigation_pending)
        .await;
    restored.read_with(&cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert_eq!(active_group(&view.data), "second-commit");
        assert_eq!(view.data.viewed.len(), 1);
        assert!(
            view.editor
                .as_ref()
                .unwrap()
                .read(cx)
                .rhs_editor()
                .read(cx)
                .text(cx)
                .contains("12")
        );
    });
}

fn pr_test_snapshot(files: Vec<FileComparison>) -> ReviewSnapshot {
    ReviewSnapshot {
        generation: 1,
        incomplete_reason: None,
        groups: vec![ComparisonGroup {
            endpoints: ResolvedEndpoints {
                group: GROUP.into(),
                left: ObjectId::for_blob(ObjectFormat::Sha1, b"base"),
                right: ObjectId::for_blob(ObjectFormat::Sha1, b"head"),
            },
            files,
        }],
    }
}

#[test]
fn pr_rename_reconciliation_preserves_both_historical_paths() -> Result<()> {
    let mut removed = comparison("old/auth.ts", b"old\n", b"");
    removed.new = None;
    let mut added = comparison("new/auth.ts", b"", b"new\n");
    added.old = None;
    let mut snapshot = pr_test_snapshot(vec![removed, added]);
    reconcile_pr_files(
        &mut snapshot,
        &[PullRequestFile {
            patch: None,
            filename: "new/auth.ts".into(),
            previous_filename: Some("old/auth.ts".into()),
            status: "renamed".into(),
        }],
    )?;
    let files = &snapshot.groups[0].files;
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].old.as_ref().map(|side| side.path.as_str()),
        Some("old/auth.ts")
    );
    assert_eq!(
        files[0].new.as_ref().map(|side| side.path.as_str()),
        Some("new/auth.ts")
    );
    assert_eq!(files[0].file_key()?, "new/auth.ts");
    Ok(())
}

#[test]
fn pr_reconciliation_refuses_truncated_or_unexpected_file_metadata() {
    let mut snapshot = pr_test_snapshot(vec![
        comparison("a.ts", b"old", b"new"),
        comparison("b.ts", b"old", b"new"),
    ]);
    let original = snapshot.clone();
    let result = reconcile_pr_files(
        &mut snapshot,
        &[PullRequestFile {
            patch: None,
            filename: "a.ts".into(),
            previous_filename: None,
            status: "modified".into(),
        }],
    );
    assert!(result.is_err());
    assert_eq!(snapshot, original);
}

#[gpui::test]
async fn pr_tree_collapse_progress_and_path_filter(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let mut data = read_data(&fixture.db, fixture.session.clone()).expect("review data");
    data.files = vec![
        ".eslintrc.json".into(),
        "apps/api/auth.ts".into(),
        "apps/api/billing.ts".into(),
        "docs/review.md".into(),
    ];
    data.viewed.insert("apps/api/auth.ts".into());
    data.session.presentation.selected_path = Some("apps/api/auth.ts".into());
    assert_eq!(
        tree_navigation_target(&data, true).as_deref(),
        Some("apps/api/billing.ts")
    );
    assert_eq!(tree_navigation_target(&data, false), None);
    data.session.presentation.selected_path = Some(".eslintrc.json".into());
    assert_eq!(
        tree_navigation_target(&data, false).as_deref(),
        Some("docs/review.md")
    );
    assert_eq!(tree_navigation_target(&data, true), None);
    data.session.presentation.selected_path = Some("apps/api/auth.ts".into());
    let rows = review_tree(&data);
    let apps = rows
        .iter()
        .find(|row| row.path == "apps")
        .expect("apps directory");
    assert_eq!((apps.total, apps.viewed), (2, 1));
    data.session
        .presentation
        .collapsed_directories
        .insert("apps".into());
    let rows = review_tree(&data);
    assert!(rows.iter().any(|row| row.path == "apps"));
    assert!(!rows.iter().any(|row| row.path == "apps/api"));
    choose_visible(&mut data);
    assert_eq!(
        data.session.presentation.selected_path.as_deref(),
        Some("apps/api/auth.ts")
    );
    assert_eq!(
        tree_navigation_target(&data, true).as_deref(),
        Some("docs/review.md")
    );
    assert_eq!(tree_navigation_target(&data, false), None);
    assert!(rows.iter().any(|row| row.path == "docs/review.md"));
    data.session.presentation.path_filter = "REVIEW".into();
    assert_eq!(paths(&data), ["docs/review.md"]);
}

#[gpui::test]
async fn pr_successful_unchanged_retry_clears_error_and_allows_review(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (multi, cx) = cx.add_window_view(|window, cx| {
        MultiWorkspace::test_new(fixture.project.clone(), window, cx)
    });
    let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
    let view = cx.update(|window, cx| {
        cx.new(|cx| {
            ReviewView::new(
                fixture.project.clone(),
                fixture.repository.clone(),
                workspace.downgrade(),
                fixture.db.clone(),
                read_data(&fixture.db, fixture.session.clone()).expect("review data"),
                window,
                cx,
            )
        })
    });
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && (view.editor.is_some() || view.error.is_some())
    })
    .await;
    let editor_id = view.read_with(cx, |view, _| {
        view.editor.as_ref().expect("loaded editor").entity_id()
    });
    view.update_in(cx, |view, window, cx| {
        view.error = Some("PR refresh failed: temporary network outage".into());
        view.finish(
            Ok(read_data(&fixture.db, fixture.session.clone()).expect("unchanged session")),
            window,
            cx,
        );
        assert!(view.error.is_none());
        assert_eq!(
            view.editor.as_ref().expect("preserved editor").entity_id(),
            editor_id
        );
        view.toggle_viewed(&ToggleReviewViewed, window, cx);
        assert!(view.busy, "successful retry must restore the Viewed action");
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(cx, |view, _| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(view.data.viewed.contains("auth.ts"));
    });
}

#[gpui::test]
async fn pr_tree_width_persists_with_legacy_default_and_bounded_drag(cx: &mut TestAppContext) {
    assert_eq!(bounded_tree_width(40.0, 1200.0), 180);
    assert_eq!(bounded_tree_width(900.0, 1200.0), 520);
    assert_eq!(bounded_tree_width(500.0, 600.0), 360);
    assert_eq!(bounded_tree_width(500.0, 700.0), 420);
    let legacy: ReviewPresentation =
        serde_json::from_str(r#"{"unreviewed_only":false}"#).expect("legacy presentation");
    assert_eq!(legacy.tree_width, None);
    let fixture = fixture(cx).await.expect("fixture");
    let mut presentation = fixture.session.presentation.clone();
    presentation.tree_width = Some(456);
    fixture
        .db
        .save_presentation(
            fixture.session.id.clone(),
            fixture.session.revision,
            presentation,
        )
        .await
        .expect("persist tree width");
    let session = fixture
        .db
        .load_session(&fixture.session.id)
        .expect("reload session");
    assert_eq!(session.presentation.tree_width, Some(456));
}

#[test]
fn pr_selection_rejects_foreign_commits_and_invalid_merge_parents() {
    let root: git::Oid = "1".repeat(40).parse().expect("synthetic root");
    let merge: git::Oid = "2".repeat(40).parse().expect("synthetic merge");
    let other: git::Oid = "3".repeat(40).parse().expect("synthetic other");
    let history = vec![
        git::review::ReviewCommit {
            commit: root,
            parents: vec![],
            subject: "Root".into(),
        },
        git::review::ReviewCommit {
            commit: merge,
            parents: vec![root, other],
            subject: "Merge".into(),
        },
    ];
    assert!(
        validate_pr_selections(
            &history,
            &[git::review::ReviewCommitSelection {
                commit: merge,
                parent: Some(other)
            }]
        )
        .is_ok()
    );
    assert!(
        validate_pr_selections(
            &history,
            &[git::review::ReviewCommitSelection {
                commit: other,
                parent: None
            }]
        )
        .is_err()
    );
    assert!(
        validate_pr_selections(
            &history,
            &[git::review::ReviewCommitSelection {
                commit: merge,
                parent: None
            }]
        )
        .is_err()
    );
    assert!(
        validate_pr_selections(
            &history,
            &[git::review::ReviewCommitSelection {
                commit: root,
                parent: Some(other)
            }]
        )
        .is_err()
    );
}

#[gpui::test]
async fn pr_commit_scope_keeps_identity_and_approval_separate_from_full_pr(
    cx: &mut TestAppContext,
) {
    let mut fixture = fixture(cx).await.expect("fixture");
    let identity = PullRequestIdentity {
        host: "github.com".into(),
        repository_node_id: "synthetic-repository".into(),
        pull_request_node_id: "synthetic-pull-request".into(),
        viewer_node_id: "synthetic-viewer".into(),
    };
    let endpoints = fixture.session.snapshot.groups[0].endpoints.clone();
    let metadata = PullRequestMetadata {
        owner: "example".into(),
        repository: "review-fixture".into(),
        number: 42,
        title: "Synthetic PR".into(),
        base_ref_name: "main".into(),
        head_ref_name: "feature".into(),
        base_ref_oid: endpoints.left.hex.clone(),
        head_ref_oid: endpoints.right.hex.clone(),
        commit_count: Some(1),
    };
    let full = fixture
        .db
        .deploy_pull_request(
            fixture.session.repository.id.clone(),
            identity.clone(),
            metadata.clone(),
            fixture.session.snapshot.clone(),
            None,
        )
        .await
        .expect("full PR");
    let mut snapshot = fixture.session.snapshot.clone();
    snapshot.groups[0].files.truncate(1);
    snapshot.groups[0].endpoints.group =
        format!("commit:{}:{}", endpoints.left.hex, endpoints.right.hex);
    let scope = PullRequestCommitScope::Commits {
        selections: vec![CommitSelection {
            commit: endpoints.right.clone(),
            parent: Some(endpoints.left.clone()),
        }],
    };
    fixture.session = fixture
        .db
        .create_pull_request_scope(
            full.repository.id.clone(),
            identity.clone(),
            metadata,
            endpoints.left,
            endpoints.right,
            scope,
            snapshot,
        )
        .await
        .expect("PR commit scope");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(
            view.editor.is_some(),
            "scope file must load before reviewing"
        );
        assert_eq!(
            view.data.session.selector.pull_request_identity(),
            Some(&identity)
        );
        assert!(!view.data.session.selector.supports_github_viewed());
        assert!(!view.checking);
        assert!(view.tab_content_text(0, cx).contains("Selected commits"));
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    assert!(
        fixture
            .db
            .checkpoint(&full.id, GROUP, "auth.ts")
            .expect("full checkpoint")
            .is_none()
    );
    view.read_with(&cx, |view, _| assert!(view.data.viewed.contains("auth.ts")));
    let revision = fixture
        .db
        .load_session(&fixture.session.id)
        .expect("scope")
        .revision;
    view.update_in(&mut cx, |view, window, cx| view.refresh(window, cx));
    cx.run_until_parked();
    assert_eq!(
        fixture
            .db
            .load_session(&fixture.session.id)
            .expect("still pinned")
            .revision,
        revision
    );
}

#[gpui::test]
async fn pr_history_overlap_and_reversed_completion_preserve_newest_modal(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let workspace = view.read_with(&cx, |view, _| view.workspace.upgrade().expect("workspace"));
    let context = PullRequestReviewContext {
        identity: PullRequestIdentity {
            host: "github.com".into(),
            repository_node_id: "synthetic-repository".into(),
            pull_request_node_id: "synthetic-pr".into(),
            viewer_node_id: "synthetic-viewer".into(),
        },
        metadata: PullRequestMetadata {
            owner: "example".into(),
            repository: "review-fixture".into(),
            number: 42,
            title: "Synthetic PR".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(1),
        },
        selection: None,
    };
    let origin = view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: context.identity.clone(),
        };
        view.data.pull_request = Some(context.metadata.clone());
        view.history_request = 2;
        view.history_loading = true;
        view.pick_pull_request_commits(window, cx);
        assert_eq!(
            view.history_request, 2,
            "overlapping clicks must not start another request"
        );
        CommitHistoryOrigin {
            session: view.data.session.id.clone(),
            selector: view.data.session.selector.clone(),
            base_tip: context.metadata.base_ref_oid.clone(),
            head_tip: context.metadata.head_ref_oid.clone(),
        }
    });
    let commit = git::review::ReviewCommit {
        commit: "2".repeat(40).parse().expect("synthetic commit"),
        parents: vec!["1".repeat(40).parse().expect("synthetic parent")],
        subject: "Synthetic documentation".into(),
    };
    view.update_in(&mut cx, |view, window, cx| {
        view.finish_commit_history(
            2,
            origin.clone(),
            context.clone(),
            Ok(vec![commit.clone()]),
            window,
            cx,
        )
    });
    let newest_modal = workspace.read_with(&cx, |workspace, cx| {
        workspace
            .active_modal::<ReviewCommitPicker>(cx)
            .expect("newest picker")
            .entity_id()
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.finish_commit_history(
            1,
            origin.clone(),
            context.clone(),
            Err(anyhow::anyhow!("late old failure")),
            window,
            cx,
        )
    });
    assert_eq!(
        workspace.read_with(&cx, |workspace, cx| workspace
            .active_modal::<ReviewCommitPicker>(cx)
            .expect("picker must remain open")
            .entity_id()),
        newest_modal
    );
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.hide_modal(window, cx);
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.history_request = 3;
        view.history_loading = true;
        view.data
            .pull_request
            .as_mut()
            .expect("metadata")
            .head_ref_oid = "3".repeat(40);
        view.finish_commit_history(
            3,
            origin.clone(),
            context.clone(),
            Ok(vec![commit.clone()]),
            window,
            cx,
        );
        assert!(!view.history_loading);
        assert!(view.history_error.is_none());
    });
    assert!(
        workspace.read_with(&cx, |workspace, cx| workspace
            .active_modal::<ReviewCommitPicker>(cx)
            .is_none()),
        "old pinned tips must not open a picker"
    );
    let other = cx.update(|window, cx| cx.new(|cx| Editor::single_line(window, cx)));
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(other), None, true, window, cx)
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.history_request = 4;
        view.history_loading = true;
        view.data.pull_request = Some(context.metadata.clone());
        view.finish_commit_history(4, origin, context, Ok(vec![commit]), window, cx);
    });
    assert!(
        workspace.read_with(&cx, |workspace, cx| workspace
            .active_modal::<ReviewCommitPicker>(cx)
            .is_none()),
        "an inactive originating view must not interrupt another item"
    );
}

#[gpui::test]
async fn pr_tree_resize_handle_has_hit_area_and_drag_persists_width(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    cx.run_until_parked();
    let handle = cx
        .debug_bounds("REVIEW_TREE_RESIZE_HANDLE")
        .expect("rendered resize handle");
    let panel = cx
        .debug_bounds("REVIEW_TREE_PANEL")
        .expect("rendered tree panel");
    assert!(
        handle.size.height > px(100.),
        "resize hit area must span the tree panel: {handle:?}"
    );
    assert!(handle.size.width >= px(7.));
    let start = handle.center();
    cx.simulate_mouse_move(start, None, gpui::Modifiers::none());
    cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_move(
        gpui::point(start.x - px(12.), start.y),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    assert!(
        cx.update(|_, cx| cx.has_active_drag()),
        "pointer movement must start the resize drag"
    );
    cx.simulate_mouse_move(
        gpui::point(start.x - px(70.), start.y),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    view.read_with(&cx, |view, _| {
        assert!(
            view.tree_width_preview.is_some(),
            "active drag must update width before release"
        )
    });
    cx.simulate_mouse_up(
        gpui::point(start.x - px(70.), start.y),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let restored = fixture
        .db
        .load_session(&fixture.session.id)
        .expect("resized session");
    let width = restored
        .presentation
        .tree_width
        .expect("drag must persist a tree width");
    assert!(
        (width as f32) < f32::from(panel.size.width),
        "dragging left must narrow the actual tree"
    );
    assert!(width >= 180);
}

#[gpui::test]
async fn pr_tree_accessible_width_actions_persist_and_reset(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    cx.focus(&view);
    cx.dispatch_action(NarrowReviewTree);
    cx.condition(&view, |view, _| {
        !view.busy
            && !view.navigation_pending
            && view.data.session.presentation.tree_width.is_some()
    })
    .await;
    let narrow = fixture
        .db
        .load_session(&fixture.session.id)
        .expect("narrowed session")
        .presentation
        .tree_width
        .expect("narrow width");
    assert!(narrow < 320);
    cx.dispatch_action(WidenReviewTree);
    cx.condition(&view, |view, _| {
        !view.busy
            && !view.navigation_pending
            && view
                .data
                .session
                .presentation
                .tree_width
                .is_some_and(|width| width > narrow)
    })
    .await;
    let wide = fixture
        .db
        .load_session(&fixture.session.id)
        .expect("widened session")
        .presentation
        .tree_width
        .expect("wide width");
    assert!(wide > narrow && wide <= 520);
    cx.dispatch_action(ResetReviewTreeWidth);
    cx.condition(&view, |view, _| {
        !view.busy
            && !view.navigation_pending
            && view.data.session.presentation.tree_width.is_none()
    })
    .await;
    assert_eq!(
        fixture
            .db
            .load_session(&fixture.session.id)
            .expect("reset session")
            .presentation
            .tree_width,
        None
    );
}

#[gpui::test]
async fn github_comment_action_recaptures_focused_split_side(cx: &mut TestAppContext) {
    use crate::github_review::DiffSide;

    let fixture = fixture(cx).await.expect("fixture");
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.editor.minimum_split_diff_width = Some(0.0);
            });
        });
    });
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "synthetic-repository".into(),
                pull_request_node_id: "synthetic-pr".into(),
                viewer_node_id: "synthetic-viewer".into(),
            },
        };
        view.data.pull_request = Some(PullRequestMetadata {
            owner: "example".into(),
            repository: "review-fixture".into(),
            number: 42,
            title: "Synthetic PR".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(1),
        });
        view.data.session.presentation.split_view = Some(true);
        view.load_selected(window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let (editor, panel) = view.read_with(&cx, |view, _| {
        (
            view.editor.clone().expect("loaded split editor"),
            view.github_panel.clone().expect("GitHub panel"),
        )
    });
    panel.update(&mut cx, |panel, _| {
        panel.set_canonical_patch_for_test("auth.ts", "@@ -1 +1 @@\n-base auth\n+reviewed auth");
    });
    let (lhs, rhs) = editor.read_with(&cx, |editor, _| {
        (
            editor.lhs_editor().expect("left editor").clone(),
            editor.rhs_editor().clone(),
        )
    });
    // Capture a valid RIGHT anchor before changing only focus. Both panes
    // already have a cursor on line one, so no selection event is required.
    cx.focus(&rhs);
    cx.run_until_parked();
    view.update_in(&mut cx, |view, _, cx| view.capture_github_selection(cx));
    view.read_with(&cx, |view, _| {
        assert_eq!(
            view.github_capture.as_ref().unwrap().anchor.side,
            DiffSide::Right
        );
    });
    cx.focus(&lhs);
    cx.run_until_parked();
    editor.read_with(&cx, |editor, _| {
        assert_eq!(editor.focused_editor().entity_id(), lhs.entity_id());
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.comment_on_selection(window, cx)
    });
    panel.read_with(&cx, |panel, _| {
        let anchor = panel.composer_anchor().expect("left composer anchor");
        assert_eq!(anchor.side, DiffSide::Left);
        assert_eq!(anchor.line, Some(1));
        assert_eq!(anchor.path, "auth.ts");
    });
    view.read_with(&cx, |view, _| {
        assert!(
            !view.github_open,
            "Inline composing never opens the side panel"
        )
    });
    // Responsive split changes must retain the composer inline at its LEFT location.
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.toggle_split(&editor::ToggleSplitDiff, window, cx);
    });
    cx.run_until_parked();
    assert!(editor.read_with(&cx, |editor, _| editor.lhs_editor().is_none()));
    assert!(
        cx.debug_bounds("GITHUB_REVIEW_BODY_EDITOR").is_some(),
        "detaching the LEFT pane must remap the composer into the unified diff"
    );
    view.read_with(&cx, |view, _| {
        assert!(view.github_blocks.iter().all(|(owner, _)| owner != &lhs));
    });
    panel.read_with(&cx, |panel, _| {
        assert_eq!(panel.composer_anchor().unwrap().side, DiffSide::Left);
    });
    // Returning to the unchanged RIGHT cursor must likewise replace the old
    // LEFT capture, even when focus changed without a selection event.
    cx.focus(&rhs);
    cx.run_until_parked();
    view.update_in(&mut cx, |view, window, cx| {
        view.comment_on_selection(window, cx)
    });
    panel.read_with(&cx, |panel, _| {
        assert_eq!(panel.composer_anchor().unwrap().side, DiffSide::Right);
    });
}

#[gpui::test]
async fn pr_since_review_uses_retained_reviewed_new_side_and_restores_mode(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let reviewed = fixture
        .session
        .snapshot
        .file(GROUP, "auth.ts")
        .expect("reviewed file")
        .clone();
    fixture
        .db
        .mark_viewed(
            ViewedTarget {
                session: fixture.session.id.clone(),
                expected_revision: fixture.session.revision,
                displayed_generation: 1,
                group: GROUP.into(),
                path: "auth.ts".into(),
                fingerprint: reviewed.fingerprint().expect("fingerprint"),
            },
            RetainedContents {
                old: Some(b"base auth\n".to_vec()),
                new: Some(b"reviewed auth\n".to_vec()),
            },
            1024,
        )
        .await
        .expect("retained review checkpoint");
    let current = comparison("auth.ts", b"UNRELATED BASE REVISION\n", b"new auth\n");
    let mut snapshot = fixture.session.snapshot.clone();
    snapshot.generation = 2;
    snapshot.groups[0].files[0] = current;
    let session = fixture
        .db
        .load_session(&fixture.session.id)
        .expect("current revision");
    fixture
        .db
        .activate_snapshot(session.id.clone(), session.revision, snapshot.clone())
        .await
        .expect("advanced snapshot");
    seed_review(&fixture, &snapshot, b"new auth\n");
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.oids.remove(
                &reviewed
                    .new
                    .as_ref()
                    .expect("reviewed new side")
                    .object
                    .hex
                    .parse()
                    .expect("object"),
            );
        })
        .expect("remove reviewed blob from Git object store");
    let session = fixture
        .db
        .load_session(&fixture.session.id)
        .expect("new generation");
    let mut presentation = session.presentation.clone();
    presentation.selected_path = Some("auth.ts".into());
    presentation.since_review = true;
    fixture
        .db
        .save_presentation(session.id.clone(), session.revision, presentation)
        .await
        .expect("persist Since review mode");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && (view.editor.is_some() || view.error.is_some())
    })
    .await;
    view.read_with(&cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(view.data.session.presentation.since_review);
        let editor = view.editor.as_ref().expect("Since review editor").read(cx);
        let rhs = editor.rhs_editor().read(cx);
        assert!(rhs.read_only(cx));
        assert!(rhs.text(cx).contains("new auth"));
        let buffer = rhs.buffer().read(cx);
        let source = buffer
            .all_buffers_iter()
            .next()
            .expect("current file buffer");
        let diff = buffer
            .diff_for(source.read(cx).remote_id())
            .expect("retained comparison");
        assert_eq!(
            diff.read(cx).snapshot(cx).base_text().text(),
            "reviewed auth\n"
        );
        assert!(
            !view.data.viewed.contains("auth.ts"),
            "incremental rendering must not approve the full changed comparison"
        );
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    view.read_with(&cx, |view, _| {
        assert!(
            view.github_viewed_error
                .as_deref()
                .is_some_and(|error| error.contains("Full PR comparison"))
        );
        assert!(!view.data.viewed.contains("auth.ts"));
    });
}

#[test]
fn pr_since_review_presentation_preserves_legacy_default() {
    let presentation: ReviewPresentation =
        serde_json::from_str(r#"{"unreviewed_only":false}"#).expect("legacy mode");
    assert!(!presentation.since_review);
    let mut presentation = presentation;
    presentation.since_review = true;
    let restored: ReviewPresentation =
        serde_json::from_str(&serde_json::to_string(&presentation).expect("serialize mode"))
            .expect("restore mode");
    assert!(restored.since_review);
}

#[gpui::test]
async fn pr_viewed_unknown_reconciliation_rejects_old_head_and_base_before_network(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let context = view.update(&mut cx, |view, _| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "synthetic-repository".into(),
                pull_request_node_id: "synthetic-pr".into(),
                viewer_node_id: "synthetic-viewer".into(),
            },
        };
        view.data.pull_request = Some(PullRequestMetadata {
            owner: "example".into(),
            repository: "review-fixture".into(),
            number: 42,
            title: "Synthetic PR".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(1),
        });
        ReviewOperationContext {
            selector: view.data.session.selector.clone(),
            metadata: view.data.pull_request.clone().expect("metadata"),
        }
    });
    let fingerprint = fixture
        .session
        .snapshot
        .file(GROUP, "auth.ts")
        .expect("unchanged file")
        .fingerprint()
        .expect("fingerprint");
    for old_base in [false, true] {
        let mut stale = context.clone();
        if old_base {
            stale.metadata.base_ref_oid = "3".repeat(40);
        } else {
            stale.metadata.head_ref_oid = "4".repeat(40);
        }
        let intent = crate::review_store::GitHubViewedIntent {
            paths: vec!["auth.ts".into()],
            viewed: true,
            fingerprints: BTreeMap::from([("auth.ts".into(), fingerprint.clone())]),
            origin: Some(crate::review_store::GitHubViewedSyncTarget {
                session: fixture.session.id.clone(),
                expected_revision: fixture.session.revision,
                displayed_generation: fixture.session.snapshot.generation,
            }),
        };
        let draft = fixture
            .db
            .create_operation(
                stale,
                ReviewOperationKind::SetFileViewed,
                serde_json::to_value(intent).expect("intent"),
                ReviewLocationState::NotRequired,
            )
            .await
            .expect("draft");
        let queued = fixture
            .db
            .queue_operation(draft.operation_id, draft.revision)
            .await
            .expect("queued");
        let sending = fixture
            .db
            .claim_operation(queued.operation_id, queued.revision, Vec::new())
            .await
            .expect("claimed");
        let unknown = fixture
            .db
            .finish_operation(
                sending.operation_id,
                sending.revision,
                ReviewOperationOutcome::DeliveryUnknown {
                    reason: "Unconfirmed synthetic request".into(),
                },
            )
            .await
            .expect("unknown");
        view.update_in(&mut cx, |view, window, cx| {
            view.handle_github_viewed_operation(unknown.clone(), true, window, cx);
            assert!(
                !view.github_viewed_busy && !view.busy && !view.navigation_pending,
                "stale context must be rejected before scheduling a network read"
            );
            assert!(
                view.github_viewed_error
                    .as_deref()
                    .is_some_and(|error| error.contains("another pinned PR head or base"))
            );
        });
        let retained = fixture
            .db
            .operation(&unknown.operation_id)
            .expect("durable outcome");
        assert_eq!(retained.state, ReviewOperationState::DeliveryUnknown);
        assert_eq!(retained.revision, unknown.revision);
    }
}

#[gpui::test]
async fn pr_filtered_folder_action_shows_all_targets_and_fits_tree(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update(&mut cx, |view, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "synthetic-repository".into(),
                pull_request_node_id: "synthetic-pr".into(),
                viewer_node_id: "synthetic-viewer".into(),
            },
        };
        let folder = "level00/level01/level02/level03/level04/level05/level06/level07/level08/level09/level10/level11/level12/level13/a-long-folder-name-that-must-truncate";
        let files = ["shown.ts", "hidden-one.ts", "hidden-two.ts"]
            .map(|name| comparison(&format!("{folder}/{name}"), b"old\n", b"new\n"))
            .to_vec();
        view.data.session.snapshot.groups[0].files = files.clone();
        view.data.files = files
            .iter()
            .map(|file| file.file_key().expect("path").to_string())
            .collect();
        view.data.viewed = BTreeSet::from([format!("{folder}/shown.ts")]);
        view.data.session.presentation.path_filter = "shown.ts".into();
        view.data.session.presentation.tree_width = Some(320);
        let row = review_tree(&view.data)
            .into_iter()
            .find(|row| row.directory)
            .expect("shown folder");
        assert_eq!((row.viewed, row.total), (1, 1));
        let (count, viewed, label) = folder_viewed_action(&view.data, folder);
        assert_eq!(count, 3);
        assert!(viewed);
        assert_eq!(label, "Mark 3 viewed");
        let targets =
            crate::review_github_viewed::folder_viewed_batches(&view.data.session, folder)
                .expect("canonical targets")
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
        assert_eq!(
            targets.len(),
            3,
            "hidden canonical files remain part of the explicit folder intent"
        );
        cx.notify();
    });
    cx.run_until_parked();
    let panel = cx.debug_bounds("REVIEW_TREE_PANEL").expect("tree");
    let action = cx
        .debug_bounds("REVIEW_FOLDER_VIEWED_ACTION")
        .expect("folder action");
    assert!(panel.size.width <= px(320.));
    assert!(
        action.left() >= panel.left() && action.right() <= panel.right(),
        "the visible all-target count/action must fit the 320px tree: action {action:?}, tree {panel:?}"
    );
    assert!(
        action.size.width >= px(32.),
        "folder count and native checkbox must remain visible while the name truncates"
    );
    let folder_index = view.read_with(&cx, |view, _| {
        review_tree(&view.data)
            .iter()
            .rposition(|row| row.directory)
            .unwrap()
    });
    let label = cx
        .debug_bounds(Box::leak(
            format!("REVIEW_TREE_NAME_{folder_index}").into_boxed_str(),
        ))
        .expect("folder name bounds");
    assert!(
        label.right() <= action.left(),
        "truncated folder name and reserved count slot cannot overlap: {label:?}, {action:?}"
    );
}

#[gpui::test]
async fn review_oversized_selected_blob_is_rejected_before_content_read_and_navigation_recovers(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    // The fake backend answers size probes from metadata without traversing
    // its content-read gate. A blocked read therefore exposes a missing probe.
    let oversized = vec![0; REVIEW_PREVIEW_BUDGET as usize + 1];
    let large = comparison("auth.ts", b"base auth\n", &oversized);
    let object = large
        .new
        .as_ref()
        .expect("new side")
        .object
        .hex
        .parse()
        .expect("object");
    let gate = ::fs::FakeBlobReadGate::default();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.oids.insert(object, oversized);
            state.blob_read_gate = Some(gate.clone());
        })
        .expect("fake Git state");
    let mut snapshot = fixture.session.snapshot.clone();
    snapshot.generation += 1;
    snapshot.groups[0].files[0] = large;
    fixture
        .db
        .activate_snapshot(
            fixture.session.id.clone(),
            fixture.session.revision,
            snapshot,
        )
        .await
        .expect("oversized snapshot");
    let (view, mut window_cx) = review_window(&fixture, cx, None);
    cx.run_until_parked();
    assert_eq!(
        gate.peak_concurrent(),
        0,
        "preview must not request either blob's contents"
    );
    view.read_with(cx, |view, _| {
        assert!(
            !view.busy && !view.navigation_pending,
            "size rejection must complete instead of waiting on content"
        );
        assert!(
            view.error
                .as_deref()
                .is_some_and(|error| error.contains("64 MiB preview limit")),
            "{:?}",
            view.error
        );
        assert!(view.editor.is_none());
        assert!(view.retained.is_none());
        assert!(!view.data.viewed.contains("auth.ts"));
    });
    assert!(
        fixture
            .db
            .checkpoint(&fixture.session.id, GROUP, "auth.ts")
            .expect("checkpoint")
            .is_none()
    );
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.blob_read_gate = None;
        })
        .expect("remove gate");
    view.update_in(&mut window_cx, |view, window, cx| {
        view.select("billing.ts".into(), window, cx)
    });
    cx.condition(&view, |view, _| {
        !view.busy && !view.navigation_pending && view.editor.is_some()
    })
    .await;
    view.read_with(cx, |view, _| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("billing.ts")
        );
    });
}

#[gpui::test]
async fn review_retained_full_snapshot_reopens_after_original_git_blobs_are_pruned(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let comparison = fixture
        .session
        .snapshot
        .file(GROUP, "auth.ts")
        .expect("comparison");
    fixture
        .db
        .mark_viewed(
            ViewedTarget {
                session: fixture.session.id.clone(),
                expected_revision: fixture.session.revision,
                displayed_generation: fixture.session.snapshot.generation,
                group: GROUP.into(),
                path: "auth.ts".into(),
                fingerprint: comparison.fingerprint().expect("fingerprint"),
            },
            RetainedContents {
                old: Some(b"base auth\n".to_vec()),
                new: Some(b"reviewed auth\n".to_vec()),
            },
            1024,
        )
        .await
        .expect("retain exact review");
    assert!(
        fixture
            .db
            .matching_checkpoint_contents(&fixture.session.id, GROUP, "auth.ts", comparison, 1,)
            .expect_err("retained size probe must enforce its budget")
            .to_string()
            .contains("preview limit")
    );
    let gate = ::fs::FakeBlobReadGate::default();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.oids.clear();
            state.blob_read_gate = Some(gate.clone());
        })
        .expect("prune original objects");
    let (view, window_cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&window_cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(view.data.viewed.contains("auth.ts"));
        let editor = view
            .editor
            .as_ref()
            .expect("retained Full snapshot")
            .read(cx);
        assert!(
            editor
                .rhs_editor()
                .read(cx)
                .text(cx)
                .contains("reviewed auth")
        );
        let retained = view.retained.as_ref().expect("retained bytes");
        assert_eq!(retained.old.as_deref(), Some(b"base auth\n".as_slice()));
        assert_eq!(retained.new.as_deref(), Some(b"reviewed auth\n".as_slice()));
    });
    assert_eq!(
        gate.peak_concurrent(),
        0,
        "retained preview must not request Git blobs"
    );
}

#[gpui::test]
async fn pr_overview_navigation_releases_review_listener_lease(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let workspace = view.read_with(&cx, |view, _| view.workspace.upgrade().expect("workspace"));
    let reference =
        PullRequestReference::parse("https://github.com/example/review-fixture/pull/42", None)
            .expect("reference");
    let overview = workspace.update_in(&mut cx, |workspace, window, cx| {
        let overview = cx.new(|cx| {
            crate::pull_request_view::PullRequestView::new_for_navigation_test(
                workspace.weak_handle(),
                reference.clone(),
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(overview.clone()), None, true, window, cx);
        workspace.activate_item(&view, true, true, window, cx);
        overview
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.data.pull_request = Some(PullRequestMetadata {
            owner: "example".into(),
            repository: "review-fixture".into(),
            number: 42,
            title: "Synthetic PR".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(1),
        });
        // This is the production Overview listener's method, invoked while its view is leased.
        view.open_overview(reference, window, cx);
    });
    // No executor poll occurs between activation and cancellation: no real GH transport is run.
    overview.update(&mut cx, |overview, _| {
        overview.cancel_navigation_test_requests()
    });
    assert_eq!(
        workspace.read_with(&cx, |workspace, cx| workspace
            .active_item_as::<crate::pull_request_view::PullRequestView>(cx)
            .expect("Overview active")
            .entity_id()),
        overview.entity_id()
    );
    assert_eq!(
        workspace.read_with(&cx, |workspace, cx| workspace
            .items_of_type::<crate::pull_request_view::PullRequestView>(cx)
            .count()),
        1,
        "reuse the existing Overview tab"
    );
    cx.run_until_parked();
}

#[gpui::test]
async fn review_open_project_navigation_releases_review_listener_lease(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let workspace = view.read_with(&cx, |view, _| view.workspace.upgrade().expect("workspace"));
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.presentation.selected_path = Some("auth.ts".into());
        view.open_in_project(window, cx);
    });
    cx.run_until_parked();
    workspace.read_with(&cx, |workspace, cx| {
        let editor = workspace
            .active_item_as::<Editor>(cx)
            .expect("project editor active");
        let text = editor.read(cx).text(cx);
        assert_eq!(
            text, "WORKTREE auth\n",
            "open checkout contents rather than immutable review content"
        );
    });
}

#[gpui::test]
async fn viewed_sync_keeps_file_navigation_responsive_without_changing_receipt_revision(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let revision = view.read_with(&cx, |view, _| view.data.session.revision);
    view.update_in(&mut cx, |view, window, cx| {
        view.github_viewed_busy = true;
        view.select("billing.ts".into(), window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, _| {
        assert!(
            view.github_viewed_busy,
            "navigation must not cancel the mutation"
        );
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("billing.ts")
        );
        assert_eq!(
            view.data.session.revision, revision,
            "presentation cannot race a pending receipt revision"
        );
        assert!(view.error.is_none(), "{:?}", view.error);
    });
    assert_eq!(
        fixture
            .db
            .load_session(&fixture.session.id)
            .expect("session")
            .revision,
        revision
    );
}

#[gpui::test]
async fn scrolling_review_unviewing_sibling_restores_its_native_buffer(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update(&mut cx, |view, cx| {
        view.collapsed_review_files.insert("auth.ts".into());
        view.apply_review_file_folds(cx);
        let editor = view
            .editor
            .as_ref()
            .expect("editor")
            .read(cx)
            .rhs_editor()
            .clone();
        let buffer = editor
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .next()
            .expect("single-file buffer");
        assert!(
            editor
                .read(cx)
                .is_buffer_folded(buffer.read(cx).remote_id(), cx),
            "single-file review uses native folding without removing the editor header"
        );
        view.collapsed_review_files.clear();
        view.apply_review_file_folds(cx);
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "fixture-repository".into(),
                pull_request_node_id: "fixture-pr".into(),
                viewer_node_id: "fixture-viewer".into(),
            },
        };
        view.data.session.presentation.multi_file = Some(true);
        view.load_selected(window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update(&mut cx, |view, cx| {
        assert!(view.multi_loaded, "{:?}", view.error);
        view.collapsed_review_files
            .extend(["auth.ts".into(), "billing.ts".into()]);
        view.apply_review_file_folds(cx);
        let editor = view
            .editor
            .as_ref()
            .expect("editor")
            .read(cx)
            .rhs_editor()
            .clone();
        let buffers = editor
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .collect::<Vec<_>>();
        for buffer in &buffers {
            assert!(
                editor
                    .read(cx)
                    .is_buffer_folded(buffer.read(cx).remote_id(), cx)
            );
        }
        view.collapsed_review_files.remove("billing.ts");
        view.apply_review_file_folds(cx);
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("auth.ts")
        );
        for buffer in buffers {
            let buffer = buffer.read(cx);
            let path = buffer.file().expect("file").path().to_string();
            assert_eq!(
                editor.read(cx).is_buffer_folded(buffer.remote_id(), cx),
                path == "auth.ts",
                "{path}"
            );
        }
    });
}

#[gpui::test]
async fn snapshot_install_keeps_coordinated_refresh_continuation_alive(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.refreshing = true;
        view.refresh_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(20))
                .await;
            view.update(cx, |view, cx| {
                view.refreshing = false;
                view.github_viewed_status = Some("refresh continuation completed".into());
                cx.notify();
            })
            .expect("live view");
        }));
        let mut data = read_data(&fixture.db, fixture.session.clone()).expect("data");
        data.session.snapshot.generation += 1;
        view.finish(Ok(data), window, cx);
        assert!(view.refreshing);
        assert!(
            view.refresh_task.is_some(),
            "snapshot replacement must not cancel the coordinator"
        );
    });
    cx.condition(&view, |view, _| !view.refreshing).await;
    view.read_with(&cx, |view, _| {
        assert_eq!(
            view.github_viewed_status.as_deref(),
            Some("refresh continuation completed")
        )
    });
}

#[gpui::test]
async fn pr_warm_files_activation_preserves_editor_and_performs_no_remote_work(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let workspace = view.read_with(&cx, |view, _| view.workspace.upgrade().unwrap());
    let details: PullRequestDetails = serde_json::from_value(json!({
        "id":"PR_fixture", "number":7,"title":"Generic review", "url":"https://github.com/example/review-fixture/pull/7", "state":"OPEN", "isDraft":false,
        "author":null,"baseRefName":"main","headRefName":"feature","updatedAt":"now", "body":"", "baseRefOid":"1".repeat(40),
        "baseRef":{"name":"main","target":{"oid":"1".repeat(40)}},"headRefOid":"2".repeat(40),"changedFiles":2,"additions":2,"deletions":2,
        "mergeable":"MERGEABLE","reviewDecision":null,"repository":{"id":"R_fixture","nameWithOwner":"example/review-fixture","url":"https://github.com/example/review-fixture"},
        "headRepository":null,"commits":{"totalCount":1},"labels":{"totalCount":0,"nodes":[]},"reviewRequests":{"totalCount":0,"nodes":[]},"recentCommits":{"nodes":[]}
    })).unwrap();
    let viewer = GitHubUser {
        node_id: Some("U_fixture".into()),
        login: "reviewer".into(),
        name: None,
        url: None,
    };
    let editor_id = view.read_with(&cx, |view, _| view.editor.as_ref().unwrap().entity_id());
    view.update(&mut cx, |view, _| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_fixture".into(),
                pull_request_node_id: "PR_fixture".into(),
                viewer_node_id: "U_fixture".into(),
            },
        }
    });
    let overview = workspace.update_in(&mut cx, |workspace, window, cx| {
        let reference = PullRequestReference::parse(&details.summary.url, None).unwrap();
        let overview = cx.new(|cx| {
            crate::pull_request_view::PullRequestView::new_for_navigation_test(
                workspace.weak_handle(),
                reference,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(overview.clone()), None, true, window, cx);
        overview
    });
    for _ in 0..20 {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.activate_item(&overview, true, true, window, cx)
        });
        // Match the actual Files-button event: the currently active overview is
        // leased while it asks the Workspace to activate the warm ReviewView.
        // Synchronous activation here used to panic while deactivating overview.
        let task = overview.update_in(&mut cx, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                ReviewView::open_pull_request(
                    workspace,
                    details.clone(),
                    viewer.clone(),
                    window,
                    cx,
                )
            })
        });
        task.await
            .expect("activation completes after the overview listener releases its lease");
        assert_eq!(
            workspace.read_with(&cx, |workspace, cx| workspace
                .active_item_as::<ReviewView>(cx)
                .expect("Files active before completion")
                .entity_id()),
            view.entity_id()
        );
    }
    overview.update(&mut cx, |overview, _| {
        overview.set_navigation_test_details(details.clone(), viewer.clone())
    });
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.activate_item(&overview, true, true, window, cx)
    });
    cx.dispatch_action(crate::pull_request_view::ReviewPullRequestFiles);
    cx.run_until_parked();
    assert_eq!(
        workspace.read_with(&cx, |workspace, cx| workspace
            .active_item_as::<ReviewView>(cx)
            .expect("command activates Files after releasing Workspace lease")
            .entity_id()),
        view.entity_id()
    );
    overview.update(&mut cx, |overview, _| {
        overview.cancel_navigation_test_requests()
    });
    view.read_with(&cx, |view, _| {
        assert_eq!(view.editor.as_ref().unwrap().entity_id(), editor_id);
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("auth.ts")
        );
        assert_eq!(view.prepared_files.len(), 1);
    });
}

#[gpui::test]
async fn review_file_switch_keeps_visible_editor_and_reuses_immutable_preparation(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let original = view.read_with(&cx, |view, cx| {
        let editor = view.editor.as_ref().unwrap();
        (
            editor.entity_id(),
            editor
                .read(cx)
                .rhs_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers_iter()
                .next()
                .unwrap()
                .entity_id(),
        )
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.select("billing.ts".into(), window, cx);
        assert!(view.busy);
        assert_eq!(
            view.editor.as_ref().unwrap().entity_id(),
            original.0,
            "prepare cannot blank the visible editor"
        );
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.select("auth.ts".into(), window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, cx| {
        let buffer = view
            .editor
            .as_ref()
            .unwrap()
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .next()
            .unwrap();
        assert_eq!(
            buffer.entity_id(),
            original.1,
            "immutable prepared Buffer/Diff entities are reused"
        );
        assert!(view.prepared_bytes <= 64 * 1024 * 1024);
        assert_eq!(view.prepared_files.len(), 2);
    });
}

#[gpui::test]
async fn selected_pr_patches_share_one_tree_but_keep_exact_immutable_contents(
    cx: &mut TestAppContext,
) {
    let mut fixture = fixture(cx).await.expect("fixture");
    let mut first = fixture.session.snapshot.groups[0].clone();
    first.files.truncate(1);
    first.endpoints.group = format!(
        "commit:{}:{}",
        first.endpoints.left.hex, first.endpoints.right.hex
    );
    let mut second = first.clone();
    second.endpoints.left = first.endpoints.right.clone();
    second.endpoints.right.hex = "3".repeat(40);
    second.endpoints.group = format!(
        "commit:{}:{}",
        second.endpoints.left.hex, second.endpoints.right.hex
    );
    second.files = vec![
        comparison("auth.ts", b"10\n", b"12\n"),
        comparison("billing.ts", b"10\n", b"12\n"),
    ];
    let identity = PullRequestIdentity {
        host: "github.com".into(),
        repository_node_id: "R_fixture".into(),
        pull_request_node_id: "PR_fixture".into(),
        viewer_node_id: "U_fixture".into(),
    };
    let metadata = PullRequestMetadata {
        owner: "example".into(),
        repository: "review-fixture".into(),
        number: 7,
        title: "Generic selected patches".into(),
        base_ref_name: "main".into(),
        head_ref_name: "feature".into(),
        base_ref_oid: first.endpoints.left.hex.clone(),
        head_ref_oid: second.endpoints.right.hex.clone(),
        commit_count: Some(3),
    };
    let selections = [first.clone(), second.clone()]
        .iter()
        .map(|group| CommitSelection {
            commit: group.endpoints.right.clone(),
            parent: Some(group.endpoints.left.clone()),
        })
        .collect();
    fixture.session = fixture
        .db
        .create_pull_request_scope(
            fixture.session.repository.id.clone(),
            identity,
            metadata,
            first.endpoints.left.clone(),
            second.endpoints.right.clone(),
            PullRequestCommitScope::Commits { selections },
            ReviewSnapshot {
                generation: 1,
                groups: vec![first.clone(), second.clone()],
                incomplete_reason: None,
            },
        )
        .await
        .expect("scope");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert_eq!(view.data.files, vec!["auth.ts", "billing.ts"]);
        assert_eq!(view.selected_patch_contents.len(), 2);
        let editor = view.editor.as_ref().unwrap().read(cx).rhs_editor().read(cx);
        let buffer = editor.buffer().read(cx);
        let buffers = buffer.all_buffers_iter().collect::<Vec<_>>();
        assert_eq!(
            buffers.len(),
            2,
            "each selected immutable parent-child patch remains visible"
        );
        let texts = buffers
            .iter()
            .map(|buffer| buffer.read(cx).text())
            .collect::<Vec<_>>();
        assert!(texts.iter().any(|text| text == "reviewed auth\n"));
        assert!(texts.iter().any(|text| text == "12\n"));
        assert!(!view.data.session.selector.supports_github_viewed());
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.toggle_viewed(&ToggleReviewViewed, window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, _| assert!(view.data.viewed.contains("auth.ts")));
    for group in [first, second] {
        assert!(
            fixture
                .db
                .checkpoint(&fixture.session.id, &group.endpoints.group, "auth.ts")
                .unwrap()
                .is_some()
        );
    }
}

#[gpui::test]
async fn unreviewed_filter_excludes_initial_server_viewed_even_when_collapsed(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_fixture".into(),
                pull_request_node_id: "PR_fixture".into(),
                viewer_node_id: "U_fixture".into(),
            },
        };
        view.data.viewed.insert("auth.ts".into());
        view.collapsed_review_files.insert("auth.ts".into());
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("auth.ts")
        );
        let mut presentation = view.data.session.presentation.clone();
        presentation.unreviewed_only = true;
        view.change_presentation(presentation, window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, _| {
        assert_eq!(paths(&view.effective_data()), vec!["billing.ts"]);
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("billing.ts")
        );
        assert!(
            view.local_viewed_pin.is_none(),
            "server state must never manufacture a local focus pin"
        );
    });
}

#[gpui::test]
async fn unreviewed_local_viewed_pin_survives_acknowledgement_until_navigation(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_fixture".into(),
                pull_request_node_id: "PR_fixture".into(),
                viewer_node_id: "U_fixture".into(),
            },
        };
        let mut presentation = view.data.session.presentation.clone();
        presentation.unreviewed_only = true;
        view.change_presentation(presentation, window, cx);
        // Production calls this seam only after bytes/checkpoint/outbox commit.
        view.github_viewed_intents.insert("auth.ts".into(), true);
        view.collapsed_review_files.insert("auth.ts".into());
        view.pin_locally_viewed_selection();
        view.apply_review_file_folds(cx);
        assert!(
            !view.data.viewed.contains("auth.ts"),
            "local intent is not server confirmation"
        );
        assert_eq!(
            view.effective_data().pinned_path.as_deref(),
            Some("auth.ts")
        );
        assert_eq!(paths(&view.effective_data()), vec!["auth.ts", "billing.ts"]);
        // Simulate ordered authoritative acknowledgement after the overlay clears.
        view.github_viewed_intents.clear();
        let mut confirmed = view.data.clone();
        confirmed.viewed.insert("auth.ts".into());
        view.finish(Ok(confirmed), window, cx);
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("auth.ts")
        );
        assert_eq!(
            view.effective_data().pinned_path.as_deref(),
            Some("auth.ts")
        );
        assert!(view.collapsed_review_files.contains("auth.ts"));
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.navigate_file(true, window, cx)
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, _| {
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("billing.ts")
        );
        assert!(view.local_viewed_pin.is_none());
        assert_eq!(paths(&view.effective_data()), vec!["billing.ts"]);
    });
}

#[gpui::test]
async fn conversation_links_require_exact_identity_head_and_current_coordinates(
    cx: &mut TestAppContext,
) {
    use crate::{
        github_pull_requests::ConversationAnchor,
        github_review::{CommentSubject, DiffSide},
    };
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let identity = PullRequestIdentity {
        host: "github.com".into(),
        repository_node_id: "R_fixture".into(),
        pull_request_node_id: "PR_fixture".into(),
        viewer_node_id: "U_fixture".into(),
    };
    let head = "2".repeat(40);
    let anchor = ConversationAnchor {
        identity: identity.clone(),
        thread_id: "thread-left".into(),
        head_oid: head.clone(),
        path: "billing.ts".into(),
        line: Some(1),
        start_line: None,
        side: DiffSide::Left,
        start_side: None,
        original_line: Some(77),
        original_start_line: None,
        is_outdated: false,
        subject_type: CommentSubject::Line,
    };
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest { identity };
        view.data.pull_request = Some(PullRequestMetadata {
            owner: "example".into(),
            repository: "review-fixture".into(),
            number: 7,
            title: "Generic linked review".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: head,
            commit_count: Some(1),
        });
        let original_path = view.data.session.presentation.selected_path.clone();
        for invalid in 0..4 {
            let mut target = anchor.clone();
            match invalid {
                0 => target.identity.viewer_node_id = "different-viewer".into(),
                1 => target.head_oid = "9".repeat(40),
                2 => target.is_outdated = true,
                _ => target.line = None,
            }
            assert!(view.reveal_conversation_anchor(target, window, cx).is_err());
            assert_eq!(view.data.session.presentation.selected_path, original_path);
            assert!(view.github_conversation_anchor.is_none());
        }
        view.reveal_conversation_anchor(anchor.clone(), window, cx)
            .expect("guarded LEFT link");
        assert_eq!(
            view.data.session.presentation.selected_path.as_deref(),
            Some("billing.ts")
        );
        assert_eq!(
            view.github_conversation_anchor.as_ref().unwrap().line,
            Some(1)
        );
        assert_eq!(
            view.github_conversation_anchor
                .as_ref()
                .unwrap()
                .original_line,
            Some(77),
            "historical coordinates are retained separately, never substituted"
        );
        assert!(
            !view.github_open,
            "conversation links must not force the optional panel open"
        );
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        assert_eq!(
            view.github_conversation_anchor.as_ref().unwrap().side,
            DiffSide::Left,
            "pending location survives file preparation while canonical thread data loads"
        );
        view.select("auth.ts".into(), window, cx);
        assert!(
            view.github_conversation_anchor.is_none(),
            "later user navigation cancels an obsolete reveal"
        );
    });
}

struct DockedReviewLayoutHarness {
    view: Entity<ReviewView>,
    _workspace: Entity<Workspace>,
}
impl Render for DockedReviewLayoutHarness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .child(div().w(px(240.)).h_full().flex_shrink_0())
            .child(div().flex_1().min_w_0().h_full().child(self.view.clone()))
    }
}

#[gpui::test]
async fn explicit_review_sidebar_preserves_diff_width_in_a_docked_1050_window(
    cx: &mut TestAppContext,
) {
    use gpui::VisualContext as _;
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let workspace = view.read_with(&cx, |view, _| view.workspace.upgrade().unwrap());
    let original = view.read_with(&cx, |view, _| view.data.session.presentation.clone());
    view.update_in(&mut cx, |view, window, cx| {
        view.github_panel = Some(cx.new(|cx| GitHubReviewPanel::preview(window, cx)));
        view.github_open = true;
        cx.notify();
    });
    let review = view.clone();
    cx.replace_root_view(|_, _| DockedReviewLayoutHarness {
        view: review,
        _workspace: workspace,
    });
    cx.simulate_resize(gpui::size(px(1050.), px(900.)));
    cx.run_until_parked();
    let body = cx
        .debug_bounds("REVIEW_FILES_LAYOUT")
        .expect("review content body");
    let diff = cx
        .debug_bounds("REVIEW_DIFF_CONTENT")
        .expect("diff content");
    let sidebar = cx
        .debug_bounds("REVIEW_GITHUB_SIDEBAR")
        .expect("explicit sidebar");
    assert!(
        body.size.width <= px(810.),
        "240px external dock reduces actual content width: {body:?}"
    );
    assert!(
        cx.debug_bounds("REVIEW_TREE_PANEL").is_none(),
        "temporarily hide the review tree rather than squeeze the diff"
    );
    assert!(
        diff.size.width >= px(360.),
        "diff must remain usable: {diff:?}"
    );
    assert!(
        sidebar.right() <= body.right() && sidebar.left() >= body.left(),
        "sidebar and its close control must remain in the review viewport"
    );
    view.update(&mut cx, |view, cx| {
        view.github_open = false;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("REVIEW_TREE_PANEL").is_some(),
        "closing Threads restores the existing tree"
    );
    view.read_with(&cx, |view, _| {
        assert_eq!(
            view.data.session.presentation, original,
            "responsive layout does not rewrite user presentation state"
        )
    });
    view.update(&mut cx, |view, cx| {
        view.github_open = true;
        cx.notify();
    });
    cx.simulate_resize(gpui::size(px(1450.), px(900.)));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("REVIEW_TREE_PANEL").is_some(),
        "a wide workspace shows tree, diff and explicit panel together"
    );
    assert!(cx.debug_bounds("REVIEW_DIFF_CONTENT").unwrap().size.width >= px(360.));
}

#[gpui::test]
async fn review_context_menu_comments_inline_after_releasing_the_editor_lease(
    cx: &mut TestAppContext,
) {
    use crate::github_review::DiffSide;
    let fixture = fixture(cx).await.expect("fixture");
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.editor.minimum_split_diff_width = Some(0.);
            })
        })
    });
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_fixture".into(),
                pull_request_node_id: "PR_fixture".into(),
                viewer_node_id: "U_fixture".into(),
            },
        };
        view.data.pull_request = Some(PullRequestMetadata {
            owner: "example".into(),
            repository: "review-fixture".into(),
            number: 7,
            title: "Generic context menu".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(1),
        });
        view.data.session.presentation.split_view = Some(true);
        view.load_selected(window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let (lhs, panel) = view.read_with(&cx, |view, cx| {
        (
            view.editor
                .as_ref()
                .unwrap()
                .read(cx)
                .lhs_editor()
                .unwrap()
                .clone(),
            view.github_panel.as_ref().unwrap().clone(),
        )
    });
    panel.update(&mut cx, |panel, _| {
        panel.set_canonical_patch_for_test("auth.ts", "@@ -1 +1 @@\n-base auth\n+reviewed auth")
    });
    cx.focus(&lhs);
    lhs.update_in(&mut cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([language::Point::zero()..language::Point::new(0, 4)]);
        })
    });
    cx.run_until_parked();
    view.update_in(&mut cx, |view, _, cx| view.capture_github_selection(cx));
    let review = view.downgrade();
    // The production factory executes while Editor is leased. It must not
    // attempt to recapture by reentering that editor from its parent view.
    let context = lhs.update_in(&mut cx, |editor, window, cx| {
        ReviewView::review_context_menu(editor, review, window, cx)
    });
    context.update_in(&mut cx, |menu, window, cx| {
        menu.select_first(&menu::SelectFirst, window, cx);
        menu.confirm(&menu::Confirm, window, cx);
    });
    cx.run_until_parked();
    panel.read_with(&cx, |panel, _| {
        let anchor = panel
            .composer_anchor()
            .expect("canonical Comment context action");
        assert_eq!(anchor.side, DiffSide::Left);
        assert_eq!(anchor.path, "auth.ts");
        assert_eq!(anchor.line, Some(1));
    });
    view.read_with(&cx, |view, _| {
        assert!(
            !view.github_open,
            "native right-click uses the same inline composer without opening the panel"
        );
        assert!(
            !view.github_blocks.is_empty(),
            "the comment composer is placed inside the immutable diff"
        );
    });
}

#[gpui::test]
async fn scrolling_navigation_and_layout_reuse_prepared_editor_without_git_objects(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_fixture".into(),
                pull_request_node_id: "PR_fixture".into(),
                viewer_node_id: "U_fixture".into(),
            },
        };
        view.data.session.presentation.multi_file = Some(true);
        view.load_selected(window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let (editor_id, prepared_ids) = view.read_with(&cx, |view, _| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(view.multi_loaded);
        assert_eq!(
            view.prepared_files.len(),
            2,
            "scrolling prepares and caches every immutable file"
        );
        (
            view.editor.as_ref().unwrap().entity_id(),
            view.prepared_files
                .iter()
                .map(|entry| entry.buffer.entity_id())
                .collect::<BTreeSet<_>>(),
        )
    });
    // Any object probe/load after this point would fail. Cached navigation and
    // native layout changes must not depend on objects being available again.
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), false, |state| {
            state.oids.clear()
        })
        .unwrap();
    view.update_in(&mut cx, |view, window, cx| {
        view.select("billing.ts".into(), window, cx);
        assert!(
            !view.busy,
            "an existing scrolling excerpt is immediately available"
        );
        assert!(view.multi_loaded);
        assert_eq!(view.editor.as_ref().unwrap().entity_id(), editor_id);
        let editor = view.editor.as_ref().unwrap().read(cx).rhs_editor().clone();
        let target = editor
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .find(|buffer| buffer.read(cx).file().unwrap().path().to_string() == "billing.ts")
            .unwrap();
        let selection = editor.read(cx).selections.newest_anchor().head();
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        assert_eq!(
            snapshot
                .anchor_to_buffer_anchor(selection)
                .expect("selected excerpt")
                .0
                .buffer_id,
            target.read(cx).remote_id()
        );
        let mut presentation = view.data.session.presentation.clone();
        presentation.split_view = Some(
            !view
                .editor
                .as_ref()
                .unwrap()
                .read(cx)
                .diff_view_style()
                .eq(&DiffViewStyle::Split),
        );
        view.change_presentation(presentation, window, cx);
        assert!(!view.busy);
        assert_eq!(view.editor.as_ref().unwrap().entity_id(), editor_id);
    });
    cx.condition(&view, |view, _| !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.load_selected(window, cx);
        assert!(
            view.multi_loaded,
            "existing scrolling layout remains reported during preparation"
        );
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.read_with(&cx, |view, _| {
        assert!(
            view.error.is_none(),
            "cached scrolling preparation must not read Git: {:?}",
            view.error
        );
        assert!(view.multi_loaded);
        assert_eq!(
            view.prepared_files
                .iter()
                .map(|entry| entry.buffer.entity_id())
                .collect::<BTreeSet<_>>(),
            prepared_ids
        );
        assert_eq!(view.prepared_files.len(), 2);
        assert!(view.prepared_bytes <= 64 * 1024 * 1024);
    });
}

#[gpui::test]
async fn viewed_work_owner_finishes_after_reload_adopts_retention_revision(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.unwrap();
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    let start_revision = view.read_with(&cx, |view, _| view.data.session.revision);
    view.update_in(&mut cx, |view, window, cx| {
        view.begin_viewed_work("Retaining review history…", cx);
        let task = cx.background_spawn(async move {
            gate.await.unwrap();
            Err(anyhow::anyhow!("Bounded retention completion"))
        });
        view.finish_viewed_work(task, view.data.session.clone(), window, cx);
    });
    // Real checkpoint persistence advances the same session while its owner is
    // still retaining. Reload is the actual UI path which adopts that revision.
    let comparison = fixture.session.snapshot.file(GROUP, "auth.ts").unwrap();
    let revision = fixture
        .db
        .mark_viewed(
            ViewedTarget {
                session: fixture.session.id.clone(),
                expected_revision: start_revision,
                displayed_generation: fixture.session.snapshot.generation,
                group: GROUP.into(),
                path: "auth.ts".into(),
                fingerprint: comparison.fingerprint().unwrap(),
            },
            RetainedContents {
                old: Some(b"base auth\n".to_vec()),
                new: Some(b"reviewed auth\n".to_vec()),
            },
            1024,
        )
        .await
        .unwrap();
    view.update_in(&mut cx, |view, window, cx| view.retry(window, cx));
    cx.condition(&view, |view, _| {
        !view.busy && view.data.session.revision == revision
    })
    .await;
    view.read_with(&cx, |view, _| assert!(view.github_viewed_busy));
    release.send(()).unwrap();
    cx.condition(&view, |view, _| !view.github_viewed_busy)
        .await;
    view.read_with(&cx, |view, _| {
        assert!(view.data.session.revision > start_revision);
        assert!(view.data.viewed.contains("auth.ts"));
        assert!(view.github_viewed_guard.is_none());
        assert!(view.github_viewed_progress_task.is_none());
        assert!(
            view.github_viewed_error
                .as_deref()
                .unwrap()
                .contains("Bounded retention completion")
        );
    });
}

#[gpui::test]
async fn older_viewed_completion_cannot_clear_new_work_owner(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.unwrap();
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let (old_release, old_gate) = futures::channel::oneshot::channel::<()>();
    let (new_release, new_gate) = futures::channel::oneshot::channel::<()>();
    view.update_in(&mut cx, |view, window, cx| {
        view.begin_viewed_work("Old retention", cx);
        let old_task = cx.background_spawn(async move {
            old_gate.await.unwrap();
            Err(anyhow::anyhow!("old completion"))
        });
        view.finish_viewed_work(old_task, view.data.session.clone(), window, cx);
        view.begin_viewed_work("New local intent owns sync", cx);
        let new_task = cx.background_spawn(async move {
            new_gate.await.unwrap();
            Err(anyhow::anyhow!("new completion"))
        });
        view.finish_viewed_work(new_task, view.data.session.clone(), window, cx);
    });
    old_release.send(()).unwrap();
    cx.run_until_parked();
    view.read_with(&cx, |view, _| {
        assert!(
            view.github_viewed_busy,
            "The old task cannot clear the newer owner's spinner"
        );
        assert_eq!(
            view.github_viewed_status.as_deref(),
            Some("New local intent owns sync")
        );
        assert!(view.github_viewed_error.is_none());
    });
    new_release.send(()).unwrap();
    cx.condition(&view, |view, _| !view.github_viewed_busy)
        .await;
    view.read_with(&cx, |view, _| {
        assert!(
            view.github_viewed_error
                .as_deref()
                .unwrap()
                .contains("new completion")
        )
    });
}

#[gpui::test]
async fn preparing_file_preserves_rendered_diff_and_tree_geometry_at_supported_widths(
    cx: &mut TestAppContext,
) {
    use gpui::VisualContext as _;
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_fixture".into(),
                pull_request_node_id: "PR_fixture".into(),
                viewer_node_id: "U_fixture".into(),
            },
        };
        view.data.session.presentation.multi_file = Some(true);
        view.load_selected(window, cx);
    });
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update(&mut cx, |view, cx| {
        assert!(view.multi_loaded);
        view.data.pull_request = Some(PullRequestMetadata {
            owner: "example".into(),
            repository: "review-fixture".into(),
            number: 7,
            title: "Generic rendering geometry".into(),
            base_ref_name: "main".into(),
            head_ref_name: "feature".into(),
            base_ref_oid: "1".repeat(40),
            head_ref_oid: "2".repeat(40),
            commit_count: Some(2),
        });
        cx.notify();
    });
    for font_size in [14., 20.] {
        view.update(&mut cx, |_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(font_size.into())
                });
            })
        });
        for width in [1050., 1450.] {
            cx.simulate_resize(gpui::size(px(width), px(900.)));
            cx.run_until_parked();
            let diff_before = cx.debug_bounds("REVIEW_DIFF_CONTENT").expect("idle diff");
            let tree_before = cx.debug_bounds("REVIEW_TREE_PANEL").expect("idle tree");
            let editor_before =
                view.read_with(&cx, |view, _| view.editor.as_ref().unwrap().entity_id());
            view.update(&mut cx, |view, cx| {
                view.busy = true;
                cx.notify();
            });
            cx.run_until_parked();
            assert_eq!(
                cx.debug_bounds("REVIEW_DIFF_CONTENT")
                    .expect("preparing diff"),
                diff_before,
                "preparing feedback must not move or resize the diff at width {width}, font {font_size}"
            );
            assert_eq!(
                cx.debug_bounds("REVIEW_TREE_PANEL")
                    .expect("preparing tree"),
                tree_before,
                "preparing feedback must not move or resize the tree at width {width}, font {font_size}"
            );
            view.read_with(&cx, |view, _| {
                assert_eq!(view.editor.as_ref().unwrap().entity_id(), editor_before);
                assert!(
                    view.multi_loaded,
                    "the visible scrolling layout remains selected while preparing"
                );
            });
            view.update(&mut cx, |view, cx| {
                view.busy = false;
                cx.notify();
            });
            cx.run_until_parked();
            assert_eq!(
                cx.debug_bounds("REVIEW_DIFF_CONTENT").unwrap(),
                diff_before,
                "completion feedback must not move the diff at width {width}, font {font_size}"
            );
        }
    }
    // A real external project dock leaves only 810px for the review at
    // enlarged font size. Count/status/Refresh must wrap as one right-aligned
    // group rather than scatter across independently wrapped toolbar rows.
    let workspace = view.read_with(&cx, |view, _| view.workspace.upgrade().unwrap());
    let review = view.clone();
    cx.replace_root_view(|_, _| DockedReviewLayoutHarness {
        view: review,
        _workspace: workspace,
    });
    cx.simulate_resize(gpui::size(px(1050.), px(900.)));
    cx.run_until_parked();
    let body = cx
        .debug_bounds("REVIEW_FILES_LAYOUT")
        .expect("docked review viewport");
    let diff_before = cx.debug_bounds("REVIEW_DIFF_CONTENT").expect("docked diff");
    let tree_before = cx.debug_bounds("REVIEW_TREE_PANEL").expect("docked tree");
    let group_before = cx
        .debug_bounds("REVIEW_REFRESH_GROUP")
        .expect("one refresh group");
    let refresh_before = cx
        .debug_bounds("REVIEW_REFRESH_ACTION")
        .expect("refresh action");
    assert!(
        group_before.left() >= body.left() && group_before.right() <= body.right(),
        "the composite status/actions must fit within the review viewport: {group_before:?}, {body:?}"
    );
    assert!(
        body.right() - refresh_before.right() <= px(24.),
        "Refresh must remain at the review viewport's right edge: {refresh_before:?}, {body:?}"
    );
    assert_eq!(
        refresh_before.right(),
        group_before.right(),
        "Refresh closes the right-aligned composite"
    );
    for busy in [true, false] {
        view.update(&mut cx, |view, cx| {
            view.busy = busy;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(cx.debug_bounds("REVIEW_DIFF_CONTENT").unwrap(), diff_before);
        assert_eq!(cx.debug_bounds("REVIEW_TREE_PANEL").unwrap(), tree_before);
        assert_eq!(
            cx.debug_bounds("REVIEW_REFRESH_GROUP").unwrap(),
            group_before
        );
        assert_eq!(
            cx.debug_bounds("REVIEW_REFRESH_ACTION").unwrap(),
            refresh_before
        );
    }
}

#[gpui::test]
async fn inline_comment_commands_do_not_open_the_optional_threads_panel(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.github_panel = Some(cx.new(|cx| GitHubReviewPanel::preview(window, cx)));
        view.github_open = false;
    });
    cx.focus(&view);
    // An absent composer is rejected locally by the preview panel. These real
    // action routes must not change the user's explicit panel visibility.
    cx.dispatch_action(PostGitHubCommentNow);
    cx.run_until_parked();
    view.read_with(&cx, |view, _| assert!(!view.github_open));
    cx.dispatch_action(SaveGitHubDraft);
    cx.run_until_parked();
    view.read_with(&cx, |view, _| assert!(!view.github_open));
}

#[test]
fn review_file_windows_cover_small_prs_and_bound_large_prs() {
    assert_eq!(review_file_window(0, 0), 0..0);
    assert_eq!(review_file_window(49, 24), 0..49);
    assert_eq!(review_file_window(50, 0), 0..6);
    assert_eq!(review_file_window(50, 25), 20..31);
    assert_eq!(review_file_window(50, 49), 44..50);
    assert_eq!(review_file_window(2_000, 1_000), 995..1006);
    // Walking adjacent centers never drops the reading file and never skips
    // a boundary when moving forward or backward through a large comparison.
    for center in 0usize..2_000 {
        let range = review_file_window(2_000, center);
        assert!(range.contains(&center));
        assert!(range.len() <= 11);
        if center > 0 {
            let previous = review_file_window(2_000, center - 1);
            assert!(range.start < previous.end);
        }
    }
}

#[gpui::test]
async fn viewed_count_uses_pending_marks_and_unmarks_without_server_ack(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update(&mut cx, |view, _| {
        view.data.viewed.insert("auth.ts".into());
        // A server flag may remain VIEWED after the exact local contents change.
        // Keep that flag visible in progress without approving changed bytes.
        view.data.github_viewed = Some(BTreeSet::from(["auth.ts".into(), "billing.ts".into()]));
        assert_eq!(
            view.effective_data().github_viewed.as_ref().unwrap().len(),
            2
        );
        assert_eq!(view.effective_data().viewed.len(), 1);
        view.github_viewed_intents.insert("auth.ts".into(), false);
        view.github_viewed_intents.insert("billing.ts".into(), true);
        let effective = view.effective_data();
        assert_eq!(effective.viewed, BTreeSet::from(["billing.ts".into()]));
        assert_eq!(effective.viewed.len(), 1);
        assert_eq!(
            effective.github_viewed,
            Some(BTreeSet::from(["billing.ts".into()]))
        );
        assert!(!view.effective_github_viewed("auth.ts"));
        assert!(view.effective_github_viewed("billing.ts"));
        assert_eq!(
            view.data.viewed,
            BTreeSet::from(["auth.ts".into()]),
            "optimistic intent does not claim server acknowledgement"
        );
        view.github_viewed_intents.insert("auth.ts".into(), true);
        assert_eq!(view.effective_data().viewed.len(), 2);
    });
}

#[gpui::test]
async fn large_pr_window_reuses_native_editor_and_overlapping_excerpts(cx: &mut TestAppContext) {
    assert_large_pr_window_collapse_after_selection_eviction(cx, false).await;
}

#[gpui::test]
async fn large_pr_split_window_collapse_after_selection_eviction(cx: &mut TestAppContext) {
    assert_large_pr_window_collapse_after_selection_eviction(cx, true).await;
}

async fn assert_large_pr_window_collapse_after_selection_eviction(
    cx: &mut TestAppContext,
    split: bool,
) {
    let fixture = fixture(cx).await.expect("fixture");
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.editor.minimum_split_diff_width = Some(0.);
            })
        })
    });
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let files = (0..50)
        .map(|index| {
            comparison(
                &format!("file-{index:03}.ts"),
                b"base\n",
                format!("change {index}\n").as_bytes(),
            )
        })
        .collect::<Vec<_>>();
    fixture
        .fs
        .with_git_state(&fixture.root.join(".git"), true, |state| {
            for (index, file) in files.iter().enumerate() {
                state.oids.insert(
                    file.old.as_ref().unwrap().object.hex.parse().unwrap(),
                    b"base\n".to_vec(),
                );
                state.oids.insert(
                    file.new.as_ref().unwrap().object.hex.parse().unwrap(),
                    format!("change {index}\n").into_bytes(),
                );
            }
        })
        .expect("fake Git objects");
    view.update_in(&mut cx, |view, window, cx| {
        view.data.session.selector = ComparisonSelector::PullRequest {
            identity: PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: "R_fixture".into(),
                pull_request_node_id: "PR_fixture".into(),
                viewer_node_id: "U_fixture".into(),
            },
        };
        view.data.files = files
            .iter()
            .map(|file| file.file_key().unwrap().to_owned())
            .collect();
        view.data.session.snapshot.groups[0].files = files;
        view.data.session.presentation.selected_path = Some("file-025.ts".into());
        view.data.session.presentation.split_view = Some(split);
        view.data.session.presentation.multi_file = None;
        view.load_selected(window, cx);
    });
    cx.condition(&view, |view, _| !view.busy).await;
    let (editor_id, overlap) = view.read_with(&cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        assert!(view.multi_loaded);
        let editor = view.editor.as_ref().unwrap();
        let paths = editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .map(|buffer| {
                (
                    buffer.read(cx).file().unwrap().path().to_string(),
                    buffer.entity_id(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(paths.len(), 11);
        assert!(paths.contains_key("file-020.ts") && paths.contains_key("file-030.ts"));
        (editor.entity_id(), paths)
    });
    // Reading/scrolling can leave either native editor's cursor in the
    // outgoing edge excerpt. The rolling update must repair both sides.
    view.update_in(&mut cx, |view, window, cx| {
        let split_editor = view.editor.as_ref().unwrap().read(cx);
        let mut editors = vec![split_editor.rhs_editor().clone()];
        if split {
            editors.push(
                split_editor
                    .lhs_editor()
                    .expect("actual split editor")
                    .clone(),
            );
        }
        for editor in editors {
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            let outgoing = editor
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers_iter()
                .find(|buffer| {
                    snapshot
                        .path_for_buffer(buffer.read(cx).remote_id())
                        .is_some_and(|key| key.path.to_string() == "file-020.ts")
                })
                .expect("outgoing edge file");
            let anchor = snapshot
                .anchor_in_excerpt(
                    outgoing
                        .read(cx)
                        .snapshot()
                        .anchor_before(language::Point::zero()),
                )
                .expect("outgoing excerpt anchor");
            editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([anchor..anchor]);
                });
            });
            assert_eq!(
                snapshot
                    .anchor_to_buffer_anchor(editor.read(cx).selections.newest_anchor().head())
                    .expect("cursor starts in outgoing file")
                    .0
                    .buffer_id,
                outgoing.read(cx).remote_id()
            );
        }
    });
    view.update_in(&mut cx, |view, window, cx| {
        view.prepared_files.clear();
        view.prepared_bytes = 0;
        view.data.session.presentation.selected_path = Some("file-026.ts".into());
        view.rolling_scroll = true;
        view.load_selected(window, cx);
    });
    cx.condition(&view, |view, _| !view.busy).await;
    view.read_with(&cx, |view, cx| {
        assert!(view.error.is_none(), "{:?}", view.error);
        let editor = view.editor.as_ref().unwrap();
        assert_eq!(
            editor.entity_id(),
            editor_id,
            "scrolling retains the native editor"
        );
        let paths = editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .map(|buffer| {
                (
                    buffer.read(cx).file().unwrap().path().to_string(),
                    buffer.entity_id(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(paths.len(), 11);
        assert!(!paths.contains_key("file-020.ts") && paths.contains_key("file-031.ts"));
        for (path, buffer_id) in overlap.iter().filter(|(path, _)| paths.contains_key(*path)) {
            assert_eq!(
                paths.get(path),
                Some(buffer_id),
                "overlapping immutable buffer retained"
            );
        }
        let actual_buffers = editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers_iter()
            .map(|buffer| {
                (
                    buffer.read(cx).file().unwrap().path().to_string(),
                    buffer.read(cx).remote_id(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for canonical in view
            .github_buffers
            .iter()
            .filter(|buffer| buffer.side == crate::github_review::DiffSide::Right)
        {
            assert_eq!(
                actual_buffers.get(&canonical.path),
                Some(&canonical.buffer_id),
                "inline anchors use the actual retained native buffer after LRU eviction"
            );
        }
        assert!(view.prepared_bytes <= 64 * 1024 * 1024);
    });
    // Exercise the same native fold action as the file-header collapse button,
    // rather than merely checking that the rolling window has eleven buffers.
    // Before the repair fold_buffer panics while resolving the evicted cursor.
    view.update_in(&mut cx, |view, _, cx| {
        let split_editor = view.editor.as_ref().unwrap().read(cx);
        let mut editors = vec![split_editor.rhs_editor().clone()];
        if split {
            editors.push(split_editor.lhs_editor().expect("actual split editor").clone());
        }
        for editor in editors {
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            let retained_id = editor.read(cx).buffer().read(cx).all_buffers_iter()
                .find(|buffer| snapshot.path_for_buffer(buffer.read(cx).remote_id())
                    .is_some_and(|key| key.path.to_string() == "file-026.ts"))
                .expect("retained reading file").read(cx).remote_id();
            editor.update(cx, |editor, cx| {
                for _ in 0..3 {
                    editor.fold_buffer(retained_id, cx);
                    assert!(editor.is_buffer_folded(retained_id, cx));
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    for selection in editor.selections.disjoint_anchors() {
                        assert!(snapshot.can_resolve(&selection.start) && snapshot.can_resolve(&selection.end),
                            "collapse must preserve resolvable selection anchors after rolling eviction");
                    }
                    editor.unfold_buffer(retained_id, cx);
                    assert!(!editor.is_buffer_folded(retained_id, cx));
                }
            });
        }
    });
    cx.run_until_parked();
    // Reverse direction, jump to a disjoint window, then advance with a cursor
    // in a retained file. Refresh must repair evicted anchors without moving a
    // still-valid cursor, on both sides of a split diff.
    for (center, cursor_path, preserve_cursor) in [
        (25, "file-031.ts", false),
        (40, "file-020.ts", false),
        (41, "file-040.ts", true),
    ] {
        let cursors = view.update_in(&mut cx, |view, window, cx| {
            let split_editor = view.editor.as_ref().unwrap().read(cx);
            let mut editors = vec![split_editor.rhs_editor().clone()];
            if split {
                editors.push(
                    split_editor
                        .lhs_editor()
                        .expect("actual split editor")
                        .clone(),
                );
            }
            let mut cursors = Vec::new();
            for editor in editors {
                let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
                let buffer = editor
                    .read(cx)
                    .buffer()
                    .read(cx)
                    .all_buffers_iter()
                    .find(|buffer| {
                        snapshot
                            .path_for_buffer(buffer.read(cx).remote_id())
                            .is_some_and(|key| key.path.to_string() == cursor_path)
                    })
                    .expect("cursor file in preceding window");
                let anchor = snapshot
                    .anchor_in_excerpt(
                        buffer
                            .read(cx)
                            .snapshot()
                            .anchor_before(language::Point::zero()),
                    )
                    .expect("cursor excerpt");
                editor.update(cx, |editor, cx| {
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_ranges([anchor..anchor]);
                    })
                });
                let (text_anchor, buffer_snapshot) = snapshot
                    .anchor_to_buffer_anchor(anchor)
                    .expect("cursor maps to immutable source buffer");
                cursors.push((
                    editor,
                    (
                        text_anchor.buffer_id,
                        buffer_snapshot.offset_for_anchor(&text_anchor),
                    ),
                ));
            }
            view.data.session.presentation.selected_path = Some(format!("file-{center:03}.ts"));
            view.rolling_scroll = true;
            view.load_selected(window, cx);
            cursors
        });
        cx.condition(&view, |view, _| !view.busy).await;
        view.read_with(&cx, |view, _| {
            assert!(view.error.is_none(), "{:?}", view.error);
            assert_eq!(view.editor.as_ref().unwrap().entity_id(), editor_id);
            assert!(view.prepared_bytes <= 64 * 1024 * 1024);
        });
        for (editor, previous_position) in cursors {
            editor.update(&mut cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                assert_eq!(editor.buffer().read(cx).all_buffers_iter().count(), 11);
                for selection in editor.selections.disjoint_anchors() {
                    assert!(snapshot.can_resolve(&selection.start) && snapshot.can_resolve(&selection.end),
                        "both native editor selections resolve after window movement to {center}");
                }
                if preserve_cursor {
                    let (text_anchor, buffer_snapshot) = snapshot.anchor_to_buffer_anchor(editor.selections.newest_anchor().head())
                        .expect("retained cursor maps to immutable source buffer");
                    assert_eq!((text_anchor.buffer_id, buffer_snapshot.offset_for_anchor(&text_anchor)), previous_position,
                        "retained excerpt cursor must preserve source buffer and byte position during scrolling");
                }
                let retained_id = editor.buffer().read(cx).all_buffers_iter()
                    .find(|buffer| snapshot.path_for_buffer(buffer.read(cx).remote_id())
                        .is_some_and(|key| key.path.to_string() == format!("file-{center:03}.ts")))
                    .expect("new reading file").read(cx).remote_id();
                editor.fold_buffer(retained_id, cx);
                assert!(editor.is_buffer_folded(retained_id, cx));
                editor.unfold_buffer(retained_id, cx);
                assert!(!editor.is_buffer_folded(retained_id, cx));
            });
        }
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn rapid_viewed_clicks_survive_file_navigation_while_retention_is_active(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        // Pin the local acceptance stage, then interleave navigation/preparation.
        view.github_viewed_preparing = true;
        view.busy = true;
        view.set_github_paths_viewed(vec!["auth.ts".into()], true, None, window, cx);
        view.set_github_paths_viewed(vec!["billing.ts".into()], true, None, window, cx);
        view.set_github_paths_viewed(vec!["auth.ts".into()], false, None, window, cx);
        assert_eq!(
            view.github_viewed_pending_clicks,
            BTreeMap::from([("auth.ts".into(), false), ("billing.ts".into(), true)])
        );
        assert!(!view.effective_github_viewed("auth.ts"));
        assert!(view.effective_github_viewed("billing.ts"));
        // Invalid/missing PR context cannot consume a queued action either.
        view.github_viewed_preparing = false;
        view.set_github_paths_viewed(vec!["billing.ts".into()], true, None, window, cx);
        assert_eq!(
            view.github_viewed_pending_clicks.get("billing.ts"),
            Some(&true)
        );
        assert!(view.github_viewed_error.is_some());
        view.refresh(window, cx);
        assert!(
            !view.refreshing,
            "refresh cannot swap snapshots beneath unsaved clicks"
        );
        assert_eq!(view.github_viewed_pending_clicks.len(), 2);
        view.busy = false;
    });
}

#[gpui::test]
async fn threads_panel_native_drag_changes_width_without_scroll_clip(cx: &mut TestAppContext) {
    let fixture = fixture(cx).await.expect("fixture");
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    view.update_in(&mut cx, |view, window, cx| {
        view.github_panel = Some(cx.new(|cx| GitHubReviewPanel::preview(window, cx)));
        view.github_open = true;
        cx.notify();
    });
    cx.simulate_resize(gpui::size(px(1450.), px(900.)));
    cx.run_until_parked();
    let sidebar = cx
        .debug_bounds("REVIEW_GITHUB_SIDEBAR")
        .expect("thread sidebar");
    let handle = cx
        .debug_bounds("REVIEW_THREADS_RESIZE_HANDLE")
        .expect("native resize handle");
    assert!(handle.size.height > px(100.) && handle.size.width >= px(12.));
    assert!(
        handle.left() >= sidebar.left() && handle.right() <= sidebar.right(),
        "the entire grip is inside the unclipped panel boundary"
    );
    let start = handle.center();
    cx.simulate_mouse_move(start, None, gpui::Modifiers::none());
    cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::none());
    cx.simulate_mouse_move(
        gpui::point(start.x - px(12.), start.y),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    assert!(
        cx.update(|_, cx| cx.has_active_drag()),
        "the scrollable thread content must not intercept the resize grip"
    );
    cx.simulate_mouse_move(
        gpui::point(start.x - px(100.), start.y),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.simulate_mouse_up(
        gpui::point(start.x - px(100.), start.y),
        gpui::MouseButton::Left,
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();
    let resized = cx
        .debug_bounds("REVIEW_GITHUB_SIDEBAR")
        .expect("resized sidebar");
    assert!(
        resized.size.width > sidebar.size.width + px(60.),
        "drag must visibly widen the actual sidebar: {sidebar:?} -> {resized:?}"
    );
    let diff = cx.debug_bounds("REVIEW_DIFF_CONTENT").expect("diff");
    assert!(diff.size.width > px(360.));
}

#[gpui::test]
async fn github_progress_is_available_before_retaining_reviewed_contents(cx: &mut TestAppContext) {
    use crate::github_review::{RemoteViewedFile, ReviewContext, ViewedState};
    use crate::review_store::GitHubViewedSyncTarget;
    let fixture = fixture(cx).await.unwrap();
    let snapshot = fixture.session.snapshot.clone();
    let identity = PullRequestIdentity {
        host: "github.com".into(),
        repository_node_id: "R_example".into(),
        pull_request_node_id: "PR_example".into(),
        viewer_node_id: "U_example".into(),
    };
    let metadata = PullRequestMetadata {
        owner: "example".into(),
        repository: "repository".into(),
        number: 1,
        title: "Example".into(),
        base_ref_name: "main".into(),
        head_ref_name: "feature".into(),
        base_ref_oid: snapshot.groups[0].endpoints.left.hex.clone(),
        head_ref_oid: snapshot.groups[0].endpoints.right.hex.clone(),
        commit_count: Some(1),
    };
    let session = fixture
        .db
        .deploy_pull_request(
            fixture.session.repository.id.clone(),
            identity.clone(),
            metadata.clone(),
            snapshot,
            None,
        )
        .await
        .unwrap();
    let before = read_data(&fixture.db, session.clone()).unwrap();
    assert!(before.github_viewed.is_none());
    let context = ReviewContext {
        reference: PullRequestReference::parse(&metadata.url(), None).unwrap(),
        identity,
        head_oid: metadata.head_ref_oid,
        base_tip_oid: metadata.base_ref_oid,
    };
    let remote = session.snapshot.groups[0]
        .files
        .iter()
        .map(|file| RemoteViewedFile {
            path: file.file_key().unwrap().into(),
            viewer_viewed_state: ViewedState::Viewed,
        })
        .collect();
    fixture
        .db
        .sync_github_viewed(
            GitHubViewedSyncTarget {
                session: session.id.clone(),
                expected_revision: session.revision,
                displayed_generation: session.snapshot.generation,
            },
            context,
            remote,
            BTreeSet::new(),
        )
        .await
        .unwrap();
    let current = fixture.db.load_session(&session.id).unwrap();
    let revision = current.revision;
    let data = read_data(&fixture.db, current).unwrap();
    assert_eq!(data.github_viewed.as_ref().unwrap().len(), 2);
    // No retained blob was needed to expose the fetched GitHub count.
    for path in &data.files {
        assert!(
            fixture
                .db
                .checkpoint(&session.id, GROUP, path)
                .unwrap()
                .is_none()
        );
    }
    let (view, mut cx) = review_window(&fixture, cx, None);
    cx.condition(&view, |view, _| !view.busy && !view.navigation_pending)
        .await;
    let editor = view.read_with(&cx, |view, _| view.editor.as_ref().unwrap().entity_id());
    let presentation = before.session.presentation.clone();
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    view.update_in(&mut cx, |view, window, cx| {
        view.data = before;
        view.observed_revision = view.data.session.revision;
        view.github_viewed_initial_pending = false;
        view.begin_viewed_work("Reading GitHub", cx);
        view.github_viewed_retention_pause = Some(cx.background_spawn(async move {
            gate.await.unwrap();
        }));
        let task = cx.background_spawn(async move {
            Ok(github_viewed::ViewedWork::observed_for_test(revision))
        });
        view.finish_viewed_work(task, view.data.session.clone(), window, cx);
    });
    cx.condition(&view, |view, _| {
        view.data
            .github_viewed
            .as_ref()
            .is_some_and(|paths| paths.len() == 2)
    })
    .await;
    view.read_with(&cx, |view, _| {
        assert!(view.github_viewed_busy, "History retention is still paused");
        assert_eq!(view.editor.as_ref().unwrap().entity_id(), editor);
        assert_eq!(view.data.session.presentation, presentation);
        assert_eq!(
            view.github_viewed_status.as_deref(),
            Some("Viewed flags confirmed · Saving review progress…")
        );
    });
    release.send(()).unwrap();
    cx.condition(&view, |view, _| !view.github_viewed_busy)
        .await;

    // A later PR revision removes a path. Its historical observation remains
    // useful for retained review history but must not invalidate current totals.
    let current = fixture.db.load_session(&session.id).unwrap();
    let file = current.snapshot.file(GROUP, "auth.ts").unwrap();
    fixture
        .db
        .mark_viewed(
            ViewedTarget {
                session: current.id.clone(),
                expected_revision: current.revision,
                displayed_generation: current.snapshot.generation,
                group: GROUP.into(),
                path: "auth.ts".into(),
                fingerprint: file.fingerprint().unwrap(),
            },
            RetainedContents {
                old: Some(b"base auth\n".to_vec()),
                new: Some(b"reviewed auth\n".to_vec()),
            },
            1024,
        )
        .await
        .unwrap();
    let current = fixture.db.load_session(&session.id).unwrap();
    let identity = current.selector.pull_request_identity().unwrap().clone();
    let mut metadata = fixture
        .db
        .pull_request_metadata(&current.id)
        .unwrap()
        .unwrap();
    metadata.head_ref_oid = "4".repeat(40);
    let mut snapshot = current.snapshot.clone();
    snapshot.groups[0].endpoints.right.hex = metadata.head_ref_oid.clone();
    snapshot.groups[0]
        .files
        .retain(|file| file.file_key().unwrap() == "auth.ts");
    snapshot.groups[0].files[0].new.as_mut().unwrap().object =
        ObjectId::for_blob(ObjectFormat::Sha1, b"changed auth\n");
    let revised = fixture
        .db
        .deploy_pull_request(
            current.repository.id.clone(),
            identity.clone(),
            metadata.clone(),
            snapshot,
            Some(current.revision),
        )
        .await
        .unwrap();
    fixture
        .db
        .sync_github_viewed(
            GitHubViewedSyncTarget {
                session: revised.id.clone(),
                expected_revision: revised.revision,
                displayed_generation: revised.snapshot.generation,
            },
            ReviewContext {
                reference: PullRequestReference::parse(&metadata.url(), None).unwrap(),
                identity,
                head_oid: metadata.head_ref_oid,
                base_tip_oid: metadata.base_ref_oid,
            },
            vec![RemoteViewedFile {
                path: "auth.ts".into(),
                viewer_viewed_state: ViewedState::Viewed,
            }],
            BTreeSet::new(),
        )
        .await
        .unwrap();
    let revised = fixture.db.load_session(&revised.id).unwrap();
    assert_eq!(
        fixture
            .db
            .github_viewed_observations(&revised.id, GROUP)
            .unwrap()
            .len(),
        2,
        "Removed path history is preserved"
    );
    let data = read_data(&fixture.db, revised.clone()).unwrap();
    assert_eq!(data.files.len(), 1);
    assert_eq!(
        data.github_viewed,
        Some(BTreeSet::from(["auth.ts".into()])),
        "Only current paths determine completeness and the GitHub total"
    );
    assert_eq!(
        data.viewed,
        data.github_viewed.clone().unwrap(),
        "Viewed UI and GitHub total must use identical states"
    );
    assert_eq!(data.group_progress[0].0, data.viewed.len());
    assert!(
        data.checkpoint_viewed.is_empty(),
        "A stale retained checkpoint must not approve changed contents"
    );
    assert!(data.changed.contains("auth.ts"));
    let mut unreviewed = data.clone();
    unreviewed.session.presentation.unreviewed_only = true;
    assert!(
        paths(&unreviewed).is_empty(),
        "GitHub Viewed files must not reappear as unchecked Unreviewed entries"
    );
    view.update(&mut cx, |view, _| {
        view.data = data;
        view.github_viewed_intents.clear();
        assert!(
            view.effective_github_viewed("auth.ts"),
            "File checkbox must match server Viewed flag even when retained history is stale"
        );
        assert_eq!(view.effective_data().viewed.len(), 1);
        // A local unmark updates checkbox/filter/count before network receipt.
        view.github_viewed_accepting_clicks
            .insert("auth.ts".into(), false);
        assert!(!view.effective_github_viewed("auth.ts"));
        assert_eq!(view.effective_data().viewed.len(), 0);
        assert_eq!(
            view.effective_data().github_viewed.as_ref().unwrap().len(),
            0
        );
        // Latest pending toggle wins without mutating confirmed data.
        view.github_viewed_pending_clicks
            .insert("auth.ts".into(), true);
        assert!(view.effective_github_viewed("auth.ts"));
        assert_eq!(view.effective_data().viewed.len(), 1);
        assert_eq!(view.data.viewed.len(), 1);
    });
    let current = fixture.db.load_session(&revised.id).unwrap();
    let metadata = fixture
        .db
        .pull_request_metadata(&current.id)
        .unwrap()
        .unwrap();
    fixture
        .db
        .sync_github_viewed(
            GitHubViewedSyncTarget {
                session: current.id.clone(),
                expected_revision: current.revision,
                displayed_generation: current.snapshot.generation,
            },
            ReviewContext {
                reference: PullRequestReference::parse(&metadata.url(), None).unwrap(),
                identity: current.selector.pull_request_identity().unwrap().clone(),
                head_oid: metadata.head_ref_oid,
                base_tip_oid: metadata.base_ref_oid,
            },
            vec![RemoteViewedFile {
                path: "auth.ts".into(),
                viewer_viewed_state: ViewedState::Dismissed,
            }],
            BTreeSet::new(),
        )
        .await
        .unwrap();
    let mut dismissed =
        read_data(&fixture.db, fixture.db.load_session(&current.id).unwrap()).unwrap();
    assert!(
        dismissed.viewed.is_empty(),
        "GitHub dismissal must clear the same UI count and checkbox"
    );
    dismissed.session.presentation.unreviewed_only = true;
    assert_eq!(
        paths(&dismissed),
        vec!["auth.ts"],
        "Changed/dismissed contents return to Unreviewed"
    );
    assert!(
        fixture
            .db
            .checkpoint(&current.id, GROUP, "auth.ts")
            .unwrap()
            .is_some(),
        "History remains available after dismissal"
    );
}

#[test]
#[ignore = "requires an isolated supplied review database"]
fn review_progress_from_supplied_database() -> Result<()> {
    // The caller must supply a private SQLite backup, never a running profile.
    // Keep real repository data outside source and printed test output.
    let config: serde_json::Value = serde_json::from_slice(&fs::read(
        std::env::var("ZED_REVIEW_PROGRESS_ORACLE")
            .context("Supply an isolated database manifest")?,
    )?)?;
    let path = config["database"].as_str().context("database path")?;
    let session = config["session"].as_str().context("session identity")?;
    let connection = futures::executor::block_on(
        ThreadSafeConnection::builder::<ReviewDb>(path, true)
            .with_connection_initialize_query("PRAGMA foreign_keys=ON;")
            .build(),
    )?;
    let db = ReviewDb::from_connection(connection)?;
    if let Some(remote) = config.get("remote_files") {
        let current = db.load_session(session)?;
        let metadata = db.pull_request_metadata(session)?.context("PR metadata")?;
        let identity = current
            .selector
            .pull_request_identity()
            .context("PR identity")?
            .clone();
        futures::executor::block_on(db.sync_github_viewed(
            crate::review_store::GitHubViewedSyncTarget {
                session: current.id.clone(),
                expected_revision: current.revision,
                displayed_generation: current.snapshot.generation,
            },
            crate::github_review::ReviewContext {
                reference: PullRequestReference::parse(&metadata.url(), None)?,
                identity,
                head_oid: metadata.head_ref_oid,
                base_tip_oid: metadata.base_ref_oid,
            },
            serde_json::from_value(remote.clone())?,
            BTreeSet::new(),
        ))?;
    }
    let data = read_data(&db, db.load_session(session)?)?;
    let remote = data
        .github_viewed
        .context("Current GitHub observations are incomplete")?;
    assert_eq!(
        data.files.len() as u64,
        config["expected_total"]
            .as_u64()
            .context("expected total")?
    );
    assert_eq!(
        remote.len() as u64,
        config["expected_viewed"]
            .as_u64()
            .context("expected viewed")?
    );
    println!(
        "Production read_data: {} current files, {} GitHub Viewed flags, {} safe local approvals",
        data.files.len(),
        remote.len(),
        data.checkpoint_viewed.len()
    );
    Ok(())
}
