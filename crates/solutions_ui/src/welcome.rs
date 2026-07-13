//! Welcome page integration: renders a Solutions section (full list +
//! Create button) by plugging into `workspace::register_welcome_section`.
//!
//! Lives here (not in `workspace`) so the dependency graph stays one-way:
//! `solutions_ui → workspace`, never the reverse. Earlier we tried to read
//! `SolutionStore` directly from `workspace::welcome` and that introduced
//! the `workspace ↔ solutions` cycle that had to be reverted.
//!
//! Combined with `restore_on_startup: "none"` in `assets/settings/default.json`,
//! this section is what the user sees on every fresh launch — it's the
//! Solutions launcher for the whole editor. See FORK.md §11.

use chrono::{DateTime, Utc};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, ClickEvent, Entity, Focusable, Global, IntoElement, MouseButton, Window,
};
use settings::Settings as _;
use solutions::{Solution, SolutionId, SolutionStore, SolutionStoreEvent, SolutionsSettings};
use std::path::PathBuf;
use ui::{ButtonLike, Divider, DividerColor, IconButtonShape, prelude::*};
use util::ResultExt as _;
use workspace::welcome::{WelcomeWindow, register_welcome_section};

use crate::open::{OpenIntent, open_solution};

/// Welcome-window-scoped state for the inline name prompt used by both
/// "Create new solution" and per-row "Rename". Lives as a `Global`
/// because the section renderer is a stateless `Fn(&mut App)` closure
/// (see `register_welcome_section`) and can't carry per-window fields
/// of its own. Reset on every new launcher window.
struct WelcomeEditState {
    editor: Entity<Editor>,
    mode: WelcomeEditMode,
}

#[derive(Clone)]
enum WelcomeEditMode {
    Idle,
    Creating,
    Renaming(SolutionId),
    /// Trash was clicked on a row; the row's pencil + trash icons are
    /// swapped for "Delete?" + [Yes][Cancel]. WelcomeWindow isn't a
    /// `Workspace`, so the regular `DeleteSolution` action never
    /// reaches `workspace.register_action` (handler is in `modals.rs`),
    /// and the click-to-trash silently no-op'd. This in-row confirm
    /// is the launcher-local replacement for `DeleteSolutionModal`.
    ConfirmingDelete(SolutionId),
}

impl Global for WelcomeEditState {}

/// Wires the Recent Solutions section into the launcher window. Called
/// once from `solutions_ui::init`.
pub fn init(cx: &mut App) {
    register_welcome_section(cx, render_section);

    // The launcher window doesn't know about SolutionStore on its own,
    // so without this hook the Recent Solutions section would render
    // once at window construction and then stay frozen. We subscribe
    // each new launcher window to SolutionStoreEvent::Changed and call
    // `cx.notify` to re-run the section renderer after solution
    // create/delete/touch. Also (re)create the inline-edit Editor used
    // by the Create / Rename prompts so it lives in this window's
    // entity tree.
    cx.observe_new::<WelcomeWindow>(|_window_view, window, cx| {
        let Some(store) = SolutionStore::try_global(cx) else {
            return;
        };
        cx.subscribe(
            &store,
            |_window_view, _store, _event: &SolutionStoreEvent, cx| {
                cx.notify();
            },
        )
        .detach();

        let Some(window) = window else {
            return;
        };
        install_edit_state(window, cx);
    })
    .detach();
}

fn install_edit_state(window: &mut Window, cx: &mut gpui::Context<WelcomeWindow>) {
    let editor = cx.new(|cx| Editor::single_line(window, cx));
    // Submit on Enter is wired through the EditorEvent stream — a
    // dedicated Edited handler would also work, but `BufferEdited` is
    // the one event that fires both for typing and for `set_text`
    // (which we use when entering rename mode), so subscribing here
    // avoids duplicating the bookkeeping.
    cx.subscribe(&editor, on_editor_event).detach();
    cx.set_global(WelcomeEditState {
        editor,
        mode: WelcomeEditMode::Idle,
    });
}

fn on_editor_event(
    _: &mut WelcomeWindow,
    _editor: Entity<Editor>,
    _event: &EditorEvent,
    cx: &mut gpui::Context<WelcomeWindow>,
) {
    // The editor itself notifies on its own; we rerender the launcher
    // when the global state changes (via cx.notify from finish/cancel).
    cx.notify();
}

fn finish_edit(commit: bool, window: &mut Window, cx: &mut App) {
    let Some(state) = cx.try_global::<WelcomeEditState>() else {
        return;
    };
    let mode = state.mode.clone();
    let editor = state.editor.clone();
    let raw = editor.read(cx).text(cx);
    let name = raw.trim().to_string();

    if commit && !name.is_empty() {
        if let Some(store) = SolutionStore::try_global(cx) {
            match mode {
                WelcomeEditMode::Creating => {
                    let root = SolutionsSettings::get_global(cx).root.clone();
                    let sol_id = store.update(cx, |s, cx| s.create_solution(&name, root, cx));
                    if let Some(sol_id) = sol_id.log_err() {
                        reset_edit_state(&editor, window, cx);
                        open_solution(sol_id, None, OpenIntent::SameWindow, cx);
                        return;
                    }
                }
                WelcomeEditMode::Renaming(id) => {
                    store
                        .update(cx, |s, cx| s.rename_solution(id, &name, cx))
                        .log_err();
                }
                WelcomeEditMode::Idle | WelcomeEditMode::ConfirmingDelete(_) => {}
            }
        }
    }
    reset_edit_state(&editor, window, cx);
}

/// Switch the launcher into "are you sure you want to delete?" mode for
/// a specific row. Doesn't touch the inline editor — the prompt is just
/// a label + two buttons rendered in place of the row's pencil + trash
/// icons.
fn enter_confirm_delete(id: SolutionId, cx: &mut App) {
    if cx.try_global::<WelcomeEditState>().is_none() {
        return;
    }
    cx.update_global::<WelcomeEditState, _>(|state, _| {
        state.mode = WelcomeEditMode::ConfirmingDelete(id);
    });
    refresh_welcome(cx);
}

/// Resolve the in-row delete confirmation. `commit == true` runs the
/// actual delete via `delete_solution_with_cleanup`; `commit == false`
/// just exits confirm mode.
fn finish_confirm_delete(commit: bool, cx: &mut App) {
    let mode = cx
        .try_global::<WelcomeEditState>()
        .map(|s| s.mode.clone())
        .unwrap_or(WelcomeEditMode::Idle);
    if let WelcomeEditMode::ConfirmingDelete(id) = mode {
        if commit {
            let root = SolutionStore::try_global(cx).and_then(|store| {
                store.read_with(cx, |s, _| {
                    s.solutions()
                        .iter()
                        .find(|sol| sol.id == id)
                        .map(|sol| sol.root.clone())
                })
            });
            if let Some(root) = root {
                crate::delete_solution_with_cleanup(id, root, cx);
            }
        }
    }
    cx.update_global::<WelcomeEditState, _>(|state, _| {
        state.mode = WelcomeEditMode::Idle;
    });
    refresh_welcome(cx);
}

fn reset_edit_state(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    editor.update(cx, |editor, cx| editor.set_text("", window, cx));
    cx.update_global::<WelcomeEditState, _>(|state, _| {
        state.mode = WelcomeEditMode::Idle;
    });
    refresh_welcome(cx);
}

fn refresh_welcome(cx: &mut App) {
    if let Some(handle) = workspace::welcome::find_existing(cx) {
        handle.update(cx, |_, _, cx| cx.notify()).ok();
    }
}

fn enter_mode(mode: WelcomeEditMode, prefill: &str, window: &mut Window, cx: &mut App) {
    let Some(editor) = cx
        .try_global::<WelcomeEditState>()
        .map(|s| s.editor.clone())
    else {
        return;
    };
    editor.update(cx, |editor, cx| {
        editor.set_text(prefill, window, cx);
        editor.select_all(&editor::actions::SelectAll, window, cx);
    });
    let focus = editor.focus_handle(cx);
    cx.update_global::<WelcomeEditState, _>(|state, _| {
        state.mode = mode;
    });
    refresh_welcome(cx);
    window.focus(&focus, cx);
}

fn render_section(cx: &mut App) -> Option<AnyElement> {
    let entries = all_solutions(cx);
    let mode = cx
        .try_global::<WelcomeEditState>()
        .map(|s| s.mode.clone())
        .unwrap_or(WelcomeEditMode::Idle);

    let mut list = ui::v_flex().w_full().gap_2();
    list = list.child(
        ui::h_flex()
            .px_1()
            .mb_1()
            .gap_2()
            .child(
                Label::new("SOLUTIONS")
                    .buffer_font(cx)
                    .color(Color::Muted)
                    .size(LabelSize::XSmall),
            )
            .child(Divider::horizontal().color(DividerColor::BorderVariant)),
    );
    if entries.is_empty() {
        list = list.child(
            ui::v_flex().px_1().py_2().child(
                Label::new("No solutions yet — create one to get started.")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            ),
        );
    } else {
        for (index, entry) in entries.into_iter().enumerate() {
            let renaming = matches!(&mode, WelcomeEditMode::Renaming(id) if id == &entry.id);
            let confirming_delete =
                matches!(&mode, WelcomeEditMode::ConfirmingDelete(id) if id == &entry.id);
            list = list.child(render_card(index, entry, renaming, confirming_delete, cx));
        }
    }
    if matches!(mode, WelcomeEditMode::Creating) {
        list = list.child(render_inline_editor(cx, "Solution name"));
    } else {
        list = list.child(render_create_button());
    }
    Some(list.into_any_element())
}

fn render_inline_editor(cx: &mut App, _placeholder: &str) -> impl IntoElement {
    let editor = cx
        .try_global::<WelcomeEditState>()
        .map(|s| s.editor.clone());
    let Some(editor) = editor else {
        return ui::div().into_any_element();
    };
    ui::h_flex()
        .key_context("WelcomeNamePrompt")
        .on_action(|_: &menu::Confirm, window, cx| finish_edit(true, window, cx))
        .on_action(|_: &menu::Cancel, window, cx| finish_edit(false, window, cx))
        .w_full()
        .gap_2()
        .px_1()
        .child(div().flex_1().child(editor))
        .child(
            ui::Button::new("welcome-prompt-confirm", "OK")
                .style(ui::ButtonStyle::Filled)
                .on_click(|_, window, cx| finish_edit(true, window, cx)),
        )
        .child(
            ui::Button::new("welcome-prompt-cancel", "Cancel")
                .on_click(|_, window, cx| finish_edit(false, window, cx)),
        )
        .into_any_element()
}

fn render_create_button() -> impl IntoElement {
    ButtonLike::new("create-solution-from-welcome")
        .full_width()
        .size(ui::ButtonSize::Medium)
        .child(
            ui::h_flex()
                .gap_2()
                .px_1()
                .child(
                    Icon::new(IconName::Plus)
                        .color(Color::Muted)
                        .size(IconSize::Small),
                )
                .child(Label::new("Create new solution")),
        )
        .on_click(|_, window, cx| {
            // Welcome is its own top-level window with no `Workspace`
            // context, so the workspace-scoped `NewSolution` action
            // wouldn't reach a handler here. Switch the section into
            // an inline name-prompt; commit creates the solution via
            // SolutionStore directly and routes through the shared
            // open flow — opens it in a fresh workspace window and
            // retires the launcher.
            enter_mode(WelcomeEditMode::Creating, "", window, cx);
        })
}

/// IDEA-style card: colored avatar, name, path, last-opened ago.
/// Trash + rename buttons on hover. When `confirming_delete` is true,
/// pencil + trash are replaced by a "Delete?" label and Yes/Cancel
/// buttons — see `WelcomeEditMode::ConfirmingDelete`.
fn render_card(
    index: usize,
    entry: RecentSolution,
    renaming: bool,
    confirming_delete: bool,
    cx: &App,
) -> impl IntoElement {
    let entry_id = entry.id;
    let entry_id_for_delete = entry.id;
    let entry_id_for_rename = entry.id;
    let original_name = entry.label.clone();

    let avatar_color = avatar_color_for(&entry.label, cx);
    let initials = initials_of(&entry.label);

    let path_display = display_path(&entry.root);
    let meta = entry
        .last_opened_at
        .map(|ts| relative_time_label(ts, Utc::now()))
        .unwrap_or_else(|| "never opened".to_string());

    // We can't put `on_mouse_down` on the parent row because then a click
    // anywhere on the row fires "open" — including on the trash button,
    // since GPUI's parent listeners run before child `stop_propagation`
    // takes effect. So the row is a non-clickable layout container; the
    // "open" handler lives on an inner `clickable_body` div that sits
    // alongside the trash button as siblings.
    ui::h_flex()
        .id(("solution-card-row", index))
        .w_full()
        .gap_2()
        .px_2()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().elevated_surface_background)
        .hover(|s| s.bg(cx.theme().colors().element_hover))
        .child(
            ui::h_flex()
                .id(("solution-card-body", index))
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .on_click(move |event: &ClickEvent, window, cx| {
                    let source = window.window_handle().downcast();
                    let intent = if click_button(event) == Some(MouseButton::Middle) {
                        OpenIntent::NewWindow
                    } else {
                        OpenIntent::SameWindow
                    };
                    open_solution(entry_id, source, intent, cx);
                })
                .child(
                    ui::h_flex()
                        .flex_none()
                        .size_8()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .bg(avatar_color)
                        .child(
                            Label::new(initials)
                                .size(LabelSize::Default)
                                .color(Color::Custom(gpui::white())),
                        ),
                )
                .child(
                    ui::v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            ui::h_flex()
                                .gap_2()
                                .when(renaming, |this| {
                                    this.key_context("WelcomeNamePrompt")
                                        .on_action(|_: &menu::Confirm, window, cx| {
                                            finish_edit(true, window, cx)
                                        })
                                        .on_action(|_: &menu::Cancel, window, cx| {
                                            finish_edit(false, window, cx)
                                        })
                                })
                                .map(|row| {
                                    if renaming {
                                        if let Some(state) = cx.try_global::<WelcomeEditState>() {
                                            row.child(div().flex_1().child(state.editor.clone()))
                                        } else {
                                            row.child(
                                                Label::new(entry.label.clone())
                                                    .size(LabelSize::Default),
                                            )
                                        }
                                    } else {
                                        row.child(
                                            Label::new(entry.label.clone())
                                                .size(LabelSize::Default),
                                        )
                                        .when(
                                            entry.is_empty,
                                            |this| {
                                                this.child(
                                                    Label::new("(empty)")
                                                        .color(Color::Muted)
                                                        .size(LabelSize::XSmall),
                                                )
                                            },
                                        )
                                    }
                                }),
                        )
                        .child(
                            Label::new(path_display)
                                .color(Color::Muted)
                                .size(LabelSize::XSmall)
                                .truncate(),
                        ),
                )
                .child(Label::new(meta).color(Color::Muted).size(LabelSize::XSmall)),
        )
        .map(|row| {
            if renaming {
                row.child(
                    ui::Button::new(("rename-confirm", index), "OK")
                        .style(ui::ButtonStyle::Filled)
                        .on_click(|_, window, cx| finish_edit(true, window, cx)),
                )
                .child(
                    ui::Button::new(("rename-cancel", index), "Cancel")
                        .on_click(|_, window, cx| finish_edit(false, window, cx)),
                )
            } else if confirming_delete {
                row.child(
                    Label::new("Delete?")
                        .size(LabelSize::Small)
                        .color(Color::Error),
                )
                .child(
                    ui::Button::new(("delete-confirm", index), "Yes")
                        .style(ui::ButtonStyle::Filled)
                        .on_click(|_, _window, cx| finish_confirm_delete(true, cx)),
                )
                .child(
                    ui::Button::new(("delete-cancel", index), "Cancel")
                        .on_click(|_, _window, cx| finish_confirm_delete(false, cx)),
                )
            } else {
                row.child(
                    IconButton::new(("rename-solution", index), IconName::Pencil)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(ui::Tooltip::text("Rename solution"))
                        .on_click(move |_, window, cx| {
                            enter_mode(
                                WelcomeEditMode::Renaming(entry_id_for_rename),
                                &original_name,
                                window,
                                cx,
                            );
                        }),
                )
                .child(
                    IconButton::new(("delete-solution", index), IconName::Trash)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(ui::Tooltip::text("Delete solution"))
                        .on_click(move |_, _window, cx| {
                            enter_confirm_delete(entry_id_for_delete, cx);
                        }),
                )
            }
        })
}

fn click_button(event: &ClickEvent) -> Option<MouseButton> {
    match event {
        ClickEvent::Mouse(mouse) => Some(mouse.down.button),
        _ => None,
    }
}

#[cfg_attr(test, derive(Debug))]
struct RecentSolution {
    id: SolutionId,
    label: String,
    root: PathBuf,
    is_empty: bool,
    last_opened_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
fn all_solutions_for_test(cx: &App) -> Vec<RecentSolution> {
    all_solutions(cx)
}

/// Returns every solution in the store, sorted by `last_opened_at` desc with
/// never-opened solutions placed last (kept in their natural store order).
/// No truncation — the Welcome page is the launcher for the whole editor and
/// the user expects to see all of their solutions, not just five.
fn all_solutions(cx: &App) -> Vec<RecentSolution> {
    let Some(store) = SolutionStore::try_global(cx) else {
        return Vec::new();
    };
    let mut sols: Vec<RecentSolution> = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .map(|sol: &Solution| RecentSolution {
                id: sol.id,
                label: sol.name.clone(),
                root: sol.root.clone(),
                is_empty: sol.members.is_empty(),
                // The store keeps epoch millis; the launcher renders a
                // relative-time label, so convert at this edge.
                last_opened_at: sol
                    .last_opened_at
                    .and_then(DateTime::<Utc>::from_timestamp_millis),
            })
            .collect()
    });
    // Opened solutions first, ordered by last_opened_at desc (newest first).
    // Never-opened solutions follow, kept in their store insertion order.
    sols.sort_by(|a, b| match (a.last_opened_at, b.last_opened_at) {
        (Some(ts_a), Some(ts_b)) => ts_b.cmp(&ts_a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    sols
}

fn initials_of(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    if parts.is_empty() {
        return "?".into();
    }
    if parts.len() == 1 {
        let s = parts[0];
        return s.chars().take(2).collect::<String>().to_uppercase();
    }
    let mut s = String::new();
    for p in parts.iter().take(2) {
        if let Some(c) = p.chars().next() {
            s.push(c.to_ascii_uppercase());
        }
    }
    if s.is_empty() { "?".into() } else { s }
}

/// Pick a stable accent color from the theme palette by hashing the name.
/// Same name → same color across launches.
fn avatar_color_for(name: &str, cx: &App) -> gpui::Hsla {
    let palette = &cx.theme().colors();
    let candidates = [
        palette.terminal_ansi_red,
        palette.terminal_ansi_green,
        palette.terminal_ansi_yellow,
        palette.terminal_ansi_blue,
        palette.terminal_ansi_magenta,
        palette.terminal_ansi_cyan,
    ];
    let h = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    candidates[(h as usize) % candidates.len()]
}

fn display_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
}

fn relative_time_label(ts: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(ts);
    let secs = delta.num_seconds();
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d ago", secs / 86_400)
    } else if secs < 30 * 86_400 {
        format!("{}w ago", secs / (7 * 86_400))
    } else if secs < 365 * 86_400 {
        format!("{}mo ago", secs / (30 * 86_400))
    } else {
        format!("{}y ago", secs / (365 * 86_400))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use solutions::SolutionStore;
    use tempfile::tempdir;

    #[gpui::test]
    async fn empty_store_yields_empty_list(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let dir = tempdir().expect("tempdir");
            let store = SolutionStore::for_test(dir.path().join("c.json"), cx);
            solutions::install_global_for_test(store, cx);
            assert!(all_solutions_for_test(cx).is_empty());
        });
    }

    #[gpui::test]
    async fn unopened_solutions_are_included(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let dir = tempdir().expect("tempdir");
            let store = SolutionStore::for_test(dir.path().join("c.json"), cx);
            store
                .update(cx, |s, cx| {
                    s.create_solution("Alpha", dir.path().to_path_buf(), cx)
                })
                .expect("create");
            solutions::install_global_for_test(store, cx);
            // Welcome shows ALL solutions — never-opened ones included.
            let entries = all_solutions_for_test(cx);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].label, "Alpha");
        });
    }

    #[gpui::test]
    async fn opened_solutions_first_then_unopened_in_store_order(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let dir = tempdir().expect("tempdir");
            let store = SolutionStore::for_test(dir.path().join("c.json"), cx);
            // Three never-opened, then two opened (so the latter sort first).
            for i in 0..3 {
                store
                    .update(cx, |s, cx| {
                        s.create_solution(
                            &format!("Unopen{i}"),
                            dir.path().join(format!("u{i}")),
                            cx,
                        )
                    })
                    .expect("create");
            }
            for i in 0..2 {
                let sol_id = store
                    .update(cx, |s, cx| {
                        s.create_solution(&format!("Open{i}"), dir.path().join(format!("o{i}")), cx)
                    })
                    .expect("create");
                // `touch_last_opened` stamps `chrono::Utc::now().timestamp_millis()`,
                // and the sort is stable — two touches inside the same millisecond
                // tie and fall back to store order, which is the opposite of what
                // this test asserts. Sleep past the clock's resolution so the two
                // stamps genuinely differ. Must be a real (blocking) sleep: the GPUI
                // test executor's virtual timers don't move the wall clock.
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                store
                    .update(cx, |s, cx| s.touch_last_opened(sol_id, cx))
                    .expect("touch");
            }
            solutions::install_global_for_test(store, cx);

            let entries = all_solutions_for_test(cx);
            assert_eq!(entries.len(), 5);
            // Most recently opened first.
            assert_eq!(entries[0].label, "Open1");
            assert_eq!(entries[1].label, "Open0");
            // Then never-opened in store insertion order.
            assert_eq!(entries[2].label, "Unopen0");
            assert_eq!(entries[3].label, "Unopen1");
            assert_eq!(entries[4].label, "Unopen2");
        });
    }

    #[test]
    fn initials_basic() {
        assert_eq!(initials_of("Alpha"), "AL");
        assert_eq!(initials_of("Alpha Bravo"), "AB");
        assert_eq!(initials_of("alpha bravo charlie"), "AB");
        assert_eq!(initials_of(""), "?");
        assert_eq!(initials_of("   "), "?");
    }

    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        let m5 = now - chrono::Duration::minutes(5);
        let h2 = now - chrono::Duration::hours(2);
        let d3 = now - chrono::Duration::days(3);
        assert_eq!(relative_time_label(m5, now), "5m ago");
        assert_eq!(relative_time_label(h2, now), "2h ago");
        assert_eq!(relative_time_label(d3, now), "3d ago");
    }
}
