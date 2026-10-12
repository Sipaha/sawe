use crate::{git_panel::GitPanel, picker_prompt::PickerPromptDelegate};
use futures::channel::oneshot;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Subscription, Task, WeakEntity, Window, rems,
};
use picker::Picker;
use project::git_store::Repository;
use ui::{Color, Label, LabelSize, prelude::*};
use util::ResultExt;

pub(crate) struct CommitMessageHistory {
    picker: Entity<Picker<PickerPromptDelegate>>,
    status: Option<String>,
    _subscription: Subscription,
    _load: Task<()>,
}

impl CommitMessageHistory {
    pub(crate) fn new(
        panel: WeakEntity<GitPanel>,
        repository: Entity<Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, _rx) = oneshot::channel();
        let repository_for_selection = repository.clone();
        let picker = cx.new(|cx| {
            let delegate =
                PickerPromptDelegate::new("Commit Message History".into(), Vec::new(), tx, 90)
                    .on_confirm(move |message, window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.use_commit_message_from_history(
                                    &repository_for_selection,
                                    message,
                                    window,
                                    cx,
                                );
                            })
                            .log_err();
                    });
            Picker::uniform_list(delegate, window, cx).initial_width(rems(34.))
        });
        let subscription = cx.subscribe(&picker, |_, _, _, cx| cx.emit(DismissEvent));
        let history = repository.update(cx, |repo, cx| repo.commit_message_history(cx));
        let picker_for_load = picker.clone();
        let load = cx.spawn_in(window, async move |this, cx| {
            let result = history.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(messages) => {
                        this.status = messages
                            .is_empty()
                            .then(|| "No previous commit messages".to_string());
                        picker_for_load.update(cx, |picker, cx| {
                            let query = picker.query(cx);
                            picker.delegate.replace_options(messages);
                            picker.update_matches(query, window, cx);
                        });
                    }
                    Err(error) => {
                        this.status = Some(format!("Unable to load commit messages: {error:#}"))
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        Self {
            picker,
            status: Some("Loading commit messages…".to_string()),
            _subscription: subscription,
            _load: load,
        }
    }
}

impl EventEmitter<DismissEvent> for CommitMessageHistory {}
impl Focusable for CommitMessageHistory {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}
impl Render for CommitMessageHistory {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(34.))
            .child(self.picker.clone())
            .children(self.status.as_ref().map(|status| {
                div().p_2().child(
                    Label::new(status.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            }))
    }
}
