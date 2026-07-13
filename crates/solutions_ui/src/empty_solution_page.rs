//! Splash page shown when an empty solution opens — guides the user to
//! add a project from the catalog instead of staring at a blank workspace.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString,
    Subscription, WeakEntity, Window, px,
};
use solutions::{SolutionId, SolutionStore, SolutionStoreEvent};
use ui::ButtonLike;
use ui::prelude::*;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

use crate::add_member_picker::AddMemberPicker;

pub struct EmptySolutionPage {
    solution_id: SolutionId,
    solution_name: SharedString,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    _store_subscription: Option<Subscription>,
}

impl EmptySolutionPage {
    pub fn new(
        solution_id: SolutionId,
        solution_name: impl Into<SharedString>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The page is the "Solution is empty" placeholder — once the
        // user lands a member (catalog clone OR `add_empty_member`), it
        // has nothing to say and should close itself instead of
        // lingering as a stale tab next to the freshly-mounted member.
        let store_subscription = SolutionStore::try_global(cx).map(|store| {
            cx.subscribe(&store, |this, store, event, cx| match event {
                SolutionStoreEvent::Changed
                | SolutionStoreEvent::MemberAddCompleted { error: None, .. } => {
                    let still_empty = store
                        .read(cx)
                        .solutions()
                        .iter()
                        .find(|s| s.id == this.solution_id)
                        .is_some_and(|s| s.members.is_empty());
                    if !still_empty {
                        cx.emit(ItemEvent::CloseItem);
                    }
                }
                _ => {}
            })
        });
        Self {
            solution_id,
            solution_name: solution_name.into(),
            workspace,
            focus_handle: cx.focus_handle(),
            _store_subscription: store_subscription,
        }
    }

    fn open_picker(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let id = self.solution_id;
        workspace.update(cx, |workspace, cx| {
            AddMemberPicker::open(workspace, id, window, cx);
        });
    }
}

impl Render for EmptySolutionPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.solution_name.clone();
        // The page mounts as a Pane Item between the side docks. With
        // both side docks open on a typical-width window the central
        // pane narrows to ~10–20px and any unbounded text in here would
        // wrap character-by-character. Cap each label to a single line
        // (`truncate()`) and clip the whole page with `overflow_hidden`
        // so a squeezed pane just shows truncated copy instead of a
        // vertical character salad. The project panel's empty-state
        // body and its `+ No project` selector remain the always-
        // clickable CTAs in the squeezed case.
        v_flex()
            .size_full()
            .min_w(px(360.))
            .overflow_hidden()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(cx.theme().colors().editor_background)
            .child(
                h_flex().max_w_full().px_4().child(
                    Label::new(format!("Solution \"{name}\" is empty"))
                        .size(LabelSize::Large)
                        .truncate(),
                ),
            )
            .child(
                h_flex().max_w_full().px_4().child(
                    Label::new(
                        "Add a project from your catalog to start working in this solution.",
                    )
                    .color(Color::Muted)
                    .size(LabelSize::Small)
                    .truncate(),
                ),
            )
            .child(
                ButtonLike::new("empty-solution-add-member")
                    .size(ui::ButtonSize::Medium)
                    .child(
                        h_flex()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .child(
                                Icon::new(IconName::Plus)
                                    .color(Color::Muted)
                                    .size(IconSize::Small),
                            )
                            .child(Label::new("Add Project from Catalog")),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_picker(window, cx);
                    })),
            )
    }
}

impl EventEmitter<ItemEvent> for EmptySolutionPage {}

impl Focusable for EmptySolutionPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for EmptySolutionPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!("{} (empty)", self.solution_name).into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
