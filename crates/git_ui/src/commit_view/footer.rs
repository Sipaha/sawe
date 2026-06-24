//! Footer component: full hash, copy button, "Open in new tab" button.

use gpui::{AnyElement, ClipboardItem, ParentElement, Styled, Window, prelude::*};
use ui::{Tooltip, prelude::*};

use crate::commit_view::OpenCommitInNewTab;

/// Render the footer of the commit view: full SHA + copy + standalone-tab
/// button. `is_stash` hides the open-in-tab button (stashes are scoped to
/// a session and don't make sense as standalone tabs).
pub(crate) fn render_footer(
    sha: &SharedString,
    is_stash: bool,
    show_open_in_new_tab: bool,
    cx: &mut App,
) -> AnyElement {
    let clipboard_has_sha = cx
        .read_from_clipboard()
        .and_then(|entry| entry.text())
        .map_or(false, |clipboard_text| {
            clipboard_text.trim() == sha.as_ref()
        });

    let (copy_icon, copy_color) = if clipboard_has_sha {
        (IconName::Check, Color::Success)
    } else {
        (IconName::Copy, Color::Muted)
    };

    let sha_for_copy = sha.to_string();
    let sha_for_open = sha.to_string();

    h_flex()
        .py_1p5()
        .px_2()
        .w_full()
        .gap_2()
        .justify_between()
        .border_t_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    Label::new(sha.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    IconButton::new("footer-copy-sha", copy_icon)
                        .icon_size(IconSize::Small)
                        .icon_color(copy_color)
                        .tooltip(Tooltip::text("Copy Commit SHA"))
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(sha_for_copy.clone()));
                        }),
                ),
        )
        .when(!is_stash && show_open_in_new_tab, |this| {
            this.child(
                Button::new("open-in-new-tab", "Open in New Tab")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                    .tooltip(Tooltip::text("Open this commit in a standalone tab"))
                    .on_click(move |_, window: &mut Window, cx| {
                        window.dispatch_action(
                            Box::new(OpenCommitInNewTab {
                                sha: sha_for_open.clone(),
                            }),
                            cx,
                        );
                    }),
            )
        })
        .into_any_element()
}
