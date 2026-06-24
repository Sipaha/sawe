use settings::{RegisterSetting, Settings};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, RegisterSetting)]
pub struct SolutionsSettings {
    pub root: PathBuf,
    /// S-SOL-LOG aggregated-log configuration.
    pub aggregated_log: AggregatedLogSettings,
    /// S-SOL-PRT branch-protection rules. Solution-wide defaults plus
    /// per-member overrides keyed by `SolutionMember::catalog_id`.
    pub branch_protection: BranchProtectionSettings,
    /// S-AI-CHP cross-member cherry-pick suggestion knobs.
    pub ai_cherry_pick_suggest: AiCherryPickSuggestSettings,
}

#[derive(Clone, Debug)]
pub struct AggregatedLogSettings {
    /// Pre-warm member buffers when a Solution is opened.
    pub background_load: bool,
    /// Hard cap on commits served per aggregated-log session.
    pub max_total_commits: u32,
}

impl Default for AggregatedLogSettings {
    fn default() -> Self {
        Self {
            background_load: true,
            max_total_commits: 50_000,
        }
    }
}

/// S-AI-CHP — cross-member cherry-pick suggestion engine config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiCherryPickSuggestSettings {
    /// Run analyze in the background on a daily cadence + every Fetch
    /// All. Off by default — every run spends AI tokens.
    pub background: bool,
    /// Hard cap on estimated tokens per analyze run.
    pub token_budget: u32,
}

impl Default for AiCherryPickSuggestSettings {
    fn default() -> Self {
        Self {
            background: false,
            token_budget: 25_000,
        }
    }
}

/// Solution-wide branch-protection policy. See
/// [`crate::branch_protection`] for the matching semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchProtectionSettings {
    /// Glob patterns matched against branch names. Applied to every
    /// member of the Solution unless a per-member override extends or
    /// supersedes them.
    pub default_protected: Vec<String>,
    /// Per-member overrides keyed by `SolutionMember::catalog_id`.
    pub members: HashMap<String, BranchProtectionMember>,
}

impl Default for BranchProtectionSettings {
    fn default() -> Self {
        Self {
            default_protected: vec!["main".into(), "master".into(), "release/*".into()],
            members: HashMap::new(),
        }
    }
}

/// Per-member tightening of [`BranchProtectionSettings`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchProtectionMember {
    /// Additional glob patterns to treat as protected for this member
    /// (in addition to `default_protected`).
    pub protected: Vec<String>,
    /// Forbid force-push to protected branches (rather than just
    /// requiring confirmation).
    pub no_force_push: bool,
    /// Forbid `reset --hard` against protected branches.
    pub no_force_reset: bool,
    /// Forbid drop-commit (rebase --interactive with `drop`) on
    /// protected branches.
    pub no_drop_commit: bool,
}

impl Default for SolutionsSettings {
    fn default() -> Self {
        Self {
            root: default_root(),
            aggregated_log: AggregatedLogSettings::default(),
            branch_protection: BranchProtectionSettings::default(),
            ai_cherry_pick_suggest: AiCherryPickSuggestSettings::default(),
        }
    }
}

/// Default solutions storage root: `<base_dir>/solutions`. The base
/// directory comes from `paths::base_dir()` (single-folder profile —
/// `~/spk-editor` for release, `~/spk-editor-dev` for debug, or any
/// `set_custom_data_dir` override) so all per-profile state lives in
/// one place.
fn default_root() -> PathBuf {
    paths::base_dir().join("solutions")
}

impl Settings for SolutionsSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let solutions = content.solutions.as_ref();
        let root = match solutions.and_then(|s| s.root.clone()) {
            Some(raw) => PathBuf::from(shellexpand::tilde(&raw).into_owned()),
            None => default_root(),
        };
        let defaults = AggregatedLogSettings::default();
        let aggregated_log = solutions
            .and_then(|s| s.git.as_ref())
            .and_then(|g| g.aggregated_log.as_ref())
            .map(|a| AggregatedLogSettings {
                background_load: a.background_load.unwrap_or(defaults.background_load),
                max_total_commits: a.max_total_commits.unwrap_or(defaults.max_total_commits),
            })
            .unwrap_or(defaults);
        let branch_protection = solutions
            .and_then(|s| s.branch_protection.as_ref())
            .map(branch_protection_from_content)
            .unwrap_or_default();
        let ai_defaults = AiCherryPickSuggestSettings::default();
        let ai_cherry_pick_suggest = solutions
            .and_then(|s| s.git.as_ref())
            .and_then(|g| g.ai_cherry_pick_suggest.as_ref())
            .map(|c| AiCherryPickSuggestSettings {
                background: c.background.unwrap_or(ai_defaults.background),
                token_budget: c.token_budget.unwrap_or(ai_defaults.token_budget),
            })
            .unwrap_or(ai_defaults);
        let result = Self {
            root,
            aggregated_log,
            branch_protection,
            ai_cherry_pick_suggest,
        };
        crate::store::set_branch_protection_settings(result.branch_protection.clone());
        result
    }
}

fn branch_protection_from_content(
    content: &settings::SolutionBranchProtectionSettingsContent,
) -> BranchProtectionSettings {
    let default_protected = content
        .default_protected
        .clone()
        .unwrap_or_else(|| BranchProtectionSettings::default().default_protected);
    let members = content
        .members
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), member_from_content(v)))
                .collect()
        })
        .unwrap_or_default();
    BranchProtectionSettings {
        default_protected,
        members,
    }
}

fn member_from_content(
    content: &settings::SolutionBranchProtectionMemberContent,
) -> BranchProtectionMember {
    BranchProtectionMember {
        protected: content.protected.clone().unwrap_or_default(),
        no_force_push: content.no_force_push.unwrap_or_default(),
        no_force_reset: content.no_force_reset.unwrap_or_default(),
        no_drop_commit: content.no_drop_commit.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_root_expands_tilde() {
        let s = SolutionsSettings::default();
        assert!(
            !s.root.starts_with("~"),
            "tilde was not expanded: {}",
            s.root.display()
        );
        // Root is `~/spk-editor/solutions` in release and
        // `~/spk-editor-dev/solutions` in debug. Either way the last
        // segment is `solutions` and the parent matches the active
        // base directory name.
        assert!(s.root.ends_with("solutions"));
    }

    #[test]
    fn aggregated_log_defaults() {
        let s = SolutionsSettings::default();
        assert!(s.aggregated_log.background_load);
        assert_eq!(s.aggregated_log.max_total_commits, 50_000);
    }

    #[test]
    fn branch_protection_defaults_cover_main_master_release() {
        let s = SolutionsSettings::default();
        let patterns: Vec<&str> = s
            .branch_protection
            .default_protected
            .iter()
            .map(|s| s.as_str())
            .collect();
        assert!(patterns.contains(&"main"));
        assert!(patterns.contains(&"master"));
        assert!(patterns.contains(&"release/*"));
    }
}
