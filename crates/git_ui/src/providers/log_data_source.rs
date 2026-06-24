//! Provider trait for swapping the git-graph log data source between
//! single-repo (default) and solution-wide aggregated.
//!
//! Implemented by `solution_git::aggregator::SolutionGitAggregator`
//! (S-SOL-LOG). `git_graph` consumes the trait through the registry in
//! [`crate::providers`] and never imports `solution_git` directly (P-9).
//!
//! ## Why the trait takes pre-rendered git args, not `LogFilters`
//!
//! `LogFilters` lives in `git_graph::filters` and `git_graph` depends on
//! `git_ui` (not the other way around), so importing it here would create
//! a circular crate dependency. The trait sidesteps the cycle by
//! accepting the already-rendered `git log` argv that the toolbar
//! produces via `LogFilters::to_git_args` + `LogFilters::paths_args` —
//! the aggregator never needs the rich filter object, only what the
//! filter would have spelled to git.

use anyhow::Result;
use gpui::{App, Hsla, SharedString, Task};
use std::ops::Range;

/// Pre-rendered query inputs for [`LogDataSource::fetch_log`].
///
/// `git_args` contains everything `git log` should accept *before* the
/// `--` separator (date / author / branches / sha / etc.). `paths` is
/// what would land *after* `--` — kept separate because the aggregator
/// needs to test each path's existence per-member (`git rev-parse
/// HEAD:<path>`) and skip members where the path doesn't exist.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub git_args: Vec<String>,
    pub paths: Vec<String>,
}

/// One commit in the aggregated log. Fields mirror what `git log
/// --format=%H%P%ct%an%ae%D%s` produces, plus the member-id / member-color
/// pair needed for the badge / column decoration introduced in S-SOL-LOG.
#[derive(Debug, Clone)]
pub struct AggregatedCommit {
    /// Stable id of the source member (the catalog id from
    /// `solutions::SolutionMember::catalog_id`). Used by the per-row
    /// context menu to resolve the right repo.
    pub member_id: SharedString,
    /// Color associated with `member_id`. Derived deterministically from
    /// the id hash; safe to render directly as a badge fill.
    pub member_color: Hsla,
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub committer_date_unix: i64,
    pub subject: String,
    pub ref_names: Vec<String>,
}

/// Inversion-of-control trait: `git_graph::GitGraph` calls this to fetch
/// commits when running in solution-wide mode. The single-repo path
/// continues to read directly from `Repository`.
pub trait LogDataSource: Send + Sync {
    /// True when the aggregated source is currently available — a Solution
    /// is open with at least one member. The git-graph toolbar offers the
    /// `Per-Repo | Solution-wide` toggle only when this returns `true`.
    fn is_active(&self) -> bool;

    /// Fetch commits for the given filter set + lazy range. The aggregator
    /// internally pages each member's `git log` and pops from a k-way
    /// merge buffer; `range.start` is the absolute commit index in the
    /// merged stream. Implementations are expected to be idempotent and
    /// safe to call repeatedly with overlapping ranges.
    ///
    /// `members` optionally narrows the active member set (Members chip
    /// filter). Empty / `None` ⇒ all members of the active Solution.
    fn fetch_log(
        &self,
        query: LogQuery,
        members: Option<Vec<SharedString>>,
        range: Range<usize>,
        cx: &mut App,
    ) -> Task<Result<Vec<AggregatedCommit>>>;
}
