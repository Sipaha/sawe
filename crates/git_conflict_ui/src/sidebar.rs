//! Sidebar listing the conflicted files in a resolver session, with
//! progress count and click-to-activate behaviour.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, div,
};
use theme::ActiveTheme as _;
use ui::{Color, Icon, IconName, Label, LabelCommon as _, LabelSize, h_flex, v_flex};

use crate::conflict_parser::ConflictedFile;

pub struct ConflictSidebar {
    files: Vec<ConflictedFile>,
    focus_handle: FocusHandle,
}

#[derive(Clone, Debug)]
pub struct FileSelected {
    pub index: usize,
}

impl EventEmitter<FileSelected> for ConflictSidebar {}

impl ConflictSidebar {
    pub fn new(files: Vec<ConflictedFile>, cx: &mut Context<Self>) -> Self {
        Self {
            files,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn select(&self, index: usize, cx: &mut Context<Self>) {
        if index < self.files.len() {
            cx.emit(FileSelected { index });
        }
    }

    pub fn set_files(&mut self, files: Vec<ConflictedFile>) {
        self.files = files;
    }

    pub fn files(&self) -> &[ConflictedFile] {
        &self.files
    }
}

impl Focusable for ConflictSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConflictSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let count = self.files.len();
        let summary = format!("{count} unresolved");

        let mut list = v_flex()
            .id("conflict-sidebar")
            .h_full()
            .w(gpui::px(220.0))
            .border_r_1()
            .border_color(colors.border)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Label::new("Conflicted Files").size(LabelSize::Small))
                    .child(
                        Label::new(summary)
                            .color(Color::Muted)
                            .size(LabelSize::XSmall),
                    ),
            );

        for (idx, file) in self.files.iter().enumerate() {
            let path_string = file.path.as_std_path().to_string_lossy().into_owned();
            let icon = IconName::FileGeneric;
            let _ = file.is_binary;
            let entity = cx.entity();
            let mut row = h_flex()
                .id(("cfl-file", idx))
                .px_3()
                .py_1()
                .gap_2()
                .cursor_pointer();
            if idx % 2 == 1 {
                row = row.bg(colors.elevated_surface_background);
            }
            let row = row
                .hover(|row| row.bg(colors.element_hover))
                .child(Icon::new(icon).color(Color::Warning))
                .child(Label::new(path_string).size(LabelSize::Small))
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx: &mut gpui::App| {
                        entity.update(cx, |_, cx: &mut Context<Self>| {
                            cx.emit(FileSelected { index: idx });
                        });
                    }
                });
            list = list.child(row);
        }
        list
    }
}
