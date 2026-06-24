//! Refs bar: large branch / tag chips for the commit. Reads ref-decoration
//! data already produced by `Repository::fetch_commit_data` callers.

use gpui::{AnyElement, ParentElement, Styled, prelude::*};
use ui::prelude::*;

/// Render the ref-decoration row above the editor, similar to the chip
/// cluster the `git_graph` detail panel produces. Returns `None` when
/// there are no decorations to avoid an empty band.
pub(crate) fn render_refs_bar(
    ref_names: &[SharedString],
    head_branch_name: Option<&SharedString>,
) -> Option<AnyElement> {
    if ref_names.is_empty() {
        return None;
    }

    let mut row = h_flex().gap_1().flex_wrap();
    for (ix, name) in ref_names.iter().enumerate() {
        let is_head = is_head_ref(name.as_ref(), head_branch_name);
        let is_tag = name.as_ref().starts_with("tag: ");
        let display = name
            .as_ref()
            .strip_prefix("HEAD -> ")
            .unwrap_or(name.as_ref())
            .strip_prefix("tag: ")
            .unwrap_or(
                name.as_ref()
                    .strip_prefix("HEAD -> ")
                    .unwrap_or(name.as_ref()),
            )
            .to_string();
        row = row.child(render_chip(ix, display, is_head, is_tag));
    }
    Some(row.into_any_element())
}

fn render_chip(ix: usize, name: String, is_head: bool, is_tag: bool) -> AnyElement {
    let color = if is_head {
        Color::Accent
    } else if is_tag {
        Color::Info
    } else {
        Color::Muted
    };
    let icon = if is_tag {
        IconName::Bookmark
    } else {
        IconName::GitBranch
    };
    h_flex()
        .id(SharedString::from(format!("ref-chip-{ix}")))
        .gap_1()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        .child(Label::new(name).size(LabelSize::Small).color(color))
        .into_any_element()
}

fn is_head_ref(name: &str, head_branch_name: Option<&SharedString>) -> bool {
    if name.starts_with("HEAD -> ") {
        return true;
    }
    head_branch_name
        .map(|head| head.as_ref() == name)
        .unwrap_or(false)
}
