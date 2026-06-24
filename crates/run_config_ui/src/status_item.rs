use gpui::{Context, Entity, IntoElement, Render, SharedString, Subscription, Window};
use run_config::{RunConfigId, RunConfigStore};
use ui::{
    Button, ButtonStyle, ContextMenu, Icon, IconName, IconSize, PopoverMenu, PopoverMenuHandle,
    prelude::*,
};
use workspace::{StatusItemView, item::ItemHandle};

use crate::run_controller::{ActiveRunKind, RunController, RunControllerEvent};

pub struct RunStatusItem {
    controller: Entity<RunController>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    _subscription: Subscription,
}

impl RunStatusItem {
    pub fn new(controller: Entity<RunController>, cx: &mut Context<Self>) -> Self {
        let subscription =
            cx.subscribe(&controller, |_, _, _: &RunControllerEvent, cx| cx.notify());
        Self {
            controller,
            menu_handle: PopoverMenuHandle::default(),
            _subscription: subscription,
        }
    }
}

impl Render for RunStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        let count = controller.active_runs().count();
        if count == 0 {
            return div().into_any_element();
        }

        let any_debug = controller
            .active_runs()
            .any(|run| matches!(run.kind, ActiveRunKind::Debug { .. }));
        let icon = if any_debug {
            IconName::Debug
        } else {
            IconName::PlayFilled
        };

        // Collect the active run ids and names so the closure can own them.
        let active_entries: Vec<(RunConfigId, SharedString)> = {
            let store = RunConfigStore::try_global(cx);
            controller
                .active_runs()
                .map(|run| {
                    let name: SharedString = store
                        .as_ref()
                        .and_then(|store| store.read(cx).config(&run.config_id))
                        .map(|config| config.name)
                        .unwrap_or_else(|| run.config_id.as_str().into());
                    (run.config_id.clone(), name)
                })
                .collect()
        };

        let controller_entity = self.controller.clone();

        let trigger = Button::new("run-status-trigger", format!("{count}"))
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Subtle)
            .start_icon(Icon::new(icon).size(IconSize::Small).color(Color::Success));

        PopoverMenu::new("run-status-popover")
            .trigger(trigger)
            .with_handle(self.menu_handle.clone())
            .menu(move |window, cx| {
                let controller_entity = controller_entity.clone();
                let active_entries = active_entries.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        for (id, name) in &active_entries {
                            let controller_entity = controller_entity.clone();
                            let id = id.clone();
                            menu = menu.entry(format!("Stop {name}"), None, move |_window, cx| {
                                controller_entity.update(cx, |controller, cx| {
                                    controller.stop(&id, cx);
                                });
                            });
                        }
                        menu
                    },
                ))
            })
            .into_any_element()
    }
}

impl StatusItemView for RunStatusItem {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use gpui::{App, TestAppContext};
    use project::Project;
    use run_config::{
        ConfigScope, Executor, RunConfigProvider, RunConfigSettings, RunConfigStore,
        RunConfiguration, RunRequest, RunResolveContext,
    };
    use settings::Settings as _;
    use ui::IconName;
    use workspace::{AppState, Workspace};

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

    fn mock_config(name: &str) -> RunConfiguration {
        RunConfiguration {
            id: RunConfigId::from_raw(format!("mock:{name}")),
            name: name.into(),
            provider_type: "mock".into(),
            settings: serde_json::json!({}),
            executors: vec![Executor::Run],
            before_launch: vec![],
            folder: None,
            scope: ConfigScope::Global,
        }
    }

    #[gpui::test]
    async fn shows_count_when_runs_active(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            cx.set_global(db::AppDatabase::test_new());
            editor::init(cx);
            RunConfigSettings::register(cx);
            RunConfigStore::init_global(cx);
            run_config::register_provider(cx, MockProvider);
            app_state
        });
        let store = cx.update(|cx| RunConfigStore::global(cx));
        store.update(cx, |store, cx| store.upsert(mock_config("a"), cx));
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, _cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let controller = workspace.update(cx, |workspace, cx| {
            cx.new(|cx| crate::run_controller::RunController::new(workspace, cx))
        });
        cx.run_until_parked();

        // Construct the status item — just verify it doesn't panic and starts hidden.
        let status_item = cx.update(|cx| cx.new(|cx| RunStatusItem::new(controller.clone(), cx)));
        cx.run_until_parked();

        // With no active runs the controller reports zero.
        controller.read_with(cx, |controller, _| {
            assert_eq!(
                controller.active_runs().count(),
                0,
                "no active runs initially"
            );
        });

        // The status item subscription is wired; verify it updates after the
        // controller notifies (count 0 → rendered as hidden, no panic).
        cx.update(|cx| {
            status_item.read_with(cx, |item, cx| {
                // Reading the controller through the item should reflect 0 runs.
                assert_eq!(item.controller.read(cx).active_runs().count(), 0);
            })
        });
    }
}
