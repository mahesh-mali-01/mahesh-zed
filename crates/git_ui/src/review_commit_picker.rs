use crate::review_view::{PullRequestReviewContext, ReviewView};
use anyhow::{Context as _, Result, ensure};
use git::review::{ReviewCommit, ReviewCommitSelection};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    ScrollStrategy, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, uniform_list,
};
use project::git_store::Repository;
use std::collections::BTreeSet;
use ui::{Checkbox, TintColor, prelude::*};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

pub(crate) struct ReviewCommitPicker {
    workspace: WeakEntity<Workspace>,
    repository: Entity<Repository>,
    commits: Vec<ReviewCommit>,
    pull_request: Option<PullRequestReviewContext>,
    search_input: Entity<InputField>,
    query: String,
    anchor: Option<usize>,
    scroll: UniformListScrollHandle,
    selected: BTreeSet<usize>,
    parents: Vec<Option<git::Oid>>,
    error: Option<String>,
    focus: FocusHandle,
    _search_subscription: Subscription,
    search_task: Option<Task<()>>,
    preview: bool,
}

impl ReviewCommitPicker {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        repository: Entity<Repository>,
        commits: Vec<ReviewCommit>,
        pull_request: Option<PullRequestReviewContext>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut parents: Vec<_> = commits
            .iter()
            .map(|commit| commit.parents.first().copied())
            .collect();
        let mut selected = BTreeSet::new();
        if let Some(scope) = pull_request
            .as_ref()
            .and_then(|context| context.selection.as_ref())
        {
            for selection in scope.selections() {
                if let Some(index) = commits
                    .iter()
                    .position(|commit| commit.commit.to_string() == selection.commit.hex)
                {
                    selected.insert(index);
                    if let Some(parent) = parents.get_mut(index) {
                        *parent = selection
                            .parent
                            .as_ref()
                            .and_then(|parent| parent.hex.parse().ok());
                    }
                }
            }
        }
        let search_input = cx.new(|cx| {
            InputField::new(window, cx, "Search commit subject or SHA")
                .start_icon(IconName::MagnifyingGlass)
                .clear_button(true)
        });
        let weak = cx.entity().downgrade();
        let editor = search_input.read(cx).editor().clone();
        let subscription = editor.subscribe(
            Box::new(move |event, window, cx| {
                if event == ErasedEditorEvent::BufferEdited {
                    weak.update(cx, |this, cx| this.schedule_search(window, cx))
                        .log_err();
                }
            }),
            window,
            cx,
        );
        Self {
            workspace,
            repository,
            commits,
            pull_request,
            search_input,
            query: String::new(),
            anchor: None,
            scroll: UniformListScrollHandle::new(),
            selected,
            parents,
            error: None,
            focus: cx.focus_handle(),
            _search_subscription: subscription,
            search_task: None,
            preview: false,
        }
    }

    fn selections(&self, range: bool) -> Result<Vec<ReviewCommitSelection>> {
        selected_commits(&self.commits, &self.selected, &self.parents, range)
    }

    fn confirm(&mut self, range: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            cx.emit(DismissEvent);
            return;
        }
        if self.selected.is_empty() {
            if let Some(context) = self.pull_request.clone() {
                let workspace = self.workspace.clone();
                let repository = self.repository.clone();
                cx.emit(DismissEvent);
                window.defer(cx, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            ReviewView::reset_pull_request_scope(
                                workspace, repository, context, window, cx,
                            )
                        })
                        .log_err();
                });
                return;
            }
        }
        match self.selections(range) {
            Ok(selections) => {
                let workspace = self.workspace.clone();
                let repository = self.repository.clone();
                let pull_request = self.pull_request.clone();
                cx.emit(DismissEvent);
                window.defer(cx, move |window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            if let Some(context) = pull_request {
                                ReviewView::deploy_pull_request_commits(
                                    workspace, repository, context, selections, range, window, cx,
                                );
                            } else {
                                ReviewView::deploy_commits(
                                    workspace, repository, selections, range, window, cx,
                                );
                            }
                        })
                        .log_err();
                });
            }
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
            }
        }
    }
}

fn selected_commits(
    commits: &[ReviewCommit],
    selected: &BTreeSet<usize>,
    parents: &[Option<git::Oid>],
    range: bool,
) -> Result<Vec<ReviewCommitSelection>> {
    ensure!(!selected.is_empty(), "Select at least one commit");
    let mut selections = Vec::new();
    for index in selected.iter().rev() {
        let commit = commits.get(*index).context("missing selected commit")?;
        let parent = parents.get(*index).copied().flatten();
        ensure!(
            commit.parents.is_empty() || parent.is_some(),
            "Choose a parent for each selected merge commit"
        );
        if range {
            ensure!(
                commit.parents.len() <= 1,
                "A net range requires a linear chain without merges; use separate groups"
            );
            if let Some(previous) = selections.last() {
                let previous: &ReviewCommitSelection = previous;
                ensure!(
                    parent == Some(previous.commit),
                    "Select every commit in a contiguous linear range"
                );
            }
        }
        selections.push(ReviewCommitSelection {
            commit: commit.commit,
            parent,
        });
    }
    Ok(selections)
}

impl ModalView for ReviewCommitPicker {
    fn render_bare(&self) -> bool {
        true
    }
    fn fade_out_background(&self) -> bool {
        true
    }
}
impl EventEmitter<DismissEvent> for ReviewCommitPicker {}
impl Focusable for ReviewCommitPicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl ReviewCommitPicker {
    pub(crate) fn preview(
        workspace: WeakEntity<Workspace>,
        repository: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let commits = (1..=2055)
            .rev()
            .map(|index| ReviewCommit {
                commit: format!("{index:040x}")
                    .parse()
                    .expect("generic fixture oid"),
                parents: if index > 1 {
                    vec![
                        format!("{:040x}", index - 1)
                            .parse()
                            .expect("generic fixture parent"),
                    ]
                } else {
                    Vec::new()
                },
                subject: format!(
                    "Refine native review navigation and preserve pending drafts {index}"
                ),
            })
            .collect();
        let mut picker = Self::new(workspace, repository, commits, None, window, cx);
        picker.preview = true;
        picker
    }

    fn schedule_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_task = None;
        if self.search_input.read(cx).is_empty(cx) {
            self.query.clear();
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
            return;
        }
        self.search_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(180))
                .await;
            this.update_in(cx, |this, _, cx| {
                this.query = this.search_input.read(cx).text(cx).trim().to_string();
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn filtered_indices(&self) -> Vec<usize> {
        matching_commit_indices(&self.commits, &self.query)
    }

    fn scroll_endpoint(&mut self, last: bool, cx: &mut Context<Self>) {
        let shown = self.filtered_indices().len();
        if shown == 0 {
            return;
        }
        self.scroll.scroll_to_item(
            if last { shown - 1 } else { 0 },
            if last {
                ScrollStrategy::Bottom
            } else {
                ScrollStrategy::Top
            },
        );
        cx.notify();
    }
    fn toggle(&mut self, index: usize, extend: bool, cx: &mut Context<Self>) {
        if extend {
            if let Some(anchor) = self.anchor {
                self.selected.extend(anchor.min(index)..=anchor.max(index));
            } else {
                self.selected.insert(index);
            }
        } else if !self.selected.remove(&index) {
            self.selected.insert(index);
        }
        self.anchor = Some(index);
        self.error = None;
        cx.notify();
    }
}

fn matching_commit_indices(commits: &[ReviewCommit], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    commits
        .iter()
        .enumerate()
        .filter(|(_, commit)| {
            query.is_empty()
                || commit.subject.to_lowercase().contains(&query)
                || commit.commit.to_string().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

fn hidden_selection_count(selected: &BTreeSet<usize>, shown: &[usize]) -> usize {
    selected.len().saturating_sub(
        shown
            .iter()
            .filter(|index| selected.contains(index))
            .count(),
    )
}

impl Render for ReviewCommitPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let indices = self.filtered_indices();
        let shown = indices.len();
        let hidden_selected = hidden_selection_count(&self.selected, &indices);
        let rows = uniform_list(
            "review-commit-list",
            shown,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range
                    .filter_map(|row| {
                        let index = *indices.get(row)?;
                        let commit = this.commits.get(index)?;
                        let full_title = format!(
                            "{}\n{}\nParents: {}",
                            commit.subject,
                            commit.commit,
                            if commit.parents.is_empty() {
                                "empty tree (root commit)".into()
                            } else {
                                commit
                                    .parents
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        );
                        let commit_id = commit.commit.to_string();
                        let visible_title = format!(
                            "{} · {}",
                            commit.subject,
                            commit_id.get(..7).unwrap_or(&commit_id)
                        );
                        let parent = this.parents.get(index).copied().flatten();
                        let parent_label = if commit.parents.is_empty() {
                            "Root commit · empty tree".to_string()
                        } else if commit.parents.len() == 1 {
                            String::new()
                        } else {
                            format!(
                                "Merge · {} parents · {}",
                                commit.parents.len(),
                                parent
                                    .map(|parent| format!(
                                        "Parent {}",
                                        parent.to_string().chars().take(7).collect::<String>()
                                    ))
                                    .unwrap_or_else(|| "Choose a parent".into())
                            )
                        };
                        Some(
                            v_flex()
                                .w_full()
                                .h(rems(3.5))
                                .justify_center()
                                .min_w_0()
                                .gap_1()
                                .px_1()
                                .py_1()
                                .border_b_1()
                                .border_color(cx.theme().colors().border)
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .min_w_0()
                                        .w_full()
                                        .child(
                                            Checkbox::new(
                                                ("review-select", index),
                                                this.selected.contains(&index).into(),
                                            )
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.focus.focus(window, cx);
                                                    this.toggle(index, window.modifiers().shift, cx)
                                                }),
                                            ),
                                        )
                                        .child(
                                            div()
                                                .id(("review-commit-title", index))
                                                .flex_1()
                                                .min_w_0()
                                                .tooltip(ui::Tooltip::text(full_title))
                                                .child(Label::new(visible_title).truncate()),
                                        ),
                                )
                                .when(commit.parents.len() != 1, |row| {
                                    row.child(
                                        h_flex()
                                            .gap_1()
                                            .min_w_0()
                                            .child(
                                                Label::new(parent_label)
                                                    .size(LabelSize::XSmall)
                                                    .color(Color::Muted)
                                                    .truncate(),
                                            )
                                            .when(commit.parents.len() > 1, |row| {
                                                row.child(
                                                    Button::new(
                                                        ("review-cycle-parent", index),
                                                        "Choose / next parent",
                                                    )
                                                    .style(ButtonStyle::Outlined)
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        let Some(commit) = this.commits.get(index)
                                                        else {
                                                            return;
                                                        };
                                                        let current = this
                                                            .parents
                                                            .get(index)
                                                            .copied()
                                                            .flatten()
                                                            .and_then(|current| {
                                                                commit.parents.iter().position(
                                                                    |parent| *parent == current,
                                                                )
                                                            });
                                                        if let Some(parent) = commit.parents.get(
                                                            current.map_or(0, |current| {
                                                                (current + 1) % commit.parents.len()
                                                            }),
                                                        ) {
                                                            if let Some(selected) =
                                                                this.parents.get_mut(index)
                                                            {
                                                                *selected = Some(*parent);
                                                            }
                                                        }
                                                        this.error = None;
                                                        cx.notify();
                                                    })),
                                                )
                                            }),
                                    )
                                }),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .w_full()
        .h(
            (window.viewport_size().height - rems(17.).to_pixels(window.rem_size()))
                .max(px(80.))
                .min(rems(24.).to_pixels(window.rem_size())),
        );
        let panel = v_flex()
            .key_context("ReviewCommitPicker")
            .track_focus(&self.focus)
            .w((window.viewport_size().width - px(32.)).max(px(0.)))
            .max_w(rems(52.))
            .p_3()
            .gap_2()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_action(
                cx.listener(|this, _: &menu::SelectFirst, _, cx| this.scroll_endpoint(false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::SelectLast, _, cx| this.scroll_endpoint(true, cx)),
            )
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(false, window, cx)),
            )
            .child(Label::new("Select commits").size(LabelSize::Large))
            .child(
                Label::new(format!(
                    "{} commits · newest first · {} selected",
                    self.commits.len(),
                    self.selected.len()
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .child(self.search_input.clone())
                    .on_action(cx.listener(|_, _: &menu::Confirm, _, cx| cx.stop_propagation())),
            )
            .child(
                Label::new(format!(
                    "{shown} shown · {hidden_selected} selected outside search"
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .when(
                self.commits.iter().any(|commit| commit.parents.len() > 1),
                |view| {
                    view.child(
                        Label::new("Merge commits use their first parent unless changed below.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                },
            )
            .child(rows)
            .when_some(self.error.clone(), |view, error| {
                view.child(Label::new(error).color(Color::Error))
            })
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("review-select-shown", "Select shown")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected.extend(this.filtered_indices());
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("review-clear-selection", "Clear selection")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected.clear();
                                this.anchor = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .w_full()
                    .flex_wrap()
                    .justify_between()
                    .child(
                        Button::new("review-all-commits", "All commits")
                            .style(ButtonStyle::Outlined)
                            .disabled(self.pull_request.is_none() && !self.preview)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.selected.clear();
                                this.confirm(false, window, cx);
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("review-cancel-selection", "Cancel")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                            )
                            .child(
                                Button::new("review-selected-commits", "Apply")
                                    .style(ButtonStyle::Tinted(TintColor::Accent))
                                    .disabled(
                                        self.selected.is_empty()
                                            && self.pull_request.is_none()
                                            && !self.preview,
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm(false, window, cx)
                                    })),
                            ),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .bg(cx.theme().colors().editor_background.opacity(0.7))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
            )
            .child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(panel),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn oid(value: char) -> git::Oid {
        value.to_string().repeat(40).parse().unwrap()
    }
    #[gpui::test]
    async fn home_end_keys_scroll_complete_and_filtered_commit_lists_without_changing_checks(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
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
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/review-picker",
            serde_json::json!({ ".git": {}, "README.md": "Generic fixture" }),
        )
        .await;
        let project =
            project::Project::test(fs, [std::path::Path::new("/review-picker")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .expect("fixture repository");
        let (picker, cx) = cx.add_window_view(|window, cx| {
            ReviewCommitPicker::preview(WeakEntity::new_invalid(), repository, window, cx)
        });
        picker.update_in(cx, |picker, window, cx| {
            picker.selected = BTreeSet::from([0, 2000]);
            picker.focus.focus(window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            // UniformList virtualizes rows outside ScrollHandle.child_bounds;
            // its generic logical child index is not the virtual row index.
            // Assert actual painted geometry, so ignored key dispatch fails.
            assert_eq!(
                picker.scroll.is_scrolled_to_end(),
                Some(true),
                "End must reach the geometric bottom of the 2,055-commit list"
            );
            assert_eq!(picker.selected, BTreeSet::from([0, 2000]));
        });
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.scroll.0.borrow().base_handle.offset().y,
                px(0.),
                "Home must restore the exact painted top offset"
            )
        });
        picker.update(cx, |picker, cx| {
            picker.query = "drafts 2049".into();
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.filtered_indices().len(), 1);
            assert_eq!(picker.scroll.0.borrow().base_handle.offset().y, px(0.));
            assert!(!picker.scroll.is_scrollable());
            assert_eq!(
                hidden_selection_count(&picker.selected, &picker.filtered_indices()),
                2
            );
        });
        picker.update(cx, |picker, cx| {
            picker.query = "not-a-commit".into();
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("end home");
        cx.run_until_parked();
        picker.read_with(cx, |picker, _| {
            assert!(picker.filtered_indices().is_empty());
            assert_eq!(picker.selected, BTreeSet::from([0, 2000]));
        });
    }

    #[test]
    fn search_reports_hidden_checks_and_restores_them_when_cleared() {
        let commits = vec![
            ReviewCommit {
                commit: oid('3'),
                parents: vec![oid('2')],
                subject: "Update navigation".into(),
            },
            ReviewCommit {
                commit: oid('2'),
                parents: vec![oid('1')],
                subject: "Refine review cards".into(),
            },
            ReviewCommit {
                commit: oid('1'),
                parents: vec![],
                subject: "Initial snapshot".into(),
            },
        ];
        let selected = BTreeSet::from([0, 2]);
        let parents = vec![Some(oid('2')), Some(oid('1')), None];
        let before = selected_commits(&commits, &selected, &parents, false).unwrap();
        let filtered = matching_commit_indices(&commits, "navigation");
        assert_eq!(filtered, vec![0]);
        assert_eq!(hidden_selection_count(&selected, &filtered), 1);
        assert_eq!(
            hidden_selection_count(&selected, &matching_commit_indices(&commits, "no match")),
            2
        );
        let restored = matching_commit_indices(&commits, "");
        assert_eq!(restored, vec![0, 1, 2]);
        assert_eq!(hidden_selection_count(&selected, &restored), 0);
        assert!(
            restored
                .iter()
                .filter(|index| selected.contains(index))
                .copied()
                .eq([0, 2])
        );
        let after = selected_commits(&commits, &selected, &parents, false).unwrap();
        assert_eq!(
            before
                .iter()
                .map(|selection| selection.commit)
                .collect::<Vec<_>>(),
            after
                .iter()
                .map(|selection| selection.commit)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn review_picker_requires_exact_linear_selection_and_merge_parent() {
        let commits = vec![
            ReviewCommit {
                commit: oid('4'),
                parents: vec![oid('3'), oid('2')],
                subject: "merge".into(),
            },
            ReviewCommit {
                commit: oid('3'),
                parents: vec![oid('2')],
                subject: "third".into(),
            },
            ReviewCommit {
                commit: oid('2'),
                parents: vec![oid('1')],
                subject: "second".into(),
            },
            ReviewCommit {
                commit: oid('1'),
                parents: vec![],
                subject: "root".into(),
            },
        ];
        let mut parents = vec![None, Some(oid('2')), Some(oid('1')), None];
        assert!(selected_commits(&commits, &BTreeSet::new(), &parents, false).is_err());
        assert!(selected_commits(&commits, &BTreeSet::from([0]), &parents, false).is_err());
        parents[0] = Some(oid('2'));
        assert_eq!(
            selected_commits(&commits, &BTreeSet::from([0]), &parents, false).unwrap()[0].parent,
            Some(oid('2'))
        );
        assert!(selected_commits(&commits, &BTreeSet::from([0]), &parents, true).is_err());
        assert!(selected_commits(&commits, &BTreeSet::from([1, 3]), &parents, true).is_err());
        let groups = selected_commits(&commits, &BTreeSet::from([1, 3]), &parents, false).unwrap();
        assert_eq!(
            groups
                .iter()
                .map(|selection| selection.commit)
                .collect::<Vec<_>>(),
            vec![oid('1'), oid('3')]
        );
        let range = selected_commits(&commits, &BTreeSet::from([1, 2, 3]), &parents, true).unwrap();
        assert_eq!(range.len(), 3);
        assert!(range[0].parent.is_none());
    }
}

#[cfg(test)]
mod large_history_tests {
    use super::*;

    #[test]
    fn picker_reaches_last_commit_and_selects_a_range_beyond_two_thousand() {
        let commits = (1..=2055)
            .rev()
            .map(|index| ReviewCommit {
                commit: format!("{index:040x}").parse().expect("synthetic commit"),
                parents: if index == 1 {
                    Vec::new()
                } else {
                    vec![
                        format!("{:040x}", index - 1)
                            .parse()
                            .expect("synthetic parent"),
                    ]
                },
                subject: format!("Synthetic documentation revision {index}"),
            })
            .collect::<Vec<_>>();
        let parents = commits
            .iter()
            .map(|commit| commit.parents.first().copied())
            .collect::<Vec<_>>();
        let tail = selected_commits(&commits, &BTreeSet::from([2054]), &parents, false)
            .expect("last commit selection");
        assert_eq!(tail[0].commit, commits[2054].commit);
        assert!(tail[0].parent.is_none());
        let range = selected_commits(&commits, &(0..commits.len()).collect(), &parents, true)
            .expect("complete uncapped range");
        assert_eq!(range.len(), 2055);
        assert_eq!(
            range.last().map(|selection| selection.commit),
            commits.first().map(|commit| commit.commit)
        );
    }
}
