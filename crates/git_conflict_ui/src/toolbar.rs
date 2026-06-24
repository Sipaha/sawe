//! Toolbar + bottom bar element renderers for `ConflictResolverView`.
//!
//! Every action is dispatched directly via `cx.listener(...)` callbacks
//! against the resolver entity — no separate Action types are introduced
//! at this stage; toolbar buttons drive resolver methods directly.

use gpui::{App, Context, InteractiveElement, IntoElement, ParentElement, Styled, Window, div};
use theme::ActiveTheme as _;
use ui::{
    Button, ButtonCommon, Clickable, Color, Disableable, Icon, IconButton, IconName, IconSize,
    Label, LabelCommon as _, LabelSize, Tooltip, h_flex,
};

use crate::resolver_view::ConflictResolverView;

pub(crate) fn render_toolbar(
    this: &ConflictResolverView,
    _window: &mut Window,
    cx: &mut Context<ConflictResolverView>,
) -> gpui::AnyElement {
    let chunks_count = this.chunks().len();
    let current = this.current_chunk().map(|i| i + 1).unwrap_or(0);
    let chunk_label = if chunks_count == 0 {
        "no conflicts".to_string()
    } else {
        format!("chunk {current}/{chunks_count}")
    };

    let mut row = h_flex()
        .id("conflict-resolver-toolbar")
        .gap_2()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(cx.theme().colors().border)
        .child(
            IconButton::new("cfl-prev", IconName::ArrowUp)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Previous chunk"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.prev_chunk(window, cx);
                })),
        )
        .child(
            IconButton::new("cfl-next", IconName::ArrowDown)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text("Next chunk"))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.next_chunk(window, cx);
                })),
        )
        .child(
            Label::new(chunk_label)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(div().flex_1())
        .child(
            Button::new("cfl-accept-yours", "Accept Yours").on_click(cx.listener(
                |this, _, window, cx| {
                    this.accept_yours(window, cx);
                },
            )),
        )
        .child(
            Button::new("cfl-accept-theirs", "Accept Theirs").on_click(cx.listener(
                |this, _, window, cx| {
                    this.accept_theirs(window, cx);
                },
            )),
        )
        .child(
            Button::new("cfl-accept-both", "Accept Both").on_click(cx.listener(
                |this, _, window, cx| {
                    this.accept_both(window, cx);
                },
            )),
        );

    if this.show_base() {
        row = row.child(
            Button::new("cfl-accept-base", "Accept Base").on_click(cx.listener(
                |this, _, window, cx| {
                    this.accept_base(window, cx);
                },
            )),
        );
    }

    let ai_pending = this.ai_suggest_pending();
    let ai_disabled_reason = this.ai_suggest_disabled_reason(cx);
    let ai_eligible = this.ai_suggest_eligible(cx);
    let ai_button = {
        let label = if ai_pending {
            "Suggesting…"
        } else {
            "Suggest AI Merge"
        };
        let mut button = Button::new("cfl-ai-suggest", label)
            .start_icon(Icon::new(IconName::ZedAssistant).size(IconSize::Small))
            .loading(ai_pending)
            .disabled(!ai_eligible || ai_pending);
        if let Some(reason) = ai_disabled_reason {
            button = button.tooltip(Tooltip::text(reason));
        } else if ai_pending {
            button = button.tooltip(Tooltip::text("AI merge in progress"));
        } else {
            button = button.tooltip(Tooltip::text(
                "Generate a 3-way merge suggestion via the active Solution's AI agent",
            ));
        }
        button.on_click(cx.listener(|this, _, window, cx| {
            this.request_ai_suggest(window, cx);
        }))
    };
    row = row.child(ai_button);

    row.child(
        Button::new("cfl-apply-non-conflicting", "Apply Non-Conflicting")
            .tooltip(Tooltip::text(
                "Strip auto-merged hunks; keep only manual conflicts",
            ))
            .on_click(cx.listener(|this, _, window, cx| {
                this.apply_non_conflicting_hunks(window, cx);
            })),
    )
    .child(
        Button::new("cfl-revert", "Revert")
            .tooltip(Tooltip::text("Reload working content from index"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.revert_to_original(window, cx);
            })),
    )
    .child(
        Button::new("cfl-mark-resolved", "Mark as Resolved").on_click(cx.listener(
            |this, _, window, cx| {
                this.mark_resolved(window, cx).detach_and_log_err(cx);
            },
        )),
    )
    .child(
        Button::new(
            "cfl-show-base",
            if this.show_base() {
                "Hide Base"
            } else {
                "Show Base"
            },
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.toggle_show_base(window, cx);
        })),
    )
    .child(
        Button::new(
            "cfl-lock-scroll",
            if this.lock_scroll() {
                "Unlock Scroll"
            } else {
                "Lock Scroll"
            },
        )
        .on_click(cx.listener(|this, _, _window, cx| {
            this.toggle_lock_scroll(cx);
        })),
    )
    .into_any_element()
}

pub(crate) fn render_bottom_bar(
    this: &ConflictResolverView,
    _window: &mut Window,
    cx: &mut Context<ConflictResolverView>,
) -> gpui::AnyElement {
    let op = this.op();

    let progress = format!(
        "{} resolved / {} total",
        this.resolved_count(),
        this.total_count()
    );

    let mut row = h_flex()
        .id("conflict-resolver-bottom")
        .gap_2()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .child(Icon::new(IconName::GitBranch).color(Color::Muted))
        .child(
            Label::new(progress)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(div().flex_1());

    let op_name = op.map(|o| o.cli_subcommand()).unwrap_or("merge");

    row = row.child(
        Button::new("cfl-continue", "Continue")
            .tooltip(Tooltip::text(format!("git {op_name} --continue")))
            .on_click(cx.listener(|this, _, _window, cx| {
                crate::operations::continue_op(this, cx);
            })),
    );
    if op.is_some_and(|o| o.supports_skip()) {
        row = row.child(
            Button::new("cfl-skip", "Skip")
                .tooltip(Tooltip::text(format!("git {op_name} --skip")))
                .on_click(cx.listener(|this, _, _window, cx| {
                    crate::operations::skip_op(this, cx);
                })),
        );
    }
    row = row.child(
        Button::new("cfl-abort", "Abort")
            .tooltip(Tooltip::text(format!("git {op_name} --abort")))
            .on_click(cx.listener(|this, _, _window, cx| {
                crate::operations::abort_op(this, cx);
            })),
    );

    row.into_any_element()
}

#[allow(dead_code)]
fn _unused(_: &App) {}
