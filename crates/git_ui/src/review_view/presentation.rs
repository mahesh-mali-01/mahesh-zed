use super::*;

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.data.files.len();
        let available_width = if self.review_content_width > 0. {
            self.review_content_width
        } else {
            f32::from(window.viewport_size().width)
        };
        let thread_panel_width = self
            .github_panel_width
            .clamp(280., (available_width * 0.6).clamp(280., 900.));
        let compact_threads = self.github_open
            && self.github_panel.is_some()
            && available_width < self.current_tree_width() + thread_panel_width + 360.;
        let pending_actions = self
            .github_panel
            .as_ref()
            .map_or(0, |panel| panel.read(cx).pending_action_count());
        let pending_mode = self
            .github_panel
            .as_ref()
            .is_some_and(|panel| panel.read(cx).showing_pending_actions());
        let layout_view = cx.weak_entity();
        let pr_subset = matches!(
            self.data.session.selector,
            ComparisonSelector::PullRequestScope { .. }
        );
        let split_mode = self.data.session.presentation.split_view.unwrap_or(
            editor::EditorSettings::get_global(cx).diff_view_style == DiffViewStyle::Split,
        );
        let multi_mode = self.multi_loaded;
        let view_for_split = cx.weak_entity();
        let view_for_unified = cx.weak_entity();
        let view_for_layout = cx.weak_entity();
        let view_for_scrolling = cx.weak_entity();
        let view_for_project = cx.weak_entity();
        let view_for_all = cx.weak_entity();
        let view_for_unreviewed = cx.weak_entity();
        let unreviewed = self.data.session.presentation.unreviewed_only;
        let group_index = self
            .data
            .session
            .snapshot
            .groups
            .iter()
            .position(|group| group.endpoints.group == active_group(&self.data))
            .unwrap_or_default();
        let group_count = self.data.session.snapshot.groups.len();
        let sync_status = self
            .github_viewed_error
            .clone()
            .or_else(|| self.check_error.clone())
            .unwrap_or_else(|| {
                if self.refreshing {
                    "Refreshing pull request…".into()
                } else if self.checking {
                    "Checking for updates…".into()
                } else if self.busy {
                    "Preparing selected file…".into()
                } else if self.github_viewed_busy {
                    self.github_viewed_status
                        .clone()
                        .unwrap_or_else(|| "Syncing with GitHub…".into())
                } else if !self.github_viewed_intents.is_empty() {
                    "Viewed changes pending · Open Pending actions".into()
                } else if self.outdated() {
                    "Review changed · Refresh to continue".into()
                } else if self.github_viewed_needs_sync() {
                    "GitHub Viewed state not synchronized · Refresh to retry".into()
                } else {
                    "Ready".into()
                }
            });
        let visible_sync_status = self
            .github_viewed_remaining
            .as_ref()
            .map(github_viewed::ViewedRemainder::summary)
            .unwrap_or_else(|| {
                if self.github_viewed_error.is_some() {
                    "Sync needs attention".into()
                } else if self.check_error.is_some() {
                    "Refresh required".into()
                } else {
                    sync_status.clone()
                }
            });
        let current = self.data.session.presentation.selected_path.clone();
        let navigation_blocked = self.outdated();
        let writes_blocked = self.cached_unverified
            || self.busy
            || navigation_blocked
            || self.checking
            || self.refreshing;
        let effective_data = self.effective_data();
        let effective_viewed = effective_data.viewed.clone();
        let effective_viewed_count = effective_data
            .github_viewed
            .as_ref()
            .unwrap_or(&effective_viewed)
            .len();
        let changed_since_review = effective_data.github_viewed.as_ref().map_or(0, |remote| {
            remote.difference(&effective_data.checkpoint_viewed).count()
        });
        let rows = review_tree(&effective_data);
        let visible_file_paths = rows
            .iter()
            .filter(|row| !row.directory)
            .map(|row| &row.path)
            .collect::<Vec<_>>();
        let position = current.as_ref().and_then(|path| {
            visible_file_paths
                .iter()
                .position(|candidate| *candidate == path)
        });
        let position_label = position
            .map(|index| format!("Visible file {} of {}", index + 1, visible_file_paths.len()))
            .unwrap_or_else(|| {
                format!(
                    "Current file hidden · {} visible files",
                    visible_file_paths.len()
                )
            });
        let empty_message = empty_file_message(
            total,
            unreviewed,
            !effective_data
                .session
                .presentation
                .path_filter
                .trim()
                .is_empty(),
            paths(&effective_data).len(),
            visible_file_paths.len(),
        );
        let previous_available = tree_navigation_target(&self.data, false).is_some();
        let next_available = tree_navigation_target(&self.data, true).is_some();
        let tree_width = self
            .tree_width_preview
            .or(self.data.session.presentation.tree_width)
            .unwrap_or(320);
        let resize_handle = gpui::deferred(
            div()
                .id("review-tree-resize-handle")
                .debug_selector(|| "REVIEW_TREE_RESIZE_HANDLE".into())
                .absolute()
                .right(px(-3.))
                .top_0()
                .bottom_0()
                .w(px(7.))
                .cursor_col_resize()
                .block_mouse_except_scroll()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(DraggedReviewTree, |_, _, _, cx| {
                    log::info!("pull request review review tree resize started");
                    cx.stop_propagation();
                    cx.new(|_| gpui::Empty)
                }),
        );
        let list = v_flex()
            .id("review-tree-panel")
            .debug_selector(|| "REVIEW_TREE_PANEL".into())
            .relative()
            .w(px(tree_width as f32))
            .max_w(relative(0.6))
            .flex_shrink_0()
            .h_full()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .p_2()
                    .child(self.path_input.clone())
                    .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                        let mut presentation = this.data.session.presentation.clone();
                        presentation.path_filter =
                            this.path_input.read(cx).text(cx).trim().to_string();
                        this.change_presentation(presentation, window, cx);
                    })),
            )
            .child(
                h_flex().px_2().pb_2().gap_2().justify_between()
                    .child(DropdownMenu::new("review-file-filter", if self.data.session.presentation.unreviewed_only { "Unreviewed" } else { "All files" }, ContextMenu::build(window, cx, |menu, _, _| {
                        menu.toggleable_entry("All files", !unreviewed, ui::IconPosition::Start, None, move |window, cx| { view_for_all.update(cx, |this, cx| { let mut presentation = this.data.session.presentation.clone(); presentation.unreviewed_only = false; this.change_presentation(presentation, window, cx); }).log_err(); })
                            .toggleable_entry("Unreviewed", unreviewed, ui::IconPosition::Start, None, move |window, cx| { view_for_unreviewed.update(cx, |this, cx| { let mut presentation = this.data.session.presentation.clone(); presentation.unreviewed_only = true; this.change_presentation(presentation, window, cx); }).log_err(); })
                    })).attach(gpui::Anchor::BottomLeft).offset(gpui::point(px(0.), px(6.))))
                    .child(Label::new(format!("{} {}", paths(&effective_data).len(), if paths(&effective_data).len() == 1 { "file" } else { "files" })).color(Color::Muted))

            )
            .child(
                uniform_list(
                    "review-file-tree",
                    rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| {
                                let entry = rows.get(index)?.clone();
                                let selected =
                                    this.data.session.presentation.selected_path.as_ref()
                                        == Some(&entry.path);
                                let name = entry
                                    .path
                                    .rsplit('/')
                                    .next()
                                    .unwrap_or(&entry.path)
                                    .to_string();
                                let collapsed = this.data.session.presentation.collapsed_directories.contains(&entry.path);
                                let icon_path = if entry.directory {
                                    FileIcons::get_folder_icon(!collapsed, std::path::Path::new(&entry.path), cx)
                                } else {
                                    FileIcons::get_icon(std::path::Path::new(&entry.path), cx)
                                };
                                let file = this.data.session.snapshot.file(active_group(&this.data), &entry.path).ok();
                                let (decoration, color) = match file {
                                    Some(file) if file.old.is_none() => ("A", Color::VersionControlAdded),
                                    Some(file) if file.new.is_none() => ("D", Color::VersionControlDeleted),
                                    Some(file) if file.old.as_ref().map(|side| &side.path) != file.new.as_ref().map(|side| &side.path) => ("R", Color::VersionControlModified),
                                    Some(_) => ("M", Color::VersionControlModified),
                                    None => ("", Color::Muted),
                                };
                                let is_viewed = effective_viewed.contains(&entry.path);
                                let end_slot = if entry.directory {
                                    let folder = entry.path.clone();
                                    let prefix = format!("{folder}/");
                                    let count = this.data.files.iter().filter(|path| path.starts_with(&prefix)).count();
                                    let viewed_count = this.data.files.iter().filter(|path| path.starts_with(&prefix) && effective_viewed.contains(*path)).count();
                                    let mark_viewed = viewed_count < count;
                                    h_flex().debug_selector(|| "REVIEW_FOLDER_VIEWED_ACTION".into()).flex_none().min_w(rems(4.5)).pl_2().gap_2().justify_end()
                                        .child(Label::new(format!("{viewed_count}/{count}")).size(LabelSize::Small).color(Color::Muted))
                                        .child(div().debug_selector(move || format!("REVIEW_TREE_VIEWED_{index}")).child(review_checkbox(("review-folder-viewed-target", index), Checkbox::new(("review-folder-viewed", index), ui::ToggleState::from_any_and_all(viewed_count > 0, count > 0 && viewed_count == count)).fill().style(ui::ToggleStyle::Custom(cx.theme().colors().text))
                                            .tooltip(ui::Tooltip::text(format!("Mark all {count} changed files in this folder {} (including hidden files)", if mark_viewed { "viewed" } else { "unviewed" })))
                                            , writes_blocked || !this.data.session.selector.supports_github_viewed() || this.data.session.presentation.since_review,
                                            {
                                                let review = cx.entity().downgrade();
                                                move |window, cx| { review.update(cx, |this, cx| this.mark_github_folder(&folder, mark_viewed, window, cx)).log_err(); }
                                            })))
                                        .into_any_element()
                                } else {
                                    let path = entry.path.clone();
                                    let review = cx.entity().downgrade();
                                    let supports_review = this.data.session.selector.supports_github_viewed()
                                        || matches!(this.data.session.selector, ComparisonSelector::PullRequestScope { .. });
                                    h_flex().flex_none().min_w(rems(4.5)).pl_2().gap_2().justify_end()
                                        .child(Label::new(decoration).size(LabelSize::Small).color(color))
                                        .child(div().debug_selector(move || format!("REVIEW_TREE_VIEWED_{index}")).child(review_checkbox(
                                            ("review-file-viewed-target", index),
                                            Checkbox::new(("review-file-viewed", index), is_viewed.into()).fill()
                                                .style(ui::ToggleStyle::Custom(cx.theme().colors().text))
                                                .tooltip(ui::Tooltip::text(if is_viewed { "Mark file unviewed" } else { "Mark file viewed" })),
                                            writes_blocked || !supports_review || this.data.session.presentation.since_review,
                                            move |window, cx| {
                                                review.update(cx, |this, cx| {
                                                    if this.data.session.selector.supports_github_viewed() {
                                                        this.set_github_paths_viewed(vec![path.clone()], !is_viewed, None, window, cx);
                                                    } else {
                                                        this.review_commit_paths(vec![path.clone()], !is_viewed, true, window, cx);
                                                    }
                                                }).log_err();
                                            },
                                        )))
                                        .into_any_element()
                                };
                                let settings = ReviewTreeSettings::get_global(cx);
                                let indent_level = entry.depth.min((tree_width.saturating_sub(180) as f32 / settings.indent_size.max(1.)).floor() as usize);
                                // Indent only the leading content. ListItem's margin-based indent
                                // would also move its trailing action column and row hover bounds.
                                Some(div().w_full().child(
                                    ListItem::new(("review-tree-row", index))
                                        .tooltip(ui::Tooltip::text(entry.path.clone()))
                                        .indent_level(0)
                                        .indent_step_size(px(settings.indent_size))
                                        .spacing(settings.spacing)
                                        .selectable(false)
                                        .toggle_state(selected)
                                        .disabled(navigation_blocked)
                                        .start_slot::<gpui::AnyElement>(Some(h_flex().gap_0p5().pl(px(indent_level as f32 * settings.indent_size))
                                            .child(if entry.directory {
                                                Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }).color(Color::Muted).into_any_element()
                                            } else {
                                                h_flex().size(IconSize::default().rems()).invisible().flex_none().into_any_element()
                                            })
                                            .when(settings.file_icons, |row| row.when_some(icon_path, |row, path| row.child(Icon::from_path(path))))
                                            .into_any_element()))
                                        .end_slot::<gpui::AnyElement>(Some(end_slot))
                                        .child(h_flex().debug_selector(move || format!("REVIEW_TREE_NAME_{index}")).h_6().flex_1().min_w_0().overflow_hidden().pr_2().child(Label::new(name).single_line().color(if is_viewed { Color::Muted } else { Color::Default }).when(settings.bold_folders && entry.directory, |label| label.weight(FontWeight::SEMIBOLD)).truncate()))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if entry.directory {
                                                let mut presentation = this.data.session.presentation.clone();
                                                if !presentation.collapsed_directories.remove(&entry.path) { presentation.collapsed_directories.insert(entry.path.clone()); }
                                                this.change_presentation(presentation, window, cx);
                                            } else { this.select(entry.path.clone(), window, cx); }
                                        })),
                                ))
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .when(ReviewTreeSettings::get_global(cx).indent_guides, |list| {
                    list.with_decoration(ui::indent_guides(
                        px(ReviewTreeSettings::get_global(cx).indent_size),
                        ui::IndentGuideColors::panel(cx),
                    ).with_left_offset(ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET)
                    .with_compute_indents_fn(cx.entity(), |this, range, _, cx| {
                        let settings = ReviewTreeSettings::get_global(cx);
                        let max_depth = ((this.current_tree_width() - 180.).max(0.) / settings.indent_size.max(1.)).floor() as usize;
                        let rows = review_tree(&this.effective_data());
                        range.filter_map(|index| rows.get(index).map(|row| row.depth.min(max_depth))).collect()
                    }))
                })
                .track_scroll(&self.tree_scroll)
                .flex_1()
                .min_h_0(),
            )
            .child(resize_handle);
        let mut content = v_flex().flex_1().min_w_0().h_full();
        if let Some(error) = &self.error {
            content = content
                .child(Label::new(error.clone()).color(Color::Error))
                .child(
                    Button::new("review-retry", "Retry")
                        .on_click(cx.listener(|this, _, window, cx| this.retry(window, cx))),
                );
        }
        if self.data.session.presentation.since_review {
            content = content.child(Label::new(self.layout_note.clone().unwrap_or_else(|| "Since review · retained reviewed contents → current contents. Return to Full PR comparison to mark Viewed or add inline comments.".into())).size(LabelSize::Small));
        }
        if let Some(editor) = &self.editor {
            content = content.child(div().flex_1().min_h_0().child(editor.clone()));
        } else if self.busy {
            content = content.child(Label::new("Loading file…"));
        } else if self.error.is_none() && !self.data.session.presentation.since_review {
            content = content.child(Label::new(empty_message));
        }
        v_flex()
            .size_full()
            .key_context("ReviewView")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &CheckReviewUpdates, window, cx| {
                this.check_for_updates(window, cx)
            }))
            .on_action(cx.listener(|this, _: &RefreshReview, window, cx| this.refresh(window, cx)))
            .on_action(cx.listener(|this, _: &ReloadReview, window, cx| this.retry(window, cx)))
            .on_action(cx.listener(|this, _: &NarrowReviewTree, window, cx| {
                this.set_tree_width(Some(this.current_tree_width() - 40.0), window, cx)
            }))
            .on_action(cx.listener(|this, _: &WidenReviewTree, window, cx| {
                this.set_tree_width(Some(this.current_tree_width() + 40.0), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ResetReviewTreeWidth, window, cx| {
                this.set_tree_width(None, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &CommentOnPullRequestSelection, window, cx| {
                    this.comment_on_selection(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &TogglePullRequestDiscussions, _, cx| {
                    let pending_mode = this.github_panel.as_ref().is_some_and(|panel| panel.read(cx).showing_pending_actions());
                    this.github_open = pending_mode || !this.github_open;
                    if let Some(panel) = &this.github_panel { panel.update(cx, |panel, cx| panel.show_threads(cx)); }
                    cx.notify();
                }),
            )
            .on_action(cx.listener(|this, _: &PostGitHubCommentNow, window, cx| {
                if let Some(panel) = this.github_panel.clone() {
                    panel.update(cx, |panel, cx| panel.post_comment_now(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &SaveGitHubDraft, window, cx| {
                if let Some(panel) = this.github_panel.clone() {
                    panel.update(cx, |panel, cx| panel.save_local_draft(window, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &CloseGitHubComposer, window, cx| {
                if let Some(panel) = this.github_panel.clone() {
                    panel.update(cx, |panel, cx| panel.close_composer(window, cx));
                }
            }))
            .on_action(
                cx.listener(|this, _: &ReconcileGitHubDelivery, window, cx| {
                    if let Some(panel) = this.github_panel.clone() {
                        this.github_open = true;
                        panel.update(cx, |panel, cx| panel.reconcile_current(window, cx));
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &SyncGitHubViewed, window, cx| {
                this.sync_github_viewed(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowChangesSinceReview, window, cx| {
                let mut presentation = this.data.session.presentation.clone();
                presentation.since_review = true;
                this.change_presentation(presentation, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShowFullPRComparison, window, cx| {
                let mut presentation = this.data.session.presentation.clone();
                presentation.since_review = false;
                this.change_presentation(presentation, window, cx);
            }))
            .on_action(cx.listener(|this, _: &StopGitHubViewedRetention, _, cx| {
                this.github_viewed_cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                this.github_viewed_guard = None;
                cx.notify();
            }))
            .on_action(cx.listener(Self::toggle_viewed))
            .on_action(cx.listener(Self::toggle_filter))
            .on_action(cx.listener(|this, _: &NextReviewFile, window, cx| {
                this.navigate_file(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousReviewFile, window, cx| {
                this.navigate_file(false, window, cx)
            }))
            .children({
                #[cfg(feature = "review-test-support")]
                { github_viewed::fixture::matches(&self.data.session.selector).then(|| github_viewed::fixture::toolbar(self, cx)) }
                #[cfg(not(feature = "review-test-support"))]
                { None::<gpui::AnyElement> }
            })
            .when_some(self.data.pull_request.clone(), |view, metadata| {
                view.child(
                    v_flex()
                        .px_3()
                        .pt_2()
                        .gap_1()
                        .min_w_0()
                        .child(
                            Label::new(format!("{} #{}", metadata.title, metadata.number))
                                .truncate(),
                        )
                        .child(
                            Label::new(format!(
                                "{} → {}",
                                metadata.head_ref_name, metadata.base_ref_name
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
            })
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .min_w_0()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .flex_wrap()
                    .child(
                        Button::new("review-back-pr-list", "Pull requests")
                            .style(ButtonStyle::Outlined)
                            .start_icon(Icon::new(IconName::ArrowLeft))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_pr_list(window, cx)),
                            ),
                    )
                    .when_some(self.data.pull_request.clone(), |row, metadata| {
                        let reference_url = metadata.url();
                        row.child(
                            Button::new("pr-back-overview", "Conversation")
                                .style(ButtonStyle::Outlined)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    match PullRequestReference::parse(&reference_url, None) {
                                        Ok(reference) => this.open_overview(reference, window, cx),
                                        Err(error) => {
                                            this.error = Some(error.to_string());
                                            cx.notify();
                                        }
                                    }
                                })),
                        )
                    })
                    .when(matches!(self.data.session.selector, ComparisonSelector::PullRequestScope { .. }), |row| row.child(
                        Button::new("review-mark-scope-reviewed", "Mark selected commits reviewed")
                            .style(ButtonStyle::Outlined)
                            .disabled(navigation_blocked || self.cached_unverified)
                            .tooltip(ui::Tooltip::text("Mark every file comparison in the selected commits reviewed; sync only files whose full PR changes are covered"))
                            .on_click(cx.listener(|this, _, window, cx| this.mark_selected_commits_reviewed(window, cx)))
                    ))
                    .child(
                        Button::new("review-select-commits", "Commits")
                            .style(ButtonStyle::Outlined)
                            .disabled(self.history_loading || navigation_blocked)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.pull_request_context().is_some() {
                                    if let Some(metadata) = this.data.pull_request.clone() {
                                        if let Ok(reference) =
                                            PullRequestReference::parse(&metadata.url(), None)
                                        {
                                            this.open_commits(reference, window, cx);
                                        }
                                    }
                                } else if let Some(group) =
                                    this.data.session.snapshot.groups.iter().find(|group| {
                                        group.endpoints.group == active_group(&this.data)
                                    })
                                {
                                    let head = group.endpoints.right.hex.parse().ok();
                                    let repository = this.repository.clone();
                                    this.workspace
                                        .update(cx, |workspace, cx| {
                                            Self::pick_commits(
                                                workspace, repository, None, head, window, cx,
                                            )
                                        })
                                        .log_err();
                                }
                            })),
                    )
                    .child(
                        Button::new("review-files-tab", "Files changed")
                            .style(ButtonStyle::Filled)
                            .toggle_state(true),
                    )
                    .child(
                        h_flex()
                            .id("review-refresh-group")
                            .debug_selector(|| "REVIEW_REFRESH_GROUP".into())
                            .ml_auto()
                            .max_w_full()
                            .min_w_0()
                            .flex_shrink_0()
                            .gap_2()
                            .justify_end()
                            .child(
                                div().flex_none().child(
                                    Label::new(format!("{} / {total} {}{}{}", effective_viewed_count,
                                        if effective_data.github_viewed.is_some() { "viewed" } else { "reviewed locally" },
                                        if changed_since_review == 0 { String::new() } else { format!(" · {changed_since_review} changed since review") },
                                        if self.github_viewed_intents.is_empty() || self.github_viewed_remaining.is_some() { String::new() } else { format!(" · {} pending", self.github_viewed_intents.len()) }))
                                        .color(Color::Muted),
                                ),
                            )
                            .child(
                                // Reserve the same compact status width in every phase.
                                // Conditional spinner/text widths must not wrap the toolbar
                                // and move the retained diff while a file is preparing.
                                h_flex()
                                    .id("review-sync-status")
                                    .w(rems(10.))
                                    .max_w(px(240.))
                                    .min_w_0()
                                    .gap_1()
                                    .tooltip(ui::Tooltip::text(if visible_sync_status == sync_status {
                                        sync_status
                                    } else {
                                        format!("{visible_sync_status} · {sync_status}")
                                    }))
                                    .child(
                                        div().size(IconSize::default().rems()).flex_none()
                                            .when(self.github_viewed_busy || self.checking || self.refreshing || self.busy,
                                                |slot| slot.child(ui::SpinnerLabel::new())),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Label::new(visible_sync_status)
                                                .truncate()
                                                .size(LabelSize::Small)
                                                .color(
                                                    if self.github_viewed_error.is_some()
                                                        || self.check_error.is_some()
                                                    {
                                                        Color::Warning
                                                    } else {
                                                        Color::Muted
                                                    },
                                                ),
                                        ),
                                    ),
                            )
                            .when_some(self.github_viewed_remaining.as_ref(), |row, remaining| {
                                row.child(div().flex_none().child(Button::new("review-resume-viewed", format!("Sync {} remaining", remaining.paths.len()))
                                    .style(ButtonStyle::Outlined)
                                    .disabled(self.github_viewed_busy || self.github_viewed_preparing || self.refreshing || self.outdated())
                                    .tooltip(ui::Tooltip::text("Create a new saved intent for files independently verified as not matching. Confirmed files are not replayed."))
                                    .on_click(cx.listener(|this, _, window, cx| this.resume_github_viewed_remaining(window, cx)))))
                            })
                            .child(
                                div().debug_selector(|| "REVIEW_REFRESH_ACTION".into()).flex_none().child(
                                Button::new("review-refresh", "Refresh")
                                    .style(ButtonStyle::Outlined)
                                    .start_icon(Icon::new(IconName::ArrowCircle))
                                    .disabled(
                                        self.refreshing
                                            || self.github_viewed_busy
                                            || self.busy,
                                    )
                                    .tooltip(ui::Tooltip::text("Refresh PR metadata, discussions, Viewed state and validate the applied commit selection"))
                                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                                ),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .min_w_0()
                    .flex_wrap()
                    .when(group_count > 1 && self.data.session.selector.pull_request_identity().is_none(), |row| {
                        row.child(
                            IconButton::new("review-previous-group", IconName::ArrowLeft)
                                .disabled(navigation_blocked || group_index == 0)
                                .tooltip(ui::Tooltip::text("Previous comparison"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.change_group(group_index.saturating_sub(1), window, cx)
                                })),
                        )
                        .child(
                            Label::new(format!("Comparison {} / {group_count}", group_index + 1))
                                .size(LabelSize::Small),
                        )
                        .child(
                            IconButton::new("review-next-group", IconName::ArrowRight)
                                .disabled(navigation_blocked || group_index + 1 >= group_count)
                                .tooltip(ui::Tooltip::text("Next comparison"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.change_group(group_index + 1, window, cx)
                                })),
                        )
                    })
                    .when(self.pull_request_context().is_some(), |row| {
                        row.child(
                            Button::new("review-compare-commits", match &self.data.session.selector {
                                ComparisonSelector::PullRequestScope { scope, .. } => format!("{} selected commits", scope.selections().len()),
                                _ => "All commits".into(),
                            })
                                .style(ButtonStyle::Outlined)
                                .disabled(navigation_blocked || self.history_loading)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.pick_pull_request_commits(window, cx)
                                })),
                        )
                    })
                    .child(DropdownMenu::new(
                        "review-view-options",
                        if self.data.session.presentation.split_view.unwrap_or(
                            editor::EditorSettings::get_global(cx).diff_view_style
                                == DiffViewStyle::Split,
                        ) {
                            "Split view"
                        } else {
                            "Unified view"
                        },
                        ContextMenu::build(window, cx, |menu, _, _| {
                            menu.toggleable_entry(
                                "Split",
                                split_mode,
                                ui::IconPosition::Start,
                                None,
                                move |window, cx| {
                                    view_for_split
                                        .update(cx, |this, cx| {
                                            let mut presentation =
                                                this.data.session.presentation.clone();
                                            presentation.split_view = Some(true);
                                            this.change_presentation(presentation, window, cx);
                                        })
                                        .log_err();
                                },
                            )
                            .toggleable_entry(
                                "Unified",
                                !split_mode,
                                ui::IconPosition::Start,
                                None,
                                move |window, cx| {
                                    view_for_unified
                                        .update(cx, |this, cx| {
                                            let mut presentation =
                                                this.data.session.presentation.clone();
                                            presentation.split_view = Some(false);
                                            this.change_presentation(presentation, window, cx);
                                        })
                                        .log_err();
                                },
                            )
                            .separator()
                            .toggleable_entry_disabled_when(
                                "Single file",
                                !multi_mode,
                                pr_subset,
                                ui::IconPosition::Start,
                                None,
                                move |window, cx| {
                                    view_for_layout
                                        .update(cx, |this, cx| {
                                            let mut presentation =
                                                this.data.session.presentation.clone();
                                            presentation.multi_file = Some(false);
                                            this.change_presentation(presentation, window, cx);
                                        })
                                        .log_err();
                                },
                            )
                            .toggleable_entry_disabled_when(
                                if pr_subset { "Scrolling files · all commits only" } else { "Scrolling files" },
                                multi_mode,
                                pr_subset,
                                ui::IconPosition::Start,
                                None,
                                move |window, cx| {
                                    view_for_scrolling
                                        .update(cx, |this, cx| {
                                            let mut presentation =
                                                this.data.session.presentation.clone();
                                            presentation.multi_file = Some(true);
                                            this.change_presentation(presentation, window, cx);
                                        })
                                        .log_err();
                                },
                            )
                            .separator()
                            .entry("Open in project", None, move |window, cx| {
                                view_for_project
                                    .update(cx, |this, cx| this.open_in_project(window, cx))
                                    .log_err();
                            })
                            .separator()
                            .action("Full PR comparison", ShowFullPRComparison.boxed_clone())
                            .action("Changes since review", ShowChangesSinceReview.boxed_clone())
                        }),
                    ).attach(gpui::Anchor::BottomLeft).offset(gpui::point(px(0.), px(6.))))
                    .when(pr_subset, |row| {
                        row.child(
                            Button::new("review-all-pr-changes", "All PR changes")
                                .style(ButtonStyle::Outlined)
                                .disabled(writes_blocked)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_all_pull_request_changes(window, cx)
                                })),
                        )
                    })
                    .when(compact_threads, |row| {
                        row.child(Button::new("review-return-to-files", "Files")
                            .style(ButtonStyle::Outlined)
                            .tooltip(ui::Tooltip::text("Close Threads & review and restore the file tree"))
                            .on_click(cx.listener(|this, _, _, cx| { this.github_open = false; cx.notify(); })))
                    })
                    .when(self.github_panel.is_some(), |row| {
                        row.child(
                            Button::new("github-discussions", "Threads & review")
                                .style(ButtonStyle::Outlined)
                                .toggle_state(self.github_open && !pending_mode)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let pending_mode = this.github_panel.as_ref().is_some_and(|panel| panel.read(cx).showing_pending_actions());
                                    this.github_open = pending_mode || !this.github_open;
                                    if let Some(panel) = &this.github_panel { panel.update(cx, |panel, cx| panel.show_threads(cx)); }
                                    cx.notify();
                                })),
                        )
                    })
                    .when(pending_actions > 0 || (self.github_open && pending_mode), |row| row.child(
                        Button::new("github-pending-actions", format!("Pending actions ({pending_actions})"))
                            .style(ButtonStyle::Outlined)
                            .toggle_state(self.github_open && pending_mode)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let pending_mode = this.github_panel.as_ref().is_some_and(|panel| panel.read(cx).showing_pending_actions());
                                this.github_open = !(this.github_open && pending_mode);
                                if this.github_open {
                                    if let Some(panel) = &this.github_panel { panel.update(cx, |panel, cx| panel.show_pending_actions(window, cx)); }
                                }
                                cx.notify();
                            }))
                    ))
                    .child(div().flex_1())
                    .child(
                        Label::new(position_label)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("review-previous", IconName::ArrowLeft)
                            .tooltip(ui::Tooltip::text("Previous visible file"))
                            .disabled(navigation_blocked || !previous_available)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_file(false, window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("review-next", IconName::ArrowRight)
                            .tooltip(ui::Tooltip::text("Next visible file"))
                            .disabled(navigation_blocked || !next_available)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_file(true, window, cx)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .on_children_prepainted(move |bounds, _, cx| {
                        let left = bounds.iter().map(|bound| f32::from(bound.left())).reduce(f32::min);
                        let right = bounds.iter().map(|bound| f32::from(bound.right())).reduce(f32::max);
                        if let Some(width) = left.zip(right).map(|(left, right)| right - left) {
                            layout_view.update(cx, |view, cx| {
                                if (view.review_content_width - width).abs() > 1. {
                                    view.review_content_width = width;
                                    cx.notify();
                                }
                            }).log_err();
                        }
                    })
                    .id("review-files-layout")
                    .debug_selector(|| "REVIEW_FILES_LAYOUT".into())
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .when(!compact_threads, |row| row.child(list))
                    .child(div().debug_selector(|| "REVIEW_DIFF_CONTENT".into()).flex_1().min_w_0().h_full().child(content))
                    .when(self.github_open, |row| {
                        row.when_some(self.github_panel.clone(), |row, panel| {
                            row.child(
                                h_flex()
                                    .id("github-review-sidebar")
                                    .debug_selector(|| "REVIEW_GITHUB_SIDEBAR".into())
                                    .relative()
                                    .w(px(thread_panel_width))
                                    .max_w(relative(0.6))
                                    .flex_shrink_0()
                                    .h_full()
                                    .border_l_1()
                                    .border_color(cx.theme().colors().border)
                                    // Reserve an unclipped sibling hit target inside
                                    // the panel boundary. Scroll content cannot cover
                                    // it and the full twelve pixels accept a drag.
                                    .child(div().id("review-threads-resize")
                                        .debug_selector(|| "REVIEW_THREADS_RESIZE_HANDLE".into())
                                        .h_full().w(px(12.)).flex_shrink_0()
                                        .cursor_col_resize().block_mouse_except_scroll()
                                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                        .on_drag(DraggedReviewThreads, |_, _, _, cx| {
                                            log::info!("pull request review review thread resize started");
                                            cx.stop_propagation(); cx.new(|_| gpui::Empty)
                                        }))
                                    .child(div().id("github-review-sidebar-scroll").flex_1().min_w_0().h_full().overflow_y_scroll().child(panel)),
                            )
                        })
                    })
                    .on_drag_move::<DraggedReviewThreads>(cx.listener(|this, event: &gpui::DragMoveEvent<DraggedReviewThreads>, _, cx| {
                        let available = f32::from(event.bounds.size.width);
                        this.github_panel_width = f32::from(event.bounds.right() - event.event.position.x).clamp(280., (available * 0.6).clamp(280., 900.));
                        cx.notify();
                    }))
                    .on_drag_move::<DraggedReviewTree>(cx.listener(
                        |this, event: &gpui::DragMoveEvent<DraggedReviewTree>, _, cx| {
                            if this.busy || this.outdated() {
                                return;
                            }
                            this.tree_resizing = true;
                            this.tree_width_preview = Some(bounded_tree_width(
                                f32::from(event.event.position.x - event.bounds.left()),
                                f32::from(event.bounds.size.width),
                            ));
                            cx.notify();
                        },
                    ))
                    .on_mouse_up_out(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.finish_tree_resize(window, cx)),
                    )
                    .on_drop::<DraggedReviewTree>(
                        cx.listener(|this, _, window, cx| this.finish_tree_resize(window, cx)),
                    ),
            )
    }
}

fn empty_file_message(
    total: usize,
    unreviewed_only: bool,
    searching: bool,
    matching_files: usize,
    visible_files: usize,
) -> &'static str {
    if total == 0 {
        "No committed differences"
    } else if matching_files == 0 {
        match (searching, unreviewed_only) {
            (true, true) => "No unreviewed files match your search.",
            (true, false) => "No files match your search.",
            (false, true) => "All files reviewed. Choose All files to revisit them.",
            (false, false) => "Select a file to review.",
        }
    } else if visible_files == 0 {
        "Expand a folder to select a file."
    } else {
        "Select a file to review."
    }
}
