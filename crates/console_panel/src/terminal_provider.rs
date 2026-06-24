use anyhow::Result;
use gpui::{App, AppContext as _, Entity, Task, WeakEntity, Window};
use project::Project;
use std::path::PathBuf;
use terminal_view::TerminalView;
use workspace::Workspace;

/// Spawns terminal items on behalf of `ConsolePanel`.
///
/// Stateless — it does not track open terminals. The owning `ConsolePanel`
/// holds the `Vec<ConsoleTab>` of created items.
pub struct TerminalProvider {
    workspace: WeakEntity<Workspace>,
}

impl TerminalProvider {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self { workspace }
    }

    /// Spawn a new terminal view at `cwd` (or the active worktree root if `None`).
    pub fn new_tab(
        &self,
        cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<TerminalView>>> {
        let workspace = self.workspace.clone();
        window.spawn(cx, async move |cx| {
            let workspace = workspace
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("workspace dropped"))?;
            let project: Entity<Project> = workspace.read_with(cx, |ws, _| ws.project().clone());
            let resolved_cwd = match cwd {
                Some(p) => Some(p),
                None => project.read_with(cx, |p, cx| {
                    p.worktrees(cx)
                        .next()
                        .map(|wt| wt.read(cx).abs_path().to_path_buf())
                }),
            };
            let terminal = project
                .update(cx, |project: &mut Project, cx| {
                    project.create_terminal_shell(resolved_cwd, cx)
                })
                .await?;
            let terminal_view = workspace.update_in(cx, |ws, window, cx| {
                cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        ws.weak_handle(),
                        ws.database_id(),
                        ws.project().downgrade(),
                        window,
                        cx,
                    )
                })
            })?;
            Ok(terminal_view)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use workspace::Workspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            terminal_view::init(cx);
        });
    }

    #[gpui::test]
    async fn new_tab_spawns_terminal_view(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", serde_json::json!({})).await;

        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let window_handle = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));

        let provider = window_handle
            .update(cx, |workspace, _, _| {
                TerminalProvider::new(workspace.weak_handle())
            })
            .unwrap();

        let task: Task<Result<Entity<TerminalView>>> = window_handle
            .update(cx, |_, window, cx| provider.new_tab(None, window, cx))
            .unwrap();

        let result = task.await;
        assert!(result.is_ok(), "new_tab should succeed: {:?}", result.err());
    }
}
