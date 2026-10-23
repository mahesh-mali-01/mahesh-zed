//! Use native checkout LSP navigation only when snapshot positions are exact.
use super::*;
use language::ToOffset as _;

const NAVIGATION_TEXT_BUDGET: usize = 2 * 1024 * 1024;

pub(super) fn checkout_definitions(
    review: &WeakEntity<ReviewView>,
    buffer: &Entity<language::Buffer>,
    position: text::Anchor,
    kind: editor::GotoDefinitionKind,
    cx: &mut App,
) -> Option<Task<Result<Option<Vec<project::LocationLink>>>>> {
    let view = review.upgrade()?;
    let workspace = view.read(cx).workspace.clone();
    let historical = buffer.read(cx).snapshot();
    let file = historical.file()?;
    if file.as_local().is_some() {
        return None;
    }
    if historical.len() > NAVIGATION_TEXT_BUDGET {
        workspace.update(cx, |workspace, cx| workspace.show_error(
            "This file exceeds the review navigation budget. Use Open current file for native Go to Definition.", cx
        )).log_err();
        return Some(Task::ready(Ok(None)));
    }
    let state = view.read(cx);
    let repo_path = RepoPath::from_rel_path(file.path().as_ref());
    let path = state
        .repository
        .read(cx)
        .repo_path_to_project_path(&repo_path, cx);
    let project = state.project.clone();
    let checkout_too_large = path
        .as_ref()
        .and_then(|path| project.read(cx).entry_for_path(path, cx))
        .is_some_and(|entry| entry.size > NAVIGATION_TEXT_BUDGET as u64);
    if checkout_too_large {
        workspace.update(cx, |workspace, cx| workspace.show_error(
            "The current checkout file exceeds the review navigation budget. Use Open current file for native navigation.", cx
        )).log_err();
        return Some(Task::ready(Ok(None)));
    }
    let offset = position.to_offset(&historical);
    let task = path
        .filter(|path| project.read(cx).entry_for_path(path, cx).is_some())
        .map(|path| project.update(cx, |project, cx| project.open_buffer(path, cx)));
    Some(cx.spawn(async move |cx| {
        let result: Result<Option<Vec<project::LocationLink>>> = async {
            ensure!(historical.len() <= NAVIGATION_TEXT_BUDGET,
                "This review file is too large for snapshot navigation. Use Open current file to navigate with the language server");
            let checkout = task.context("File is absent from the current checkout. Open current file or inspect the reviewed revision instead")?.await?;
            let snapshot = checkout.read_with(cx, |buffer, _| buffer.snapshot());
            let expected_version = snapshot.version().clone();
            let matches = cx.background_spawn(async move {
                snapshot.len() == historical.len() && snapshot.text() == historical.text()
            }).await;
            ensure!(matches,
                "Reviewed contents differ from the current checkout. Use Open current file for Go to Definition; historical positions were not sent to the language server");
            let task = project.update(cx, |project, cx| {
                ensure!(checkout.read(cx).version() == expected_version,
                    "The checkout changed during navigation. Try again from the current file");
                Ok::<_, anyhow::Error>(match kind {
                    editor::GotoDefinitionKind::Symbol => project.definitions(&checkout, offset, cx),
                    editor::GotoDefinitionKind::Declaration => project.declarations(&checkout, offset, cx),
                    editor::GotoDefinitionKind::Type => project.type_definitions(&checkout, offset, cx),
                    editor::GotoDefinitionKind::Implementation => project.implementations(&checkout, offset, cx),
                })
            })?;
            task.await
        }.await;
        if let Err(error) = &result {
            workspace.update(cx, |workspace, cx| workspace.show_error(error.to_string(), cx)).log_err();
        }
        result
    }))
}
