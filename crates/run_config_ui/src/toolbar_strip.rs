use gpui::{Action as _, App, Entity, IntoElement, ParentElement, Render, Styled, Window, div};
use project::WorktreeId;
use run_config::{
    ConfigScope, Executor, RunCommand, RunConfigId, RunConfigSettings, RunConfigStore,
    RunConfigStoreEvent, RunConfiguration,
};
use settings::Settings as _;
use ui::{
    Button, ButtonStyle, ContextMenu, Icon, IconButton, IconButtonShape, IconName, IconSize,
    PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*,
};
use workspace::Workspace;

use crate::actions;
use crate::run_controller::{RunController, RunControllerEvent};

/// Compact inline widget rendered right-aligned in the title bar (IDEA-style).
/// Shows the run-config picker dropdown plus Run / Debug / Stop buttons.
pub struct RunConfigStrip {
    controller: Entity<RunController>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    _subscriptions: Vec<gpui::Subscription>,
}

/// Keep only the configs visible for the solution-wide active worktree:
/// the active member's `Project`-scoped configs plus all `Global` configs.
///
/// `Ephemeral` configs are always kept: `RunConfiguration` carries no worktree
/// information separate from its `scope`, and `ConfigScope::Ephemeral` itself
/// holds no `WorktreeId`, so there is nothing to filter them by here.
///
/// When `active_worktree` is `None` (no resolvable active member), `Project`-scoped
/// configs are hidden and only `Global`/`Ephemeral` remain.
pub(crate) fn filter_configs_for_active_worktree(
    configs: &[RunConfiguration],
    active_worktree: Option<WorktreeId>,
) -> Vec<RunConfiguration> {
    configs
        .iter()
        .filter(|c| match c.scope {
            ConfigScope::Project { worktree } => Some(worktree) == active_worktree,
            ConfigScope::Global => true,
            ConfigScope::Ephemeral => true,
        })
        .cloned()
        .collect()
}

/// Create the `RunController`, register it on the workspace, build the strip
/// view and register it on the workspace. Called once per workspace from
/// `run_config_ui::init`. Honours the `run_config.toolbar` setting.
pub fn install(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !RunConfigSettings::get_global(cx).toolbar {
        return;
    }
    let controller = cx.new(|cx| RunController::new(workspace, cx));
    workspace.set_run_config_controller(controller.clone().into());
    let status_item = cx.new(|cx| crate::status_item::RunStatusItem::new(controller.clone(), cx));
    workspace.status_bar().update(cx, |status_bar, cx| {
        status_bar.add_left_item(status_item, window, cx);
    });
    let strip = cx.new(|cx| {
        let mut subscriptions = Vec::new();
        if let Some(store) = RunConfigStore::try_global(cx) {
            subscriptions
                .push(cx.subscribe(&store, |_, _, _: &RunConfigStoreEvent, cx| cx.notify()));
        }
        subscriptions
            .push(cx.subscribe(&controller, |_, _, _: &RunControllerEvent, cx| cx.notify()));
        // Re-render and re-validate the selection when the solution-wide active
        // project changes, so the strip follows the active member's
        // project-scoped configs and never keeps another project's config
        // selected (the trigger label + Run/Debug/Stop act on `selected_id`).
        if let Some(solution_store) = solutions::SolutionStore::try_global(cx) {
            subscriptions.push(cx.subscribe(
                &solution_store,
                |this: &mut RunConfigStrip, _, event, cx| {
                    if let solutions::SolutionStoreEvent::ActiveMemberChanged { .. } = event {
                        this.revalidate_selection(cx);
                        cx.notify();
                    }
                },
            ));
        }
        RunConfigStrip {
            controller: controller.clone(),
            menu_handle: PopoverMenuHandle::default(),
            _subscriptions: subscriptions,
        }
    });
    workspace.set_run_config_strip(strip.into(), cx);
}

/// Route a `RunCommand` (issued by an MCP tool, which lives in `run_config`
/// and can't reach `RunController` directly) to a window's `RunController`.
///
/// Targets the active window if it hosts a `MultiWorkspace`; otherwise the
/// first window that does. Best-effort: with no such window the command is
/// dropped (logged). v1 single-workspace assumption — if several windows have
/// run controllers, only the first reachable one acts.
pub fn dispatch_run_command(command: RunCommand, cx: &mut App) {
    let active = cx.active_window();
    let mut candidates: Vec<gpui::AnyWindowHandle> = Vec::new();
    if let Some(active) = active {
        candidates.push(active);
    }
    for handle in cx.windows() {
        if Some(handle) != active {
            candidates.push(handle);
        }
    }

    for handle in candidates {
        let Some(window_handle) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let command = command.clone();
        let dispatched = window_handle
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    if workspace.run_config_controller().is_none() {
                        return false;
                    }
                    apply_run_command(workspace, command, window, cx);
                    true
                })
            })
            .unwrap_or(false);
        if dispatched {
            return;
        }
    }
    log::warn!("run_config: no window with a RunController to handle {command:?}");
}

fn apply_run_command(
    workspace: &mut Workspace,
    command: RunCommand,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_controller(workspace, cx, |controller, cx| match command {
        RunCommand::Run { id, executor } => controller.run(id, executor, window, cx),
        RunCommand::Stop { id } => controller.stop(&id, cx),
        RunCommand::Select { id } => controller.select(id, cx),
    });
}

/// Run `f` against the workspace's `RunController`, if one is installed.
pub fn with_controller(
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut RunController, &mut Context<RunController>),
) {
    let Some(any) = workspace.run_config_controller().cloned() else {
        return;
    };
    if let Ok(controller) = any.downcast::<RunController>() {
        controller.update(cx, |controller, cx| f(controller, cx));
    }
}

impl RunConfigStrip {
    /// Resolve the solution-wide active member's `WorktreeId` for this strip's
    /// project, or `None` if there is no active solution / member / matching
    /// worktree (a plain non-solution project, or a solution with no recorded
    /// active member). `None` means "show only Global/Ephemeral configs".
    fn active_worktree(&self, cx: &App) -> Option<WorktreeId> {
        let project = self.controller.read(cx).project().clone();
        let store = solutions::SolutionStore::try_global(cx)?;
        let store = store.read(cx);
        let solution = project
            .read(cx)
            .worktrees(cx)
            .find_map(|worktree| store.solution_for_path(&worktree.read(cx).abs_path()))?
            .clone();
        store
            .active_member_worktree(&solution, &project, cx)
            .map(|(_catalog, worktree)| worktree)
    }

    /// Drop the controller's selection if it points at a config not visible for
    /// the new active member, reselecting the first visible config (or clearing
    /// when none remain). Keeps the trigger label + Run/Debug/Stop targets in
    /// sync with the project switch.
    fn revalidate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(store) = RunConfigStore::try_global(cx) else {
            return;
        };
        let active_worktree = self.active_worktree(cx);
        let allowed_ids: Vec<RunConfigId> =
            filter_configs_for_active_worktree(&store.read(cx).configs(), active_worktree)
                .into_iter()
                .map(|config| config.id)
                .collect();
        self.controller.update(cx, |controller, cx| {
            controller.revalidate_selection_against(&allowed_ids, cx);
        });
    }
}

impl Render for RunConfigStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(store_entity) = RunConfigStore::try_global(cx) else {
            return div().id("run-config-widget-empty").into_any_element();
        };
        let active_worktree = self.active_worktree(cx);
        let store = store_entity.read(cx);
        let configs = filter_configs_for_active_worktree(&store.configs(), active_worktree);
        if configs.is_empty() {
            // No run configurations defined yet — show an "Add Configuration…"
            // affordance (IDEA-style) instead of hiding the widget entirely, so
            // the user has a discoverable entry point into the Edit modal.
            return h_flex()
                .id("run-config-widget-add")
                .h_full()
                .child(
                    Button::new("run-config-add", "Add Configuration…")
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::Small))
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Subtle)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(actions::EditConfigurations.boxed_clone(), cx)
                        }),
                )
                .into_any_element();
        }

        let controller = self.controller.read(cx);
        let selected = controller.selected_id().and_then(|id| store.config(id));
        let selected_running = selected
            .as_ref()
            .map(|config| controller.is_running(&config.id))
            .unwrap_or(false);
        let supports_run = selected
            .as_ref()
            .map(|config| config.executors.contains(&Executor::Run))
            .unwrap_or(false);
        let supports_debug = selected
            .as_ref()
            .map(|config| config.executors.contains(&Executor::Debug))
            .unwrap_or(false);
        let selected_name: SharedString = selected
            .as_ref()
            .map(|config| config.name.clone())
            .unwrap_or_else(|| "Add Configuration…".into());
        let selected_icon = selected
            .as_ref()
            .and_then(|config| {
                store
                    .provider(&config.provider_type)
                    .map(|provider| provider.icon())
            })
            .unwrap_or(IconName::PlayFilled);

        let controller_entity = self.controller.clone();
        let menu_entries: Vec<(SharedString, RunConfigId)> = configs
            .iter()
            .map(|config| (config.name.clone(), config.id.clone()))
            .collect();

        let run_icon = if selected_running {
            IconName::Rerun
        } else {
            IconName::PlayFilled
        };

        h_flex()
            .id("run-config-widget")
            .h_full()
            .gap_0p5()
            .child(
                PopoverMenu::new("run-config-picker")
                    .trigger(
                        Button::new("run-config-trigger", selected_name)
                            .start_icon(Icon::new(selected_icon).size(IconSize::Small))
                            .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::Small))
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Subtle),
                    )
                    .with_handle(self.menu_handle.clone())
                    .menu(move |window, cx| {
                        let controller_entity = controller_entity.clone();
                        let menu_entries = menu_entries.clone();
                        Some(ContextMenu::build(
                            window,
                            cx,
                            move |mut menu, _window, _cx| {
                                for (name, id) in &menu_entries {
                                    let controller_entity = controller_entity.clone();
                                    let id = id.clone();
                                    menu = menu.entry(name.clone(), None, move |_window, cx| {
                                        controller_entity.update(cx, |controller, cx| {
                                            controller.select(id.clone(), cx);
                                        });
                                    });
                                }
                                menu.separator().action(
                                    "Edit Configurations…",
                                    actions::EditConfigurations.boxed_clone(),
                                )
                            },
                        ))
                    }),
            )
            .child(
                IconButton::new("run-config-run", run_icon)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .disabled(!supports_run)
                    .tooltip(Tooltip::text(if selected_running {
                        "Rerun"
                    } else {
                        "Run"
                    }))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(actions::Run.boxed_clone(), cx)
                    }),
            )
            .when(supports_debug, |this| {
                this.child(
                    IconButton::new("run-config-debug", IconName::Debug)
                        .shape(IconButtonShape::Square)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Debug"))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(actions::Debug.boxed_clone(), cx)
                        }),
                )
            })
            .child(
                IconButton::new("run-config-stop", IconName::Stop)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .disabled(!selected_running)
                    .tooltip(Tooltip::text("Stop"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(actions::Stop.boxed_clone(), cx)
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use gpui::{App, TestAppContext};
    use project::Project;
    use run_config::{
        ConfigScope, RunConfigProvider, RunConfiguration, RunRequest, RunResolveContext,
    };
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
        mock_config_scoped(name, ConfigScope::Global)
    }

    fn mock_config_scoped(name: &str, scope: ConfigScope) -> RunConfiguration {
        RunConfiguration {
            id: RunConfigId::from_raw(format!("mock:{name}")),
            name: name.into(),
            provider_type: "mock".into(),
            settings: serde_json::json!({}),
            executors: vec![Executor::Run],
            before_launch: vec![],
            folder: None,
            scope,
        }
    }

    #[test]
    fn filter_keeps_active_project_and_global_hides_other_project() {
        use project::WorktreeId;

        let w_a = WorktreeId::from_usize(1);
        let w_b = WorktreeId::from_usize(2);
        let a = mock_config_scoped("a", ConfigScope::Project { worktree: w_a });
        let b = mock_config_scoped("b", ConfigScope::Project { worktree: w_b });
        let g = mock_config_scoped("g", ConfigScope::Global);

        let out =
            filter_configs_for_active_worktree(&[a.clone(), b.clone(), g.clone()], Some(w_a));
        assert!(out.iter().any(|c| c.id == a.id));
        assert!(out.iter().any(|c| c.id == g.id));
        assert!(!out.iter().any(|c| c.id == b.id));

        // With no active worktree, Project-scoped configs are hidden, Global kept.
        let none = filter_configs_for_active_worktree(&[a.clone(), g.clone()], None);
        assert!(!none.iter().any(|c| c.id == a.id));
        assert!(none.iter().any(|c| c.id == g.id));

        // Ephemeral (no worktree info on RunConfiguration) is always kept.
        let e = mock_config_scoped("e", ConfigScope::Ephemeral);
        let with_eph = filter_configs_for_active_worktree(std::slice::from_ref(&e), Some(w_a));
        assert!(with_eph.iter().any(|c| c.id == e.id));
    }

    #[gpui::test]
    async fn install_registers_controller_and_selects_first(cx: &mut TestAppContext) {
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
        for name in ["a", "b"] {
            store.update(cx, |store, cx| store.upsert(mock_config(name), cx));
        }
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            install(workspace, window, cx);
        });
        cx.run_until_parked();

        workspace.update(cx, |workspace, _| {
            assert!(
                workspace.run_config_controller().is_some(),
                "install should register a RunController on the workspace"
            );
        });
        workspace.update(cx, |workspace, cx| {
            with_controller(workspace, cx, |controller, _| {
                assert_eq!(
                    controller.selected_id().map(RunConfigId::as_str),
                    Some("mock:a"),
                    "the first config should be auto-selected"
                );
            });
        });
    }

    #[gpui::test]
    async fn revalidate_reselects_when_selection_not_allowed(cx: &mut TestAppContext) {
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
        for name in ["a", "b", "c"] {
            store.update(cx, |store, cx| store.upsert(mock_config(name), cx));
        }
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        let controller = workspace.update(cx, |workspace, cx| cx.new(|cx| RunController::new(workspace, cx)));

        let id_a = RunConfigId::from_raw("mock:a");
        let id_b = RunConfigId::from_raw("mock:b");
        let id_c = RunConfigId::from_raw("mock:c");

        // Select `a`, then revalidate against a set that excludes it: reselects
        // the first allowed id (`b`).
        controller.update(cx, |c, cx| c.select(id_a.clone(), cx));
        controller.update(cx, |c, cx| {
            c.revalidate_selection_against(&[id_b.clone(), id_c.clone()], cx)
        });
        controller.read_with(cx, |c, _| {
            assert_eq!(
                c.selected_id().map(RunConfigId::as_str),
                Some("mock:b"),
                "selection not in the allowed set should reselect the first allowed id"
            );
        });

        // Revalidate against a set that still contains the selection: unchanged.
        controller.update(cx, |c, cx| {
            c.revalidate_selection_against(&[id_b.clone(), id_c.clone()], cx)
        });
        controller.read_with(cx, |c, _| {
            assert_eq!(c.selected_id().map(RunConfigId::as_str), Some("mock:b"));
        });

        // Revalidate against an empty set: selection is cleared.
        controller.update(cx, |c, cx| c.revalidate_selection_against(&[], cx));
        controller.read_with(cx, |c, _| {
            assert_eq!(
                c.selected_id(),
                None,
                "an empty allowed set should clear the selection"
            );
        });
    }
}
