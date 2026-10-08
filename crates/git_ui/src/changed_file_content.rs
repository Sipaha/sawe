//! Shared file-row content for Changes and commit previews. Staging controls
//! and revision-specific activation belong to the caller.
use crate::{git_panel_settings::GitPanelSettings, git_status_icon};
use file_icons::FileIcons;
use git::{
    repository::RepoPath,
    status::{DiffStat, FileStatus},
};
use gpui::{App, ElementId, FontWeight, RenderOnce, Window};
use project::project_settings::{GitPathStyle, ProjectSettings};
use settings::{Settings, StatusStyle};
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon, prelude::*};
use util::paths::PathStyle;

#[derive(IntoElement)]
pub(crate) struct ChangedFileContent {
    pub id: ElementId,
    pub path: RepoPath,
    pub status: FileStatus,
    pub path_style: PathStyle,
    pub tree_view: bool,
    pub bold: bool,
    pub stat: Option<DiffStat>,
}

impl RenderOnce for ChangedFileContent {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let settings = GitPanelSettings::get_global(cx);
        let status = self.status;
        let label_color = if settings.status_style == StatusStyle::LabelColor {
            if status.is_conflicted() {
                Color::VersionControlConflict
            } else if status.is_created() {
                Color::VersionControlUntracked
            } else if status.is_modified() {
                Color::VersionControlModified
            } else if status.is_deleted() {
                Color::Disabled
            } else {
                Color::VersionControlAdded
            }
        } else if status.is_created() {
            Color::VersionControlUntracked
        } else {
            Color::Default
        };
        let path_color = if status.is_deleted() {
            Color::Disabled
        } else {
            Color::Muted
        };
        let file_name = self
            .path
            .file_name()
            .map(str::to_owned)
            .unwrap_or_else(|| self.path.display(self.path_style).to_string());
        let name = Label::new(if self.tree_view {
            file_name.clone()
        } else {
            format!("{file_name} ")
        })
        .color(label_color)
        .when(self.bold, |label| label.weight(FontWeight::BOLD))
        .when(status.is_deleted(), Label::strikethrough);
        let git_path_style = ProjectSettings::get_global(cx).git.path_style;
        let label = if self.tree_view {
            div().min_w_0().child(name.truncate())
        } else {
            h_flex()
                .min_w_0()
                .overflow_hidden()
                .when(git_path_style == GitPathStyle::FilePathFirst, |row| {
                    row.flex_row_reverse()
                })
                .child(div().flex_none().child(name))
                .when_some(self.path.parent(), |row, directory| {
                    let mut directory = directory.display(self.path_style).to_string();
                    if git_path_style != GitPathStyle::FileNameFirst {
                        directory.push_str(self.path_style.primary_separator());
                    }
                    row.child(
                        Label::new(directory)
                            .color(path_color)
                            .truncate_start()
                            .when(status.is_deleted(), Label::strikethrough),
                    )
                })
        };
        let name_row = h_flex()
            .min_w_0()
            .flex_1()
            .gap_1()
            .when(settings.file_icons, |row| {
                row.child(
                    FileIcons::get_icon(self.path.as_std_path(), cx)
                        .map(|icon| {
                            Icon::from_path(icon)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                        })
                        .unwrap_or_else(|| {
                            Icon::new(IconName::File)
                                .size(IconSize::Small)
                                .color(Color::Muted)
                        }),
                )
            })
            .when(settings.status_style != StatusStyle::LabelColor, |row| {
                row.child(git_status_icon(status))
            })
            .child(label);
        h_flex()
            // Keep the complete configured UI font (including fallbacks and
            // features) identical in the panel and in button-based previews.
            .font(theme::theme_settings(cx).ui_font(cx).clone())
            .min_w_0()
            .flex_1()
            .gap_1p5()
            .child(name_row)
            .when(settings.diff_stats, |row| {
                row.when_some(self.stat, |row, stat| {
                    row.child(div().flex_shrink_0().child(ui::DiffStat::new(
                        self.id,
                        stat.added as usize,
                        stat.deleted as usize,
                    )))
                })
            })
    }
}
