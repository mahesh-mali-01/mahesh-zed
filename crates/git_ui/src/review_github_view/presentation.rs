use super::*;
use editor::hover_markdown_style;
use gpui::{Render, WeakEntity};
use markdown::MarkdownElement;
use ui::{ContextMenu, DropdownMenu, TintColor, Tooltip};

/// Shared native review-card surface for Conversation, inline threads and the
/// optional side panel. Callers provide semantic header/body/action slots.
pub(crate) fn review_card_surface(cx: &App) -> gpui::Div {
    v_flex()
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .rounded_md()
        .overflow_hidden()
        .border_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().elevated_surface_background)
}

impl GitHubReviewPanel {
    pub(crate) fn render_thread(
        &self,
        thread: &RemoteThread,
        inline_surface: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let owns_composer = inline_surface == self.inline_composer;
        let delivery_operations = self.thread_delivery_operations(thread);
        let mut card = review_card_surface(cx).child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .p_2()
                .bg(cx.theme().colors().surface_background)
                .border_b_1()
                .border_color(cx.theme().colors().border)
                .child(Icon::new(IconName::File).size(IconSize::Small))
                .child(
                    div()
                        .debug_selector(|| "GITHUB_THREAD_CARD_PATH".into())
                        .id(SharedString::from(format!(
                            "github-thread-path:{}",
                            thread.id
                        )))
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .tooltip(Tooltip::text(thread.path.clone()))
                        .child(Label::new(thread.path.clone()).single_line().truncate()),
                )
                .child(
                    Label::new(github_thread_location(thread))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .when(thread.is_resolved, |row| {
                    row.child(
                        Label::new("Resolved")
                            .size(LabelSize::Small)
                            .color(Color::Success),
                    )
                })
                .when(thread.is_outdated, |row| {
                    row.child(
                        Label::new("Outdated")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                }),
        );
        for comment in &thread.comments {
            let mut row = v_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .p_3()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .flex_wrap()
                        .child(
                            Icon::new(IconName::Person)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(
                            comment
                                .author
                                .as_ref()
                                .map(|author| author.login.as_str())
                                .unwrap_or("Deleted account"),
                        ))
                        .child(
                            Label::new(github_comment_timestamp(&comment.created_at))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when(comment.state == "PENDING", |header| {
                            header.child(
                                Label::new("Pending review · only you")
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                            )
                        }),
                );
            // Keep an edit in the original comment row, rather than rendering
            // both the unchanged comment and a second editor below the thread.
            if owns_composer
                && matches!(self.intent.as_ref(), Some(ReviewIntent::Edit { comment_id, .. }) if comment_id == &comment.id)
            {
                card = card.child(row.child(self.render_composer_in_thread(window, cx)));
                continue;
            }
            let edit_operations = delivery_operations
                .iter()
                .filter(|operation| {
                    matches!(serde_json::from_value::<ReviewIntent>(operation.payload.clone()), Ok(ReviewIntent::Edit { comment_id, .. }) if comment_id == comment.id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if let Some(edit) = edit_operations
                .iter()
                .filter(|operation| operation.state != ReviewOperationState::LocalDraft)
                .max_by_key(|operation| {
                    (
                        operation.created_at_unix_seconds,
                        operation.updated_at_unix_seconds,
                    )
                })
            {
                if let Ok(intent) = serde_json::from_value::<ReviewIntent>(edit.payload.clone()) {
                    row = row.child(
                        div()
                            .debug_selector(|| "GITHUB_COMMENT_EDIT_DELIVERY_BODY".into())
                            .child(Label::new(intent.body().to_owned())),
                    );
                }
                card = card.child(row.child(self.render_delivery_operations(
                    edit_operations,
                    false,
                    cx,
                )));
                continue;
            }
            if !edit_operations.is_empty() {
                // An unsent draft is recoverable, but it must not repeat the
                // canonical comment body in a second local card.
                row = row.child(self.render_delivery_operations(edit_operations, false, cx));
            }
            if let Some(markdown) = self.markdown.get(&comment.id) {
                row = row.child(
                    div()
                        .debug_selector(|| "GITHUB_CANONICAL_COMMENT_BODY".into())
                        .child(MarkdownElement::new(
                            markdown.clone(),
                            hover_markdown_style(window, cx),
                        )),
                );
            }
            let mut actions = h_flex().gap_2().flex_wrap();
            if comment.viewer_did_author && comment.viewer_can_update {
                let id = comment.id.clone();
                let body = comment.body.clone();
                actions = actions.child(
                    Button::new(
                        gpui::SharedString::from(format!("edit-comment:{}", id.clone())),
                        "Edit",
                    )
                    .start_icon(Icon::new(IconName::Pencil))
                    .style(ButtonStyle::Tinted(TintColor::Accent))
                    .disabled(self.writing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.begin_edit(id.clone(), body.clone(), window, cx)
                    })),
                );
            }
            if comment.viewer_did_author && comment.viewer_can_delete {
                let id = comment.id.clone();
                actions = actions.child(
                    Button::new(
                        gpui::SharedString::from(format!("delete-comment:{}", id.clone())),
                        "Delete…",
                    )
                    .style(ButtonStyle::Tinted(TintColor::Error))
                    .disabled(self.writing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_delete(id.clone(), window, cx)
                    })),
                );
            }
            card = card.child(row.child(actions));
        }
        let replying = owns_composer
            && matches!(self.intent.as_ref(), Some(ReviewIntent::Reply { thread_id, .. }) if thread_id == &thread.id);
        let mut actions = h_flex().w_full().gap_2().flex_wrap().p_2();
        if thread.viewer_can_reply && !replying {
            let thread = thread.clone();
            actions = actions.child(
                Button::new(
                    gpui::SharedString::from(format!("reply:{}", thread.id)),
                    "Reply",
                )
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .disabled(self.writing)
                .on_click(
                    cx.listener(move |this, _, window, cx| this.begin_reply(&thread, window, cx)),
                ),
            );
        }
        if (!thread.is_resolved && thread.viewer_can_resolve)
            || (thread.is_resolved && thread.viewer_can_unresolve)
        {
            let id = thread.id.clone();
            let resolved = !thread.is_resolved;
            actions = actions.child(
                Button::new(
                    gpui::SharedString::from(format!("resolve:{}", id)),
                    if resolved { "Resolve" } else { "Unresolve" },
                )
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .disabled(self.writing)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.persist(
                        ReviewIntent::Resolve {
                            thread_id: id.clone(),
                            resolved,
                        },
                        None,
                        true,
                        window,
                        cx,
                    )
                })),
            );
        }
        card = card.child(actions).child(
            self.render_delivery_operations(
                delivery_operations
                    .into_iter()
                    .filter(|operation| {
                        !matches!(
                            serde_json::from_value::<ReviewIntent>(operation.payload.clone()),
                            Ok(ReviewIntent::Edit { .. })
                        )
                    })
                    .collect(),
                true,
                cx,
            ),
        );
        if replying {
            card = card.child(self.render_composer_in_thread(window, cx));
        }
        card.into_any_element()
    }

    /// Delivery feedback belongs to the thread even when its optional sidebar
    /// is closed. Canonical readback replaces this local state, never guesses
    /// whether an uncertain write can be replayed.
    pub(super) fn thread_delivery_operations(&self, thread: &RemoteThread) -> Vec<ReviewOperation> {
        self.operations
            .iter()
            .filter(|operation| {
                if operation.context != self.context
                    || self
                        .draft
                        .as_ref()
                        .is_some_and(|draft| draft.operation_id == operation.operation_id)
                    || operation.state == ReviewOperationState::Sent
                        && self.canonical_threads_generation == Some(self.generation)
                        && !self.loading
                {
                    return false;
                }
                match serde_json::from_value::<ReviewIntent>(operation.payload.clone()) {
                    Ok(
                        ReviewIntent::Edit { comment_id, .. } | ReviewIntent::Delete { comment_id },
                    ) => thread
                        .comments
                        .iter()
                        .any(|comment| comment.id == comment_id),
                    Ok(
                        ReviewIntent::Reply { thread_id, .. }
                        | ReviewIntent::Resolve { thread_id, .. },
                    ) => thread.id == thread_id,
                    _ => false,
                }
            })
            .cloned()
            .collect()
    }

    pub(crate) fn thread_has_delivery(&self, id: &str) -> bool {
        self.threads
            .iter()
            .find(|thread| thread.id == id)
            .is_some_and(|thread| !self.thread_delivery_operations(thread).is_empty())
    }

    pub(super) fn render_delivery_operations(
        &self,
        operations: Vec<ReviewOperation>,
        show_body: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut content = v_flex().w_full().min_w_0();
        for operation in operations {
            let status = match operation.state {
                ReviewOperationState::LocalDraft => "Saved draft · only on this computer",
                ReviewOperationState::Queued | ReviewOperationState::Sending => {
                    "Saved locally · syncing with GitHub…"
                }
                ReviewOperationState::Sent => "GitHub confirmed · updating thread…",
                ReviewOperationState::Failed => "Not sent · your change is saved on this computer",
                ReviewOperationState::DeliveryUnknown => {
                    "Delivery unconfirmed · verify before retrying"
                }
            };
            let mut row = v_flex()
                .debug_selector(|| "GITHUB_THREAD_DELIVERY_STATUS".into())
                .w_full()
                .min_w_0()
                .gap_2()
                .p_3()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Label::new(status).size(LabelSize::Small))
                        .when(
                            matches!(
                                operation.state,
                                ReviewOperationState::Queued | ReviewOperationState::Sending
                            ),
                            |row| row.child(ui::SpinnerLabel::new()),
                        ),
                );
            if show_body {
                if let Ok(intent) =
                    serde_json::from_value::<ReviewIntent>(operation.payload.clone())
                {
                    if !intent.body().is_empty() {
                        row = row.child(Label::new(intent.body().to_owned()));
                    }
                }
            }
            if let Some(error) = &operation.last_error {
                row = row.child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Error),
                );
            }
            if matches!(
                operation.state,
                ReviewOperationState::LocalDraft | ReviewOperationState::Failed
            ) {
                row = row.child(
                    Button::new(
                        SharedString::from(format!(
                            "resume-thread-change:{}",
                            operation.operation_id
                        )),
                        "Edit saved change",
                    )
                    .style(ButtonStyle::Filled)
                    .disabled(self.writing || self.intent.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.resume_draft(operation.clone(), window, cx)
                    })),
                );
            } else if operation.state == ReviewOperationState::DeliveryUnknown {
                row = row.child(
                    Button::new(
                        SharedString::from(format!(
                            "reconcile-thread-change:{}",
                            operation.operation_id
                        )),
                        "Reconcile delivery",
                    )
                    .style(ButtonStyle::Filled)
                    .disabled(self.writing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reconcile(operation.clone(), window, cx);
                    })),
                );
            }
            content = content.child(row);
        }
        content.into_any_element()
    }

    pub(crate) fn local_inline_comments(&self) -> Vec<(String, CommentAnchor)> {
        self.operations
            .iter()
            .filter_map(|operation| {
                if operation.context != self.context
                    || operation.location == ReviewLocationState::NeedsLocation
                {
                    return None;
                }

                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| draft.operation_id == operation.operation_id)
                {
                    return None;
                }
                if operation.state == ReviewOperationState::Sent
                    && (self.canonical_threads_generation == Some(self.generation) && !self.loading
                        || self
                            .threads
                            .iter()
                            .any(|thread| operation_matches_thread(operation, thread)))
                {
                    return None;
                }
                match serde_json::from_value::<ReviewIntent>(operation.payload.clone()).ok()? {
                    ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } => {
                        Some((operation.operation_id.clone(), intent.anchor))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    pub(super) fn render_local_operation(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(operation) = self
            .operations
            .iter()
            .find(|operation| operation.operation_id == id)
            .cloned()
        else {
            return div().into_any_element();
        };
        if operation.state == ReviewOperationState::Sent {
            if (self.canonical_threads_generation == Some(self.generation) && !self.loading)
                || self
                    .threads
                    .iter()
                    .any(|thread| operation_matches_thread(&operation, thread))
            {
                return div().into_any_element();
            }
        }
        let Ok(intent) = serde_json::from_value::<ReviewIntent>(operation.payload.clone()) else {
            return div().into_any_element();
        };
        let body = intent.body().to_owned();
        let markdown = match self.operation_markdown.get(id) {
            Some((old, markdown)) if old == &body => markdown.clone(),
            _ => {
                let markdown = cx.new(|cx| {
                    Markdown::new_with_options(
                        body.clone().into(),
                        None,
                        None,
                        MarkdownOptions {
                            parse_html: true,
                            ..Default::default()
                        },
                        cx,
                    )
                });
                self.operation_markdown
                    .insert(id.to_owned(), (body, markdown.clone()));
                markdown
            }
        };
        let status = match operation.state {
            ReviewOperationState::LocalDraft => "Draft · only on this computer",
            ReviewOperationState::Queued | ReviewOperationState::Sending => {
                "Saved locally · syncing with GitHub"
            }
            ReviewOperationState::Sent => pending_publication_status(&intent),
            ReviewOperationState::Failed => "Saved locally · not sent",
            ReviewOperationState::DeliveryUnknown => "Saved locally · delivery needs verification",
        };
        let location = match &intent {
            ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } => format!(
                "{} · {}",
                intent.anchor.path,
                github_location(
                    intent.anchor.side,
                    intent.anchor.start_line,
                    intent.anchor.line
                )
            ),
            _ => "Pull request review".into(),
        };
        let created = i64::try_from(operation.created_at_unix_seconds)
            .ok()
            .and_then(|timestamp| time::OffsetDateTime::from_unix_timestamp(timestamp).ok())
            .map(review_timestamp)
            .unwrap_or_else(|| "Saved locally".into());
        let mut card = review_card_surface(cx)
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(location).size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .p_2()
                    .flex_wrap()
                    .bg(cx.theme().colors().surface_background)
                    .child(Icon::new(IconName::Person).size(IconSize::Small))
                    .child(Label::new(
                        self.viewer_login.clone().unwrap_or_else(|| "You".into()),
                    ))
                    .child(
                        Label::new(created)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(status).size(LabelSize::Small).color(
                        if matches!(
                            operation.state,
                            ReviewOperationState::Failed | ReviewOperationState::DeliveryUnknown
                        ) {
                            Color::Warning
                        } else {
                            Color::Muted
                        },
                    )),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .p_3()
                    .child(MarkdownElement::new(
                        markdown,
                        hover_markdown_style(window, cx),
                    )),
            );
        if let Some(error) = &operation.last_error {
            card = card.child(
                div().px_3().pb_2().child(
                    Label::new(error.clone())
                        .size(LabelSize::Small)
                        .color(Color::Default),
                ),
            );
        }
        if self.pending_review_is_older() && operation.state != ReviewOperationState::Sent {
            card = card.child(
                div().px_3().pb_2().child(
                    Label::new(OLDER_PENDING_REVIEW_WARNING)
                        .size(LabelSize::Small)
                        .color(Color::Default),
                ),
            );
        }
        let mut actions = h_flex().w_full().p_2().gap_2().flex_wrap();
        if matches!(
            operation.state,
            ReviewOperationState::Queued | ReviewOperationState::Sending
        ) {
            actions = actions.child(ui::SpinnerLabel::new());
        }
        if matches!(
            operation.state,
            ReviewOperationState::LocalDraft | ReviewOperationState::Failed
        ) && operation.context == self.context
            && operation.location == ReviewLocationState::Ready
        {
            if let Some(review) = self.pending() {
                let review_id = review.id.clone();
                let saved = operation.clone();
                actions = actions.child(
                    Button::new(
                        SharedString::from(format!(
                            "inline-add-existing:{}",
                            operation.operation_id
                        )),
                        "Add to existing review",
                    )
                    .style(ButtonStyle::Filled)
                    .disabled(self.writing || self.pending_review_is_older())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_saved_comment_to_pending(
                            saved.clone(),
                            review_id.clone(),
                            window,
                            cx,
                        )
                    })),
                );
            }
        }
        if operation.state == ReviewOperationState::Sent {
            actions = actions.child(
                Label::new(if self.loading {
                    "Posted · loading discussion…"
                } else {
                    "Posted on GitHub"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
            if self.error.is_some() {
                actions = actions.child(
                    Button::new(
                        SharedString::from(format!(
                            "inline-load-thread:{}",
                            operation.operation_id
                        )),
                        "Retry loading discussion",
                    )
                    .style(ButtonStyle::Outlined)
                    .disabled(self.loading || self.preview)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.refresh_discussion(window, cx)),
                    ),
                );
            }
        }

        if matches!(
            operation.state,
            ReviewOperationState::LocalDraft | ReviewOperationState::Failed
        ) {
            let resume = operation.clone();
            actions = actions.child(
                Button::new(
                    SharedString::from(format!("inline-resume:{}", operation.operation_id)),
                    "Edit draft",
                )
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.resume_operation(resume.clone(), window, cx)
                })),
            );
            let draft = operation.clone();
            actions = actions.child(
                Button::new(
                    SharedString::from(format!("inline-discard:{}", operation.operation_id)),
                    "Discard local draft…",
                )
                .style(ButtonStyle::Tinted(TintColor::Error))
                .disabled(self.writing)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.discard_draft(draft.clone(), window, cx)
                })),
            );
        } else if operation.state == ReviewOperationState::DeliveryUnknown {
            actions = actions.child(
                Button::new(
                    SharedString::from(format!("inline-reconcile:{}", operation.operation_id)),
                    "Verify delivery",
                )
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.reconcile(operation.clone(), window, cx)
                })),
            );
        }
        card.child(actions).into_any_element()
    }
    pub(crate) fn render_composer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_composer_surface(false, window, cx)
    }

    pub(super) fn render_composer_in_thread(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_composer_surface(true, window, cx)
    }

    pub(super) fn render_composer_surface(
        &self,
        embedded: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(intent) = &self.intent else {
            return div().into_any_element();
        };
        let title = match intent {
            ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } => format!(
                "New comment · {} · {}",
                intent.anchor.path,
                github_location(
                    intent.anchor.side,
                    intent.anchor.start_line,
                    intent.anchor.line
                )
            ),
            ReviewIntent::Reply { .. } => "Reply to thread".into(),
            ReviewIntent::Edit { .. } => "Edit comment".into(),
            ReviewIntent::Submit { event, .. } | ReviewIntent::SubmitNew { event, .. } => {
                format!("Submit review · {event:?}")
            }
            _ => "Review".into(),
        };
        let delivery_locked = self.composer_delivery_locked();
        let composer_locked = self.preview
            || self.writing
            || delivery_locked
            || (self.incremental_mode
                && matches!(
                    intent,
                    ReviewIntent::Comment { .. } | ReviewIntent::AddToReview { .. }
                ));
        let needs_location = self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.location == ReviewLocationState::NeedsLocation)
            || matches!(
                intent,
                ReviewIntent::Comment { .. } | ReviewIntent::AddToReview { .. }
            ) && !self.context.selector.supports_github_viewed();
        let immediate_block_reason = self.immediate_post_block_reason(intent);
        let mut actions = h_flex().gap_2().flex_wrap().child(
            Button::new(
                "github-send",
                match intent {
                    ReviewIntent::Submit { event, .. } | ReviewIntent::SubmitNew { event, .. } => {
                        match event {
                            ReviewEvent::Comment => "Submit comment review",
                            ReviewEvent::Approve => "Submit approval",
                            ReviewEvent::RequestChanges => "Submit requested changes",
                        }
                    }
                    ReviewIntent::Edit { .. } => "Save on GitHub",
                    _ => "Post comment",
                },
            )
            .style(ButtonStyle::Filled)
            .disabled(composer_locked || needs_location || immediate_block_reason.is_some())
            .when_some(immediate_block_reason, |button, reason| {
                button.tooltip(Tooltip::text(reason))
            })
            .on_click(cx.listener(|this, _, window, cx| {
                if matches!(
                    this.intent,
                    Some(
                        ReviewIntent::Comment { .. }
                            | ReviewIntent::AddToReview { .. }
                            | ReviewIntent::Reply { .. }
                    )
                ) {
                    this.post_comment_now(window, cx);
                } else {
                    this.save(true, window, cx);
                }
            })),
        );
        if matches!(intent, ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent } if intent.anchor.subject_type == crate::github_review::CommentSubject::File)
        {
            actions = actions.child(
                Button::new("github-add-file-review", "Add file comment to review")
                    .style(ButtonStyle::Filled)
                    .disabled(composer_locked || needs_location || self.pending_review_is_older())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.add_current_to_review(window, cx)),
                    ),
            );
        }
        if needs_location
            && matches!(
                intent,
                ReviewIntent::Comment { .. } | ReviewIntent::AddToReview { .. }
            )
            && self.context.selector.supports_github_viewed()
        {
            actions = actions.child(
                Button::new("github-reanchor-draft", "Use selected diff location")
                    .disabled(composer_locked || self.anchor.is_none())
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let (
                            Some(anchor),
                            Some(
                                ReviewIntent::Comment { intent }
                                | ReviewIntent::AddToReview { intent },
                            ),
                        ) = (this.anchor.clone(), &mut this.intent)
                        {
                            intent.anchor = anchor;
                            intent.context = this.remote.clone();
                            if let Some(draft) = &mut this.draft {
                                draft.context = this.context.clone();
                                draft.location = ReviewLocationState::Ready;
                            }
                            this.save(false, window, cx);
                        }
                    })),
            );
        }
        actions = actions.child(
            Button::new("github-close-composer", "Cancel")
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .disabled(self.writing)
                .on_click(cx.listener(|this, _, window, cx| this.close_composer(window, cx))),
        );
        if delivery_locked {
            actions = actions.child(
                Button::new("github-composer-reconcile", "Reconcile delivery")
                    .disabled(self.writing)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.reconcile_current(window, cx)),
                    ),
            );
        }
        // Distinguish the single editor's owning surface in native layout diagnostics.
        // Both a sidebar card and its diff card can be mounted simultaneously.
        let inline_owner = self.inline_composer;
        let mut card = v_flex()
            .debug_selector(move || {
                if embedded && inline_owner {
                    "GITHUB_THREAD_EMBEDDED_COMPOSER".into()
                } else if embedded {
                    "GITHUB_PANEL_EMBEDDED_COMPOSER".into()
                } else {
                    "GITHUB_STANDALONE_COMPOSER".into()
                }
            })
            .w_full()
            .min_w_0()
            .gap_2()
            .p_3()
            .when(!embedded, |card| {
                card.rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border_focused)
                    .bg(cx.theme().colors().elevated_surface_background)
            })
            .child(Label::new(title))
            .child(
                Label::new(if self.writing {
                    if self.reconciling {
                        "Checking delivery with GitHub…"
                    } else {
                        "Saving locally…"
                    }
                } else {
                    "Draft autosaves on this computer"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        if let Some(error) = &self.error {
            card = card.child(
                Label::new(error.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error),
            );
        }
        if let Some(reason) = immediate_block_reason {
            card = card.child(
                Label::new(reason)
                    .size(LabelSize::Small)
                    .color(Color::Default),
            );
        }
        if self.pending_review_is_older()
            && matches!(
                intent,
                ReviewIntent::AddToReview { .. }
                    | ReviewIntent::Comment {
                        intent: CommentIntent {
                            publication: CommentPublication::Pending { .. },
                            ..
                        }
                    }
            )
        {
            card = card.child(
                Label::new(OLDER_PENDING_REVIEW_WARNING)
                    .size(LabelSize::Small)
                    .color(Color::Default),
            );
        }
        if let ReviewIntent::Submit { event, .. } | ReviewIntent::SubmitNew { event, .. } = intent {
            let review_id = match intent {
                ReviewIntent::Submit { review_id, .. } => Some(review_id),
                _ => None,
            };
            let comments = self
                .threads
                .iter()
                .flat_map(|thread| thread.comments.iter().map(move |comment| (thread, comment)))
                .filter(|(_, comment)| {
                    comment.state == "PENDING"
                        && comment
                            .pull_request_review
                            .as_ref()
                            .is_some_and(|review| Some(&review.id) == review_id)
                })
                .collect::<Vec<_>>();
            card = card.child(Label::new(format!("{} · #{} · {}", self.remote.reference.repository.full_name(), self.remote.reference.number, self.context.metadata.title)))
                .child(Label::new(format!("Account: {}", self.viewer_login.as_deref().unwrap_or("Reconnect to verify account"))).size(LabelSize::Small))
                .child(Label::new(format!("Pinned head: {}", self.remote.head_oid)).size(LabelSize::Small))
                .child(Label::new(format!("{event:?} · {} GitHub pending comments will be submitted. Unsent local drafts are excluded.", comments.len())).size(LabelSize::Small));
            for (thread, comment) in comments {
                card = card.child(
                    Label::new(format!(
                        "{} · {}",
                        thread.path,
                        github_location(
                            thread.diff_side,
                            comment.start_line.or(thread.start_line),
                            comment.line.or(thread.line)
                        )
                    ))
                    .size(LabelSize::Small),
                );
                if let Some(markdown) = self.markdown.get(&comment.id) {
                    card = card.child(MarkdownElement::new(
                        markdown.clone(),
                        hover_markdown_style(_window, cx),
                    ));
                }
            }
        }
        let body_editor = self.body.read(cx).editor().clone();
        if delivery_locked {
            body_editor.set_read_only(true, cx);
        }
        let focused = body_editor.focus_handle(cx).contains_focused(_window, cx);
        card.when(self.incremental_mode && matches!(intent, ReviewIntent::Comment { .. } | ReviewIntent::AddToReview { .. }), |card| card.child(Label::new("This is an incremental Since review view. Open Full PR comparison to post at a canonical GitHub line; this local draft remains saved.").size(LabelSize::Small).color(Color::Warning))).when(delivery_locked, |card| card.child(Label::new("Delivery is unconfirmed. This text is preserved and cannot be edited or resent. Reconcile with GitHub or close the composer.").size(LabelSize::Small).color(Color::Warning))).child(v_flex().id("github-review-body-editor").debug_selector(|| "GITHUB_REVIEW_BODY_EDITOR".into()).w_full().min_w_0().min_h(px(72.)).p_2().rounded_md().border_1().border_color(if focused { cx.theme().colors().border_focused } else { cx.theme().colors().border }).bg(cx.theme().colors().editor_background).child(body_editor.render(_window, cx)))
            .when(needs_location,|card|card.child(Label::new("Draft preserved. Open All PR changes and select a canonical diff location before posting.").color(Color::Warning)))
            .child(actions).into_any_element()
    }
}

impl Render for GitHubReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .p_2()
            .key_context("GitHubReviewPanel")
            .on_action(
                cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(GitHubReviewEvent::ClosePanel)),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            Label::new(if self.pending_actions_mode {
                                format!("Pending actions · {}", self.pending_action_count())
                            } else {
                                format!(
                                    "Threads · {}/{} resolved",
                                    self.threads
                                        .iter()
                                        .filter(|thread| thread.is_resolved)
                                        .count(),
                                    self.threads.len()
                                )
                            })
                            .single_line()
                            .truncate(),
                        ),
                    )
                    .child(
                        IconButton::new("github-close-panel", IconName::Close)
                            .tooltip(Tooltip::text("Close threads and review"))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(GitHubReviewEvent::ClosePanel)),
                            ),
                    ),
            );
        if self.preview {
            content = content.child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("github-preview-threads", "Threads")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _, cx| this.show_threads(cx))),
                    )
                    .child(
                        Button::new("github-preview-pending", "Pending actions (2)")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_pending_actions(window, cx)
                            })),
                    ),
            );
        }
        if let Some(error) = &self.error {
            content = content.child(
                review_card_surface(cx)
                    .p_3()
                    .gap_2()
                    .child(Label::new("GitHub request failed").color(Color::Error))
                    .child(Label::new(error.clone()).size(LabelSize::Small))
                    .child(
                        Button::new("github-retry-read", "Retry loading discussions")
                            .style(ButtonStyle::Filled)
                            .disabled(self.loading || self.writing)
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    ),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.child(Label::new(notice.clone()).size(LabelSize::Small));
        }
        let busy = self.writing
            || self.operations.iter().any(|operation| {
                matches!(
                    operation.state,
                    ReviewOperationState::Queued | ReviewOperationState::Sending
                )
            });
        if self.loading || busy {
            content = content.child(
                h_flex().gap_2().child(ui::SpinnerLabel::new()).child(
                    Label::new(if self.writing {
                        if self.reconciling {
                            "Checking delivery with GitHub…"
                        } else {
                            "Saving locally…"
                        }
                    } else if busy {
                        "Syncing with GitHub…"
                    } else {
                        "Loading discussions…"
                    })
                    .size(LabelSize::Small),
                ),
            );
        }
        if !self.pending_actions_mode {
            if let Some(pending) = self.pending() {
                let id = pending.id.clone();
                content = content.child(v_flex().debug_selector(|| "GITHUB_PENDING_STATUS".into()).w_full().gap_2().p_2().border_1().border_color(cx.theme().colors().border)
                .child(Label::new("You have an unpublished review on GitHub").size(LabelSize::Small))
                .child(Label::new("GitHub requires publishing or discarding this review before posting new inline comments. Existing pending comments remain private.").size(LabelSize::Small).color(Color::Muted))
                .child(Button::new("github-publish-pending", "Publish pending review…")
                    .style(ButtonStyle::Filled).full_width().disabled(busy || self.loading || self.composer_delivery_locked())
                    .on_click(cx.listener(|this, _, window, cx| this.begin_submit(ReviewEvent::Comment, window, cx))))
                .child(Button::new("github-discard-pending-review", "Discard pending review…")
                    .style(ButtonStyle::Tinted(TintColor::Error)).full_width().disabled(busy || self.composer_delivery_locked())
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_discard_review(id.clone(), window, cx)))));
            }
            let weak = cx.entity().downgrade();
            let menu = ContextMenu::build(window, cx, |mut menu, _, _| {
                for (label, event) in [
                    ("Comment review…", ReviewEvent::Comment),
                    ("Approve…", ReviewEvent::Approve),
                    ("Request changes…", ReviewEvent::RequestChanges),
                ] {
                    let weak = weak.clone();
                    menu = menu.entry(label, None, move |window, cx| {
                        weak.update(cx, |this, cx| this.begin_submit(event, window, cx))
                            .log_err();
                    });
                }
                menu
            });
            content = content.child(
                DropdownMenu::new("github-review-menu", "Review…", menu)
                    .style(ui::DropdownStyle::Outlined)
                    .full_width(true)
                    .disabled(busy || self.loading || self.composer_delivery_locked())
                    .attach(gpui::Anchor::BottomLeft)
                    .offset(gpui::point(px(0.), px(6.))),
            );
            if self.intent.is_some() && !self.inline_composer && self.composer_thread().is_none() {
                content = content.child(self.render_composer(window, cx));
            }
        }
        if !self.pending_actions_mode {
            let weak = cx.entity().downgrade();
            let menu = ContextMenu::build(window, cx, |mut menu, _, _| {
                for filter in [
                    ThreadFilter::All,
                    ThreadFilter::Unresolved,
                    ThreadFilter::Resolved,
                ] {
                    let weak = weak.clone();
                    menu = menu.entry(filter.label(), None, move |_, cx| {
                        weak.update(cx, |this, cx| {
                            this.thread_filter = filter;
                            cx.notify();
                        })
                        .log_err();
                    });
                }
                menu
            });
            content = content.child(
                DropdownMenu::new("github-thread-filter", self.thread_filter.label(), menu)
                    .style(ui::DropdownStyle::Outlined)
                    .full_width(true)
                    .attach(gpui::Anchor::BottomLeft)
                    .offset(gpui::point(px(0.), px(6.))),
            );
            for thread in self
                .threads
                .clone()
                .into_iter()
                .filter(|thread| self.thread_filter.includes(thread.is_resolved))
            {
                let id = thread.id.clone();
                let expanded = self.thread_expanded(&thread);
                let author = thread
                    .comments
                    .first()
                    .and_then(|comment| comment.author.as_ref())
                    .map(|author| author.login.as_str())
                    .unwrap_or("Discussion");
                content = content.child(
                    h_flex()
                        .debug_selector(|| "GITHUB_THREAD_SUMMARY".into())
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .py_1()
                        .child(
                            Button::new(
                                SharedString::from(format!("github-thread-expand:{id}")),
                                format!(
                                    "{} · {} {} · {}",
                                    author,
                                    thread.comments.len(),
                                    if thread.comments.len() == 1 {
                                        "comment"
                                    } else {
                                        "comments"
                                    },
                                    if thread.is_resolved {
                                        "Resolved"
                                    } else {
                                        "Unresolved"
                                    }
                                ),
                            )
                            .start_icon(Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            }))
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .full_width()
                            .truncate(true)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.thread_expansion.insert(id.clone(), !expanded);
                                    cx.notify();
                                },
                            )),
                        ),
                );
                if expanded {
                    content = content.child(self.render_thread(&thread, false, window, cx));
                }
            }
        }
        let pending_actions_mode = self.pending_actions_mode;
        let visible_operations = self
            .operations
            .iter()
            .filter(|operation| {
                operation.state != ReviewOperationState::Sent
                    && (!pending_actions_mode || pending_operation(operation))
            })
            .cloned()
            .collect::<Vec<_>>();
        for operation in visible_operations {
            let status = format!(
                "{}{}",
                match operation.state {
                    ReviewOperationState::LocalDraft => "Local draft",
                    ReviewOperationState::Queued => "Saved locally · pending delivery",
                    ReviewOperationState::Sending => "Sending · awaiting confirmation",
                    ReviewOperationState::Failed => "Not sent",
                    ReviewOperationState::DeliveryUnknown => "Delivery unknown",
                    ReviewOperationState::Sent => "Confirmed",
                },
                if operation.location == ReviewLocationState::NeedsLocation {
                    " · Needs location"
                } else {
                    ""
                }
            );
            let intent = serde_json::from_value::<ReviewIntent>(operation.payload.clone()).ok();
            if !self.pending_actions_mode
                && matches!(
                    &intent,
                    Some(ReviewIntent::Comment { .. } | ReviewIntent::AddToReview { .. })
                )
            {
                content =
                    content.child(self.render_local_operation(&operation.operation_id, window, cx));
                continue;
            }
            let path = match &intent {
                Some(ReviewIntent::Comment { intent } | ReviewIntent::AddToReview { intent }) => {
                    intent.anchor.path.clone()
                }
                Some(
                    ReviewIntent::Reply { thread_id, .. } | ReviewIntent::Resolve { thread_id, .. },
                ) => self
                    .threads
                    .iter()
                    .find(|thread| &thread.id == thread_id)
                    .map(|thread| thread.path.clone())
                    .unwrap_or_else(|| "Thread reply".into()),
                Some(
                    ReviewIntent::Edit { comment_id, .. } | ReviewIntent::Delete { comment_id },
                ) => self
                    .threads
                    .iter()
                    .find(|thread| {
                        thread
                            .comments
                            .iter()
                            .any(|comment| &comment.id == comment_id)
                    })
                    .map(|thread| thread.path.clone())
                    .unwrap_or_else(|| "Comment".into()),
                _ => "Pull request review".into(),
            };
            let viewed_intent = if operation.kind == ReviewOperationKind::SetFileViewed {
                serde_json::from_value::<crate::review_store::GitHubViewedIntent>(
                    operation.payload.clone(),
                )
                .ok()
            } else {
                None
            };
            let path = viewed_intent
                .as_ref()
                .map(|intent| {
                    if intent.paths.len() == 1 {
                        intent.paths[0].clone()
                    } else {
                        format!(
                            "{} files · GitHub {}",
                            intent.paths.len(),
                            if intent.viewed { "Viewed" } else { "unviewed" }
                        )
                    }
                })
                .unwrap_or(path);
            let excerpt = intent
                .as_ref()
                .map(|intent| {
                    intent
                        .body()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(100)
                        .collect::<String>()
                })
                .unwrap_or_default();
            let excerpt = viewed_intent
                .as_ref()
                .map(|intent| {
                    format!(
                        "Mark {} on GitHub · {}",
                        if intent.viewed { "Viewed" } else { "unviewed" },
                        intent
                            .paths
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .unwrap_or(excerpt);
            let mut row = h_flex()
                .w_full()
                .gap_2()
                .flex_wrap()
                .child(Label::new(status).size(LabelSize::Small));
            if operation.state == ReviewOperationState::Failed {
                let failed = operation.clone();
                row = row.child(
                    Button::new(
                        SharedString::from(format!(
                            "github-retry-failed:{}",
                            operation.operation_id
                        )),
                        "Retry action",
                    )
                    .style(ButtonStyle::Filled)
                    .disabled(self.writing || self.intent.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.retry_failed_operation(failed.clone(), window, cx)
                    })),
                );
            }
            if matches!(
                operation.state,
                ReviewOperationState::LocalDraft
                    | ReviewOperationState::Failed
                    | ReviewOperationState::Queued
            ) {
                let draft = operation.clone();
                row = row.child(
                    Button::new(
                        gpui::SharedString::from(format!(
                            "github-resume-draft:{}",
                            operation.operation_id.clone()
                        )),
                        "Resume",
                    )
                    .style(ButtonStyle::Outlined)
                    .disabled(
                        self.writing
                            || crate::review_outbox_runtime::is_in_flight(
                                &operation.operation_id,
                                cx,
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.resume_operation(draft.clone(), window, cx)
                    })),
                );
            }
            if matches!(
                operation.state,
                ReviewOperationState::LocalDraft | ReviewOperationState::Failed
            ) {
                let draft = operation.clone();
                row = row.child(
                    Button::new(
                        gpui::SharedString::from(format!(
                            "github-delete-draft:{}",
                            operation.operation_id
                        )),
                        "Discard local draft…",
                    )
                    .style(ButtonStyle::Tinted(TintColor::Error))
                    .disabled(self.writing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.discard_draft(draft.clone(), window, cx)
                    })),
                );
            }
            if operation.state == ReviewOperationState::DeliveryUnknown {
                let unknown = operation.clone();
                row = row.child(
                    Button::new(
                        gpui::SharedString::from(format!(
                            "github-reconcile:{}",
                            operation.operation_id.clone()
                        )),
                        "Reconcile delivery",
                    )
                    .style(ButtonStyle::Filled)
                    .disabled(self.writing)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reconcile(unknown.clone(), window, cx)
                    })),
                );
            }
            let action_label = match operation.kind {
                ReviewOperationKind::PostInlineComment => "Post comment",
                ReviewOperationKind::Reply => "Reply to thread",
                ReviewOperationKind::EditComment => "Edit comment",
                ReviewOperationKind::DeleteComment => "Delete comment",
                ReviewOperationKind::CreatePendingReview => "Create pending review",
                ReviewOperationKind::DeletePendingReview => "Discard pending review",
                ReviewOperationKind::AddPendingThread => "Add review comment",
                ReviewOperationKind::SubmitReview => "Submit review",
                ReviewOperationKind::SetThreadResolved => "Update thread resolution",
                ReviewOperationKind::SetFileViewed => "Update Viewed state",
            };
            content = content.child(
                review_card_surface(cx)
                    .gap_2()
                    .p_3()
                    .child(Label::new(action_label))
                    .when(operation.last_error.is_some(), |card| {
                        card.child(
                            Label::new(operation.last_error.clone().unwrap_or_default())
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    })
                    .child(
                        div()
                            .id(gpui::SharedString::from(format!(
                                "github-draft-path:{}",
                                operation.operation_id
                            )))
                            .w_full()
                            .tooltip(ui::Tooltip::text(path.clone()))
                            .child(Label::new(path).size(LabelSize::Small).truncate()),
                    )
                    .when(!excerpt.is_empty(), |card| {
                        card.child(
                            Label::new(excerpt)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })
                    .child(row),
            );
        }
        content
    }
}

pub(crate) struct GitHubInlineCard {
    panel: WeakEntity<GitHubReviewPanel>,
    thread_id: Option<String>,
    operation_id: Option<String>,
    _subscription: Subscription,
}
impl GitHubInlineCard {
    pub(crate) fn new(
        panel: &Entity<GitHubReviewPanel>,
        thread_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            panel: panel.downgrade(),
            thread_id,
            operation_id: None,
            _subscription: cx.observe(panel, |_, _, cx| cx.notify()),
        }
    }
    pub(crate) fn new_operation(
        panel: &Entity<GitHubReviewPanel>,
        operation_id: String,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            panel: panel.downgrade(),
            thread_id: None,
            operation_id: Some(operation_id),
            _subscription: cx.observe(panel, |_, _, cx| cx.notify()),
        }
    }
}
impl Render for GitHubInlineCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.panel
            .update(cx, |panel, cx| {
                if let Some(id) = &self.operation_id {
                    return panel.render_local_operation(id, window, cx);
                }
                if let Some(id) = &self.thread_id {
                    if let Some(thread) = panel
                        .threads
                        .iter()
                        .find(|thread| &thread.id == id)
                        .cloned()
                    {
                        return panel.render_thread(&thread, true, window, cx);
                    }
                    div().into_any_element()
                } else {
                    panel.render_composer(window, cx)
                }
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

pub(super) fn pending_publication_status(intent: &ReviewIntent) -> &'static str {
    match intent {
        ReviewIntent::AddToReview { .. }
        | ReviewIntent::Comment {
            intent:
                CommentIntent {
                    publication: CommentPublication::Pending { .. },
                    ..
                },
        }
        | ReviewIntent::Reply {
            pending_review_id: Some(_),
            ..
        } => "Pending review · only you",
        _ => "Posted on GitHub",
    }
}

pub(super) fn review_timestamp(timestamp: time::OffsetDateTime) -> String {
    time_format::format_localized_timestamp(
        timestamp,
        time::OffsetDateTime::now_utc(),
        time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC),
        time_format::TimestampFormat::MediumAbsolute,
    )
}
pub(super) fn github_comment_timestamp(source: &str) -> String {
    time::OffsetDateTime::parse(source, &time::format_description::well_known::Rfc3339)
        .map(review_timestamp)
        .unwrap_or_else(|_| source.to_owned())
}

pub(super) fn github_thread_location(thread: &RemoteThread) -> String {
    if thread.line.is_some() {
        github_location(thread.diff_side, thread.start_line, thread.line)
    } else {
        github_location(
            thread.diff_side,
            thread.original_start_line,
            thread.original_line,
        )
    }
}

pub(super) fn github_location(
    side: crate::github_review::DiffSide,
    start: Option<u32>,
    end: Option<u32>,
) -> String {
    let side = match side {
        crate::github_review::DiffSide::Left => "LEFT",
        crate::github_review::DiffSide::Right => "RIGHT",
    };
    match (start, end) {
        (Some(start), Some(end)) if start < end => format!("{side} {start}–{end}"),
        (_, Some(end)) => format!("{side} {end}"),
        _ => format!("{side} file"),
    }
}
