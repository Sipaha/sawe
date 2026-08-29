//! The Solution band's utility button group, mounted in the status bar's
//! left group next to `session_tab_strip` (which selects the band's *other*
//! half). Three buttons — Terminal / Git Graph / Debug Panel — pick which
//! content `Workspace::solution_band_utility_item` slot the band's utility
//! section shows, and hide the section when the shown one is clicked again
//! (`solution_band::utility_button_click` is the rule; spec §3).
//!
//! **Why this lives in `solution_agent`.** The buttons only ever speak
//! `workspace::UtilityKind` and `SolutionBand`; they never need the concrete
//! occupant types. That matters because `workspace` must not depend on
//! `console_panel` / `git_graph` / `debugger_ui`, and each of those three
//! already depends on `solution_agent` (for `SolutionAgentStore`), so a
//! button group that imported them would cycle. `solution_agent` is the one
//! crate that owns the band and depends on none of its occupants —
//! the same reasoning that put `session_tab_strip` here (see its module doc)
//! rather than in `console_panel` or `title_bar`.
//!
//! **Why it drives `SolutionBand` and not `SolutionAgentStore` directly.**
//! The store only knows Solutions. A plain-folder window that resolves to no
//! Solution has no `BandState` row at all and falls back to
//! `SolutionBand::local_state`; going straight to the store would leave the
//! buttons inert in exactly that window, which is a case the band explicitly
//! supports (`ctrl-\`` works there today). `SolutionBand`'s
//! `set_utility_kind` / `set_utility_visible` pair is the layer that already
//! branches on it, so the buttons go through the band and inherit the
//! branch. As a bonus this keeps the click handler off the `Workspace`
//! entity entirely — `SolutionBand` resolves its Solution off `Entity<Project>`
//! precisely so its mutators are safe under a live `&mut Workspace` borrow,
//! and a status item's click handler must assume it is under one.
//!
//! **Focus is deliberately not touched.** `ctrl-\`` and `ctrl-shift-d` stay
//! the focus path (they are tri-state: show+focus / focus / hide). A button
//! is a two-state content switch. The two layers still agree on what *the
//! active content* is — `utility_kind` while `utility_visible` — so a button
//! can never contradict its hotkey about which content is current; they only
//! differ on what a click does when the content is already shown but
//! unfocused.

use gpui::{
    App, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window,
};
use ui::{IconButton, IconName, IconSize, Tooltip, prelude::*};
use workspace::item::ItemHandle;
use workspace::{HideStatusItem, StatusItemView, UtilityKind};

use crate::solution_band::{SolutionBand, utility_button_selected};

/// The action whose keybinding each button advertises in its tooltip, looked
/// up by name (`cx.build_action`) rather than imported — importing
/// `console_panel::ToggleFocus` / `debug_panel::ToggleFocus` would be the
/// crate cycle this module's doc explains. `None` for the git graph, whose
/// own toggle action was deleted with its dock registration in phase 2b
/// task 4 (nothing referenced it, and its handler could not compile without
/// the `Panel` impl).
pub fn toggle_action_name(kind: UtilityKind) -> Option<&'static str> {
    match kind {
        UtilityKind::Terminal => Some("console_panel::ToggleFocus"),
        UtilityKind::GitGraph => None,
        UtilityKind::Debug => Some("debug_panel::ToggleFocus"),
    }
}

fn icon(kind: UtilityKind) -> IconName {
    match kind {
        // Deliberately NOT `IconName::Console`, which is what the old
        // `ConsolePanel::icon()` returned — `console.svg` was the icon of the
        // merged Terminal + AI-chat panel, and the chat half moved out to the
        // band's dialog side in phase 2a. The occupant is terminal-only now
        // and the button is labelled "Terminal", so `terminal.svg` is the
        // honest glyph. `GitGraph` and `Debug` do reuse their old dock icons.
        UtilityKind::Terminal => IconName::Terminal,
        UtilityKind::GitGraph => IconName::GitGraph,
        UtilityKind::Debug => IconName::Debug,
    }
}

pub struct UtilityButtons {
    /// Handed in by the installer, which creates the band in the same
    /// `observe_new` closure — never resolved back out of `Workspace`, so
    /// this view touches the Workspace entity nowhere at all.
    band: Entity<SolutionBand>,
    _subscriptions: Vec<Subscription>,
}

impl UtilityButtons {
    pub fn new(band: Entity<SolutionBand>, cx: &mut Context<Self>) -> Self {
        // The band notifies on every path that can change its state: its own
        // setters (the `local_state` branch) and the store's
        // `BandStateChanged` (the persisted branch, including hydration from
        // the DB and writes made by the keybindings). Observing it is
        // therefore strictly enough, and strictly cheaper than re-deriving
        // the state during layout — which a `notify` raised mid-draw would
        // silently discard anyway.
        let subscription = cx.observe(&band, |_, _, cx| cx.notify());
        Self {
            band,
            _subscriptions: vec![subscription],
        }
    }

    fn render_button(&self, kind: UtilityKind, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.band.read(cx).band_state(cx);
        let selected = utility_button_selected(kind, &state);
        let label = kind.label();
        let action_name = toggle_action_name(kind);
        let band = self.band.clone();

        // The toggled state is part of the element id so the cached tooltip
        // is invalidated when the selection changes out from under the mouse
        // (e.g. via `ctrl-\``) — same trick as upstream's `PanelButtons`.
        IconButton::new((kind.as_str(), selected as u64), icon(kind))
            .icon_size(IconSize::Small)
            .toggle_state(selected)
            .tooltip(move |window, cx| {
                let title = if selected {
                    format!("Hide {label}")
                } else {
                    format!("Show {label}")
                };
                // A build failure here means the action's crate did not
                // register it (a rename, or an `init` that never ran). Log it
                // and fall back to a keybinding-less tooltip rather than
                // losing the whole tooltip: a button whose hotkey hint is
                // missing is still a working button. Renames are caught at
                // test time by the two `toggle_focus_action_matches_the_\
                // utility_button_tooltip_lookup` pins in `console_panel` and
                // `debugger_ui`, which is why this is a log and not an error
                // surfaced to the user.
                let action = action_name.and_then(|name| match cx.build_action(name, None) {
                    Ok(action) => Some(action),
                    Err(err) => {
                        log::error!("utility_buttons: {name} unavailable for tooltip: {err}");
                        None
                    }
                });
                match action {
                    Some(action) => Tooltip::for_action(title, action.as_ref(), cx),
                    None => Tooltip::text(title)(window, cx),
                }
            })
            .on_click(move |_, _window, cx| {
                band.update(cx, |band, cx| band.activate_utility_kind(kind, cx));
            })
    }
}

impl Render for UtilityButtons {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex().gap_1().children(
            UtilityKind::ALL
                .iter()
                .map(|kind| self.render_button(*kind, cx).into_any_element())
                .collect::<Vec<_>>(),
        )
    }
}

impl StatusItemView for UtilityButtons {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // No user-facing hide setting: these three buttons are the only way
        // to reach the git graph at all, so letting them be hidden would
        // strand a whole content behind no affordance.
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::model::BandState;
    use crate::solution_band::{utility_button_click, utility_button_selected};
    use workspace::UtilityKind;

    fn state(kind: UtilityKind, visible: bool) -> BandState {
        BandState {
            utility_kind: kind,
            utility_visible: visible,
            ..BandState::default()
        }
    }

    #[test]
    fn clicking_an_inactive_button_switches_and_shows() {
        let shown_terminal = state(UtilityKind::Terminal, true);
        assert_eq!(
            utility_button_click(UtilityKind::GitGraph, &shown_terminal),
            (UtilityKind::GitGraph, true)
        );
        assert_eq!(
            utility_button_click(UtilityKind::Debug, &shown_terminal),
            (UtilityKind::Debug, true)
        );
    }

    #[test]
    fn clicking_the_active_button_hides_and_leaves_the_kind_untouched() {
        let shown_debug = state(UtilityKind::Debug, true);
        let (kind, visible) = utility_button_click(UtilityKind::Debug, &shown_debug);
        assert!(!visible);
        assert_eq!(
            kind,
            UtilityKind::Debug,
            "hiding must not rewrite the remembered kind — re-showing has to \
             come back to the debugger, not to the default"
        );

        // …and re-showing does exactly that, from the state the hide left.
        let hidden_debug = state(kind, visible);
        assert_eq!(
            utility_button_click(UtilityKind::Debug, &hidden_debug),
            (UtilityKind::Debug, true)
        );
    }

    #[test]
    fn a_hidden_section_renders_every_button_unselected() {
        for remembered in UtilityKind::ALL {
            let hidden = state(remembered, false);
            for kind in UtilityKind::ALL {
                assert!(
                    !utility_button_selected(kind, &hidden),
                    "{kind:?} must render unselected while the section is hidden \
                     (remembered kind: {remembered:?})"
                );
                assert_eq!(
                    utility_button_click(kind, &hidden),
                    (kind, true),
                    "with nothing selected, every button is a one-click reveal"
                );
            }
        }
    }

    #[test]
    fn exactly_one_button_is_selected_while_the_section_is_shown() {
        for shown in UtilityKind::ALL {
            let state = state(shown, true);
            let selected: Vec<_> = UtilityKind::ALL
                .into_iter()
                .filter(|kind| utility_button_selected(*kind, &state))
                .collect();
            assert_eq!(selected, vec![shown]);
        }
    }
}
