//! Generic destructive-action confirmation modal.
//!
//! Used by the solution-tab context menu and the `+` dropdown trash icon
//! to confirm deletion of a solution (registry + folder on disk). Phase 3
//! will reuse it for member removal from a solution.
//!
//! UX rules:
//!   * Default focus is `Cancel`. Esc cancels.
//!   * `Delete` is destructive-styled (red tint) and is NOT bound to the
//!     `menu::Confirm` action — pressing Enter does not trigger Delete.
//!     Only a deliberate click on the Delete button confirms.
//!   * Folder size is shown next to each item; computed asynchronously
//!     after the modal opens (placeholder "…" while pending, "—" if the
//!     folder is empty / fails to stat / is missing).

use gpui::{
    Context, DismissEvent, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Task, Window,
};
use std::path::{Path, PathBuf};
use ui::{TintColor, prelude::*};
use workspace::{ModalView, Workspace};

/// Body item shown in the modal — one bullet describing what will be
/// removed, with an optional folder path whose size is computed
/// asynchronously.
pub struct DeleteConfirmItem {
    pub label: SharedString,
    pub path: Option<PathBuf>,
}

pub struct DeleteConfirmModal {
    title: SharedString,
    intro: SharedString,
    items: Vec<DeleteConfirmItem>,
    /// `None` while the size is still being computed; `Some(bytes)` when
    /// finished. `Some(0)` is rendered as "—" so we can also use it as
    /// the "stat failed / nothing on disk" sentinel without a separate
    /// state. Index-aligned with `items`.
    sizes: Vec<Option<u64>>,
    focus_handle: FocusHandle,
    on_confirm: Option<Box<dyn FnOnce(&mut Window, &mut gpui::App) + 'static>>,
    _size_tasks: Vec<Task<()>>,
}

impl DeleteConfirmModal {
    pub fn new<F>(
        title: impl Into<SharedString>,
        intro: impl Into<SharedString>,
        items: Vec<DeleteConfirmItem>,
        on_confirm: F,
        cx: &mut Context<Self>,
    ) -> Self
    where
        F: FnOnce(&mut Window, &mut gpui::App) + 'static,
    {
        let focus_handle = cx.focus_handle();
        let sizes = vec![None; items.len()];

        let mut tasks = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let Some(path) = item.path.clone() else {
                continue;
            };
            tasks.push(cx.spawn(async move |this, cx| {
                let bytes = compute_folder_size(&path).await.unwrap_or(0);
                this.update(cx, |this, cx| {
                    if let Some(slot) = this.sizes.get_mut(index) {
                        *slot = Some(bytes);
                    }
                    cx.notify();
                })
                .ok();
            }));
        }

        Self {
            title: title.into(),
            intro: intro.into(),
            items,
            sizes,
            focus_handle,
            on_confirm: Some(Box::new(on_confirm)),
            _size_tasks: tasks,
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(callback) = self.on_confirm.take() {
            callback(window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

/// Walk `path` recursively and sum file sizes. Errors at any level are
/// swallowed (we just skip the offending entry) — this is a UI hint, not
/// a budgeting decision.
async fn compute_folder_size(path: &Path) -> std::io::Result<u64> {
    use smol::stream::StreamExt as _;

    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = match smol::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        while let Some(entry) = entries.next().await {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                if let Ok(metadata) = entry.metadata().await {
                    total = total.saturating_add(metadata.len());
                }
            }
        }
    }
    Ok(total)
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

impl EventEmitter<DismissEvent> for DeleteConfirmModal {}

impl Focusable for DeleteConfirmModal {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for DeleteConfirmModal {
    fn debug_kind(&self) -> &'static str {
        "DeleteConfirm"
    }
}

impl Render for DeleteConfirmModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Snapshot per-item rows up front — `cx.listener` borrows `self`
        // again below, so `self.items` cannot be borrowed inside the
        // child builders.
        let item_rows: Vec<gpui::AnyElement> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let size_label = match self.sizes.get(index).copied().flatten() {
                    None if item.path.is_some() => "…".to_string(),
                    None => String::new(),
                    Some(0) => "—".to_string(),
                    Some(bytes) => format_size(bytes),
                };
                h_flex()
                    .gap_2()
                    .child(Label::new(format!("• {}", item.label.clone())).color(Color::Default))
                    .when(!size_label.is_empty(), |this| {
                        this.child(Label::new(size_label).color(Color::Muted))
                    })
                    .into_any_element()
            })
            .collect();

        v_flex()
            .key_context("DeleteConfirmModal")
            // Only Cancel is bound to a key action. We deliberately do NOT
            // register `menu::Confirm` — Enter would otherwise default-fire
            // a destructive action. The user has to click Delete on purpose.
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .w(rems(34.))
            .p_4()
            .gap_3()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::Warning)
                            .color(Color::Warning)
                            .size(IconSize::Medium),
                    )
                    .child(Label::new(self.title.clone()).size(LabelSize::Large)),
            )
            .child(Label::new(self.intro.clone()).color(Color::Default))
            .child(v_flex().gap_0p5().pl_3().children(item_rows))
            .child(Label::new("This action cannot be undone.").color(Color::Warning))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("delete-confirm-cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("delete-confirm-delete", "Delete")
                            .style(ButtonStyle::Tinted(TintColor::Error))
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

/// Convenience helper that mirrors the `register` pattern in `modals.rs`.
/// Action handlers can call this directly.
pub fn open_delete_confirm<F>(
    workspace: &mut Workspace,
    title: impl Into<SharedString>,
    intro: impl Into<SharedString>,
    items: Vec<DeleteConfirmItem>,
    on_confirm: F,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) where
    F: FnOnce(&mut Window, &mut gpui::App) + 'static,
{
    let title = title.into();
    let intro = intro.into();
    workspace.toggle_modal(window, cx, move |_window, cx| {
        DeleteConfirmModal::new(title, intro, items, on_confirm, cx)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_renders_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2 * 1024), "2 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024 + 500_000_000), "2.5 GB");
    }
}
