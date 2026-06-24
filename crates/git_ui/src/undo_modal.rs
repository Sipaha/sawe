//! S-BAK undo modal — list recent destructive ops + offer one-click rollback.
//!
//! Triggered by the `git::UndoLast` action (registered on `Workspace`).
//! Reads from [`git::undo_registry`], filtered to entries from the active
//! repository in the last 24 hours.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    AppContext, ClickEvent, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, ParentElement, Render, SharedString, Styled, Window, div,
};
use project::git_store::Repository;
use ui::{
    App, Button, Clickable, Color, Context, Headline, HeadlineSize, Icon, IconName, IconSize,
    IntoElement, Label, LabelCommon, LabelSize, ListItem, ListItemSpacing, StyledExt, h_flex, rems,
    v_flex,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

const TWENTY_FOUR_HOURS_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone)]
struct ModalEntry {
    id: u64,
    op: String,
    branch: String,
    timestamp_unix: i64,
    before_sha: String,
    /// True when the recorded op didn't complete cleanly. Surfaced as a
    /// dimmer style so users see they have a half-done op to clean up.
    failed: bool,
}

pub(crate) struct UndoModal {
    repo: Entity<Repository>,
    entries: Vec<ModalEntry>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for UndoModal {}
impl ModalView for UndoModal {}
impl Focusable for UndoModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl UndoModal {
    fn new(repo: Entity<Repository>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let work_dir = repo.read(cx).work_directory_abs_path.clone();
        let cutoff = current_unix_seconds() - TWENTY_FOUR_HOURS_SECONDS;
        let entries: Vec<ModalEntry> = git::undo_registry::list(cutoff)
            .log_err()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| Self::path_matches(&work_dir, &e.repo_path))
            .map(|e| ModalEntry {
                id: e.id,
                op: e.op,
                branch: e.branch,
                timestamp_unix: e.timestamp_unix,
                before_sha: e.before_sha,
                failed: e.failed,
            })
            .collect();

        Self {
            repo,
            entries,
            focus_handle: cx.focus_handle(),
        }
    }

    fn path_matches(want: &Arc<std::path::Path>, candidate: &std::path::Path) -> bool {
        candidate == &**want
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn restore(&mut self, entry: ModalEntry, cx: &mut Context<Self>) {
        let work_dir = self.repo.read(cx).work_directory_abs_path.clone();
        match crate::backup_mcp::create_restore_ref(&work_dir, &entry.branch, &entry.before_sha) {
            Ok(ref_name) => {
                log::info!(
                    "git::undo_modal: created restore ref {ref_name} for entry {}",
                    entry.id
                );
            }
            Err(err) => {
                log::warn!(
                    "git::undo_modal: failed to restore entry {}: {err}",
                    entry.id
                );
            }
        }
        cx.emit(DismissEvent);
    }

    fn forget(&mut self, entry_id: u64, cx: &mut Context<Self>) {
        git::undo_registry::forget(entry_id).log_err();
        self.entries.retain(|e| e.id != entry_id);
        cx.notify();
    }
}

impl Render for UndoModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now_unix = current_unix_seconds();
        let entries = self.entries.clone();
        let header_row = h_flex()
            .gap_2()
            .child(
                Icon::new(IconName::HistoryRerun)
                    .size(IconSize::Small)
                    .color(Color::Accent),
            )
            .child(Headline::new("Undo Recent Git Op").size(HeadlineSize::Small));

        let body = if entries.is_empty() {
            div()
                .py_4()
                .child(
                    Label::new("No recent destructive operations recorded.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(entries.len());
            for (ix, entry) in entries.into_iter().enumerate() {
                rows.push(render_entry(ix, entry, now_unix, cx).into_any_element());
            }
            v_flex()
                .gap_1()
                .id("undo-modal-list")
                .children(rows)
                .into_any_element()
        };

        v_flex()
            .key_context("UndoModal")
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(rems(36.))
            .max_h(rems(28.))
            .p_3()
            .gap_2()
            .child(header_row)
            .child(body)
    }
}

fn render_entry(
    ix: usize,
    entry: ModalEntry,
    now_unix: i64,
    cx: &mut Context<UndoModal>,
) -> impl IntoElement {
    let label_color = if entry.failed {
        Color::Error
    } else {
        Color::Default
    };
    let header = format!(
        "{op} on {branch} · {ago}{failed}",
        op = entry.op,
        branch = entry.branch,
        ago = format_relative(now_unix - entry.timestamp_unix),
        failed = if entry.failed { " [failed]" } else { "" },
    );
    let entry_for_restore = entry.clone();
    let entry_for_forget = entry.id;
    ListItem::new(SharedString::from(format!("undo-entry-{ix}")))
        .spacing(ListItemSpacing::Sparse)
        .child(
            v_flex()
                .gap_1()
                .child(Label::new(header).size(LabelSize::Small).color(label_color))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(
                                SharedString::from(format!("restore-{ix}")),
                                "Restore Branch",
                            )
                            .on_click(cx.listener({
                                let entry = entry_for_restore;
                                move |this, _: &ClickEvent, _, cx| this.restore(entry.clone(), cx)
                            })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("forget-{ix}")), "Forget")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.forget(entry_for_forget, cx)
                                })),
                        ),
                ),
        )
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn format_relative(seconds_ago: i64) -> String {
    if seconds_ago < 60 {
        return "just now".to_string();
    }
    if seconds_ago < 3_600 {
        let m = seconds_ago / 60;
        return format!("{m}m ago");
    }
    if seconds_ago < 86_400 {
        let h = seconds_ago / 3_600;
        return format!("{h}h ago");
    }
    let d = seconds_ago / 86_400;
    format!("{d}d ago")
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &git::UndoLast, window, cx| {
        let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
            log::info!("git::UndoLast: no active repository");
            return;
        };
        workspace.toggle_modal(window, cx, |window, cx| UndoModal::new(repo, window, cx));
    });
    workspace.register_action(|workspace, action: &git::CleanupBackups, _window, cx| {
        let Some(repo) = workspace.project().read(cx).active_repository(cx) else {
            return;
        };
        let work_dir = repo.read(cx).work_directory_abs_path.clone();
        let days = action.older_than_days;
        cx.background_spawn(async move {
            match git::backup::cleanup(&work_dir, days) {
                Ok(n) => log::info!("git::CleanupBackups: removed {n} backup-refs"),
                Err(err) => log::warn!("git::CleanupBackups: {err}"),
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_under_minute() {
        assert_eq!(format_relative(30), "just now");
    }

    #[test]
    fn relative_minutes() {
        assert_eq!(format_relative(120), "2m ago");
    }

    #[test]
    fn relative_hours() {
        assert_eq!(format_relative(7_200), "2h ago");
    }

    #[test]
    fn relative_days() {
        assert_eq!(format_relative(172_800), "2d ago");
    }
}
