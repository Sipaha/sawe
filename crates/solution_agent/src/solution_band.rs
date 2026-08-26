//! `SolutionBand`: the full-width dialog region rendered between the
//! project zone and the status bar (`Workspace::solution_band_item`, phase
//! 2a task 1). Shows the `SolutionSessionView` for
//! `SolutionAgentStore::active_dialog_session` — `None` collapses the band
//! to a zero-height `div` so the project zone reclaims the space. The
//! utility section (terminal beside the dialog) lands in task 6, the
//! draggable divider + persisted height in task 7.
//!
//! Installed from `crates/zed/src/zed.rs` (NOT `title_bar`, which cannot
//! depend on `solution_agent` — see the `SessionTabStrip` install a few
//! lines above it for the same reasoning) inside the
//! `cx.observe_new(|workspace: &mut Workspace, window, cx| ...)` closure
//! that also installs `SessionTabStrip`. That closure already holds
//! `&mut Workspace`, so `SolutionBand::new` takes only a `WeakEntity<Workspace>`
//! and never reads the entity during construction — reading it there would
//! double-lease-panic (the same trap `ProjectToolbar::new` sidesteps by
//! taking `&Workspace` as a plain parameter instead of upgrading a weak
//! handle).

use std::collections::HashMap;

use gpui::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, WeakEntity, Window, div,
};
use solutions::{SolutionId, SolutionStore};
use workspace::Workspace;

use crate::model::SolutionSessionId;
use crate::session_view::SolutionSessionView;
use crate::store::{SolutionAgentStore, SolutionAgentStoreEvent};

pub struct SolutionBand {
    workspace: WeakEntity<Workspace>,
    /// `SolutionSessionView::new` constructs a real `editor::Editor` for the
    /// compose box, so rebuilding one every paint is out of the question —
    /// cache by session id and reuse across renders. Evicted on
    /// `SessionClosed` so a closed session's view (and the subscriptions/
    /// editor entity it holds) doesn't linger past the session's lifetime.
    views: HashMap<SolutionSessionId, Entity<SolutionSessionView>>,
    _subscriptions: Vec<Subscription>,
}

impl SolutionBand {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let store = SolutionAgentStore::global(cx);
        let subscription = cx.subscribe(&store, |this, _store, event, cx| match event {
            SolutionAgentStoreEvent::ActiveDialogSessionChanged { .. } => cx.notify(),
            SolutionAgentStoreEvent::SessionClosed(id) => {
                if this.views.remove(id).is_some() {
                    cx.notify();
                }
            }
            _ => {}
        });

        Self {
            workspace,
            views: HashMap::new(),
            _subscriptions: vec![subscription],
        }
    }

    /// Walk the bound workspace's worktrees for the Solution that owns
    /// them. Mirrors `solutions_ui::project_tab_strip::solution_id_for_workspace`
    /// and `session_tab_strip::SessionTabStrip::active_solution_id` —
    /// duplicated rather than shared for the same cross-crate-cycle reason
    /// documented on the latter (`solution_agent` can't depend on
    /// `solutions_ui`). Safe to call from `render`/event callbacks (unlike
    /// from `new` — see the module doc comment): both run outside the
    /// `&mut Workspace` borrow that installs this band.
    fn solution_id(&self, cx: &App) -> Option<SolutionId> {
        let workspace = self.workspace.upgrade()?;
        let store = SolutionStore::global(cx);
        let store = store.read(cx);
        let project = workspace.read(cx).project().clone();
        project.read(cx).worktrees(cx).find_map(|tree| {
            store
                .solution_for_path(&tree.read(cx).abs_path())
                .map(|sol| sol.id)
        })
    }

    /// The session id whose dialog the band would show on the next render,
    /// resolved fresh from the store rather than cached — cheap (a worktree
    /// walk + two hashmap lookups) and avoids a second place that can go
    /// stale relative to `SolutionAgentStore::active_dialog_session`.
    fn resolve_active_session(&self, cx: &App) -> Option<SolutionSessionId> {
        let solution_id = self.solution_id(cx)?;
        SolutionAgentStore::global(cx)
            .read(cx)
            .active_dialog_session(solution_id)
    }

    /// Test-only mirror of what `render` would show, without needing a live
    /// `Window` to drive a draw. Production code always goes through
    /// `render`.
    #[cfg(test)]
    fn active_view(&self, cx: &App) -> Option<SolutionSessionId> {
        self.resolve_active_session(cx)
    }
}

impl Render for SolutionBand {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session_id) = self.resolve_active_session(cx) else {
            return div().into_any_element();
        };

        let view = if let Some(view) = self.views.get(&session_id) {
            view.clone()
        } else {
            let Some(session) = SolutionAgentStore::global(cx).read(cx).session(session_id) else {
                // Stale selection racing a concurrent session removal —
                // `clear_active_dialog_for_session` will emit
                // `ActiveDialogSessionChanged` momentarily and this frame's
                // absence will self-correct. Render nothing rather than
                // reach into a missing entity.
                return div().into_any_element();
            };
            let view = cx.new(|cx| {
                SolutionSessionView::new(session_id, session, self.workspace.clone(), window, cx)
            });
            self.views.insert(session_id, view.clone());
            view
        };

        div().w_full().child(view).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Arc;

    #[gpui::test]
    async fn the_band_is_absent_when_no_dialog_is_active(cx: &mut TestAppContext) {
        let (_solution_id, _tmp, project) =
            crate::store::tests::setup_solution_and_project(cx).await;

        cx.update(|cx| {
            // `Workspace::new` calls `theme_settings::track_window_appearance`,
            // which requires `GlobalSystemAppearance` to be initialized —
            // mirrors `compact::tests::cold_compact_queues_prompt_and_kicks_resume`.
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            let registry = Arc::new(crate::adapter::AdapterRegistry::new());
            SolutionAgentStore::init_global(cx, registry);
        });

        let (workspace, cx) =
            cx.add_window_view(|window, cx| workspace::Workspace::test_new(project, window, cx));

        let band = workspace.update_in(cx, |workspace, _window, cx| {
            cx.new(|cx| SolutionBand::new(workspace.weak_handle(), cx))
        });

        band.read_with(cx, |band, cx| {
            assert!(
                band.active_view(cx).is_none(),
                "a fresh Solution has no active dialog session, so the band must render nothing"
            );
        });
    }
}
