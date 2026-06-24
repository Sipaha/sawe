use editor::Editor;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    KeyContext, ParentElement, Render, SharedString, Styled, WeakEntity, Window, div, px,
};
use run_config::{
    BeforeLaunchStep, ConfigScope, Executor, RunConfigId, RunConfigStore, RunConfiguration,
};
use ui::{
    Button, ButtonStyle, Checkbox, ContextMenu, Icon, IconButton, IconName, Label, LabelSize,
    PopoverMenu, PopoverMenuHandle, ToggleState, Tooltip, prelude::*,
};
use workspace::{ModalView, Workspace};

use crate::schema_form::SchemaForm;

/// A working copy of one configuration being edited in the modal.
struct DraftConfig {
    config: RunConfiguration,
    /// True for ephemeral (discovered) configs shown read-only with a "Save" affordance.
    is_ephemeral: bool,
}

/// The "Edit Configurations" modal: a two-pane editor for the set of run
/// configurations. The left pane lists the configs (with +/-/duplicate), the
/// right pane edits the selected one. `apply` updates the in-memory
/// `RunConfigStore` and writes the changes back to disk.
pub struct EditConfigurationsModal {
    workspace: WeakEntity<Workspace>,
    drafts: Vec<DraftConfig>,
    /// Index into `drafts`; meaningless when `drafts` is empty.
    selected: usize,
    name_editor: Entity<Editor>,
    /// Form for the selected draft's provider settings.
    form: Option<Entity<SchemaForm>>,
    /// "Store in: Project | Global" for the selected draft.
    store_in_global: bool,
    /// "Save all files" before-launch checkbox for the selected draft.
    before_save_all: bool,
    /// The selected draft's enabled executors (subset of the provider's supported set).
    executors: Vec<Executor>,
    add_menu_handle: PopoverMenuHandle<ContextMenu>,
    focus_handle: FocusHandle,
}

impl EditConfigurationsModal {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let weak = workspace.weak_handle();
        workspace.toggle_modal(window, cx, move |window, cx| {
            EditConfigurationsModal::new(weak, window, cx)
        });
    }

    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut drafts = Vec::new();
        if let Some(store) = RunConfigStore::try_global(cx) {
            for config in store.read(cx).configs() {
                let is_ephemeral = matches!(config.scope, ConfigScope::Ephemeral);
                drafts.push(DraftConfig {
                    config,
                    is_ephemeral,
                });
            }
        }

        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            workspace,
            drafts,
            selected: 0,
            name_editor,
            form: None,
            store_in_global: false,
            before_save_all: false,
            executors: Vec::new(),
            add_menu_handle: PopoverMenuHandle::default(),
            focus_handle,
        };
        this.rebuild_detail_pane(window, cx);
        this
    }

    /// The first worktree id of the project, if any.
    fn first_worktree_scope(&self, cx: &App) -> ConfigScope {
        let worktree_id = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
        });
        match worktree_id {
            Some(worktree) => ConfigScope::Project { worktree },
            None => ConfigScope::Global,
        }
    }

    fn rebuild_detail_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafts.is_empty() {
            self.form = None;
            self.store_in_global = false;
            self.before_save_all = false;
            self.executors = Vec::new();
            self.name_editor.update(cx, |editor, cx| {
                editor.set_text("", window, cx);
                editor.set_read_only(false);
            });
            cx.notify();
            return;
        }
        self.selected = self.selected.min(self.drafts.len() - 1);
        let selected_draft = &self.drafts[self.selected];
        let is_ephemeral = selected_draft.is_ephemeral;
        let draft = selected_draft.config.clone();
        self.name_editor.update(cx, |editor, cx| {
            editor.set_text(draft.name.to_string(), window, cx);
            editor.set_read_only(is_ephemeral);
        });
        self.store_in_global = matches!(draft.scope, ConfigScope::Global);
        self.before_save_all = draft
            .before_launch
            .iter()
            .any(|step| matches!(step, BeforeLaunchStep::SaveAllFiles));
        self.executors = draft.executors.clone();
        self.form = RunConfigStore::try_global(cx)
            .and_then(|store| store.read(cx).provider(&draft.provider_type))
            .map(|provider| {
                let schema = provider.settings_schema();
                cx.new(|cx| SchemaForm::new(&schema, &draft.settings, window, cx))
            });
        cx.notify();
    }

    fn select_draft(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.drafts.len() {
            self.flush_detail_into_draft(cx);
            self.selected = index;
            self.rebuild_detail_pane(window, cx);
        }
    }

    /// Pull the detail-pane widgets back into `drafts[selected]`. Called before
    /// switching selection or applying. The config id is *not* recomputed here —
    /// `apply` does that at apply time.
    fn flush_detail_into_draft(&mut self, cx: &mut App) {
        if self.drafts.is_empty() {
            return;
        }
        let index = self.selected.min(self.drafts.len() - 1);
        if self.drafts[index].is_ephemeral {
            return;
        }
        let name = self.name_editor.read(cx).text(cx);
        let project_scope = self.first_worktree_scope(cx);
        let settings = self.form.as_ref().map(|form| form.read(cx).value(cx));
        let provider_default_executor = RunConfigStore::try_global(cx)
            .and_then(|store| {
                store
                    .read(cx)
                    .provider(&self.drafts[index].config.provider_type)
            })
            .and_then(|provider| provider.supported_executors().first().copied());

        let draft = &mut self.drafts[index].config;
        if !name.trim().is_empty() {
            draft.name = name.into();
        }
        draft.scope = if self.store_in_global {
            ConfigScope::Global
        } else {
            project_scope
        };
        draft.before_launch = if self.before_save_all {
            vec![BeforeLaunchStep::SaveAllFiles]
        } else {
            vec![]
        };
        draft.executors = if self.executors.is_empty() {
            provider_default_executor
                .map(|executor| vec![executor])
                .unwrap_or_else(|| vec![Executor::Run])
        } else {
            self.executors.clone()
        };
        if let Some(settings) = settings {
            draft.settings = settings;
        }
    }

    fn unique_name(&self, base: &str) -> SharedString {
        let exists = |candidate: &str| {
            self.drafts
                .iter()
                .any(|draft| draft.config.name.as_ref() == candidate)
        };
        if !exists(base) {
            return base.to_string().into();
        }
        let mut counter = 2;
        loop {
            let candidate = format!("{base} {counter}");
            if !exists(&candidate) {
                return candidate.into();
            }
            counter += 1;
        }
    }

    fn add_config(&mut self, provider_type: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) =
            RunConfigStore::try_global(cx).and_then(|store| store.read(cx).provider(provider_type))
        else {
            return;
        };
        let template = provider.new_template(cx);
        let supported = provider.supported_executors().to_vec();
        let name = self.unique_name(&format!("New {}", provider.display_name()));
        self.flush_detail_into_draft(cx);
        let scope = self.first_worktree_scope(cx);
        let config = RunConfiguration {
            id: RunConfigId::new_random(),
            name,
            provider_type: provider_type.into(),
            settings: template,
            executors: if supported.is_empty() {
                vec![Executor::Run]
            } else {
                supported
            },
            before_launch: vec![],
            folder: None,
            scope,
        };
        self.drafts.push(DraftConfig {
            config,
            is_ephemeral: false,
        });
        self.selected = self.drafts.len() - 1;
        self.rebuild_detail_pane(window, cx);
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafts.is_empty() {
            return;
        }
        let index = self.selected.min(self.drafts.len() - 1);
        if self.drafts[index].is_ephemeral {
            return;
        }
        self.drafts.remove(index);
        self.selected = index.min(self.drafts.len().saturating_sub(1));
        self.rebuild_detail_pane(window, cx);
    }

    fn duplicate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafts.is_empty() {
            return;
        }
        self.flush_detail_into_draft(cx);
        let index = self.selected.min(self.drafts.len() - 1);
        let mut config = self.drafts[index].config.clone();
        let name = self.unique_name(&format!("{} copy", config.name));
        config.id = RunConfigId::new_random();
        config.name = name;
        if matches!(config.scope, ConfigScope::Ephemeral) {
            config.scope = self.first_worktree_scope(cx);
        }
        self.drafts.push(DraftConfig {
            config,
            is_ephemeral: false,
        });
        self.selected = self.drafts.len() - 1;
        self.rebuild_detail_pane(window, cx);
    }

    /// Promote the currently-selected ephemeral draft into a new persisted draft.
    /// The original ephemeral entry remains in the list (it represents the
    /// discovered source and will be re-populated by discovery on next open).
    fn promote_ephemeral(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drafts.is_empty() {
            return;
        }
        let index = self.selected.min(self.drafts.len() - 1);
        if !self.drafts[index].is_ephemeral {
            return;
        }
        // flush_detail_into_draft no-ops on ephemeral, so no stale edits to flush.
        let mut config = self.drafts[index].config.clone();
        let name = self.unique_name(&config.name);
        config.id = RunConfigId::new_random();
        config.name = name;
        config.scope = self.first_worktree_scope(cx);
        self.drafts.push(DraftConfig {
            config,
            is_ephemeral: false,
        });
        self.selected = self.drafts.len() - 1;
        self.rebuild_detail_pane(window, cx);
    }

    fn toggle_executor(&mut self, executor: Executor, cx: &mut Context<Self>) {
        if let Some(position) = self.executors.iter().position(|other| *other == executor) {
            if self.executors.len() > 1 {
                self.executors.remove(position);
            }
        } else {
            self.executors.push(executor);
        }
        cx.notify();
    }

    fn apply(&mut self, cx: &mut App) {
        self.flush_detail_into_draft(cx);
        // Each draft keeps the stable id it already carries — new drafts got a
        // fresh random id when created/duplicated/promoted, existing ones kept
        // the id loaded from disk. Renaming no longer changes the id, and ids
        // are inherently unique so no dedup is needed here (display-name dedup,
        // if any, happens via `unique_name`).
        if let Some(store) = RunConfigStore::try_global(cx) {
            let new_list: Vec<RunConfiguration> = self
                .drafts
                .iter()
                .filter(|draft| !draft.is_ephemeral)
                .map(|draft| draft.config.clone())
                .collect();
            store.update(cx, |store, cx| {
                store.set_persisted(new_list, cx);
                store.save_to_disk(cx).detach();
            });
        }
    }

    fn confirm(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.apply(cx);
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn provider_icon(&self, provider_type: &str, cx: &App) -> IconName {
        RunConfigStore::try_global(cx)
            .and_then(|store| store.read(cx).provider(provider_type))
            .map(|provider| provider.icon())
            .unwrap_or(IconName::PlayFilled)
    }

    fn render_list_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_is_ephemeral = self
            .drafts
            .get(self.selected)
            .map(|draft| draft.is_ephemeral)
            .unwrap_or(false);
        let drafts_empty = self.drafts.is_empty();

        let entries =
            self.drafts
                .iter()
                .enumerate()
                .map(|(index, draft)| {
                    let icon = self.provider_icon(&draft.config.provider_type, cx);
                    let name = draft.config.name.clone();
                    let is_ephemeral = draft.is_ephemeral;
                    ui::ListItem::new(("draft", index))
                        .toggle_state(index == self.selected)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_draft(index, window, cx);
                        }))
                        .start_slot(Icon::new(icon).size(IconSize::Small))
                        .child(h_flex().gap_1p5().child(Label::new(name)).when(
                            is_ephemeral,
                            |this| {
                                this.child(
                                    Label::new("detected")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            },
                        ))
                })
                .collect::<Vec<_>>();

        v_flex()
            .w(px(240.))
            .h_full()
            .border_r_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .p_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child({
                        let modal = cx.entity().downgrade();
                        PopoverMenu::new("edit-config-add")
                            .trigger(
                                IconButton::new("edit-config-add-trigger", IconName::Plus)
                                    .tooltip(Tooltip::text("Add Configuration")),
                            )
                            .with_handle(self.add_menu_handle.clone())
                            .menu(move |window, cx| {
                                let modal = modal.clone();
                                let providers: Vec<(SharedString, &'static str)> =
                                    RunConfigStore::try_global(cx)
                                        .map(|store| {
                                            store
                                                .read(cx)
                                                .providers()
                                                .map(|provider| {
                                                    (
                                                        SharedString::from(provider.display_name()),
                                                        provider.type_id(),
                                                    )
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                Some(ContextMenu::build(
                                    window,
                                    cx,
                                    move |mut menu, _window, _cx| {
                                        for (display_name, type_id) in &providers {
                                            let type_id = *type_id;
                                            let modal = modal.clone();
                                            menu = menu.entry(display_name.clone(), None, {
                                                move |window, cx| {
                                                    modal
                                                        .update(cx, |modal, cx| {
                                                            modal.add_config(type_id, window, cx);
                                                        })
                                                        .ok();
                                                }
                                            });
                                        }
                                        menu
                                    },
                                ))
                            })
                    })
                    .child(
                        IconButton::new("edit-config-delete", IconName::Dash)
                            .disabled(drafts_empty || selected_is_ephemeral)
                            .tooltip(Tooltip::text("Remove Configuration"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.delete_selected(window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("edit-config-duplicate", IconName::Copy)
                            .disabled(drafts_empty)
                            .tooltip(Tooltip::text("Duplicate Configuration"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.duplicate_selected(window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("edit-config-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_1()
                    .children(entries),
            )
    }

    fn render_detail_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let detail = v_flex().flex_1().p_3().gap_3();
        if self.drafts.is_empty() {
            return detail
                .child(Label::new("No configurations. Click + to add one.").color(Color::Muted));
        }
        let index = self.selected.min(self.drafts.len() - 1);
        let draft = &self.drafts[index];
        let is_ephemeral = draft.is_ephemeral;
        let provider_type = draft.config.provider_type.clone();
        let supported_executors: Vec<Executor> = RunConfigStore::try_global(cx)
            .and_then(|store| store.read(cx).provider(&provider_type))
            .map(|provider| provider.supported_executors().to_vec())
            .unwrap_or_default();

        if is_ephemeral {
            let provider_display_name: SharedString = RunConfigStore::try_global(cx)
                .and_then(|store| store.read(cx).provider(&provider_type))
                .map(|provider| SharedString::from(provider.display_name()))
                .unwrap_or_else(|| provider_type.clone());
            return detail
                .child(
                    v_flex().gap_1().child(Label::new("Name")).child(
                        div()
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant)
                            .bg(cx.theme().colors().editor_background)
                            .child(self.name_editor.clone()),
                    ),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(Label::new("Type").color(Color::Muted))
                        .child(Label::new(provider_display_name)),
                )
                .child(
                    Label::new("Detected configuration — read-only. Save it to edit.")
                        .color(Color::Muted),
                )
                .child(
                    Button::new(
                        "edit-config-save-ephemeral",
                        "Save as project configuration",
                    )
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.promote_ephemeral(window, cx);
                    })),
                );
        }

        let mut detail = detail
            .child(
                v_flex().gap_1().child(Label::new("Name")).child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .bg(cx.theme().colors().editor_background)
                        .child(self.name_editor.clone()),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Store in:"))
                    .child(
                        Button::new("edit-config-store-project", "Project")
                            .toggle_state(!self.store_in_global)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store_in_global = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("edit-config-store-global", "Global")
                            .toggle_state(self.store_in_global)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store_in_global = true;
                                cx.notify();
                            })),
                    ),
            );

        if let Some(form) = self.form.clone() {
            detail = detail.child(div().child(form));
        }

        detail = detail.child(
            v_flex().gap_1().child(Label::new("Before launch")).child(
                Checkbox::new(
                    "edit-config-before-save-all",
                    if self.before_save_all {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label("Save all files")
                .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                    this.before_save_all = *state == ToggleState::Selected;
                    cx.notify();
                })),
            ),
        );

        if supported_executors.len() > 1 {
            let mut executor_row = h_flex().gap_3().child(Label::new("Executors:"));
            for (executor_index, executor) in supported_executors.into_iter().enumerate() {
                let label = match executor {
                    Executor::Run => "Run",
                    Executor::Debug => "Debug",
                };
                let enabled = self.executors.contains(&executor);
                executor_row = executor_row.child(
                    Checkbox::new(
                        ("edit-config-executor", executor_index),
                        if enabled {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        },
                    )
                    .label(label)
                    .on_click(cx.listener(
                        move |this, _state: &ToggleState, _window, cx| {
                            this.toggle_executor(executor, cx);
                        },
                    )),
                );
            }
            detail = detail.child(v_flex().gap_1().child(executor_row));
        }

        detail
    }
}

impl EventEmitter<DismissEvent> for EditConfigurationsModal {}
impl ModalView for EditConfigurationsModal {}
impl Focusable for EditConfigurationsModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditConfigurationsModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context({
                let mut key_context = KeyContext::new_with_defaults();
                key_context.add("EditConfigurationsModal");
                key_context
            })
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(720.))
            .h(px(480.))
            .overflow_hidden()
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| this.cancel(window, cx)))
            .child(
                h_flex()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new("Run/Debug Configurations").size(LabelSize::Large)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_list_pane(cx))
                    .child(self.render_detail_pane(cx)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Button::new("edit-config-cancel", "Cancel")
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        Button::new("edit-config-apply", "Apply")
                            .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    )
                    .child(
                        Button::new("edit-config-ok", "OK")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use gpui::TestAppContext;
    use project::Project;
    use run_config::{RunConfigProvider, RunConfigSettings, RunRequest, RunResolveContext};
    use settings::Settings as _;
    use workspace::AppState;

    struct MockProvider;

    impl RunConfigProvider for MockProvider {
        fn type_id(&self) -> &'static str {
            "mock"
        }
        fn display_name(&self) -> &'static str {
            "Mock"
        }
        fn icon(&self) -> IconName {
            IconName::Terminal
        }
        fn supported_executors(&self) -> &'static [Executor] {
            &[Executor::Run]
        }
        fn settings_schema(&self) -> schemars::Schema {
            schemars::json_schema!({ "type": "object" })
        }
        fn new_template(&self, _cx: &App) -> serde_json::Value {
            serde_json::json!({})
        }
        fn resolve(
            &self,
            _config: &RunConfiguration,
            _executor: Executor,
            _cx: &mut RunResolveContext,
            _app: &App,
        ) -> Result<RunRequest> {
            Ok(RunRequest::Terminal(task::SpawnInTerminal {
                command: Some("true".into()),
                ..Default::default()
            }))
        }
    }

    #[gpui::test]
    async fn add_delete_duplicate(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            cx.set_global(db::AppDatabase::test_new());
            editor::init(cx);
            RunConfigSettings::register(cx);
            RunConfigStore::init_global(cx);
            run_config::register_provider(cx, MockProvider);
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let weak_workspace = workspace.downgrade();
        let modal = workspace.update_in(cx, |_, window, cx| {
            cx.new(|cx| EditConfigurationsModal::new(weak_workspace.clone(), window, cx))
        });
        cx.run_until_parked();

        modal.update(cx, |modal, _| {
            assert_eq!(modal.drafts.len(), 0, "no configs to start");
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.add_config("mock", window, cx);
        });
        let added_id = modal.update(cx, |modal, _| {
            assert_eq!(modal.drafts.len(), 1, "add_config should add one draft");
            assert_eq!(modal.selected, 0, "the new draft is selected");
            modal.drafts[0].config.id.clone()
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.duplicate_selected(window, cx);
        });
        modal.update(cx, |modal, _| {
            assert_eq!(modal.drafts.len(), 2, "duplicate adds a draft");
            assert_eq!(modal.selected, 1, "the duplicate is selected");
            let dup = &modal.drafts[1].config;
            assert!(
                dup.name.ends_with("copy"),
                "duplicate name should end with 'copy', got {:?}",
                dup.name
            );
            assert_ne!(dup.id, added_id, "the duplicate gets a fresh id");
        });

        modal.update_in(cx, |modal, window, cx| {
            modal.delete_selected(window, cx);
        });
        modal.update(cx, |modal, _| {
            assert_eq!(modal.drafts.len(), 1, "delete removes the selected draft");
        });

        // `apply` should push the in-memory drafts into the store.
        modal.update(cx, |modal, cx| modal.apply(cx));
        cx.run_until_parked();
        cx.update(|_window, cx| {
            let store = RunConfigStore::global(cx);
            assert_eq!(
                store.read(cx).configs().len(),
                1,
                "apply should update the in-memory store"
            );
        });
    }

    #[gpui::test]
    async fn promote_ephemeral(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            cx.set_global(db::AppDatabase::test_new());
            editor::init(cx);
            RunConfigSettings::register(cx);
            RunConfigStore::init_global(cx);
            run_config::register_provider(cx, MockProvider);
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let weak_workspace = workspace.downgrade();
        let modal = workspace.update_in(cx, |_, window, cx| {
            cx.new(|cx| EditConfigurationsModal::new(weak_workspace.clone(), window, cx))
        });
        cx.run_until_parked();

        // Inject an ephemeral draft directly to simulate a discovered config.
        modal.update_in(cx, |modal, window, cx| {
            modal.drafts.push(DraftConfig {
                config: RunConfiguration {
                    id: RunConfigId::discovered("mock", "detected-run"),
                    name: "Detected Run".into(),
                    provider_type: "mock".into(),
                    settings: serde_json::json!({}),
                    executors: vec![Executor::Run],
                    before_launch: vec![],
                    folder: None,
                    scope: ConfigScope::Ephemeral,
                },
                is_ephemeral: true,
            });
            modal.selected = 0;
            modal.rebuild_detail_pane(window, cx);
        });

        modal.update(cx, |modal, _| {
            assert_eq!(modal.drafts.len(), 1);
            assert!(modal.drafts[0].is_ephemeral, "injected draft is ephemeral");
        });

        // Promote the ephemeral draft.
        modal.update_in(cx, |modal, window, cx| {
            modal.promote_ephemeral(window, cx);
        });

        modal.update(cx, |modal, _| {
            assert_eq!(
                modal.drafts.len(),
                2,
                "original ephemeral draft is preserved"
            );
            assert!(modal.drafts[0].is_ephemeral, "original is still ephemeral");
            assert!(!modal.drafts[1].is_ephemeral, "promoted draft is persisted");
            assert_eq!(modal.selected, 1, "promoted draft is selected");
            let promoted = &modal.drafts[1].config;
            assert!(
                !matches!(promoted.scope, ConfigScope::Ephemeral),
                "promoted draft has a non-ephemeral scope"
            );
            assert_eq!(promoted.provider_type, "mock", "provider type is preserved");
        });

        // apply should write only the promoted (non-ephemeral) draft to the store.
        modal.update(cx, |modal, cx| modal.apply(cx));
        cx.run_until_parked();
        cx.update(|_window, cx| {
            let store = RunConfigStore::global(cx);
            assert_eq!(
                store.read(cx).configs().len(),
                1,
                "only the promoted config is persisted"
            );
        });
    }
}
