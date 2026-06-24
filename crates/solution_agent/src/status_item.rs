use gpui::{Context, IntoElement, ParentElement, Render, Styled, Subscription, Window, div};
use ui::Label;
use workspace::StatusItemView;
use workspace::item::ItemHandle;

use crate::model::SessionState;
use crate::store::SolutionAgentStore;

pub struct SolutionAgentStatusItem {
    _store_subscription: Subscription,
}

impl SolutionAgentStatusItem {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = SolutionAgentStore::global(cx);
        let subscription = cx.subscribe(&store, |_, _, _, cx| cx.notify());
        Self {
            _store_subscription: subscription,
        }
    }
}

impl StatusItemView for SolutionAgentStatusItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for SolutionAgentStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = SolutionAgentStore::global(cx);
        let count = store.read_with(cx, |store, cx| {
            store
                .all_sessions()
                .filter(|session| {
                    matches!(
                        session.read(cx).state,
                        SessionState::Running { .. } | SessionState::Stopping { .. }
                    )
                })
                .count()
        });
        if count == 0 {
            div().w_0()
        } else {
            div().px_2().child(Label::new(format!("AI: {count}")))
        }
    }
}
