//! Row decoration / highlight state for the Git Graph view (S-FLT in
//! `docs/superpowers/plans/git-panel-plan.md`).
//!
//! Skeleton — actual decoration rendering lands when chip-MyCommits and
//! chip-NewSinceRefresh wire up.

use git::Oid;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HighlightSet {
    /// Highlight commits authored by the local `user.email`. Plan default
    /// `git.log.show_my_commits_highlight = true`, so this starts on.
    pub my_commits: bool,

    /// Highlight commits authored after the last close of the panel.
    /// `last_seen_sha` is the tip-at-close anchor; commits between
    /// `last_seen_sha..HEAD` get a "new" decoration when on.
    pub new_since_refresh: bool,
    pub last_seen_sha: Option<Oid>,
}

impl HighlightSet {
    pub fn any_active(&self) -> bool {
        self.my_commits || self.new_since_refresh
    }
}
