# GitHub Pull Request Review Architecture

Pull request review reuses Zed's native editor and immutable Git snapshots. It
must not depend on application-specific repositories, sample pull requests or
credentials in source. User documentation is in [Git](../git.md#pull-request-reviews).

## Boundaries

- `git::review` defines immutable endpoint comparisons and Git fetch semantics.
- `project::git_store` binds those operations to the active local repository.
- `git_ui::review` owns serializable identity, snapshot and operation models.
- `git_ui::review_store` persists sessions, retained contents, coverage receipts and the outbox.
- `git_ui::review_coverage` evaluates exact file-state transitions; `review_view::coverage` coordinates selected-commit receipts and full-file promotion.
- `git_ui::github_pull_requests` implements provider reads; its `transport`
  module owns bounded `gh` subprocess execution and sanitized errors.
- `git_ui::github_review` validates review context and implements guarded writes;
  its `protocol` module decodes and validates responses.
- `git_ui::review_github_view` coordinates thread state; `presentation` owns
  shared cards/composers and `delivery` dispatches and reconciles operations.
- `git_ui::review_view` coordinates file navigation and editor lifetimes;
  `presentation` renders the native toolbar/tree and `github_viewed` handles
  Viewed delivery and history retention.

## State invariants

A GitHub observation is accepted only for its account, repository, pull request,
head/base and current snapshot context. Historical cache rows may survive newer
snapshots; completeness and counts project only over current canonical paths.

Visible Viewed state is server state plus the newest local intent. The same
projection drives checkboxes, folders, filtering and the header. Retained-content
approval is independent: old source bytes and approval fingerprints must not be
replaced just because GitHub still reports Viewed.

Selected-commit receipts are scoped to repository, PR, viewer and pinned base.
Only endpoints in the currently verified Git history can contribute. Exact
path/blob/mode transitions must cover the canonical old-to-new file comparison;
unknown rename provenance fails closed. Inferred canonical approvals are tagged
separately from explicit full-file approvals and cannot validate themselves.
Bulk receipt persistence is atomic. Coverage evaluation and historical snapshot
decoding run in bounded background work, and each accepted synchronization batch
is dispatched independently of subsequent navigation or batch failures.

Writes enter a durable outbox before network dispatch. Compatible Viewed intents
coalesce and batch; dispatch ordering and revision checks prevent stale receipts
from overriding newer intent. Unknown delivery is reconciled without blindly
replaying a mutation. Comment drafts and inline composer ownership survive
transitions without rendering duplicate copies of a canonical comment.

Large reviews reuse a bounded native multibuffer window. Refresh selection anchors
only after outgoing excerpts are removed and incoming excerpts installed, before
folding or rendering blocks. Refresh and navigation have separate task ownership.

## Testing

Unit and GPUI tests live with their owning components. Fake provider transports
exercise pagination, stale context, partial delivery and recovery without network
writes. Process tests retain restart and durable outbox coverage.

`git_ui/review-test-support` explicitly enables the marked disposable native
Viewed fault fixture. Normal debug and release builds do not register or route
through this fixture. Its marker and active repository context are both checked;
failure never falls back to network writes.

Live network-writing tests are opt-in, require an external configuration, and
must use owned test comments/data with independently verified cleanup. Private
acceptance payloads and screenshots must remain outside tracked source.

The pull request workflow currently requires `gh`, supports `github.com` local
repositories and retains history without automatic expiration. Enterprise,
remote-project support and a retention UI are separate integration decisions.
