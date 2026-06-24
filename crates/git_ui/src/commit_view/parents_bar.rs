//! Parents bar component: clickable short-hash buttons that navigate the
//! Git Graph to the parent commit (S-DET).

use gpui::{AnyElement, ParentElement, Styled, Window, prelude::*};
use ui::{Tooltip, prelude::*};

use crate::git_graph::OpenAtCommit;

pub(crate) fn render_parents_bar(parents: &[SharedString]) -> Option<AnyElement> {
    if parents.is_empty() {
        return None;
    }

    let label = if parents.len() == 1 {
        "Parent"
    } else {
        "Parents"
    };

    let row = h_flex()
        .gap_1p5()
        .items_center()
        .child(Label::new(label).size(LabelSize::Small).color(Color::Muted));

    let row = parents.iter().enumerate().fold(row, |row, (ix, parent)| {
        let short = parent.get(0..7).unwrap_or(parent.as_ref()).to_string();
        let full = parent.to_string();
        let tooltip_full = full.clone();
        row.child(
            Button::new(SharedString::from(format!("parent-{ix}-{short}")), short)
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .color(Color::Accent)
                .tooltip(move |_, cx| Tooltip::simple(tooltip_full.clone(), cx))
                .on_click(move |_, window: &mut Window, cx| {
                    window.dispatch_action(Box::new(OpenAtCommit { sha: full.clone() }), cx);
                }),
        )
    });

    Some(row.into_any_element())
}
