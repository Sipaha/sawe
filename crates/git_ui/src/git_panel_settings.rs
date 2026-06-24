use editor::{EditorSettings, ui_scrollbar_settings_from_raw};
use gpui::Pixels;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, StatusStyle};
use ui::{
    px,
    scrollbars::{ScrollbarVisibility, ShowScrollbar},
};
use workspace::dock::DockPosition;

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ScrollbarSettings {
    pub show: Option<ShowScrollbar>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitViewSettings {
    pub fetch_avatars: bool,
    pub affected_files_lazy_threshold: usize,
    pub parse_issue_references: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveRebaseSettings {
    pub allow_exec_via_mcp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowAtRevisionSettings {
    pub cleanup_orphans_older_than_h: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitExplanationsSettings {
    pub cache_ttl_days: u32,
}

impl Default for CommitExplanationsSettings {
    fn default() -> Self {
        Self {
            cache_ttl_days: crate::commit_view::ai_explain::DEFAULT_CACHE_TTL_DAYS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct GitPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub status_style: StatusStyle,
    pub file_icons: bool,
    pub folder_icons: bool,
    pub scrollbar: ScrollbarSettings,
    pub fallback_branch_name: String,
    pub sort_by_path: bool,
    pub collapse_untracked_diff: bool,
    pub tree_view: bool,
    pub diff_stats: bool,
    pub show_count_badge: bool,
    pub starts_open: bool,
    pub commit_title_max_length: usize,
    pub commit_view: CommitViewSettings,
    pub interactive_rebase: InteractiveRebaseSettings,
    pub show_at_revision: ShowAtRevisionSettings,
    pub run_pre_commit_hooks_in_panel: bool,
    pub commit_explanations: CommitExplanationsSettings,
}

#[derive(Default)]
pub(crate) struct GitPanelScrollbarAccessor;

impl ScrollbarVisibility for GitPanelScrollbarAccessor {
    fn visibility(&self, cx: &ui::App) -> ShowScrollbar {
        // TODO: This PR should have defined Editor's `scrollbar.axis`
        // as an Option<ScrollbarAxis>, not a ScrollbarAxes as it would allow you to
        // `.unwrap_or(EditorSettings::get_global(cx).scrollbar.show)`.
        //
        // Once this is fixed we can extend the GitPanelSettings with a `scrollbar.axis`
        // so we can show each axis based on the settings.
        //
        // We should fix this. PR: https://github.com/zed-industries/zed/pull/19495
        GitPanelSettings::get_global(cx)
            .scrollbar
            .show
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl Settings for GitPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let git_panel = content.git_panel.clone().unwrap();
        Self {
            button: git_panel.button.unwrap(),
            dock: git_panel.dock.unwrap().into(),
            default_width: px(git_panel.default_width.unwrap()),
            status_style: git_panel.status_style.unwrap(),
            file_icons: git_panel.file_icons.unwrap(),
            folder_icons: git_panel.folder_icons.unwrap(),
            scrollbar: ScrollbarSettings {
                show: git_panel
                    .scrollbar
                    .unwrap()
                    .show
                    .map(ui_scrollbar_settings_from_raw),
            },
            fallback_branch_name: git_panel.fallback_branch_name.unwrap(),
            sort_by_path: git_panel.sort_by_path.unwrap(),
            collapse_untracked_diff: git_panel.collapse_untracked_diff.unwrap(),
            tree_view: git_panel.tree_view.unwrap(),
            diff_stats: git_panel.diff_stats.unwrap(),
            show_count_badge: git_panel.show_count_badge.unwrap(),
            starts_open: git_panel.starts_open.unwrap(),
            commit_title_max_length: git_panel.commit_title_max_length.unwrap(),
            commit_view: {
                let raw = git_panel.commit_view.unwrap();
                CommitViewSettings {
                    fetch_avatars: raw.fetch_avatars.unwrap_or(false),
                    affected_files_lazy_threshold: raw.affected_files_lazy_threshold.unwrap_or(500),
                    parse_issue_references: raw.parse_issue_references.unwrap_or(true),
                }
            },
            interactive_rebase: {
                let raw = git_panel.interactive_rebase.unwrap_or_default();
                InteractiveRebaseSettings {
                    allow_exec_via_mcp: raw.allow_exec_via_mcp.unwrap_or(false),
                }
            },
            show_at_revision: {
                let raw = git_panel.show_at_revision.unwrap_or_default();
                ShowAtRevisionSettings {
                    cleanup_orphans_older_than_h: raw.cleanup_orphans_older_than_h.unwrap_or(
                        crate::handlers::show_at_revision::DEFAULT_CLEANUP_ORPHANS_OLDER_THAN_H,
                    ),
                }
            },
            run_pre_commit_hooks_in_panel: git_panel.run_pre_commit_hooks_in_panel.unwrap_or(true),
            commit_explanations: {
                let raw = git_panel.commit_explanations.unwrap_or_default();
                CommitExplanationsSettings {
                    cache_ttl_days: raw
                        .cache_ttl_days
                        .unwrap_or(crate::commit_view::ai_explain::DEFAULT_CACHE_TTL_DAYS),
                }
            },
        }
    }
}
