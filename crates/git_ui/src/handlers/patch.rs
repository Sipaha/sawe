//! S-PCH patch handlers — UI dispatch for `git format-patch` /
//! `git apply` / `git am`. Wraps [`git::operations::patch`] with file
//! pickers, a preview modal, and conflict-resolver fall-through on
//! `ApplyOutcome::Conflict`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use git::operations::patch::{
    ApplyOptions, ApplyOutcome, PatchFormat, apply_patch, create_patch, detect_patch_format,
    parse_patch_summary,
};
use git_conflict_ui::ConflictResolverView;
use gpui::{
    App, AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    PathPromptOptions, Render, SharedString, Task, WeakEntity, Window,
};
use menu::{Cancel, Confirm};
use notifications::status_toast::StatusToast;
use project::Project;
use ui::{Headline, HeadlineSize, Icon, IconName, IconSize, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

/// Spawn a save dialog and write a single-commit (or range) patch to the
/// chosen path / directory.
pub fn create_patch_action(
    workspace: WeakEntity<Workspace>,
    repo_path: PathBuf,
    sha: String,
    sha_to: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let (default_filename, is_range) = match &sha_to {
        Some(to) if !to.trim().is_empty() => {
            let from_short: String = sha.chars().take(7).collect();
            let to_short: String = to.chars().take(7).collect();
            (format!("{from_short}-{to_short}.patch"), true)
        }
        _ => {
            let short: String = sha.chars().take(7).collect();
            (format!("{short}.patch"), false)
        }
    };

    let initial_dir = repo_path.clone();
    if is_range {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Output Directory".into()),
        });
        window
            .spawn(cx, async move |cx| {
                let chosen = match receiver.await {
                    Ok(Ok(Some(paths))) => paths.into_iter().next(),
                    _ => None,
                };
                let Some(out_dir) = chosen else {
                    return anyhow::Ok(());
                };
                let task = cx.background_spawn({
                    let repo_path = repo_path.clone();
                    let sha = sha.clone();
                    let sha_to_clone = sha_to.clone();
                    async move {
                        create_patch(&repo_path, &sha, sha_to_clone.as_deref(), Some(&out_dir))
                    }
                });
                let result = task.await;
                cx.update(|_window, cx| {
                    let workspace = workspace.clone();
                    match result {
                        Ok(paths) => {
                            let count = paths.len();
                            notify_via_handle(
                                &workspace,
                                format!(
                                    "Wrote {count} patch file{} to {}",
                                    if count == 1 { "" } else { "s" },
                                    paths
                                        .first()
                                        .and_then(|p| p
                                            .parent()
                                            .map(|p| p.to_string_lossy().to_string()))
                                        .unwrap_or_default()
                                ),
                                IconName::Check,
                                ui::Color::Success,
                                cx,
                            );
                        }
                        Err(err) => {
                            notify_via_handle(
                                &workspace,
                                format!("Create patch failed: {err}"),
                                IconName::XCircle,
                                ui::Color::Error,
                                cx,
                            );
                        }
                    }
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        return;
    }

    let receiver = cx.prompt_for_new_path(&initial_dir, Some(&default_filename));
    window
        .spawn(cx, async move |cx| {
            let chosen = match receiver.await {
                Ok(Ok(Some(path))) => Some(path),
                _ => None,
            };
            let Some(target_path) = chosen else {
                return anyhow::Ok(());
            };
            let task = cx.background_spawn({
                let repo_path = repo_path.clone();
                let sha = sha.clone();
                async move {
                    let scratch = create_patch(&repo_path, &sha, None, None)?;
                    let scratch = scratch
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow!("create_patch returned no files"))?;
                    if let Some(parent) = target_path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    std::fs::copy(&scratch, &target_path)
                        .map_err(|err| anyhow!("write patch: {err}"))?;
                    let _ = std::fs::remove_file(&scratch);
                    Ok::<PathBuf, anyhow::Error>(target_path)
                }
            });
            let result = task.await;
            cx.update(|_window, cx| {
                let workspace = workspace.clone();
                match result {
                    Ok(path) => {
                        notify_via_handle(
                            &workspace,
                            format!("Wrote patch: {}", path.display()),
                            IconName::Check,
                            ui::Color::Success,
                            cx,
                        );
                    }
                    Err(err) => {
                        notify_via_handle(
                            &workspace,
                            format!("Create patch failed: {err}"),
                            IconName::XCircle,
                            ui::Color::Error,
                            cx,
                        );
                    }
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

/// Workspace action handler for `git: apply patch from file…`. Opens a
/// file picker, then routes through the [`PatchPreviewModal`].
pub fn apply_patch_from_file_action(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo_path) = active_repo_path(workspace, cx) else {
        notify(
            workspace,
            "No active repository — open a project first.",
            IconName::Warning,
            ui::Color::Warning,
            cx,
        );
        return;
    };
    let project = workspace.project().clone();
    let workspace_handle = workspace.weak_handle();
    let receiver = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Choose Patch File".into()),
    });
    window
        .spawn(cx, async move |cx| {
            let chosen = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(patch_path) = chosen else {
                return anyhow::Ok(());
            };
            let bytes_task = cx.background_spawn({
                let patch_path = patch_path.clone();
                async move {
                    let bytes = std::fs::read(&patch_path)
                        .map_err(|err| anyhow!("read {}: {err}", patch_path.display()))?;
                    let format = detect_patch_format(&bytes)?;
                    Ok::<(Vec<u8>, PatchFormat), anyhow::Error>((bytes, format))
                }
            });
            let result = bytes_task.await;
            cx.update(|window, cx| match result {
                Ok((bytes, format)) => {
                    open_preview_modal(
                        workspace_handle.clone(),
                        project.clone(),
                        repo_path.clone(),
                        patch_path.clone(),
                        bytes,
                        format,
                        false,
                        window,
                        cx,
                    );
                }
                Err(err) => {
                    notify_via_handle(
                        &workspace_handle,
                        format!("Patch detection failed: {err}"),
                        IconName::XCircle,
                        ui::Color::Error,
                        cx,
                    );
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

/// Workspace action handler for `git: apply patch from clipboard`.
pub fn apply_patch_from_clipboard_action(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(repo_path) = active_repo_path(workspace, cx) else {
        notify(
            workspace,
            "No active repository — open a project first.",
            IconName::Warning,
            ui::Color::Warning,
            cx,
        );
        return;
    };
    let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
        notify(
            workspace,
            "Clipboard does not contain text",
            IconName::XCircle,
            ui::Color::Error,
            cx,
        );
        return;
    };
    if text.trim().is_empty() {
        notify(
            workspace,
            "Clipboard is empty",
            IconName::XCircle,
            ui::Color::Error,
            cx,
        );
        return;
    }
    let bytes = text.into_bytes();
    let format = match detect_patch_format(&bytes) {
        Ok(format) => format,
        Err(err) => {
            notify(
                workspace,
                format!("Clipboard contents are not a recognized patch: {err}"),
                IconName::XCircle,
                ui::Color::Error,
                cx,
            );
            return;
        }
    };
    let temp_path = match write_clipboard_tempfile(&bytes) {
        Ok(p) => p,
        Err(err) => {
            notify(
                workspace,
                format!("Could not write clipboard to disk: {err}"),
                IconName::XCircle,
                ui::Color::Error,
                cx,
            );
            return;
        }
    };
    let project = workspace.project().clone();
    let workspace_handle = workspace.weak_handle();
    open_preview_modal(
        workspace_handle,
        project,
        repo_path,
        temp_path,
        bytes,
        format,
        true,
        window,
        cx,
    );
}

/// Path-extension predicate used by drag-drop and the file picker's
/// suggested filter list.
pub fn is_patchlike_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "patch" | "diff" | "mbox")
}

/// Open the apply-patch preview modal against the supplied patch. Used
/// from drag-drop and external callers that already know the path.
pub fn open_apply_modal_for_path(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repo_path: PathBuf,
    patch_path: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    let bytes_task = cx.background_spawn({
        let patch_path = patch_path.clone();
        async move {
            let bytes = std::fs::read(&patch_path)
                .map_err(|err| anyhow!("read {}: {err}", patch_path.display()))?;
            let format = detect_patch_format(&bytes)?;
            Ok::<(Vec<u8>, PatchFormat), anyhow::Error>((bytes, format))
        }
    });
    window
        .spawn(cx, async move |cx| {
            match bytes_task.await {
                Ok((bytes, format)) => {
                    cx.update(|window, cx| {
                        open_preview_modal(
                            workspace.clone(),
                            project.clone(),
                            repo_path.clone(),
                            patch_path.clone(),
                            bytes,
                            format,
                            false,
                            window,
                            cx,
                        );
                    })?;
                }
                Err(err) => {
                    cx.update(|_window, cx| {
                        notify_via_handle(
                            &workspace,
                            format!("Patch detection failed: {err}"),
                            IconName::XCircle,
                            ui::Color::Error,
                            cx,
                        );
                    })?;
                }
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

fn open_preview_modal(
    workspace_handle: WeakEntity<Workspace>,
    project: Entity<Project>,
    repo_path: PathBuf,
    patch_path: PathBuf,
    patch_bytes: Vec<u8>,
    format: PatchFormat,
    delete_on_apply: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace_handle.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        workspace.toggle_modal(window, cx, move |_, cx| {
            PatchPreviewModal::new(
                workspace_handle,
                project,
                repo_path,
                patch_path,
                patch_bytes,
                format,
                delete_on_apply,
                cx,
            )
        });
    });
}

fn write_clipboard_tempfile(bytes: &[u8]) -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("spke-clipboard-{nanos}.patch"));
    std::fs::write(&path, bytes).map_err(|err| anyhow!("write {}: {err}", path.display()))?;
    Ok(path)
}

fn active_repo_path(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let project = workspace.project().read(cx);
    let repo = project.active_repository(cx)?;
    Some(repo.read(cx).work_directory_abs_path.to_path_buf())
}

fn notify(
    workspace: &mut Workspace,
    message: impl Into<SharedString>,
    icon: IconName,
    color: ui::Color,
    cx: &mut Context<Workspace>,
) {
    let toast = StatusToast::new(message, cx, move |this, _cx| {
        this.icon(Icon::new(icon).size(IconSize::Small).color(color))
    });
    workspace.toggle_status_toast(toast, cx);
}

fn notify_via_handle(
    workspace: &WeakEntity<Workspace>,
    message: impl Into<SharedString>,
    icon: IconName,
    color: ui::Color,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    let message = message.into();
    workspace.update(cx, |workspace, cx| {
        let toast = StatusToast::new(message, cx, move |this, _cx| {
            this.icon(Icon::new(icon).size(IconSize::Small).color(color))
        });
        workspace.toggle_status_toast(toast, cx);
    });
}

/// Modal shown after the user picks a patch file: format label, list of
/// affected paths with `+`/`-` counts, plus toggles for `--3way` /
/// `--keep-cr`. Confirm spawns [`apply_patch`] in a background task.
pub struct PatchPreviewModal {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    repo_path: PathBuf,
    patch_path: PathBuf,
    format: PatchFormat,
    summary: Vec<(String, u32, u32)>,
    three_way: bool,
    keep_cr: bool,
    delete_on_apply: bool,
    focus_handle: FocusHandle,
}

impl PatchPreviewModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        repo_path: PathBuf,
        patch_path: PathBuf,
        patch_bytes: Vec<u8>,
        format: PatchFormat,
        delete_on_apply: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let summary = parse_patch_summary(&patch_bytes);
        let three_way_default = matches!(format, PatchFormat::UnifiedWithIndex | PatchFormat::Mbox);
        let keep_cr_default = matches!(format, PatchFormat::Mbox);
        Self {
            workspace,
            project,
            repo_path,
            patch_path,
            format,
            summary,
            three_way: three_way_default,
            keep_cr: keep_cr_default,
            delete_on_apply,
            focus_handle: cx.focus_handle(),
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let repo_path = self.repo_path.clone();
        let patch_path = self.patch_path.clone();
        let three_way = self.three_way;
        let keep_cr = self.keep_cr;
        let delete_on_apply = self.delete_on_apply;
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        let task: Task<Result<ApplyOutcome>> = cx.background_spawn(async move {
            apply_patch(
                &repo_path,
                &patch_path,
                ApplyOptions {
                    three_way,
                    keep_cr,
                    apply_with_reject: false,
                },
            )
        });

        cx.spawn_in(window, async move |this, cx| {
            let outcome = task.await;
            this.update_in(cx, |this, window, cx| {
                if delete_on_apply {
                    let _ = std::fs::remove_file(&this.patch_path);
                }
                let work_dir_path = this.repo_path.clone();
                cx.emit(DismissEvent);
                match outcome {
                    Ok(ApplyOutcome::Clean) => {
                        notify_via_handle(
                            &workspace,
                            "Patch applied cleanly",
                            IconName::Check,
                            ui::Color::Success,
                            cx,
                        );
                    }
                    Ok(ApplyOutcome::Conflict { conflicted_files }) => {
                        if let Some(ws) = workspace.upgrade() {
                            let work_dir: Arc<Path> = Arc::from(work_dir_path.as_path());
                            let weak = ws.downgrade();
                            ConflictResolverView::open(project.clone(), weak, work_dir, window, cx)
                                .detach_and_log_err(cx);
                        }
                        notify_via_handle(
                            &workspace,
                            format!(
                                "Patch conflict: {} file(s) need resolution",
                                conflicted_files.len()
                            ),
                            IconName::Warning,
                            ui::Color::Warning,
                            cx,
                        );
                    }
                    Ok(ApplyOutcome::RejectedHunks { reject_files }) => {
                        notify_via_handle(
                            &workspace,
                            format!(
                                "Some hunks rejected; {} .rej file(s) created",
                                reject_files.len()
                            ),
                            IconName::Warning,
                            ui::Color::Warning,
                            cx,
                        );
                    }
                    Err(err) => {
                        notify_via_handle(
                            &workspace,
                            format!("Apply patch failed: {err}"),
                            IconName::XCircle,
                            ui::Color::Error,
                            cx,
                        );
                    }
                }
            })
            .log_err();
        })
        .detach();
    }

    fn toggle_three_way(&mut self, cx: &mut Context<Self>) {
        self.three_way = !self.three_way;
        cx.notify();
    }

    fn toggle_keep_cr(&mut self, cx: &mut Context<Self>) {
        self.keep_cr = !self.keep_cr;
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for PatchPreviewModal {}
impl ModalView for PatchPreviewModal {}
impl Focusable for PatchPreviewModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PatchPreviewModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let format_label: SharedString = match self.format {
            PatchFormat::Mbox => "Mailbox (git format-patch)".into(),
            PatchFormat::UnifiedWithIndex => "Unified diff (with index)".into(),
            PatchFormat::UnifiedNoIndex => "Unified diff (no index)".into(),
        };
        let allow_three_way = matches!(
            self.format,
            PatchFormat::UnifiedWithIndex | PatchFormat::Mbox
        );
        let allow_keep_cr = matches!(self.format, PatchFormat::Mbox);
        let three_way = self.three_way;
        let keep_cr = self.keep_cr;
        let summary_count = self.summary.len();
        let title = format!(
            "Apply Patch ({} file{})",
            summary_count,
            if summary_count == 1 { "" } else { "s" }
        );
        let mut path_rows = v_flex().gap_0p5();
        for (path, add, del) in self.summary.iter().take(40) {
            path_rows = path_rows.child(
                h_flex()
                    .gap_2()
                    .child(Label::new(format!("+{add}")).color(ui::Color::Success))
                    .child(Label::new(format!("-{del}")).color(ui::Color::Error))
                    .child(Label::new(path.clone()).color(ui::Color::Default)),
            );
        }
        if summary_count > 40 {
            path_rows = path_rows.child(
                Label::new(format!("… and {} more", summary_count - 40)).color(ui::Color::Muted),
            );
        }

        v_flex()
            .key_context("PatchPreviewModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(48.))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .child(Icon::new(IconName::FileDiff).size(IconSize::XSmall))
                    .child(Headline::new(title).size(HeadlineSize::XSmall)),
            )
            .child(
                v_flex()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .child(Label::new(format!("Format: {format_label}")).color(ui::Color::Muted))
                    .child(
                        Label::new(format!("Patch: {}", self.patch_path.display()))
                            .color(ui::Color::Muted),
                    ),
            )
            .child(div().px_3().pb_2().child(path_rows))
            .child(
                h_flex()
                    .px_3()
                    .pb_2()
                    .gap_3()
                    .when(allow_three_way, |this| {
                        this.child(
                            Button::new("toggle-three-way", "--3way")
                                .toggle_state(three_way)
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_three_way(cx))),
                        )
                    })
                    .when(allow_keep_cr, |this| {
                        this.child(
                            Button::new("toggle-keep-cr", "--keep-cr")
                                .toggle_state(keep_cr)
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_keep_cr(cx))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .px_3()
                    .pb_3()
                    .gap_2()
                    .child(
                        Button::new("cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("apply", "Apply")
                            .style(ui::ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&Confirm, window, cx);
                            })),
                    ),
            )
    }
}
