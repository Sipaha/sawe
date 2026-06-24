//! Render-only view-mode toggles for the Git Graph view (S-FLT in
//! `docs/superpowers/plans/git-panel-plan.md`).
//!
//! Distinct from `LogFilters` (which change the `git log` query) and
//! `HighlightSet` (row-decoration toggles). View options affect ONLY how
//! already-loaded rows are rendered, so flipping them never re-runs git.

use gpui::App;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewOptions {
    /// When a commit has more refs than the configured threshold, collapse
    /// the trailing chips into a `+N` badge instead of rendering them all
    /// inline. Threshold reads from `git.log.compact_refs_threshold` via
    /// [`compact_refs_threshold`].
    pub compact_refs: bool,

    /// When true, the table renders a date label at the top of the
    /// description cell on the first commit of each local-day boundary.
    /// `uniform_list` rows are 1-to-1 with commits, so v1 inlines the
    /// header instead of inserting a separator row.
    pub group_by_date: bool,
}

/// Resolve the `git.log.compact_refs_threshold` setting.
pub fn compact_refs_threshold(cx: &App) -> usize {
    use settings::Settings as _;
    project::project_settings::ProjectSettings::get_global(cx)
        .git
        .log
        .compact_refs_threshold as usize
}
