//! Log filter state for the Git Graph view (S-FLT in
//! `docs/superpowers/plans/git-panel-plan.md`).
//!
//! `LogFilters` is the pure-data input that the graph passes through to
//! `git::repository::initial_graph_data` (extended to accept it). Each
//! optional field maps to a git CLI argument set; an empty `LogFilters`
//! produces no extra args and matches the pre-S-FLT behavior.
//!
//! Skeleton — chip-by-chip wiring lands in follow-up commits as each filter
//! UI lights up.

use git::{Oid, repository::RepoPath};
use gpui::SharedString;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LogFilters {
    /// Multi-select branches/refs (chip-Branch). When non-empty, replaces
    /// the implicit `--all` traversal — git log is invoked with these refs
    /// as positional arguments.
    pub branches: Vec<SharedString>,

    /// Multi-select authors (chip-User). Combined into a single
    /// `--author=<re>` regex (alternation). Plan: autocomplete from
    /// `git shortlog -sne`.
    pub authors: Vec<SharedString>,

    /// Date filter (chip-Date). Maps to `--since` / `--until` CLI args.
    pub date_range: Option<DateRange>,

    /// Multi-select paths (chip-Path). Trailing `-- <paths>` after CLI args.
    #[serde(with = "repo_path_vec_serde")]
    #[schemars(with = "Vec<String>")]
    pub paths: Vec<RepoPath>,

    /// Free-text query (chip-Query). Maps to `--grep` / `-G` / direct hash
    /// lookup depending on flags.
    pub query: Option<QueryFilter>,

    /// If true and `branches` is empty, log is invoked with `--all`.
    /// Ignored when `branches` is non-empty (per plan precedence rule).
    pub all_refs: bool,

    /// Optional pin to a specific SHA — used by Show At Revision and
    /// commit-targeted log views. Layered orthogonally to the other
    /// filters; converts to a positional arg.
    #[schemars(with = "Option<String>")]
    pub sha: Option<Oid>,
}

impl LogFilters {
    pub fn is_empty(&self) -> bool {
        self.branches.is_empty()
            && self.authors.is_empty()
            && self.date_range.is_none()
            && self.paths.is_empty()
            && self.query.is_none()
            && !self.all_refs
            && self.sha.is_none()
    }

    /// Number of *active* filters — the chip toolbar uses this for its
    /// "Clear filters" button visibility.
    pub fn active_count(&self) -> usize {
        let mut n = 0;
        if !self.branches.is_empty() {
            n += 1;
        }
        if !self.authors.is_empty() {
            n += 1;
        }
        if self.date_range.is_some() {
            n += 1;
        }
        if !self.paths.is_empty() {
            n += 1;
        }
        if self.query.is_some() {
            n += 1;
        }
        n
    }

    /// Convert the filter state into a list of `git log` CLI arguments.
    ///
    /// The arguments are appended to the base `git log <log-source-arg>`
    /// command in `git::repository::initial_graph_data`. Empty filters
    /// produce an empty `Vec`, which preserves the pre-S-FLT behavior.
    ///
    /// Order matters for `--` separator: anything after `--` is a path,
    /// so [`Self::paths`] is emitted last; the caller MUST keep this list
    /// at the tail of its argv. Other args (`--author`, `--since`, etc.)
    /// can appear in any order before paths.
    pub fn to_git_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        // --all toggle. Per plan precedence rule, ignored when `branches`
        // is non-empty: explicit branches define the traversal set.
        if self.all_refs && self.branches.is_empty() {
            args.push("--all".to_string());
        }

        // Multi-author: a single `--author=<re>` with alternation. Authors
        // are escaped against regex metachars by the caller's UI layer
        // before they land here (chip-User input is matched against
        // shortlog output, not raw user text).
        if !self.authors.is_empty() {
            let pattern = self.authors.join("|");
            args.push(format!("--author={pattern}"));
        }

        if let Some(range) = self.date_range {
            match range {
                DateRange::Since(unix) => args.push(format!("--since=@{unix}")),
                DateRange::Until(unix) => args.push(format!("--until=@{unix}")),
                DateRange::Between { since, until } => {
                    args.push(format!("--since=@{since}"));
                    args.push(format!("--until=@{until}"));
                }
            }
        }

        if let Some(query) = &self.query {
            if query.search_in_diffs {
                // -G searches commit *content* (added/removed lines).
                // Mutually exclusive with --grep at the user-facing level —
                // chip-Query toggles control which mode is active.
                args.push(format!("-G{}", query.text));
            } else {
                args.push(format!("--grep={}", query.text));
            }
            if query.regex {
                args.push("--extended-regexp".to_string());
            }
            // git --grep is case-insensitive by default; case-sensitive
            // requires no flag inversion — git treats `-i` as the
            // case-insensitive opt-in, so case_sensitive=true == default.
            if !query.case_sensitive {
                args.push("--regexp-ignore-case".to_string());
            }
        }

        // Branches are positional refs, so they must come before `--`.
        // Caller appends paths after — see [`Self::paths_args`].
        for branch in &self.branches {
            args.push(branch.to_string());
        }

        // SHA pin is also positional — git log <sha> walks the chain.
        if let Some(sha) = &self.sha {
            args.push(sha.to_string());
        }

        args
    }

    /// Path filter args — must be appended *after* a `--` separator.
    /// Returned separately so the caller controls placement.
    pub fn paths_args(&self) -> Vec<String> {
        self.paths
            .iter()
            .map(|p| p.as_unix_str().to_string())
            .collect()
    }
}

/// Date filter for chip-Date. Unix seconds; UI offers presets (Today /
/// Yesterday / This Week / Last 30 days / All Time) plus custom range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DateRange {
    Since(i64),
    Until(i64),
    Between { since: i64, until: i64 },
}

/// Free-text query filter. The toolbar's text input + toggle row populates
/// this; `git_graph.rs` translates the flag combo into git CLI args at
/// log-time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct QueryFilter {
    pub text: SharedString,
    pub regex: bool,
    pub case_sensitive: bool,
    /// `-G <pat>` instead of `--grep` — searches commit *content*, not the
    /// commit message. Slow on large histories; UI warns about it.
    pub search_in_diffs: bool,
}

/// Marshal `Vec<RepoPath>` through unix-string form. RepoPath itself has no
/// serde impl (lives in upstream `git` crate); this shim avoids modifying
/// it and the on-the-wire form is the same string git itself sees.
mod repo_path_vec_serde {
    use git::repository::RepoPath;
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

    pub fn serialize<S: Serializer>(paths: &[RepoPath], s: S) -> Result<S::Ok, S::Error> {
        let strings: Vec<&str> = paths.iter().map(|p| p.as_unix_str()).collect();
        strings.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<RepoPath>, D::Error> {
        let raw: Vec<String> = Vec::deserialize(d)?;
        raw.into_iter()
            .map(|s| RepoPath::new(&s).map_err(D::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn empty_default() {
        let f = LogFilters::default();
        assert!(f.is_empty());
        assert_eq!(f.active_count(), 0);
    }

    #[test]
    fn active_count_counts_each_dimension_once() {
        let f = LogFilters {
            branches: vec!["main".into(), "dev".into()],
            authors: vec!["alice@example.com".into()],
            date_range: Some(DateRange::Since(0)),
            ..LogFilters::default()
        };
        assert_eq!(f.active_count(), 3);
    }

    #[test]
    fn json_roundtrip_preserves_all_fields() {
        let original = LogFilters {
            branches: vec!["main".into(), "feat/x".into()],
            authors: vec!["alice@example.com".into()],
            date_range: Some(DateRange::Between {
                since: 100,
                until: 200,
            }),
            paths: vec![
                RepoPath::new("crates/foo").expect("repo path"),
                RepoPath::new("README.md").expect("repo path"),
            ],
            query: Some(QueryFilter {
                text: "fix".into(),
                regex: true,
                case_sensitive: true,
                search_in_diffs: false,
            }),
            all_refs: true,
            sha: Some(Oid::from_str("0123456789abcdef0123456789abcdef01234567").expect("oid")),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: LogFilters = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    #[test]
    fn json_roundtrip_empty() {
        let original = LogFilters::default();
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: LogFilters = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
        assert!(parsed.is_empty());
    }
}
