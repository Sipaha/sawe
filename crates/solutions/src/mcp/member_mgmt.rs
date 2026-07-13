use crate::SolutionStore;
use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

pub(crate) fn register_member_mgmt(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AddMemberTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AddEmptyMemberTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RemoveMemberTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ReorderMembersTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SetActiveMemberTool);
    });
}

// =====================================================================
// solutions.add_member
// =====================================================================

/// Add a catalog project as a member of a Solution. Clones the project into
/// the Solution's root (using cached source if available) and registers it.
/// Returns `operation_id` immediately; the clone is spawned in the
/// background and progress can be polled via `editor.get_operation`.
///
/// **Slow**: cloning can take seconds-to-minutes for large repos.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AddMemberParams {
    /// Absent on a per-solution socket: the server injects the socket's bound
    /// Solution and overrides any value sent here. Required only on the
    /// editor-global socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<i64>,
    pub catalog_id: i64,
}

impl<'de> Deserialize<'de> for AddMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: Option<i64>,
            catalog_id: i64,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            catalog_id: inner.catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AddMemberResult {
    pub operation_id: String,
}

#[derive(Clone)]
pub struct AddMemberTool;

impl McpServerTool for AddMemberTool {
    type Input = AddMemberParams;
    type Output = AddMemberResult;
    const NAME: &'static str = "solutions.add_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let solution_id = crate::mcp::resolve_solution_id(input.solution_id)?.0;
        anyhow::ensure!(input.catalog_id > 0, "invalid_params: catalog_id is required");

        let sol_id = crate::SolutionId(solution_id);
        let cat_id = crate::CatalogId(input.catalog_id);
        let cache_root = crate::default_cache_root();

        let operation_id = cx.update(|cx| editor_mcp::op_start("solutions.add_member", cx));

        let op_id_for_task = operation_id.clone();
        let solution_id_for_log = solution_id;
        let catalog_id_for_log = input.catalog_id;

        cx.spawn(async move |cx| {
            // Forward every git progress tick to op_record_progress so the
            // operation's published `operation_progress` notifications stay
            // in sync with what the in-process store events broadcast.
            let op_id_for_cb = op_id_for_task.clone();
            let on_progress: crate::add_member::AddProgressCallback = Box::new(
                move |stage: &str, percent: Option<u8>, app: &mut gpui::App| {
                    editor_mcp::op_record_progress(&op_id_for_cb, stage.to_string(), percent, app);
                },
            );

            let task = cx.update(|cx| {
                let store = SolutionStore::global(cx);
                store.update(cx, |s, cx| {
                    s.add_member_with_progress(sol_id, cat_id, cache_root, on_progress, cx)
                })
            });
            let result = task.await;

            cx.update(|cx| match result {
                Ok(()) => {
                    editor_mcp::op_complete_ok(
                        &op_id_for_task,
                        serde_json::json!({
                            "solution_id": solution_id_for_log,
                            "catalog_id": catalog_id_for_log,
                        }),
                        cx,
                    );
                }
                Err(err) => {
                    editor_mcp::op_complete_err(&op_id_for_task, err.to_string(), cx);
                }
            });
        })
        .detach();

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "queued add_member: {}/{}",
                    solution_id, input.catalog_id
                ),
            }],
            structured_content: AddMemberResult { operation_id },
        })
    }
}

// =====================================================================
// solutions.add_empty_member
// =====================================================================

/// Create a new empty project as a member of a Solution. Creates the
/// directory `solution.root/<slug>` (slug derived from `name` and
/// uniquified against existing members), `git init`s it with no remote so
/// history can be pushed somewhere later, and registers it — no clone. The
/// member never enters the catalog, so a remote-less local project is not
/// offered in the picker for other solutions. Returns the new member's
/// `member_id` synchronously.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AddEmptyMemberParams {
    /// Absent on a per-solution socket: the server injects the socket's bound
    /// Solution and overrides any value sent here. Required only on the
    /// editor-global socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<i64>,
    pub name: String,
}

impl<'de> Deserialize<'de> for AddEmptyMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: Option<i64>,
            name: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            name: inner.name,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AddEmptyMemberResult {
    pub member_id: i64,
}

#[derive(Clone)]
pub struct AddEmptyMemberTool;

impl McpServerTool for AddEmptyMemberTool {
    type Input = AddEmptyMemberParams;
    type Output = AddEmptyMemberResult;
    const NAME: &'static str = "solutions.add_empty_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let solution_id = crate::mcp::resolve_solution_id(input.solution_id)?.0;
        anyhow::ensure!(
            !input.name.trim().is_empty(),
            "invalid_params: name is required"
        );

        let sol_id = crate::SolutionId(solution_id);
        let member_id = cx.update(|cx| -> Result<crate::MemberId> {
            let store = SolutionStore::global(cx);
            store.update(cx, |s, cx| s.add_empty_member(sol_id, &input.name, cx))
        })?;

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: member_id.to_string(),
            }],
            structured_content: AddEmptyMemberResult {
                member_id: member_id.0,
            },
        })
    }
}

// =====================================================================
// solutions.remove_member
// =====================================================================

/// Remove a member from a Solution. Config-only: the on-disk worktree
/// directory is NOT deleted; the user can re-add later by `add_member`
/// (the existing dir will be reused if origin matches).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RemoveMemberParams {
    pub member_id: i64,
}

impl<'de> Deserialize<'de> for RemoveMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            member_id: i64,
        }
        Ok(Self {
            member_id: Option::<Inner>::deserialize(de)?.unwrap_or_default().member_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RemoveMemberResult {
    pub removed: bool,
}

#[derive(Clone)]
pub struct RemoveMemberTool;

impl McpServerTool for RemoveMemberTool {
    type Input = RemoveMemberParams;
    type Output = RemoveMemberResult;
    const NAME: &'static str = "solutions.remove_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(input.member_id > 0, "invalid_params: member_id is required");
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let member_id = crate::MemberId(input.member_id);
            store.update(cx, |s, cx| s.remove_member(member_id, cx))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "removed".to_string(),
            }],
            structured_content: RemoveMemberResult { removed: true },
        })
    }
}

// =====================================================================
// solutions.reorder_members
// =====================================================================

/// Reorder Solution members. The new order MUST contain exactly the same
/// member_ids as the current member list (same set, different order).
/// Order matters — the first member becomes the agent CWD.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ReorderMembersParams {
    pub solution_id: i64,
    pub member_ids: Vec<i64>,
}

impl<'de> Deserialize<'de> for ReorderMembersParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
            member_ids: Vec<i64>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            member_ids: inner.member_ids,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReorderMembersResult {
    pub ok: bool,
}

#[derive(Clone)]
pub struct ReorderMembersTool;

impl McpServerTool for ReorderMembersTool {
    type Input = ReorderMembersParams;
    type Output = ReorderMembersResult;
    const NAME: &'static str = "solutions.reorder_members";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let sol_id = crate::SolutionId(input.solution_id);
            let order: Vec<crate::MemberId> = input
                .member_ids
                .into_iter()
                .map(crate::MemberId)
                .collect();
            store.update(cx, |s, cx| s.reorder_members(sol_id, order, cx))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "reordered".to_string(),
            }],
            structured_content: ReorderMembersResult { ok: true },
        })
    }
}

// =====================================================================
// solutions.set_active_member
// =====================================================================

/// Set the solution-wide active member (the selected project tab). Emits
/// `ActiveMemberChanged`, which drives the per-member layout swap and the
/// project-panel tree rebuild — the same path a project-tab click triggers.
/// No-op if `member_id` is already the active member.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SetActiveMemberParams {
    pub solution_id: i64,
    pub member_id: i64,
}

impl<'de> Deserialize<'de> for SetActiveMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
            member_id: i64,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            member_id: inner.member_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SetActiveMemberResult {
    pub solution_id: i64,
    pub active_member: i64,
}

#[derive(Clone)]
pub struct SetActiveMemberTool;

impl McpServerTool for SetActiveMemberTool {
    type Input = SetActiveMemberParams;
    type Output = SetActiveMemberResult;
    const NAME: &'static str = "solutions.set_active_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            input.solution_id > 0,
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(input.member_id > 0, "invalid_params: member_id is required");
        let (solution_id, member_id) = (input.solution_id, input.member_id);
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let sol = crate::SolutionId(solution_id);
            let member = crate::MemberId(member_id);
            // Guard against recording a bogus active member: the member must
            // actually belong to the solution (a stranger would leave the window
            // pointing at a project with no worktree).
            let is_member = store
                .read(cx)
                .solutions()
                .iter()
                .find(|s| s.id == sol)
                .is_some_and(|s| s.members.iter().any(|m| m.id == member));
            anyhow::ensure!(
                is_member,
                "not_found: member_id is not a member of the solution"
            );
            store.update(cx, |s, cx| s.set_active_member(sol, member, cx));
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("active_member: {solution_id} -> {member_id}"),
            }],
            structured_content: SetActiveMemberResult {
                solution_id,
                active_member: member_id,
            },
        })
    }
}

