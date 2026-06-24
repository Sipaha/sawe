//! Mini-view for binary file conflicts. The 3-way text layout is not
//! applicable, so we render a metadata block + three resolution buttons:
//! Accept Ours, Accept Theirs, Keep Both Renamed.

use anyhow::Result;
use git::repository::RepoPath;
use gpui::{
    AppContext as _, Context, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Window, div,
};
use std::path::Path;
use std::sync::Arc;
use theme::ActiveTheme as _;
use ui::{
    Button, Clickable as _, Color, Icon, IconName, Label, LabelCommon as _, LabelSize, h_flex,
    v_flex,
};

pub struct BinaryConflictView {
    path: RepoPath,
    work_dir: Arc<Path>,
    last_action: Option<String>,
    last_error: Option<String>,
}

impl BinaryConflictView {
    pub fn new(path: RepoPath, work_dir: Arc<Path>) -> Self {
        Self {
            path,
            work_dir,
            last_action: None,
            last_error: None,
        }
    }

    pub fn path(&self) -> &RepoPath {
        &self.path
    }

    fn run_with_feedback<F>(&mut self, label: &str, cx: &mut Context<Self>, op: F)
    where
        F: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let label = label.to_string();
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(op).await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(()) => {
                        this.last_action = Some(label.clone());
                        this.last_error = None;
                    }
                    Err(err) => {
                        this.last_error = Some(format!("{label}: {err}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn accept_ours(&mut self, cx: &mut Context<Self>) {
        let work = self.work_dir.to_path_buf();
        let path = self.path.clone();
        self.run_with_feedback("Accept Ours", cx, async move {
            let path_str = path.as_std_path().to_string_lossy().into_owned();
            crate::operations::run_git_void(&work, &["checkout", "--ours", "--", &path_str])
                .await?;
            crate::operations::run_git_void(&work, &["add", "--", &path_str]).await
        });
    }

    pub fn accept_theirs(&mut self, cx: &mut Context<Self>) {
        let work = self.work_dir.to_path_buf();
        let path = self.path.clone();
        self.run_with_feedback("Accept Theirs", cx, async move {
            let path_str = path.as_std_path().to_string_lossy().into_owned();
            crate::operations::run_git_void(&work, &["checkout", "--theirs", "--", &path_str])
                .await?;
            crate::operations::run_git_void(&work, &["add", "--", &path_str]).await
        });
    }

    pub fn keep_both_renamed(&mut self, cx: &mut Context<Self>) {
        let work = self.work_dir.to_path_buf();
        let path = self.path.clone();
        self.run_with_feedback("Keep Both Renamed", cx, async move {
            let path_str = path.as_std_path().to_string_lossy().into_owned();
            let abs = work.join(path.as_std_path());
            let ours_path = format!("{path_str}.ours");
            let theirs_path = format!("{path_str}.theirs");

            // checkout ours into <path>.ours
            crate::operations::run_git_void(&work, &["checkout", "--ours", "--", &path_str])
                .await?;
            std::fs::rename(&abs, work.join(&ours_path))?;
            crate::operations::run_git_void(&work, &["checkout", "--theirs", "--", &path_str])
                .await?;
            std::fs::rename(&abs, work.join(&theirs_path))?;
            // remove the original from index
            crate::operations::run_git_void(&work, &["rm", "--", &path_str]).await?;
            // stage both renamed files
            crate::operations::run_git_void(&work, &["add", "--", &ours_path, &theirs_path]).await
        });
    }
}

impl Render for BinaryConflictView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path_str = self.path.as_std_path().to_string_lossy().into_owned();

        let entity = cx.entity();

        let mut root = v_flex()
            .id("binary-conflict-view")
            .size_full()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::FileGeneric).color(Color::Warning))
                    .child(Label::new(path_str).size(LabelSize::Default))
                    .child(
                        Label::new("(binary)")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    ),
            )
            .child(
                Label::new(
                    "Three-way text merge isn't possible for binary files. \
                     Choose which side to keep, or stash both with new names.",
                )
                .color(Color::Muted)
                .size(LabelSize::Small),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("binary-accept-ours", "Accept Ours").on_click({
                        let entity = entity.clone();
                        move |_, _, cx: &mut gpui::App| {
                            entity.update(cx, |this, cx: &mut Context<Self>| this.accept_ours(cx));
                        }
                    }))
                    .child(
                        Button::new("binary-accept-theirs", "Accept Theirs").on_click({
                            let entity = entity.clone();
                            move |_, _, cx: &mut gpui::App| {
                                entity.update(cx, |this, cx: &mut Context<Self>| {
                                    this.accept_theirs(cx)
                                });
                            }
                        }),
                    )
                    .child(
                        Button::new("binary-keep-both", "Keep Both Renamed").on_click(
                            move |_, _, cx: &mut gpui::App| {
                                entity.update(cx, |this, cx: &mut Context<Self>| {
                                    this.keep_both_renamed(cx)
                                });
                            },
                        ),
                    ),
            );
        if let Some(label) = self.last_action.as_ref() {
            root = root.child(
                Label::new(format!("✓ {label}"))
                    .color(Color::Success)
                    .size(LabelSize::Small),
            );
        }
        if let Some(err) = self.last_error.as_ref() {
            root = root.child(
                div()
                    .border_1()
                    .border_color(cx.theme().status().error_background)
                    .p_2()
                    .child(
                        Label::new(err.clone())
                            .color(Color::Error)
                            .size(LabelSize::Small),
                    ),
            );
        }
        root
    }
}
