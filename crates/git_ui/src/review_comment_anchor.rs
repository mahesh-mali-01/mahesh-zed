use crate::{
    github_review::{CommentAnchor, CommentSubject, DiffSide, RemoteThread, ReviewContext},
    review::{ComparisonKind, ComparisonSelector, FileComparison},
};
use anyhow::{Context as _, Result, ensure};
use buffer_diff::BufferDiffSnapshot;
use editor::{Editor, SplittableEditor};
use gpui::{App, Entity};
use language::{Bias, BufferId, BufferSnapshot, Point, ToPoint as _};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot};
use std::ops::{Range, RangeInclusive};

#[derive(Clone)]
pub(super) struct CanonicalCommentBuffer {
    pub buffer_id: BufferId,
    pub path: String,
    pub side: DiffSide,
    pub allowed_lines: Vec<RangeInclusive<u32>>,
    buffer: BufferSnapshot,
    rhs: BufferSnapshot,
    diff: BufferDiffSnapshot,
}

pub(super) struct CapturedCommentAnchor {
    pub anchor: CommentAnchor,
    pub editor: Entity<Editor>,
    pub placement: Anchor,
}

pub(super) fn canonical_comment_buffers(
    file: &FileComparison,
    rhs: &BufferSnapshot,
    diff: &BufferDiffSnapshot,
    binary: bool,
) -> Result<Vec<CanonicalCommentBuffer>> {
    ensure!(
        file.kind == ComparisonKind::Full,
        "Inline comments require the canonical full PR file"
    );
    ensure!(!binary, "Binary files do not support inline comments");
    ensure!(
        file.old
            .iter()
            .chain(file.new.iter())
            .all(|side| matches!(side.mode, 0o100644 | 0o100755)),
        "Only regular text files support inline comments"
    );
    let path = file.file_key()?.to_owned();
    let base = diff.base_text();
    let mut buffers = Vec::new();
    if file.old.is_some() {
        buffers.push(CanonicalCommentBuffer {
            buffer_id: base.remote_id(),
            path: path.clone(),
            side: DiffSide::Left,
            allowed_lines: Vec::new(),
            buffer: base.clone(),
            rhs: rhs.clone(),
            diff: diff.clone(),
        });
    }
    if file.new.is_some() {
        buffers.push(CanonicalCommentBuffer {
            buffer_id: rhs.remote_id(),
            path,
            side: DiffSide::Right,
            allowed_lines: Vec::new(),
            buffer: rhs.clone(),
            rhs: rhs.clone(),
            diff: diff.clone(),
        });
    }
    Ok(buffers)
}

#[derive(Debug)]
struct PatchLines {
    left: Vec<RangeInclusive<u32>>,
    right: Vec<RangeInclusive<u32>>,
}

pub(super) fn restrict_to_github_patch(
    buffers: &mut [CanonicalCommentBuffer],
    patch: Option<&str>,
) -> Result<()> {
    // Coordinates remain disabled until the complete server patch validates.
    for buffer in buffers.iter_mut() {
        buffer.allowed_lines.clear();
    }
    let lines = parse_github_patch(patch)?;
    for buffer in buffers.iter() {
        let ranges = match buffer.side {
            DiffSide::Left => &lines.left,
            DiffSide::Right => &lines.right,
        };
        let maximum = buffer.buffer.max_point();
        let line_count = if maximum.column == 0 {
            maximum.row
        } else {
            maximum.row.saturating_add(1)
        };
        ensure!(
            ranges.iter().all(|range| *range.end() <= line_count),
            "GitHub patch does not match the pinned file"
        );
    }
    for buffer in buffers {
        buffer.allowed_lines = match buffer.side {
            DiffSide::Left => lines.left.clone(),
            DiffSide::Right => lines.right.clone(),
        };
    }
    Ok(())
}

pub(super) fn validate_github_anchor(anchor: &CommentAnchor, patch: Option<&str>) -> Result<()> {
    ensure!(
        anchor.subject_type == CommentSubject::Line,
        "Expected an inline comment location"
    );
    let end = anchor.line.context("Comment has no line")?;
    let start = anchor.start_line.unwrap_or(end);
    ensure!(
        start > 0 && start <= end && end <= i32::MAX as u32,
        "Invalid comment line range"
    );
    ensure!(
        anchor.start_line.is_some() == anchor.start_side.is_some()
            && anchor.start_side.is_none_or(|side| side == anchor.side),
        "Invalid comment range side"
    );
    let lines = parse_github_patch(patch)?;
    let hunks = match anchor.side {
        DiffSide::Left => &lines.left,
        DiffSide::Right => &lines.right,
    };
    ensure!(
        lines_allowed(&(start..=end), hunks),
        "Comment is outside the canonical GitHub patch"
    );
    Ok(())
}

fn patch_range(value: &str, prefix: char) -> Result<(u32, u32)> {
    let value = value
        .strip_prefix(prefix)
        .context("Malformed GitHub patch hunk")?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    ensure!(
        !start.is_empty()
            && !count.is_empty()
            && start
                .bytes()
                .chain(count.bytes())
                .all(|byte| byte.is_ascii_digit()),
        "Malformed GitHub patch range"
    );
    let start = start.parse::<u32>()?;
    let count = count.parse::<u32>()?;
    ensure!(
        (count == 0 || start > 0) && start <= i32::MAX as u32,
        "Invalid GitHub patch range"
    );
    ensure!(
        start
            .checked_add(count)
            .is_some_and(|end| end <= i32::MAX as u32),
        "GitHub patch range overflow"
    );
    Ok((start, count))
}

fn parse_github_patch(patch: Option<&str>) -> Result<PatchLines> {
    let patch = patch
        .filter(|patch| !patch.is_empty())
        .context("GitHub did not provide a complete text patch for this file")?;
    let mut input = patch.lines().peekable();
    let mut result = PatchLines {
        left: Vec::new(),
        right: Vec::new(),
    };
    let mut previous_end = None;
    while let Some(header) = input.next() {
        let mut fields = header.splitn(5, ' ');
        ensure!(
            fields.next() == Some("@@"),
            "Malformed or truncated GitHub patch"
        );
        let (old_start, old_count) =
            patch_range(fields.next().context("Missing old hunk range")?, '-')?;
        let (new_start, new_count) =
            patch_range(fields.next().context("Missing new hunk range")?, '+')?;
        ensure!(
            fields.next() == Some("@@") && (old_count > 0 || new_count > 0),
            "Malformed GitHub patch header"
        );
        if let Some((old_end, new_end)) = previous_end {
            ensure!(
                old_start >= old_end && new_start >= new_end,
                "GitHub patch hunks overlap or are out of order"
            );
        }
        previous_end = Some((old_start + old_count, new_start + new_count));
        let mut old_seen = 0u32;
        let mut new_seen = 0u32;
        let mut changed = false;
        let mut previous_content = false;
        while let Some(line) = input.peek() {
            if line.starts_with("@@") {
                break;
            }
            let line = input.next().context("Missing GitHub patch body")?;
            match line.as_bytes().first().copied() {
                Some(b' ') => {
                    old_seen = old_seen.checked_add(1).context("Patch count overflow")?;
                    new_seen = new_seen.checked_add(1).context("Patch count overflow")?;
                    previous_content = true;
                }
                Some(b'-') => {
                    old_seen = old_seen.checked_add(1).context("Patch count overflow")?;
                    changed = true;
                    previous_content = true;
                }
                Some(b'+') => {
                    new_seen = new_seen.checked_add(1).context("Patch count overflow")?;
                    changed = true;
                    previous_content = true;
                }
                Some(b'\\') => {
                    ensure!(
                        line == "\\ No newline at end of file" && previous_content,
                        "Malformed patch newline marker"
                    );
                    previous_content = false;
                }
                _ => anyhow::bail!("Malformed or truncated GitHub patch body"),
            }
            ensure!(
                old_seen <= old_count && new_seen <= new_count,
                "GitHub patch body exceeds its hunk counts"
            );
        }
        ensure!(
            changed && old_seen == old_count && new_seen == new_count,
            "GitHub patch body is incomplete"
        );
        // A zero-count side has no commentable line, including insertions at EOF.
        if old_count > 0 {
            result.left.push(old_start..=old_start + old_count - 1);
        }
        if new_count > 0 {
            result.right.push(new_start..=new_start + new_count - 1);
        }
    }
    ensure!(
        !result.left.is_empty() || !result.right.is_empty(),
        "GitHub patch has no hunks"
    );
    Ok(result)
}

pub(super) fn capture_selection(
    editor: &SplittableEditor,
    buffers: &[CanonicalCommentBuffer],
    context: &ReviewContext,
    selector: &ComparisonSelector,
    cx: &mut App,
) -> Result<CapturedCommentAnchor> {
    ensure!(
        selector.supports_github_viewed(),
        "Open the full PR diff to add inline comments"
    );
    context.validate()?;
    ensure!(
        matches!(selector, ComparisonSelector::PullRequest { identity } if identity == &context.identity),
        "Comment context belongs to a different PR or account"
    );
    let owner = editor.focused_editor().clone();
    let (anchor, placement) = owner.update(cx, |focused, cx| {
        ensure!(
            focused.selections.count() == 1,
            "Select one contiguous range to comment"
        );
        let display = focused.display_snapshot(cx);
        let selection = focused.selections.newest::<MultiBufferOffset>(&display);
        let snapshot = focused.buffer().read(cx).snapshot(cx);
        let mut regions = snapshot.range_to_buffer_ranges_with_deleted_hunks(selection.range());
        let (buffer, mut range, _) = regions.next().context("Select a line in the PR diff")?;
        let target = buffers
            .iter()
            .find(|target| target.buffer_id == buffer.remote_id())
            .context("Selection is not part of the canonical PR diff")?;
        for (next_buffer, next_range, _) in regions {
            ensure!(
                next_buffer.remote_id() == buffer.remote_id() && range.end == next_range.start,
                "Comments cannot span files, diff sides, or omitted lines"
            );
            range.end = next_range.end;
        }
        let points = range.start.to_point(buffer)..range.end.to_point(buffer);
        let lines = inclusive_lines(points)?;
        ensure!(
            lines_allowed(&lines, &target.allowed_lines),
            "Select lines inside a single canonical PR diff hunk"
        );
        let anchor = CommentAnchor {
            path: target.path.clone(),
            commit_oid: context.head_oid.clone(),
            side: target.side,
            line: Some(*lines.end()),
            start_line: (*lines.start() != *lines.end()).then_some(*lines.start()),
            start_side: (*lines.start() != *lines.end()).then_some(target.side),
            subject_type: CommentSubject::Line,
        };
        anchor.validate(context)?;
        // Preserve the deleted-hunk anchor from this exact multibuffer instead of
        // remapping its visual row onto the right-hand-side source file.
        let placement_offset = if selection.end > selection.start {
            snapshot.clip_offset(selection.end - 1, Bias::Left)
        } else {
            selection.end
        };
        Ok::<_, anyhow::Error>((anchor, snapshot.anchor_before(placement_offset)))
    })?;
    Ok(CapturedCommentAnchor {
        anchor,
        editor: owner,
        placement,
    })
}

fn inclusive_lines(range: Range<Point>) -> Result<RangeInclusive<u32>> {
    ensure!(range.start <= range.end, "Invalid selection range");
    let first = range
        .start
        .row
        .checked_add(1)
        .context("Comment line overflow")?;
    let final_row = if range.start != range.end && range.end.column == 0 {
        range.end.row.saturating_sub(1)
    } else {
        range.end.row
    };
    let last = final_row.checked_add(1).context("Comment line overflow")?;
    ensure!(
        first <= last && last <= i32::MAX as u32,
        "Invalid comment line range"
    );
    Ok(first..=last)
}

fn lines_allowed(lines: &RangeInclusive<u32>, hunks: &[RangeInclusive<u32>]) -> bool {
    hunks
        .iter()
        .any(|hunk| hunk.start() <= lines.start() && hunk.end() >= lines.end())
}

pub(super) fn placement_for_thread(
    editor: &SplittableEditor,
    buffers: &[CanonicalCommentBuffer],
    thread: &RemoteThread,
    cx: &App,
) -> Result<(Entity<Editor>, Anchor)> {
    ensure!(
        !thread.is_outdated,
        "Outdated threads are not anchored to the current diff"
    );
    let line = thread
        .line
        .context("This thread has no current inline location")?;
    placement_for_anchor(editor, buffers, &thread.path, thread.diff_side, line, cx)
}

pub(super) fn placement_for_anchor(
    editor: &SplittableEditor,
    buffers: &[CanonicalCommentBuffer],
    path: &str,
    side: DiffSide,
    line: u32,
    cx: &App,
) -> Result<(Entity<Editor>, Anchor)> {
    let target = buffers
        .iter()
        .find(|target| target.path == path && target.side == side)
        .context("Comment file is not loaded in this diff")?;
    ensure!(
        lines_allowed(&(line..=line), &target.allowed_lines),
        "Thread is outside the canonical diff hunks"
    );
    let owner = if target.side == DiffSide::Left && editor.is_split() {
        editor
            .lhs_editor()
            .context("Left diff editor is unavailable")?
            .clone()
    } else {
        editor.rhs_editor().clone()
    };
    let snapshot = owner.read(cx).buffer().read(cx).snapshot(cx);
    let unified_left = (target.side == DiffSide::Left && !editor.is_split())
        .then_some((&target.rhs, &target.diff));
    let placement = placement_for_line(&snapshot, &target.buffer, line, unified_left)?;
    Ok((owner, placement))
}

pub(super) fn placement_for_line(
    snapshot: &MultiBufferSnapshot,
    buffer: &BufferSnapshot,
    line: u32,
    unified_left: Option<(&BufferSnapshot, &BufferDiffSnapshot)>,
) -> Result<Anchor> {
    ensure!(line > 0 && line <= i32::MAX as u32, "Invalid comment line");
    let point = Point::new(line - 1, 0);
    ensure!(point <= buffer.max_point(), "Comment line no longer exists");
    let anchor = buffer.anchor_before(point);
    if let Some(placement) = snapshot.anchor_in_excerpt(anchor) {
        return Ok(placement);
    }
    let (rhs, diff) = unified_left.context("Comment line is outside the visible diff")?;
    ensure!(
        diff.base_text().remote_id() == buffer.remote_id(),
        "Comment belongs to a different diff base"
    );
    let rhs_point = diff.base_text_point_to_buffer_point(point, rhs);
    let placement = snapshot
        .anchor_in_excerpt(rhs.anchor_before(rhs_point))
        .context("Comment line is outside the visible diff")?;
    Ok(placement.with_diff_base_anchor(anchor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn canonical_rename_and_deleted_left_placement_use_source_buffer_ids(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::review::{ComparisonKind, FileSide, ObjectFormat, ObjectId};
        use gpui::AppContext as _;
        use language::Buffer;
        use multi_buffer::MultiBuffer;

        let rhs = cx.new(|cx| Buffer::local("first\nreplacement\nlast\n", cx));
        let rhs_snapshot = rhs.read_with(cx, |buffer, _| buffer.snapshot());
        let diff = cx.new(|cx| {
            buffer_diff::BufferDiff::new_with_base_text("first\nremoved\nlast\n", &rhs_snapshot, cx)
        });
        let diff_snapshot = diff.read_with(cx, |diff, cx| diff.snapshot(cx));
        let side = |path: &str, text: &[u8]| FileSide {
            path: path.to_owned(),
            object: ObjectId::for_blob(ObjectFormat::Sha1, text),
            mode: 0o100644,
        };
        let file = FileComparison {
            kind: ComparisonKind::Full,
            old: Some(side("src/previous.rs", b"first\nremoved\nlast\n")),
            new: Some(side("src/current.rs", b"first\nreplacement\nlast\n")),
        };
        let mut buffers =
            canonical_comment_buffers(&file, &rhs_snapshot, &diff_snapshot, false).unwrap();
        assert_eq!(buffers.len(), 2);
        assert!(buffers.iter().all(|buffer| buffer.allowed_lines.is_empty()));
        restrict_to_github_patch(
            &mut buffers,
            Some("@@ -1,3 +1,3 @@\n first\n-removed\n+replacement\n last"),
        )
        .unwrap();
        assert!(
            buffers
                .iter()
                .all(|buffer| buffer.allowed_lines == vec![1..=3])
        );
        assert!(
            restrict_to_github_patch(&mut buffers, Some("@@ -1,2 +1 @@\n-wrong\n+replacement"))
                .is_err()
        );
        assert!(buffers.iter().all(|buffer| buffer.allowed_lines.is_empty()));

        assert!(buffers.iter().all(|buffer| buffer.path == "src/current.rs"));
        assert!(buffers.iter().any(|buffer| buffer.side == DiffSide::Left
            && buffer.buffer_id == diff_snapshot.base_text().remote_id()));
        assert!(
            buffers.iter().any(|buffer| buffer.side == DiffSide::Right
                && buffer.buffer_id == rhs_snapshot.remote_id())
        );
        assert!(canonical_comment_buffers(&file, &rhs_snapshot, &diff_snapshot, true).is_err());
        let mut submodule = file;
        submodule.new.as_mut().unwrap().mode = 0o160000;
        assert!(
            canonical_comment_buffers(&submodule, &rhs_snapshot, &diff_snapshot, false).is_err()
        );

        let multi = cx.new(|cx| {
            let mut multi = MultiBuffer::singleton(rhs.clone(), cx);
            multi.add_diff(diff.clone(), cx);
            multi.set_all_diff_hunks_expanded(cx);
            multi
        });
        let snapshot = multi.read_with(cx, |multi, cx| multi.snapshot(cx));
        let placement = placement_for_line(
            &snapshot,
            diff_snapshot.base_text(),
            2,
            Some((&rhs_snapshot, &diff_snapshot)),
        )
        .unwrap();
        assert_eq!(
            placement.diff_base_anchor().unwrap().buffer_id,
            diff_snapshot.base_text().remote_id()
        );
        assert!(
            placement_for_line(
                &snapshot,
                diff_snapshot.base_text(),
                99,
                Some((&rhs_snapshot, &diff_snapshot))
            )
            .is_err()
        );
        let deleted = snapshot
            .range_to_buffer_ranges_with_deleted_hunks(placement..placement)
            .next()
            .unwrap();
        assert_eq!(deleted.0.remote_id(), diff_snapshot.base_text().remote_id());
        assert_eq!(deleted.1.start.to_point(deleted.0).row, 1);
    }

    #[test]
    fn comment_lines_are_inclusive_and_exclude_unselected_final_row() {
        assert_eq!(
            inclusive_lines(Point::new(3, 2)..Point::new(3, 2)).unwrap(),
            4..=4
        );
        assert_eq!(
            inclusive_lines(Point::new(3, 0)..Point::new(5, 0)).unwrap(),
            4..=5
        );
        assert_eq!(
            inclusive_lines(Point::new(3, 2)..Point::new(5, 1)).unwrap(),
            4..=6
        );
        assert!(inclusive_lines(Point::new(5, 0)..Point::new(3, 0)).is_err());
        assert!(inclusive_lines(Point::new(u32::MAX, 0)..Point::new(u32::MAX, 0)).is_err());
    }

    #[test]
    fn comment_ranges_cannot_cross_canonical_hunk_gaps() {
        let hunks = [2..=6, 10..=14];
        assert!(lines_allowed(&(2..=6), &hunks));
        assert!(lines_allowed(&(11..=11), &hunks));
        assert!(!lines_allowed(&(6..=10), &hunks));
        assert!(!lines_allowed(&(1..=2), &hunks));
        assert!(!lines_allowed(&(1..=1), &[]));
    }

    #[test]
    fn github_patch_empty_sides_never_invent_lines() {
        let inserted = parse_github_patch(Some("@@ -0,0 +1,2 @@\n+first\n+second")).unwrap();
        assert!(inserted.left.is_empty());
        assert_eq!(inserted.right, vec![1..=2]);
        let deleted = parse_github_patch(Some("@@ -1,2 +0,0 @@\n-first\n-second")).unwrap();
        assert_eq!(deleted.left, vec![1..=2]);
        assert!(deleted.right.is_empty());
        let interior = parse_github_patch(Some("@@ -7,0 +8 @@\n+inserted")).unwrap();
        assert!(interior.left.is_empty());
        assert_eq!(interior.right, vec![8..=8]);
        let deletion = parse_github_patch(Some("@@ -8 +7,0 @@\n-removed")).unwrap();
        assert_eq!(deletion.left, vec![8..=8]);
        assert!(deletion.right.is_empty());
    }

    #[test]
    fn github_patch_repeated_text_uses_server_coordinates_and_counts() {
        let patch = "@@ -2,2 +2,2 @@ example\n repeated\n-old\n+new\n@@ -9 +9 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file";
        let lines = parse_github_patch(Some(patch)).unwrap();
        assert_eq!(lines.left, vec![2..=3, 9..=9]);
        assert_eq!(lines.right, vec![2..=3, 9..=9]);
        assert!(!lines_allowed(&(3..=9), &lines.right));
    }

    #[test]
    fn github_patch_missing_truncated_and_malformed_are_rejected() {
        assert!(parse_github_patch(None).is_err());
        for patch in [
            "",
            "@@ -1 +1 @@\n-old",
            "@@ -1 +1 @@\n-old\n+new\n+extra",
            "@@ -0 +1 @@\n-old\n+new",
            "@@ -1 +1 @@\n unchanged",
            "@@ -1 +1 @@\n\\ No newline at end of file\n-old\n+new",
            "@@ -1 +1 @@\n-old\n+new\n...",
            "@@ -2 +2 @@\n-old\n+new\n@@ -1 +1 @@\n-old\n+new",
        ] {
            assert!(
                parse_github_patch(Some(patch)).is_err(),
                "Accepted {patch:?}"
            );
        }
    }
}
