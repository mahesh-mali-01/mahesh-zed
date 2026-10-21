use crate::github_pull_requests::{
    ConversationActivity, ConversationAnchor, ConversationCursor, ConversationPage, GitHubClient,
    GitHubRequestGuard, GitHubUser, PullRequestDetails, PullRequestFilter, PullRequestReference,
    PullRequestSummary, RepositoryName,
};
use editor::hover_markdown_style;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListState, Render, SharedString, Task, UniformListScrollHandle, Window, actions,
    list as variable_height_list, uniform_list,
};
use gpui::{DismissEvent, Subscription, WeakEntity};
use markdown::{Markdown, MarkdownElement, MarkdownOptions};
use picker::{Picker, PickerDelegate};
use std::{sync::Arc, time::Duration};
use ui::{ContextMenu, DropdownMenu, ListItem, ListItemSpacing, prelude::*};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent};

actions!(
    git,
    [
        /// Browse GitHub pull requests with the current GH CLI account.
        OpenPullRequests,
        /// Open Files changed for the active pull request, using the same guarded and cached path as the button.
        ReviewPullRequestFiles,
        /// Open an offline, generic native PR review component preview.
        PreviewPullRequestReview,
        /// Open the native commit picker with an offline large-history fixture.
        PreviewReviewCommitPicker,
        /// Open offline native review comment cards and composer.
        PreviewReviewComments,
    ]
);

struct ActivityEntry {
    id: String,
    created_at: String,
    author_login: String,
    action_label: String,
    body_text: String,
    body: Entity<Markdown>,
    location: Option<String>,
    anchor: Option<ConversationAnchor>,
    url: Option<String>,
    children: Vec<ActivityEntry>,
}

fn reconcile_activity(
    previous: Vec<ActivityEntry>,
    incoming: Vec<ConversationActivity>,
    cx: &mut Context<PullRequestView>,
) -> Vec<ActivityEntry> {
    let mut previous = previous
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect::<std::collections::HashMap<_, _>>();
    incoming
        .into_iter()
        .map(|activity| {
            let existing = previous.remove(&activity.id);
            let (body, children) = if let Some(existing) = existing {
                if existing.body_text != activity.body {
                    existing
                        .body
                        .update(cx, |body, cx| body.replace(activity.body.clone(), cx));
                }
                (existing.body, existing.children)
            } else {
                (
                    cx.new(|cx| {
                        Markdown::new_with_options(
                            activity.body.clone().into(),
                            None,
                            None,
                            MarkdownOptions {
                                parse_html: true,
                                ..Default::default()
                            },
                            cx,
                        )
                    }),
                    Vec::new(),
                )
            };
            ActivityEntry {
                id: activity.id,
                created_at: activity.created_at,
                author_login: activity.author_login,
                action_label: activity.action_label,
                body_text: activity.body,
                body,
                location: activity.location,
                anchor: activity.anchor,
                url: activity.url,
                children: reconcile_activity(children, activity.children, cx),
            }
        })
        .collect()
}

fn activity_timestamp(value: &str) -> String {
    let Ok(timestamp) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    else {
        return "Time unavailable".into();
    };
    time_format::format_localized_timestamp(
        timestamp,
        time::OffsetDateTime::now_utc(),
        time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC),
        time_format::TimestampFormat::Relative,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CommitHistoryRow {
    Date(String),
    Commit(usize),
}

fn commit_history_rows(
    commits: &[crate::github_pull_requests::PullRequestCommitSummary],
) -> Vec<CommitHistoryRow> {
    let mut rows = Vec::with_capacity(commits.len());
    let mut previous_date = None;
    for (index, commit) in commits.iter().enumerate().rev() {
        let date = commit.committed_date.get(..10).unwrap_or("Unknown date");
        if previous_date != Some(date) {
            rows.push(CommitHistoryRow::Date(date.to_string()));
            previous_date = Some(date);
        }
        rows.push(CommitHistoryRow::Commit(index));
    }
    rows
}

pub struct PullRequestView {
    workspace: WeakEntity<Workspace>,
    preview: bool,
    preview_panel: Option<Entity<crate::review_github_view::GitHubReviewPanel>>,
    files_task: Option<Task<()>>,
    showing_commits: bool,
    commits: Vec<crate::github_pull_requests::PullRequestCommitSummary>,
    commits_task: Option<Task<()>>,
    commits_guard: Option<GitHubRequestGuard>,
    commits_loading: bool,
    commit_scroll: UniformListScrollHandle,
    commit_rows: Vec<CommitHistoryRow>,
    commits_error: Option<String>,
    copied_commit: Option<String>,
    loading_files: bool,
    files_error: Option<String>,
    repository: Option<RepositoryName>,
    catalog: Vec<PullRequestSummary>,
    catalog_task: Option<Task<()>>,
    catalog_progress_task: Option<Task<()>>,
    catalog_guard: Option<GitHubRequestGuard>,
    catalog_error: Option<String>,
    catalog_loading: bool,
    debounce_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    reference_input: Entity<InputField>,
    search_input: Entity<InputField>,
    author: Option<String>,
    source: Option<String>,
    head_repository: Option<String>,
    state: Option<String>,
    review_requested: bool,

    viewer: Option<GitHubUser>,
    account_error: Option<String>,
    items: Vec<PullRequestSummary>,
    selected: Option<PullRequestReference>,
    details: Option<PullRequestDetails>,
    description: Option<Entity<Markdown>>,
    activity: Vec<ActivityEntry>,
    activity_cursor: ConversationCursor,
    activity_snapshot: ConversationPage,
    activity_rows: Vec<Vec<usize>>,
    conversation_list: ListState,
    activity_warnings: Vec<String>,
    error: Option<String>,
    detail_error: Option<String>,
    activity_error: Option<String>,
    loading_details: bool,
    loading_activity: bool,
    account_generation: u64,
    list_generation: u64,
    detail_generation: u64,
    account_guard: Option<GitHubRequestGuard>,
    detail_guard: Option<GitHubRequestGuard>,
    activity_guard: Option<GitHubRequestGuard>,
    account_task: Option<Task<()>>,
    detail_task: Option<Task<()>>,
    activity_task: Option<Task<()>>,
}

impl PullRequestView {
    #[cfg(test)]
    pub(crate) fn new_for_navigation_test(
        workspace: WeakEntity<Workspace>,
        reference: PullRequestReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::new(workspace, Some(reference.repository.clone()), window, cx);
        view.selected = Some(reference);
        view
    }

    #[cfg(test)]
    pub(crate) fn set_navigation_test_details(
        &mut self,
        details: PullRequestDetails,
        viewer: GitHubUser,
    ) {
        self.details = Some(details);
        self.viewer = Some(viewer);
    }

    #[cfg(test)]
    pub(crate) fn cancel_navigation_test_requests(&mut self) {
        self.catalog_guard = None;
        self.catalog_task = None;
        self.catalog_progress_task = None;
        self.debounce_task = None;
        self.commits_guard = None;
        self.commits_task = None;
        self.account_guard = None;
        self.detail_guard = None;
        self.activity_guard = None;
        self.account_task = None;
        self.detail_task = None;
        self.activity_task = None;
    }
    pub fn register(workspace: &mut Workspace, _cx: &mut Context<Workspace>) {
        #[cfg(feature = "review-test-support")]
        workspace.register_action(|workspace, _: &PreviewPullRequestReview, window, cx| {
            let view = cx.new(|cx| {
                Self::new(
                    workspace.weak_handle(),
                    Some(RepositoryName {
                        owner: "example".into(),
                        name: "review-preview".into(),
                    }),
                    window,
                    cx,
                )
            });
            view.update(cx, |view, cx| {
                view.preview = true;
                view.install_preview(window, cx);
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
        #[cfg(feature = "review-test-support")]
        workspace.register_action(|workspace, _: &PreviewReviewCommitPicker, window, cx| {
            if let Some(repository) = workspace.project().read(cx).active_repository(cx) {
                let weak = workspace.weak_handle();
                workspace.toggle_modal(window, cx, move |window, cx| {
                    crate::review_commit_picker::ReviewCommitPicker::preview(
                        weak, repository, window, cx,
                    )
                });
            }
        });
        #[cfg(feature = "review-test-support")]
        workspace.register_action(|workspace, _: &PreviewReviewComments, window, cx| {
            let view = cx.new(|cx| {
                Self::new(
                    workspace.weak_handle(),
                    Some(RepositoryName {
                        owner: "example".into(),
                        name: "review-preview".into(),
                    }),
                    window,
                    cx,
                )
            });
            view.update(cx, |view, cx| {
                view.preview = true;
                view.install_preview(window, cx);
                view.selected = Some(PullRequestReference {
                    repository: view.repository.clone().expect("preview repository"),
                    number: 1,
                });
                view.install_preview_details(window, cx);
                let panel =
                    cx.new(|cx| crate::review_github_view::GitHubReviewPanel::preview(window, cx));
                view._subscriptions
                    .push(cx.subscribe(&panel, |view, _, event, cx| {
                        if matches!(
                            event,
                            crate::review_github_view::GitHubReviewEvent::ClosePanel
                        ) {
                            view.preview_panel = None;
                            cx.notify();
                        }
                    }));
                view.preview_panel = Some(panel);
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
        workspace.register_action(|workspace, _: &ReviewPullRequestFiles, window, cx| {
            if let Some(view) = workspace.active_item_as::<Self>(cx) {
                // Workspace action listeners hold its entity lease. Release it
                // before Files may activate a cached item through that workspace.
                view.update(cx, |_, cx| {
                    cx.defer_in(window, |view, window, cx| view.open_files(window, cx))
                });
            }
        });
        workspace.register_action(|workspace, _: &OpenPullRequests, window, cx| {
            let repository = workspace
                .project()
                .read(cx)
                .active_repository(cx)
                .and_then(|repository| repository.read(cx).default_remote_url())
                .and_then(|remote| repository_from_remote(&remote));
            let existing = workspace
                .items_of_type::<Self>(cx)
                .find(|view| !view.read(cx).preview);
            if let Some(existing) = existing {
                existing.update(cx, |view, cx| {
                    if view.repository != repository {
                        view.repository = repository.clone();
                        view.author = None;
                        view.source = None;
                        view.head_repository = None;
                        view.selected = None;
                        view.connect(window, cx);
                    }
                });
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let view = cx.new(|cx| Self::new(workspace.weak_handle(), repository, window, cx));
            view.update(cx, |view, cx| {
                view.connect(window, cx);
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
    }

    pub(crate) fn open_list(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|view| !view.read(cx).preview);
        if let Some(existing) = existing {
            existing.update(cx, |view, cx| view.clear_selection(cx));
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let repository = workspace
                .project()
                .read(cx)
                .active_repository(cx)
                .and_then(|repository| repository.read(cx).default_remote_url())
                .and_then(|remote| repository_from_remote(&remote));
            let view = cx.new(|cx| Self::new(workspace.weak_handle(), repository, window, cx));
            view.update(cx, |view, cx| view.connect(window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        }
    }

    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.detail_generation += 1;
        self.selected = None;
        self.details = None;
        self.description = None;
        self.activity.clear();
        self.activity_cursor = ConversationCursor::default();
        self.activity_snapshot = ConversationPage::default();
        self.activity_rows.clear();
        self.conversation_list.reset(2);
        self.activity_warnings.clear();
        self.detail_task = None;
        self.detail_guard = None;
        self.activity_task = None;
        self.activity_guard = None;
        self.commits_task = None;
        self.commits_guard = None;
        self.files_task = None;
        self.loading_files = false;
        self.loading_details = false;
        self.loading_activity = false;
        self.showing_commits = false;
        self.detail_error = None;
        self.files_error = None;
        cx.notify();
        cx.emit(ItemEvent::UpdateTab);
    }

    pub(crate) fn open_overview(
        workspace: &mut Workspace,
        reference: PullRequestReference,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|view| !view.read(cx).preview);
        if let Some(existing) = existing {
            existing.update(cx, |view, cx| {
                view.showing_commits = false;
                let same_reference = view.selected.as_ref() == Some(&reference);
                let reusable = same_reference
                    && view.viewer.is_some()
                    && view.details.is_some()
                    && !view.loading_details
                    && view.detail_error.is_none()
                    && view.account_error.is_none();
                if reusable || (same_reference && view.loading_details) {
                    cx.notify();
                } else if view.viewer.is_some() {
                    view.load(reference, window, cx);
                } else {
                    view.selected = Some(reference);
                    view.connect(window, cx);
                }
            });
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let repository = reference.repository.clone();
            let view =
                cx.new(|cx| Self::new(workspace.weak_handle(), Some(repository), window, cx));
            view.update(cx, |view, cx| {
                view.selected = Some(reference);
                view.connect(window, cx);
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        }
    }

    pub(crate) fn open_commits(
        workspace: &mut Workspace,
        reference: PullRequestReference,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        Self::open_overview(workspace, reference, window, cx);
        let view = workspace
            .items_of_type::<Self>(cx)
            .find(|view| !view.read(cx).preview);
        if let Some(view) = view {
            view.update(cx, |view, cx| view.show_commits(window, cx));
        }
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        repository: Option<RepositoryName>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| {
            InputField::new(window, cx, "Search pull requests")
                .start_icon(IconName::MagnifyingGlass)
                .clear_button(true)
        });
        let weak = cx.entity().downgrade();
        let search_editor = search_input.read(cx).editor().clone();
        let subscription = search_editor.subscribe(
            Box::new(move |event, window, cx| {
                if event == ErasedEditorEvent::BufferEdited {
                    weak.update(cx, |this, cx| this.debounce_search(window, cx))
                        .log_err();
                }
            }),
            window,
            cx,
        );
        let subscriptions = vec![subscription];
        let weak_workspace = workspace.clone();
        // Construction commonly runs inside Workspace::update, so reading it
        // here would re-enter its entity lease.
        cx.defer_in(window, move |this, window, cx| {
            if this.preview {
                return;
            }
            if let Some(workspace) = weak_workspace.upgrade() {
                let project = workspace.read(cx).project().clone();
                let git_store = project.read(cx).git_store().clone();
                this._subscriptions.push(cx.subscribe_in(
                    &git_store,
                    window,
                    move |this, _, event, window, cx| {
                        if matches!(
                            event,
                            project::git_store::GitStoreEvent::ActiveRepositoryChanged(_)
                        ) {
                            let repository = project
                                .read(cx)
                                .active_repository(cx)
                                .and_then(|repository| repository.read(cx).default_remote_url())
                                .and_then(|remote| repository_from_remote(&remote));
                            if this.repository != repository {
                                this.repository = repository;
                                this.author = None;
                                this.source = None;
                                this.head_repository = None;
                                this.selected = None;
                                this.connect(window, cx);
                            }
                        }
                    },
                ));
            }
        });
        Self {
            workspace,
            preview: false,
            preview_panel: None,
            files_task: None,
            showing_commits: false,
            commits: Vec::new(),
            commits_task: None,
            commits_guard: None,
            commits_loading: false,
            commit_scroll: UniformListScrollHandle::new(),
            commit_rows: Vec::new(),
            commits_error: None,
            copied_commit: None,
            loading_files: false,
            files_error: None,
            repository,
            catalog: Vec::new(),
            catalog_task: None,
            catalog_progress_task: None,
            catalog_guard: None,
            catalog_error: None,
            catalog_loading: false,
            debounce_task: None,
            _subscriptions: subscriptions,
            reference_input: cx
                .new(|cx| InputField::new(window, cx, "Pull request URL or #number")),
            search_input,
            author: None,
            source: None,
            head_repository: None,
            state: Some("OPEN".into()),
            review_requested: false,

            viewer: None,
            account_error: None,
            items: Vec::new(),
            selected: None,
            details: None,
            description: None,
            activity: Vec::new(),
            activity_cursor: ConversationCursor::default(),
            activity_snapshot: ConversationPage::default(),
            activity_rows: Vec::new(),
            conversation_list: {
                let state = ListState::new(2, ListAlignment::Top, px(600.));
                state.set_scroll_handler(cx.listener(
                    |this, event: &gpui::ListScrollEvent, window, cx| {
                        if event.visible_range.end.saturating_add(2) >= event.count {
                            if let Some(reference) = this.selected.clone() {
                                this.load_activity(reference, this.detail_generation, window, cx);
                            }
                        }
                    },
                ));
                state
            },
            activity_warnings: Vec::new(),
            error: None,
            detail_error: None,
            activity_error: None,
            loading_details: false,
            loading_activity: false,
            account_generation: 0,
            list_generation: 0,
            detail_generation: 0,
            account_guard: None,
            detail_guard: None,
            activity_guard: None,
            account_task: None,
            detail_task: None,
            activity_task: None,
        }
    }

    fn install_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.viewer = Some(GitHubUser {
            node_id: None,
            login: "reviewer".into(),
            name: Some("Example Reviewer".into()),
            url: None,
        });
        self.catalog = (1..=8).map(|number| serde_json::from_value(serde_json::json!({
            "id": format!("preview-{number}"), "number": number,
            "title": if number == 1 { "Improve native review navigation and preserve pending comments" } else { "Refine file tree presentation and keyboard navigation" },
            "url": format!("https://github.com/example/review-preview/pull/{number}"), "state": if number == 8 { "CLOSED" } else { "OPEN" },
            "isDraft": number == 3, "author": { "login": if number % 2 == 0 { "alex" } else { "sam" } },
            "baseRefName": "main", "headRefName": format!("feature/review-navigation-{number}"), "updatedAt": "2026-10-20T12:00:00Z"
        })).expect("valid generic PR preview fixture")).collect();
        self.commits = (1..=12)
            .map(
                |number| crate::github_pull_requests::PullRequestCommitSummary {
                    oid: format!("{number:040x}"),
                    message_headline: format!(
                        "Improve review interaction {number} while preserving keyboard navigation"
                    ),
                    author_name: Some(
                        if number % 2 == 0 {
                            "Alex Example"
                        } else {
                            "Sam Example"
                        }
                        .into(),
                    ),
                    committed_date: format!("2026-10-{:02}T12:00:00Z", 20 - number / 5),
                },
            )
            .collect();
        self.search(false, window, cx);
    }

    fn install_preview_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let number = self
            .selected
            .as_ref()
            .map_or(1, |reference| reference.number);
        let Some(summary) = self
            .catalog
            .iter()
            .find(|summary| summary.number == number)
            .cloned()
        else {
            return;
        };
        let details: PullRequestDetails = serde_json::from_value(serde_json::json!({
            "id": summary.id, "number": number, "title": summary.title, "url": summary.url, "state": summary.state,
            "isDraft": summary.is_draft, "author": { "login": "sam" }, "baseRefName": "main", "headRefName": summary.head_ref_name,
            "updatedAt": summary.updated_at, "body": "## Native component preview\n\nGeneric, offline fixture using the actual PR renderer. Open Filters, Author and Source branch menus, then inspect the Commits tab.\n\n- [x] Native components\n- [x] No network or GitHub writes",
            "baseRefOid": "1111111111111111111111111111111111111111", "baseRef": null,
            "headRefOid": "2222222222222222222222222222222222222222", "changedFiles": 4, "additions": 120, "deletions": 24,
            "mergeable": "MERGEABLE", "reviewDecision": null,
            "repository": { "id": "preview-repository", "nameWithOwner": "example/review-preview", "url": "https://github.com/example/review-preview" },
            "headRepository": null, "commits": { "totalCount": 12 }, "labels": { "totalCount": 0, "nodes": [] },
            "reviewRequests": { "totalCount": 0, "nodes": [] }, "recentCommits": { "nodes": [] }
        })).expect("valid generic PR details fixture");
        self.description =
            Some(cx.new(|cx| Markdown::new(details.body.clone().into(), None, None, cx)));
        self.details = Some(details);
        let fixture = vec![
            ConversationActivity { id: "issue-comment-1".into(), created_at: "2026-10-20T10:00:00Z".into(), author_login: "alex".into(), action_label: "commented".into(), body: "The native controls should remain recognizable at every font size.\n\n**Review focus:** keyboard navigation and pending sync.".into(), location: None, anchor: None, url: None, children: Vec::new() },
            ConversationActivity { id: "review-1".into(), created_at: "2026-10-20T11:00:00Z".into(), author_login: "sam".into(), action_label: "approved these changes".into(), body: "The interaction changes are ready to review.".into(), location: None, anchor: None, url: None, children: vec![
                ConversationActivity { id: "thread-1".into(), created_at: "2026-10-20T10:40:00Z".into(), author_login: "sam".into(), action_label: "reviewed · Resolved".into(), body: "Preserve the selected file while the comment is sending.".into(), location: Some("src/review.rs:42".into()), anchor: None, url: None, children: vec![
                    ConversationActivity { id: "reply-1".into(), created_at: "2026-10-20T10:45:00Z".into(), author_login: "alex".into(), action_label: "replied".into(), body: "Updated the stable editor state and added coverage.".into(), location: None, anchor: None, url: None, children: Vec::new() }
                ] }
            ] },
        ];
        self.activity = reconcile_activity(std::mem::take(&mut self.activity), fixture, cx);
        self.commit_rows = commit_history_rows(&self.commits);
        self.rebuild_activity_rows();
        let _ = window;
    }

    fn show_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.showing_commits = true;
        if self.viewer.is_none() {
            cx.notify();
            return;
        }
        if self.commits_loading || !self.commits.is_empty() {
            cx.notify();
            return;
        }
        let Some(reference) = self.selected.clone() else {
            return;
        };
        let generation = self.detail_generation;
        let (client, guard) = GitHubClient::cancellable();
        self.commits_guard = Some(guard);
        self.commits_loading = true;
        self.commits_error = None;
        self.commits_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.pull_request_commits(&reference) })
                .await;
            this.update_in(cx, |this, _, cx| {
                if this.detail_generation != generation {
                    return;
                }
                this.commits_loading = false;
                match result {
                    Ok(commits) => {
                        this.commit_rows = commit_history_rows(&commits);
                        this.commits = commits;
                    }
                    Err(error) => this.commits_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_files_with_picker(false, window, cx);
    }

    fn open_files_with_picker(
        &mut self,
        open_picker: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_files_with_target(open_picker, None, window, cx);
    }

    fn open_activity_location(
        &mut self,
        anchor: ConversationAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_files_with_target(false, Some(anchor), window, cx);
    }

    fn open_files_with_target(
        &mut self,
        open_picker: bool,
        anchor: Option<ConversationAnchor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview {
            self.files_error =
                Some("Offline component preview: open a real PR to review files.".into());
            cx.notify();
            return;
        }
        let Some(details) = self.details.clone() else {
            return;
        };
        let Some(viewer) = self.viewer.clone() else {
            return;
        };
        let task = self.workspace.update(cx, |workspace, cx| {
            crate::review_view::ReviewView::open_pull_request(
                workspace, details, viewer, window, cx,
            )
        });
        match task {
            Ok(task) => {
                self.loading_files = true;
                self.files_error = None;
                self.files_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let result = task.await;
                    this.update_in(cx, |this, window, cx| {
                        let mut result = result;
                        this.loading_files = false;
                        if result.is_ok() {
                            if let Some(anchor) = anchor {
                                result = this.workspace.update(cx, |workspace, cx| {
                                    let Some(review) = workspace.active_item_as::<crate::review_view::ReviewView>(cx) else {
                                        anyhow::bail!("The PR files view is no longer active; reopen the discussion location");
                                    };
                                    review.update(cx, |review, cx| review.reveal_conversation_anchor(anchor, window, cx))
                                }).and_then(|result| result);
                            }
                        }
                        if result.is_ok() && open_picker {
                            this.workspace
                                .update(cx, |workspace, cx| {
                                    let review = workspace
                                        .active_item_as::<crate::review_view::ReviewView>(cx);
                                    if let Some(review) = review {
                                        review.update(cx, |review, cx| {
                                            review.show_commit_picker(window, cx)
                                        });
                                    }
                                })
                                .log_err();
                        }
                        this.files_error = result
                            .err()
                            .map(|error| format!("Unable to open PR files: {error:#}"));
                        cx.notify();
                    })
                    .log_err();
                }));
            }
            Err(error) => self.files_error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            self.search(false, window, cx);
            return;
        }
        self.files_task = None;
        self.loading_files = false;
        self.files_error = None;
        self.account_generation += 1;
        let generation = self.account_generation;
        self.list_generation += 1;
        self.detail_generation += 1;
        self.detail_guard = None;
        self.activity_guard = None;
        self.detail_task = None;
        self.activity_task = None;
        self.items.clear();
        self.details = None;
        self.commits.clear();
        self.commit_rows.clear();
        self.commits_task = None;
        self.commits_guard = None;
        self.commits_loading = false;
        self.commits_error = None;
        self.description = None;
        self.activity.clear();
        self.activity_cursor = ConversationCursor::default();
        self.activity_snapshot = ConversationPage::default();
        self.activity_rows.clear();
        self.conversation_list.reset(2);
        self.activity_warnings.clear();
        self.error = None;
        self.detail_error = None;
        self.activity_error = None;
        self.loading_details = self.selected.is_some();
        self.loading_activity = false;
        self.account_error = None;
        self.viewer = None;
        self.catalog_guard = None;
        self.catalog_task = None;
        self.catalog_progress_task = None;
        self.catalog.clear();
        let (client, guard) = GitHubClient::cancellable();
        self.account_guard = Some(guard);
        self.account_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { client.viewer() }).await;
            this.update_in(cx, |this, window, cx| {
                if this.account_generation != generation {
                    return;
                }
                match result {
                    Ok(viewer) => {
                        this.viewer = Some(viewer);
                        this.load_catalog(window, cx);
                        if let Some(reference) = this.selected.clone() {
                            this.load(reference, window, cx);
                        }
                    }
                    Err(error) => {
                        this.account_error = Some(error.to_string());
                        this.loading_details = false;
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn debounce_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.debounce_task = None;
        if self.search_input.read(cx).is_empty(cx) {
            self.search(false, window, cx);
            return;
        }
        self.debounce_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(275))
                .await;
            this.update_in(cx, |this, window, cx| this.search(false, window, cx))
                .log_err();
        }));
    }

    fn load_catalog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview {
            self.search(false, window, cx);
            return;
        }
        let Some(repository) = self.repository.clone() else {
            self.error =
                Some("Open a workspace with a GitHub repository to browse pull requests.".into());
            cx.notify();
            return;
        };
        self.list_generation += 1;
        let generation = self.list_generation;
        self.catalog_loading = true;
        self.catalog_error = None;
        let (sender, receiver) = async_channel::unbounded::<Vec<PullRequestSummary>>();
        self.catalog_progress_task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Ok(page) = receiver.recv().await {
                this.update_in(cx, |this, window, cx| {
                    if this.list_generation != generation || !this.catalog_loading {
                        return;
                    }
                    for item in page {
                        if let Some(existing) = this
                            .catalog
                            .iter_mut()
                            .find(|existing| existing.id == item.id)
                        {
                            *existing = item;
                        } else {
                            this.catalog.push(item);
                        }
                    }
                    this.search(false, window, cx);
                })
                .log_err();
            }
        }));
        let (client, guard) = GitHubClient::cancellable();
        self.catalog_guard = Some(guard);
        self.catalog_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.pull_request_catalog_with_progress(&repository, |page| {
                    if let Err(error) = sender.try_send(page) { log::debug!("PR catalog observer closed: {error}"); }
                }) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.list_generation != generation {
                    return;
                }
                this.catalog_loading = false;
                match result {
                    Ok(catalog) => {
                        this.catalog = catalog;
                        this.search(false, window, cx);
                    }
                    Err(error) => this.catalog_error = Some(format!("Could not finish loading pull requests. Showing {} loaded results. {error}", this.catalog.len())),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn search(&mut self, _more: bool, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search_input.read(cx).text(cx).trim().to_lowercase();
        self.error = if self.review_requested {
            self.catalog
                .iter()
                .find_map(|item| item.review_request_error.clone())
                .or_else(|| {
                    self.catalog_loading.then(|| {
                        "Review-request results are partial while team membership is verified…"
                            .into()
                    })
                })
        } else {
            None
        };
        let filter = PullRequestFilter {
            state: self.state.clone(),
            author: self.author.clone(),
            source: self.source.clone(),
            head_repository: self.head_repository.clone(),
            review_requested: self.review_requested.then(|| "viewer".into()),
            text,
            ..Default::default()
        };
        self.items = self
            .catalog
            .iter()
            .filter(|item| matches_catalog_filter(item, &filter))
            .cloned()
            .collect();
        cx.notify();
    }

    fn open_reference(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_none() {
            self.detail_error =
                Some("Connect a GitHub account before opening a pull request".into());
            cx.notify();
            return;
        }
        let repository = self.repository.clone();
        match PullRequestReference::parse(
            self.reference_input.read(cx).text(cx).trim(),
            repository.as_ref(),
        ) {
            Ok(reference)
                if repository.as_ref().is_some_and(|repository| {
                    repository
                        .full_name()
                        .eq_ignore_ascii_case(&reference.repository.full_name())
                }) =>
            {
                self.showing_commits = false;
                self.load(reference, window, cx)
            }
            Ok(_) => {
                self.detail_error = Some("This pull request belongs to another repository. Open that repository in a workspace first.".into());
                cx.notify();
            }
            Err(error) => {
                self.detail_error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    fn load(
        &mut self,
        reference: PullRequestReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview {
            self.selected = Some(reference);
            self.install_preview_details(window, cx);
            cx.notify();
            return;
        }
        self.files_task = None;
        self.loading_files = false;
        self.files_error = None;
        self.detail_generation += 1;
        let generation = self.detail_generation;
        let same_reference = self.selected.as_ref() == Some(&reference);
        self.selected = Some(reference.clone());
        if !same_reference {
            self.details = None;
        }
        self.commits.clear();
        self.commit_rows.clear();
        self.commits_task = None;
        self.commits_guard = None;
        self.commits_loading = false;
        self.commits_error = None;
        if !same_reference {
            self.description = None;
            self.activity.clear();
            self.activity_cursor = ConversationCursor::default();
            self.activity_snapshot = ConversationPage::default();
            self.activity_rows.clear();
            self.conversation_list.reset(2);
            self.activity_warnings.clear();
        }
        self.activity_cursor = ConversationCursor::default();
        self.activity_snapshot = ConversationPage::default();
        self.detail_error = None;
        self.activity_error = None;
        self.loading_details = true;
        self.loading_activity = false;
        self.activity_task = None;
        self.activity_guard = None;
        let (client, guard) = GitHubClient::cancellable();
        self.detail_guard = Some(guard);
        self.detail_task = Some(cx.spawn_in(window, async move |this, cx| {
            let task_reference = reference.clone();
            let result = cx
                .background_spawn(async move { client.pull_request(&task_reference) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if this.detail_generation != generation {
                    return;
                }
                this.loading_details = false;
                match result {
                    Ok(details) => {
                        let body = if details.body.trim().is_empty() {
                            "_No description provided._".to_string()
                        } else {
                            details.body.clone()
                        };
                        this.description = Some(cx.new(|cx| {
                            Markdown::new_with_options(
                                body.into(),
                                None,
                                None,
                                MarkdownOptions {
                                    parse_html: true,
                                    ..Default::default()
                                },
                                cx,
                            )
                        }));
                        this.details = Some(details);
                        if this.showing_commits {
                            this.show_commits(window, cx);
                        }
                        this.load_activity(reference, generation, window, cx);
                    }
                    Err(error) => this.detail_error = Some(error.to_string()),
                }
                cx.notify();
                cx.emit(ItemEvent::UpdateTab);
            })
            .log_err();
        }));
        cx.notify();
        cx.emit(ItemEvent::UpdateTab);
    }

    fn render_activity(
        entry: &ActivityEntry,
        starts_card: bool,
        ends_card: bool,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // A thread remains one visual card while each comment stays a
        // separately virtualized row. Replies share its edges, without nesting.
        let mut card = v_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .overflow_hidden()
            .border_l_1()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .when(starts_card, |card| card.rounded_t_md())
            .border_t_1()
            .when(ends_card, |card| card.border_b_1().rounded_b_md())
            .id(SharedString::from(entry.id.clone()))
            .child(
                v_flex()
                    .flex_shrink_0()
                    .w_full()
                    .min_w_0()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .bg(cx.theme().colors().surface_background)
                    .when_some(entry.location.clone(), |header, location| {
                        if let Some(anchor) = entry.anchor.clone() {
                            header.child(
                                Button::new(
                                    SharedString::from(format!("activity-location-{}", entry.id)),
                                    location,
                                )
                                .style(ButtonStyle::Transparent)
                                .label_size(LabelSize::Small)
                                .truncate(true)
                                .disabled(preview)
                                .tooltip(ui::Tooltip::text(
                                    "Open this discussion location in PR files",
                                ))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.open_activity_location(anchor.clone(), window, cx)
                                    },
                                )),
                            )
                        } else {
                            header.child(Label::new(location).size(LabelSize::Small).truncate())
                        }
                    })
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .flex_wrap()
                            .when(!entry.author_login.is_empty(), |header| {
                                header
                                    .child(
                                        Icon::new(IconName::Person)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(Label::new(entry.author_login.clone()))
                            })
                            .child(Label::new(entry.action_label.clone()).color(Color::Muted))
                            .child(
                                Label::new(activity_timestamp(&entry.created_at))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when_some(entry.url.clone(), |header, url| {
                                header.child(
                                    IconButton::new(
                                        SharedString::from(format!("activity-open-{}", entry.id)),
                                        IconName::ArrowUpRight,
                                    )
                                    .style(ButtonStyle::Outlined)
                                    .disabled(preview)
                                    .tooltip(ui::Tooltip::text("Open this conversation on GitHub"))
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                                )
                            }),
                    ),
            );
        if !entry.body_text.trim().is_empty() {
            card = card.child(div().flex_shrink_0().w_full().min_w_0().p_3().child(
                MarkdownElement::new(entry.body.clone(), hover_markdown_style(window, cx)),
            ));
        }
        card.into_any_element()
    }

    fn rebuild_activity_rows(&mut self) {
        fn walk(entries: &[ActivityEntry], prefix: &[usize], rows: &mut Vec<Vec<usize>>) {
            for (index, entry) in entries.iter().enumerate() {
                let mut path = prefix.to_vec();
                path.push(index);
                rows.push(path.clone());
                walk(&entry.children, &path, rows);
            }
        }
        self.activity_rows.clear();
        walk(&self.activity, &[], &mut self.activity_rows);
        let count = self.activity_rows.len() + 2;
        let old_count = self.conversation_list.item_count();
        // A later source page can reparent or reorder rows; invalidate heights while the caller restores its semantic scroll anchor.
        self.conversation_list.splice(0..old_count, count);
    }

    fn activity_at(&self, path: &[usize]) -> Option<&ActivityEntry> {
        let mut entries = self.activity.as_slice();
        let mut selected = None;
        for index in path {
            let entry = entries.get(*index)?;
            selected = Some(entry);
            entries = &entry.children;
        }
        selected
    }

    /// File discussions own their comments/replies, including orphan threads
    /// that arrive before their review summary on another source page.
    fn activity_group_path<'a>(&self, path: &'a [usize]) -> &'a [usize] {
        for depth in 1..=path.len() {
            let prefix = &path[..depth];
            if self
                .activity_at(prefix)
                .is_some_and(|entry| entry.location.is_some())
            {
                return prefix;
            }
        }
        path
    }

    fn load_activity(
        &mut self,
        reference: PullRequestReference,
        generation: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preview
            || self.loading_activity
            || !self.activity_cursor.has_more()
            || self.activity_error.is_some()
        {
            return;
        }
        let (Some(details), Some(viewer_id)) = (
            self.details.as_ref(),
            self.viewer
                .as_ref()
                .and_then(|viewer| viewer.node_id.clone()),
        ) else {
            return;
        };
        let Ok(base_tip_oid) = details.base_tip_oid() else {
            return;
        };
        let context = crate::github_review::ReviewContext {
            reference,
            identity: crate::review::PullRequestIdentity {
                host: "github.com".into(),
                repository_node_id: details.repository.id.clone(),
                pull_request_node_id: details.summary.id.clone(),
                viewer_node_id: viewer_id,
            },
            head_oid: details.head_ref_oid.clone(),
            base_tip_oid: base_tip_oid.into(),
        };
        let cursor = self.activity_cursor.clone();
        self.loading_activity = true;
        cx.notify();
        let (client, guard) = GitHubClient::cancellable();
        self.activity_guard = Some(guard);
        self.activity_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    client
                        .activity_page(&context, &cursor)
                        .map(|(page, cursor)| (page, cursor, context))
                })
                .await;
            this.update_in(cx, |this, _, cx| {
                if this.detail_generation != generation {
                    return;
                }
                this.loading_activity = false;
                match result {
                    Ok((page, cursor, context)) => {
                        let offset = this.conversation_list.logical_scroll_top();
                        let anchor_id = offset
                            .item_ix
                            .checked_sub(1)
                            .and_then(|index| this.activity_rows.get(index))
                            .and_then(|path| this.activity_at(path))
                            .map(|entry| entry.id.clone());
                        let activity = this.activity_snapshot.append(page, &context);
                        this.activity =
                            reconcile_activity(std::mem::take(&mut this.activity), activity, cx);
                        this.activity_cursor = cursor;
                        this.rebuild_activity_rows();
                        let restored_index = anchor_id
                            .as_ref()
                            .and_then(|anchor_id| {
                                this.activity_rows
                                    .iter()
                                    .position(|path| {
                                        this.activity_at(path)
                                            .is_some_and(|entry| &entry.id == anchor_id)
                                    })
                                    .map(|index| index + 1)
                            })
                            .unwrap_or(
                                offset
                                    .item_ix
                                    .min(this.conversation_list.item_count().saturating_sub(1)),
                            );
                        this.conversation_list.scroll_to(gpui::ListOffset {
                            item_ix: restored_index,
                            offset_in_item: offset.offset_in_item,
                        });
                    }
                    Err(error) => this.activity_error = Some(error.to_string()),
                }
                cx.notify();
            })
            .log_err();
        }));
    }
}

fn matches_catalog_filter(item: &PullRequestSummary, filter: &PullRequestFilter) -> bool {
    filter
        .state
        .as_ref()
        .is_none_or(|state| &item.state == state)
        && filter.author.as_ref().is_none_or(|author| {
            item.author
                .as_ref()
                .is_some_and(|user| &user.login == author)
        })
        && filter
            .source
            .as_ref()
            .is_none_or(|source| &item.head_ref_name == source)
        && filter
            .head_repository
            .as_ref()
            .is_none_or(|repository| item.head_repository_name.as_ref() == Some(repository))
        && (filter.review_requested.is_none() || item.review_requested_by_viewer)
        && (filter.text.is_empty()
            || item.title.to_lowercase().contains(&filter.text)
            || item.number.to_string() == filter.text.trim_start_matches('#'))
}

pub(crate) fn repository_from_remote(remote: &str) -> Option<RepositoryName> {
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))?;
    RepositoryName::parse(
        path.trim_end_matches('/')
            .strip_suffix(".git")
            .unwrap_or(path.trim_end_matches('/')),
    )
    .ok()
}

impl EventEmitter<ItemEvent> for PullRequestView {}
impl Focusable for PullRequestView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.reference_input.focus_handle(cx)
    }
}
impl Item for PullRequestView {
    type Event = ItemEvent;
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        if self.preview {
            return "PR Review · Offline Preview".into();
        }
        self.details
            .as_ref()
            .map(|details| {
                format!("#{} · {}", details.summary.number, details.summary.title).into()
            })
            .unwrap_or_else(|| "Pull Requests".into())
    }
    fn to_item_events(event: &ItemEvent, callback: &mut dyn FnMut(ItemEvent)) {
        callback(*event);
    }
}

impl PullRequestView {
    fn state_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> DropdownMenu {
        let weak = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            for (label, state) in [
                ("Open", Some("OPEN")),
                ("Closed", Some("CLOSED")),
                ("Merged", Some("MERGED")),
                ("All states", None),
            ] {
                let weak = weak.clone();
                menu = menu.toggleable_entry(
                    label,
                    self.state.as_deref() == state,
                    ui::IconPosition::Start,
                    None,
                    move |window, cx| {
                        weak.update(cx, |this, cx| {
                            this.state = state.map(str::to_string);
                            this.search(false, window, cx);
                        })
                        .log_err();
                    },
                );
            }
            menu
        });
        let label = match self.state.as_deref() {
            Some("OPEN") => "Open",
            Some("CLOSED") => "Closed",
            Some("MERGED") => "Merged",
            _ => "All states",
        };
        DropdownMenu::new("pr-state-filter", label, menu)
            .style(ui::DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Large)
            .full_width(true)
            .attach(gpui::Anchor::BottomLeft)
            .offset(gpui::point(px(0.), px(6.)))
    }

    fn facet_picker(&self, kind: FacetKind, cx: &mut Context<Self>) -> AnyElement {
        let mut choices: Vec<FacetChoice> = match kind {
            FacetKind::Author => self
                .catalog
                .iter()
                .filter_map(|item| {
                    item.author.as_ref().map(|user| FacetChoice {
                        label: user.login.clone(),
                        value: Some(user.login.clone()),
                        repository: None,
                    })
                })
                .collect(),
            FacetKind::Source => self
                .catalog
                .iter()
                .map(|item| FacetChoice {
                    label: item
                        .head_repository_name
                        .as_ref()
                        .map(|repository| format!("{} · {}", item.head_ref_name, repository))
                        .unwrap_or_else(|| item.head_ref_name.clone()),
                    value: Some(item.head_ref_name.clone()),
                    repository: item.head_repository_name.clone(),
                })
                .collect(),
        };
        choices.sort_by(|left, right| left.label.cmp(&right.label));
        choices.dedup_by(|left, right| left.label == right.label);
        choices.insert(
            0,
            FacetChoice {
                label: match kind {
                    FacetKind::Author => "Any author",
                    FacetKind::Source => "Any source branch",
                }
                .into(),
                value: None,
                repository: None,
            },
        );
        let label = match kind {
            FacetKind::Author => self.author.as_deref().unwrap_or("Any author"),
            FacetKind::Source => self.source.as_deref().unwrap_or("Any source branch"),
        }
        .to_string();
        let weak = cx.entity().downgrade();
        ui::PopoverMenu::new(match kind {
            FacetKind::Author => "pr-author-picker",
            FacetKind::Source => "pr-source-picker",
        })
        .trigger(
            Button::new("facet-trigger", label)
                .style(ButtonStyle::Outlined)
                .size(ButtonSize::Large)
                .end_icon(Icon::new(IconName::ChevronDown)),
        )
        .menu(move |window, cx| {
            Some(cx.new(|cx| {
                let delegate = FacetDelegate {
                    view: weak.clone(),
                    kind,
                    matches: choices.clone(),
                    choices: choices.clone(),
                    selected: 0,
                };
                Picker::uniform_list(delegate, window, cx)
                    .initial_width(rems(24.))
                    .max_height(rems(24.))
                    .show_scrollbar(true)
                    .popover()
            }))
        })
        .anchor(gpui::Anchor::TopLeft)
        .attach(gpui::Anchor::BottomLeft)
        .offset(gpui::point(px(0.), px(6.)))
        .full_width(true)
        .into_any_element()
    }
}

impl Render for PullRequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let repository = self
            .repository
            .as_ref()
            .map(RepositoryName::full_name)
            .unwrap_or_else(|| "No GitHub repository in this workspace".into());
        let list = uniform_list(
            "pull-request-results",
            self.items.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .filter_map(|index| {
                        let item = this.items.get(index)?;
                        let reference = PullRequestReference {
                            repository: this.repository.clone()?,
                            number: item.number,
                        };
                        let selected = this.selected.as_ref() == Some(&reference);
                        let author = item
                            .author
                            .as_ref()
                            .map(|author| author.login.as_str())
                            .unwrap_or("Deleted account");
                        Some(
                            div()
                                .w_full()
                                .px_3()
                                .py_1()
                                .border_b_1()
                                .border_color(cx.theme().colors().border_variant)
                                .child(
                                    ListItem::new(("pull-request-result", index))
                                        .spacing(ListItemSpacing::Sparse)
                                        .inset(true)
                                        .toggle_state(selected)
                                        .tooltip(ui::Tooltip::text(item.title.clone()))
                                        .start_slot::<Icon>(Some(
                                            Icon::new(IconName::GitBranch).color(
                                                match item.state.as_str() {
                                                    "OPEN" => Color::Success,
                                                    "MERGED" => Color::Accent,
                                                    _ => Color::Muted,
                                                },
                                            ),
                                        ))
                                        .child(
                                            v_flex()
                                                .w_full()
                                                .min_w_0()
                                                .gap_2()
                                                .child(Label::new(item.title.clone()).truncate())
                                                .child(
                                                    Label::new(format!(
                                                        "#{} · {} · {}",
                                                        item.number,
                                                        author,
                                                        if item.is_draft {
                                                            "Draft"
                                                        } else {
                                                            &item.state
                                                        }
                                                    ))
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(format!(
                                                        "{} → {}",
                                                        item.head_ref_name, item.base_ref_name
                                                    ))
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .truncate(),
                                                ),
                                        )
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.showing_commits = false;
                                            this.load(reference.clone(), window, cx);
                                        })),
                                ),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0();
        let sidebar =
            v_flex()
                .w_full()
                .min_h_0()
                .h_full()
                .border_r_1()
                .border_color(cx.theme().colors().border)
                .child(h_flex().p_3().justify_between().child(Label::new(
                    if self.viewer.is_none() {
                        if self.account_error.is_some() {
                            "GitHub account unavailable".to_string()
                        } else {
                            "Connecting to GitHub…".to_string()
                        }
                    } else {
                        format!("{} pull requests", self.items.len())
                    },
                )))
                .when(self.catalog_loading, |column| {
                    column.child(
                    div().p_3().child(
                        Label::new(
                            "Loading all pull requests and filter choices… Results are partial.",
                        )
                        .color(Color::Muted),
                    ),
                )
                })
                .when_some(
                    self.catalog_error.clone().or(self.error.clone()),
                    |column, error| {
                        column.child(
                            v_flex()
                                .p_3()
                                .gap_2()
                                .child(Label::new(error).color(Color::Error))
                                .child(
                                    h_flex().child(
                                        Button::new("retry-pr-list", "Retry")
                                            .style(ButtonStyle::Outlined)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.load_catalog(window, cx)
                                            })),
                                    ),
                                ),
                        )
                    },
                )
                .when(
                    self.viewer.is_some()
                        && !self.catalog_loading
                        && self.items.is_empty()
                        && self.catalog_error.is_none(),
                    |column| {
                        column.child(
                            div().p_3().child(
                                Label::new("No matching pull requests.").color(Color::Muted),
                            ),
                        )
                    },
                )
                .child(list);
        let mut content = v_flex()
            .id("pull-request-content")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .p_4()
            .gap_4();
        if let Some(error) = self.detail_error.clone() {
            content = content.child(Label::new(error).color(Color::Error));
        }
        if self.loading_details {
            content = content.child(
                Label::new(if self.viewer.is_none() {
                    "Connecting to GitHub…"
                } else {
                    "Loading pull request…"
                })
                .color(Color::Muted),
            );
        }
        if self.selected.is_some() {
            if let Some(error) = self.account_error.clone() {
                content = content.child(Label::new(error).color(Color::Error)).child(
                    h_flex().child(
                        Button::new("pr-retry-account", "Reconnect GitHub")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
                    ),
                );
            }
        }
        if let Some(details) = self.details.as_ref() {
            if self.showing_commits {
                content = content.child(
                    h_flex()
                        .justify_between()
                        .child(
                            Label::new(format!("{} commits", details.commits.total_count))
                                .size(LabelSize::Large),
                        )
                        .child(
                            Button::new("pr-compare-commits", "Compare commits…")
                                .style(ButtonStyle::Outlined)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_files_with_picker(true, window, cx)
                                })),
                        ),
                );
                if self.commits_loading {
                    content = content
                        .child(Label::new("Loading complete commit history…").color(Color::Muted));
                }
                if let Some(error) = self.commits_error.clone() {
                    content =
                        content.child(Label::new(error).color(Color::Error)).child(
                            h_flex().child(
                                Button::new("retry-commits", "Retry")
                                    .style(ButtonStyle::Outlined)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_commits(window, cx)
                                    })),
                            ),
                        );
                }
                let history = self.commit_rows.clone();
                let commits = uniform_list(
                    "pr-commit-history",
                    history.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|row| {
                                let entry = history.get(row)?;
                                if let CommitHistoryRow::Date(date) = entry {
                                    return Some(
                                        h_flex()
                                            .h(rems(4.))
                                            .w_full()
                                            .items_end()
                                            .pb_2()
                                            .child(
                                                Icon::new(IconName::GitCommit).color(Color::Muted),
                                            )
                                            .child(
                                                Label::new(format!("Committed on {date}"))
                                                    .color(Color::Muted),
                                            )
                                            .gap_2()
                                            .into_any_element(),
                                    );
                                }
                                let CommitHistoryRow::Commit(index) = entry else {
                                    return None;
                                };
                                let index = *index;
                                let commit = this.commits.get(index)?;
                                let oid = commit.oid.clone();
                                let copy_oid = oid.clone();
                                let open_oid = oid.clone();
                                let url = this.repository.as_ref().map(|repository| {
                                    format!(
                                        "https://github.com/{}/commit/{oid}",
                                        repository.full_name()
                                    )
                                });
                                let preview = this.preview;
                                Some(
                                    h_flex()
                                        .h(rems(4.))
                                        .w_full()
                                        .min_w_0()
                                        .px_3()
                                        .gap_3()
                                        .border_b_1()
                                        .border_color(cx.theme().colors().border_variant)
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_1()
                                                .child(
                                                    Label::new(commit.message_headline.clone())
                                                        .truncate(),
                                                )
                                                .child(
                                                    Label::new(format!(
                                                        "{} · committed {}",
                                                        commit
                                                            .author_name
                                                            .as_deref()
                                                            .unwrap_or("Unknown author"),
                                                        activity_timestamp(&commit.committed_date)
                                                    ))
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .truncate(),
                                                ),
                                        )
                                        .child(
                                            Button::new(
                                                ("pr-commit-native", index),
                                                oid.chars().take(8).collect::<String>(),
                                            )
                                            .style(ButtonStyle::Outlined)
                                            .disabled(preview)
                                            .tooltip(ui::Tooltip::text("Open commit in Zed"))
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    let repository = this
                                                        .workspace
                                                        .upgrade()
                                                        .and_then(|workspace| {
                                                            workspace
                                                                .read(cx)
                                                                .project()
                                                                .read(cx)
                                                                .active_repository(cx)
                                                        });
                                                    if let Some(repository) = repository {
                                                        crate::commit_view::CommitView::open(
                                                            open_oid.clone(),
                                                            repository.downgrade(),
                                                            this.workspace.clone(),
                                                            None,
                                                            None,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                }),
                                            ),
                                        )
                                        .child(
                                            IconButton::new(
                                                ("pr-copy-commit", index),
                                                if this.copied_commit.as_deref()
                                                    == Some(oid.as_str())
                                                {
                                                    IconName::Check
                                                } else {
                                                    IconName::Copy
                                                },
                                            )
                                            .style(ButtonStyle::Outlined)
                                            .tooltip(ui::Tooltip::text("Copy full commit SHA"))
                                            .on_click(
                                                cx.listener(move |this, _, _, cx| {
                                                    cx.write_to_clipboard(
                                                        gpui::ClipboardItem::new_string(
                                                            copy_oid.clone(),
                                                        ),
                                                    );
                                                    this.copied_commit = Some(copy_oid.clone());
                                                    cx.notify();
                                                }),
                                            ),
                                        )
                                        .when_some(url, |row, url| {
                                            row.child(
                                                IconButton::new(
                                                    ("pr-open-commit", index),
                                                    IconName::ArrowUpRight,
                                                )
                                                .style(ButtonStyle::Outlined)
                                                .disabled(preview)
                                                .tooltip(ui::Tooltip::text("Open commit on GitHub"))
                                                .on_click(move |_, _, cx| cx.open_url(&url)),
                                            )
                                        })
                                        .into_any_element(),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.commit_scroll)
                .flex_1()
                .min_h_0()
                .w_full();
                content = content.child(commits);
            } else {
                content = content.child(
                    variable_height_list(
                        self.conversation_list.clone(),
                        cx.processor(|this, index: usize, window, cx| {
                            if index == 0 {
                                let mut header = v_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_4()
                                    .pb_4()
                                    .child(Label::new("Description").size(LabelSize::Large));
                                if let Some(description) = this.description.as_ref() {
                                    header = header.child(MarkdownElement::new(
                                        description.clone(),
                                        hover_markdown_style(window, cx),
                                    ));
                                }
                                return header
                                    .child(
                                        Label::new("Conversation and reviews")
                                            .size(LabelSize::Large),
                                    )
                                    .into_any_element();
                            }
                            if let Some(path) = this.activity_rows.get(index - 1) {
                                if let Some(entry) = this.activity_at(path.as_slice()) {
                                    let group = this.activity_group_path(path);
                                    let starts_card = index == 1
                                        || this.activity_rows.get(index - 2).is_none_or(
                                            |previous| this.activity_group_path(previous) != group,
                                        );
                                    let ends_card = this
                                        .activity_rows
                                        .get(index)
                                        .is_none_or(|next| this.activity_group_path(next) != group);
                                    return div()
                                        .w_full()
                                        .min_w_0()
                                        .when(ends_card, |row| row.pb_3())
                                        .child(Self::render_activity(
                                            entry,
                                            starts_card,
                                            ends_card,
                                            this.preview,
                                            window,
                                            cx,
                                        ))
                                        .into_any_element();
                                }
                            }
                            let mut footer = v_flex().w_full().gap_2().py_3();
                            if this.loading_activity {
                                footer = footer
                                    .child(Label::new("Loading conversation…").color(Color::Muted));
                            } else if let Some(error) = this.activity_error.clone() {
                                footer = footer.child(Label::new(error).color(Color::Error)).child(
                                    Button::new("retry-conversation", "Retry loading conversation")
                                        .style(ButtonStyle::Outlined)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.activity_error = None;
                                            if let Some(reference) = this.selected.clone() {
                                                this.load_activity(
                                                    reference,
                                                    this.detail_generation,
                                                    window,
                                                    cx,
                                                );
                                            }
                                        })),
                                );
                            } else if this.activity_cursor.has_more() && !this.preview {
                                footer = footer.child(
                                    Button::new("more-conversation", "Load more conversation")
                                        .style(ButtonStyle::Outlined)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            if let Some(reference) = this.selected.clone() {
                                                this.load_activity(
                                                    reference,
                                                    this.detail_generation,
                                                    window,
                                                    cx,
                                                );
                                            }
                                        })),
                                );
                            } else if this.activity.is_empty() {
                                footer = footer
                                    .child(Label::new("No conversation yet").color(Color::Muted));
                            }
                            footer.into_any_element()
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .w_full(),
                );
            }
        } else if !self.loading_details && self.selected.is_none() {
            content = content.child(v_flex().gap_3().child(Label::new("Choose a pull request to start reviewing").size(LabelSize::Large)).child(Label::new("Browse this repository or open a pull request using its URL or number.").color(Color::Muted)));
        }
        let mut review = v_flex().flex_1().min_w_0().min_h_0();
        if self.selected.is_some() {
            review = review.child(
                h_flex().p_3().child(
                    Button::new("pr-back-to-list", "Pull requests")
                        .style(ButtonStyle::Outlined)
                        .start_icon(Icon::new(IconName::ArrowLeft))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_selection(cx))),
                ),
            );
        }
        if self.details.is_none() {
            if let Some(reference) = self.selected.as_ref() {
                review = review.child(
                    v_flex()
                        .p_4()
                        .gap_3()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            Label::new(format!("Pull request #{}", reference.number))
                                .size(LabelSize::Large),
                        )
                        .child(Label::new(reference.repository.full_name()).color(Color::Muted))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("pr-loading-conversation", "Conversation")
                                        .style(ButtonStyle::Outlined)
                                        .size(ButtonSize::Large)
                                        .toggle_state(!self.showing_commits)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.showing_commits = false;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("pr-loading-commits", "Commits")
                                        .style(ButtonStyle::Outlined)
                                        .size(ButtonSize::Large)
                                        .toggle_state(self.showing_commits)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.show_commits(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("pr-loading-files", "Files changed")
                                        .style(ButtonStyle::Outlined)
                                        .size(ButtonSize::Large)
                                        .disabled(true),
                                ),
                        ),
                );
            }
        }
        if let Some(details) = self.details.as_ref() {
            let reference = self.selected.clone();
            let number = details.summary.number;
            let commits = details.commits.total_count;
            let files = details.changed_files;
            review = review.child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .id("pr-title-heading")
                                    .flex_1()
                                    .min_w_0()
                                    .tooltip(ui::Tooltip::text(details.summary.title.clone()))
                                    .child(
                                        Label::new(format!(
                                            "{}  #{}",
                                            details.summary.title, number
                                        ))
                                        .size(LabelSize::Large)
                                        .truncate(),
                                    ),
                            )
                            .child(
                                div().flex_shrink_0().child(
                                    Button::new("pr-refresh", "Refresh")
                                        .style(ButtonStyle::Outlined)
                                        .start_icon(Icon::new(IconName::ArrowCircle))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if let Some(reference) = reference.clone() {
                                                this.load(reference, window, cx);
                                            }
                                        })),
                                ),
                            ),
                    )
                    .child(
                        div().min_w_0().w_full().child(
                            Label::new(format!(
                                "{} · {} · {} → {}",
                                if details.summary.is_draft {
                                    "Draft"
                                } else {
                                    &details.summary.state
                                },
                                details
                                    .summary
                                    .author
                                    .as_ref()
                                    .map(|user| user.login.as_str())
                                    .unwrap_or("Deleted account"),
                                details.summary.head_ref_name,
                                details.summary.base_ref_name
                            ))
                            .color(Color::Muted)
                            .truncate(),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("pr-conversation-tab", "Conversation")
                                    .style(ButtonStyle::Outlined)
                                    .toggle_state(!self.showing_commits)
                                    .size(ButtonSize::Large)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.showing_commits = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("pr-commits-tab", format!("Commits ({commits})"))
                                    .style(ButtonStyle::Outlined)
                                    .toggle_state(self.showing_commits)
                                    .size(ButtonSize::Large)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.show_commits(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("pr-files-tab", format!("Files changed ({files})"))
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Large)
                                    .loading(self.loading_files)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_files(window, cx)
                                    })),
                            ),
                    )
                    .when(self.loading_files, |column| {
                        column.child(
                            Label::new(
                                "Loading files… You can continue reading this pull request.",
                            )
                            .color(Color::Muted),
                        )
                    })
                    .when_some(self.files_error.clone(), |column, error| {
                        column.child(Label::new(error).color(Color::Error))
                    }),
            );
        }
        review = review.child(content);
        let weak = cx.entity().downgrade();
        let filter_count = usize::from(self.author.is_some())
            + usize::from(self.source.is_some())
            + usize::from(self.review_requested)
            + usize::from(self.state.as_deref() != Some("OPEN"));
        let filters = ui::PopoverMenu::new("pr-filters-popover")
            .trigger(
                Button::new(
                    "pr-filters",
                    if filter_count == 0 {
                        "Filters".to_string()
                    } else {
                        format!("Filters ({filter_count})")
                    },
                )
                .style(ButtonStyle::Outlined)
                .size(ButtonSize::Large)
                .start_icon(Icon::new(IconName::Filter))
                .end_icon(Icon::new(IconName::ChevronDown)),
            )
            .menu(move |_, cx| {
                Some(cx.new(|cx| {
                    let subscription = weak
                        .upgrade()
                        .map(|view| cx.observe(&view, |_, _, cx| cx.notify()));
                    FiltersPopover {
                        view: weak.clone(),
                        focus: cx.focus_handle(),
                        _subscription: subscription,
                    }
                }))
            })
            .anchor(gpui::Anchor::TopLeft)
            .attach(gpui::Anchor::BottomLeft)
            .offset(gpui::point(px(0.), px(6.)));
        let main = v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .when(self.selected.is_none(), |root| {
                root.child(
                    v_flex()
                        .p_3()
                        .gap_3()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_3()
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(Label::new("Pull requests").size(LabelSize::Large))
                                        .child(Label::new(repository).color(Color::Muted)),
                                )
                                .child(
                                    Button::new(
                                        "pr-connect",
                                        self.viewer
                                            .as_ref()
                                            .map(|user| format!("{} · Refresh list", user.login))
                                            .unwrap_or_else(|| {
                                                if self.account_error.is_some() {
                                                    "Reconnect GitHub".into()
                                                } else {
                                                    "Connecting GitHub…".into()
                                                }
                                            }),
                                    )
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Large)
                                    .disabled(self.viewer.is_none() && self.account_error.is_none())
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.connect(window, cx)),
                                    ),
                                ),
                        )
                        .when_some(self.account_error.clone(), |column, error| {
                            column.child(
                                Label::new(format!("{error}. Run gh auth login, then reconnect."))
                                    .color(Color::Error),
                            )
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(self.reference_input.clone())
                                        .on_action(cx.listener(
                                            |this, _: &menu::Confirm, window, cx| {
                                                this.open_reference(window, cx)
                                            },
                                        )),
                                )
                                .child(
                                    Button::new("pr-open", "Open PR")
                                        .style(ButtonStyle::Filled)
                                        .size(ButtonSize::Large)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.open_reference(window, cx)
                                        })),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .flex_wrap()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(rems(16.))
                                        .child(self.search_input.clone()),
                                )
                                .child(filters),
                        ),
                )
            })
            .child(if self.selected.is_some() {
                review.into_any_element()
            } else {
                sidebar.into_any_element()
            });
        h_flex()
            .size_full()
            .child(div().flex_1().min_w_0().h_full().child(main))
            .when_some(self.preview_panel.clone(), |root, panel| {
                root.child(
                    div()
                        .w(rems(30.))
                        .max_w(window.viewport_size().width * 0.55)
                        .h_full()
                        .child(panel),
                )
            })
    }
}

struct FiltersPopover {
    view: WeakEntity<PullRequestView>,
    focus: FocusHandle,
    _subscription: Option<Subscription>,
}
impl Focusable for FiltersPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EventEmitter<DismissEvent> for FiltersPopover {}
impl Render for FiltersPopover {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let controls = self
            .view
            .update(cx, |view, cx| {
                let state = view.state_menu(window, cx);
                let author = view.facet_picker(FacetKind::Author, cx);
                let source = view.facet_picker(FacetKind::Source, cx);
                v_flex()
                    .track_focus(&self.focus)
                    .w(rems(26.))
                    .max_w((window.viewport_size().width - px(32.)).max(px(0.)))
                    .p_3()
                    .gap_3()
                    .rounded_md()
                    .bg(cx.theme().colors().elevated_surface_background)
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new("Filter pull requests").size(LabelSize::Large))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new("State").color(Color::Muted))
                            .child(state),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new("Author").color(Color::Muted))
                            .child(author),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(Label::new("Source branch").color(Color::Muted))
                            .child(source),
                    )
                    .when(view.catalog_loading, |column| {
                        column.child(
                            Label::new("Loading complete filter choices…")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Button::new("pr-requested-filter", "Review requested from me")
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Large)
                            .toggle_state(view.review_requested)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.review_requested = !view.review_requested;
                                view.search(false, window, cx);
                            })),
                    )
                    .child(
                        Button::new("pr-reset-filters", "Reset filters")
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Large)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.author = None;
                                view.source = None;
                                view.head_repository = None;
                                view.review_requested = false;
                                view.state = Some("OPEN".into());
                                view.search(false, window, cx);
                            })),
                    )
                    .into_any_element()
            })
            .unwrap_or_else(|_| div().into_any_element());
        div()
            .key_context("PullRequestFilters")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(controls)
    }
}

#[derive(Clone, Copy)]
enum FacetKind {
    Author,
    Source,
}
#[derive(Clone)]
struct FacetChoice {
    label: String,
    value: Option<String>,
    repository: Option<String>,
}
struct FacetDelegate {
    view: WeakEntity<PullRequestView>,
    kind: FacetKind,
    choices: Vec<FacetChoice>,
    matches: Vec<FacetChoice>,
    selected: usize,
}
impl PickerDelegate for FacetDelegate {
    type ListItem = ListItem;
    fn name() -> &'static str {
        "pull request filter"
    }
    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        match self.kind {
            FacetKind::Author => "Search authors…",
            FacetKind::Source => "Search source branches…",
        }
        .into()
    }
    fn match_count(&self) -> usize {
        self.matches.len()
    }
    fn selected_index(&self) -> usize {
        self.selected
    }
    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = index;
    }
    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matches = self
            .choices
            .iter()
            .filter(|choice| choice.label.to_lowercase().contains(&query))
            .cloned()
            .collect();
        self.selected = 0;
        cx.notify();
        Task::ready(())
    }
    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(choice) = self.matches.get(self.selected).cloned() {
            self.view
                .update(cx, |view, cx| {
                    match self.kind {
                        FacetKind::Author => view.author = choice.value,
                        FacetKind::Source => {
                            view.source = choice.value;
                            view.head_repository = choice.repository;
                        }
                    }
                    view.search(false, window, cx);
                })
                .log_err();
            cx.emit(DismissEvent);
        }
    }
    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }
    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<ListItem> {
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(
                    div().flex_1().min_w_0().overflow_hidden().child(
                        Label::new(self.matches.get(index)?.label.clone())
                            .single_line()
                            .truncate(),
                    ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[gpui::test]
    async fn overview_and_commits_activation_reuse_loaded_tab_without_requests(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let fs = project::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/review-project",
            serde_json::json!({".git":{},"example.rs":"fn main() {}"}),
        )
        .await;
        let project =
            project::Project::test(fs, [std::path::Path::new("/review-project")], cx).await;
        let (multi, cx) = cx
            .add_window_view(|window, cx| workspace::MultiWorkspace::test_new(project, window, cx));
        let workspace = multi.read_with(cx, |multi, _| multi.workspace().clone());
        let reference =
            PullRequestReference::parse("https://github.com/example/review-preview/pull/1", None)
                .unwrap();
        let overview = workspace.update_in(cx, |workspace, window, cx| {
            let overview = cx.new(|cx| {
                let mut view = PullRequestView::new(
                    workspace.weak_handle(),
                    Some(reference.repository.clone()),
                    window,
                    cx,
                );
                view.preview = true;
                view.install_preview(window, cx);
                view.selected = Some(reference.clone());
                view.install_preview_details(window, cx);
                view.preview = false;
                view
            });
            workspace.add_item_to_active_pane(Box::new(overview.clone()), None, true, window, cx);
            overview
        });
        let before = overview.read_with(cx, |view, _| {
            (
                view.detail_generation,
                view.description.as_ref().unwrap().entity_id(),
                view.activity_rows.clone(),
                view.commits.len(),
            )
        });
        for _ in 0..3 {
            workspace.update_in(cx, |workspace, window, cx| {
                PullRequestView::open_overview(workspace, reference.clone(), window, cx)
            });
            overview.read_with(cx, |view, _| {
                assert!(!view.showing_commits);
                assert_eq!(view.detail_generation, before.0);
                assert_eq!(view.description.as_ref().unwrap().entity_id(), before.1);
                assert_eq!(view.activity_rows, before.2);
                assert!(
                    view.detail_task.is_none()
                        && view.activity_task.is_none()
                        && view.commits_task.is_none()
                );
            });
            workspace.update_in(cx, |workspace, window, cx| {
                PullRequestView::open_commits(workspace, reference.clone(), window, cx)
            });
            overview.read_with(cx, |view, _| {
                assert!(view.showing_commits);
                assert_eq!(view.detail_generation, before.0);
                assert_eq!(view.commits.len(), before.3);
                assert!(
                    view.detail_task.is_none()
                        && view.activity_task.is_none()
                        && view.commits_task.is_none()
                );
            });
        }
        assert_eq!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_item_as::<PullRequestView>(cx)
                .unwrap()
                .entity_id()),
            overview.entity_id()
        );
        overview.update_in(cx, |view, window, cx| {
            view.load(reference, window, cx);
            assert_eq!(
                view.detail_generation,
                before.0 + 1,
                "explicit refresh still starts a new request generation"
            );
            assert!(view.detail_task.is_some());
            view.cancel_navigation_test_requests();
        });
    }

    #[gpui::test]
    fn account_loading_preserves_selected_review_and_defers_commit_reads(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let reference =
            PullRequestReference::parse("https://github.com/example/repository/pull/12", None)
                .expect("reference");
        let (view, cx) = cx.add_window_view(|window, cx| {
            PullRequestView::new(
                WeakEntity::new_invalid(),
                Some(reference.repository.clone()),
                window,
                cx,
            )
        });
        view.update_in(cx, |view, window, cx| {
            view.selected = Some(reference.clone());
            view.connect(window, cx);
            assert_eq!(view.selected.as_ref(), Some(&reference));
            assert!(view.loading_details);
            assert!(view.viewer.is_none());
            view.show_commits(window, cx);
            assert!(view.showing_commits);
            assert!(
                view.commits_task.is_none(),
                "commit reads wait for authentication"
            );
            view.clear_selection(cx);
            assert!(
                view.selected.is_none(),
                "Back remains authoritative while authentication is pending"
            );
            view.cancel_navigation_test_requests();
        });
    }

    #[gpui::test]
    fn offline_component_preview_uses_actual_views_without_requests(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
        let (view, cx) = cx.add_window_view(|window, cx| {
            let mut view = PullRequestView::new(
                WeakEntity::new_invalid(),
                Some(RepositoryName {
                    owner: "example".into(),
                    name: "review-preview".into(),
                }),
                window,
                cx,
            );
            view.preview = true;
            view.install_preview(window, cx);
            view
        });
        view.update_in(cx, |view, window, cx| {
            assert_eq!(view.items.len(), 7);
            view.connect(window, cx);
            view.load_catalog(window, cx);
            view.load(
                PullRequestReference::parse(
                    "https://github.com/example/review-preview/pull/1",
                    None,
                )
                .unwrap(),
                window,
                cx,
            );
            view.show_commits(window, cx);
            assert_eq!(view.commits.len(), 12);
            assert_eq!(view.activity.len(), 2);
            assert_eq!(
                view.activity[1].children[0].location.as_deref(),
                Some("src/review.rs:42")
            );
            assert_eq!(
                view.activity[1].children[0].children[0].author_login,
                "alex"
            );
            assert_eq!(
                view.activity_rows.len(),
                4,
                "review, thread and reply retain separate virtual rows inside shared visual cards"
            );
            assert_eq!(
                view.conversation_list.item_count(),
                6,
                "description and paging footer bracket activity"
            );
            assert_eq!(
                view.activity_group_path(&[1]),
                &[1],
                "review summary owns its own card"
            );
            assert_eq!(view.activity_group_path(&[1, 0]), &[1, 0]);
            assert_eq!(
                view.activity_group_path(&[1, 0, 0]),
                &[1, 0],
                "reply shares the file discussion card"
            );
            // A thread can arrive before its review page; grouping must also
            // work at the top level and must never merge unrelated comments.
            let thread = view.activity[1].children.remove(0);
            view.activity.push(thread);
            assert_eq!(view.activity_group_path(&[2, 0]), &[2]);
            assert_eq!(view.activity_group_path(&[0]), &[0]);
            let thread = view.activity.pop().unwrap();
            view.activity[1].children.push(thread);
            let paths = view.activity_rows.clone();
            let before_commits = view
                .commits
                .iter()
                .map(|commit| commit.oid.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                view.commit_rows.iter().find_map(|row| match row {
                    CommitHistoryRow::Commit(index) => Some(*index),
                    _ => None,
                }),
                Some(11)
            );
            view.showing_commits = false;
            assert_eq!(
                view.activity_rows, paths,
                "warm tab retains loaded conversation"
            );
            assert_eq!(
                view.commits
                    .iter()
                    .map(|commit| commit.oid.clone())
                    .collect::<Vec<_>>(),
                before_commits,
                "display ordering never mutates backend history"
            );
            let first_body = view.activity[0].body.entity_id();
            let reply_body = view.activity[1].children[0].children[0].body.entity_id();
            view.install_preview_details(window, cx);
            assert_eq!(first_body, view.activity[0].body.entity_id());
            assert_eq!(
                reply_body,
                view.activity[1].children[0].children[0].body.entity_id()
            );
            view.open_files(window, cx);
            assert!(
                view.files_error
                    .as_deref()
                    .unwrap()
                    .contains("Offline component preview")
            );
            assert!(view.account_task.is_none());
            assert!(view.catalog_task.is_none());
            assert!(view.detail_task.is_none());
            assert!(view.activity_task.is_none());
            assert!(view.files_task.is_none());
            assert!(view.commits_task.is_none());
        });
    }

    #[test]
    fn commit_date_headers_reverse_display_without_mutating_api_order() {
        let commits = (0..2055)
            .map(
                |index| crate::github_pull_requests::PullRequestCommitSummary {
                    oid: format!("{index:040x}"),
                    message_headline: "Subject".into(),
                    author_name: None,
                    committed_date: format!("2026-10-{:02}T12:00:00Z", 20 - index / 1000),
                },
            )
            .collect::<Vec<_>>();
        let rows = commit_history_rows(&commits);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, CommitHistoryRow::Date(_)))
                .count(),
            3
        );
        assert_eq!(
            rows.iter()
                .filter_map(|row| if let CommitHistoryRow::Commit(index) = row {
                    Some(*index)
                } else {
                    None
                })
                .collect::<Vec<_>>(),
            (0..2055).rev().collect::<Vec<_>>()
        );
    }

    fn summary() -> PullRequestSummary {
        serde_json::from_value(serde_json::json!({
            "id":"PR_test", "number":12, "title":"Improve native review", "url":"https://github.com/example/repository/pull/12", "state":"OPEN", "isDraft":false,
            "author":{"login":"review-author"}, "baseRefName":"main", "headRefName":"feature/review", "updatedAt":"2026-01-01T00:00:00Z"
        })).expect("valid fixture")
    }

    #[test]
    fn source_filter_is_exact_and_does_not_use_target_branch() {
        let item = summary();
        let mut filter = PullRequestFilter {
            source: Some("feature/review".into()),
            ..Default::default()
        };
        assert!(matches_catalog_filter(&item, &filter));
        filter.source = Some("feature".into());
        assert!(!matches_catalog_filter(&item, &filter));
        filter.source = Some("main".into());
        assert!(!matches_catalog_filter(&item, &filter));
    }

    #[test]
    fn author_state_text_and_reviewer_filters_compose() {
        let mut item = summary();
        let filter = PullRequestFilter {
            state: Some("OPEN".into()),
            author: Some("review-author".into()),
            text: "native".into(),
            review_requested: Some("viewer".into()),
            ..Default::default()
        };
        assert!(!matches_catalog_filter(&item, &filter));
        item.review_requested_by_viewer = true;
        assert!(matches_catalog_filter(&item, &filter));
        item.state = "MERGED".into();
        assert!(!matches_catalog_filter(&item, &filter));
    }

    #[test]
    fn branch_filter_distinguishes_fork_repositories() {
        let mut item = summary();
        item.head_repository_name = Some("contributor/repository".into());
        let mut filter = PullRequestFilter {
            source: Some("feature/review".into()),
            head_repository: Some("example/repository".into()),
            ..Default::default()
        };
        assert!(!matches_catalog_filter(&item, &filter));
        filter.head_repository = Some("contributor/repository".into());
        assert!(matches_catalog_filter(&item, &filter));
    }

    #[test]
    fn detects_only_explicit_github_remotes() {
        for remote in [
            "git@github.com:owner/repo.git",
            "https://github.com/owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
        ] {
            assert_eq!(
                repository_from_remote(remote).map(|repository| repository.full_name()),
                Some("owner/repo".into())
            );
        }
        for remote in [
            "https://example.com/owner/repo",
            "https://github.com.evil/owner/repo",
            "git@github.com:owner/repo/extra.git",
        ] {
            assert!(repository_from_remote(remote).is_none());
        }
    }
}
