use crate::SolutionStore;
use anyhow::Result;
use gpui::App;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod buffer_ops;
mod code_nav;
mod fs_ops;

pub use buffer_ops::*;
pub use code_nav::*;
pub use fs_ops::*;

pub(crate) fn register_project_files(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListFilesTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ReadBufferTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ApplyEditTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(SaveBufferTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(OpenFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CloseBufferTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(CreateFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(DeleteFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RenameFileTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(FindInBuffersTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GotoDefinitionTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(FindReferencesTool);
    });
}

// =====================================================================
// Path-validation helper (cross-cutting security primitive)
// =====================================================================

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum PathValidationError {
    SolutionNotFound,
    PathOutsideSolution,
    InvalidPath,
}

impl std::fmt::Display for PathValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SolutionNotFound => write!(f, "solution_not_found"),
            Self::PathOutsideSolution => write!(f, "path_outside_solution"),
            Self::InvalidPath => write!(f, "invalid_path"),
        }
    }
}

impl std::error::Error for PathValidationError {}

/// Verify that `path` lies under at least one worktree root of the
/// named Solution. Returns the canonicalized absolute path.
///
/// Used by every Phase 6 `project.*` tool to prevent agents from
/// escaping into arbitrary filesystem via `apply_edit("/etc/passwd", ...)`.
#[allow(dead_code)]
pub fn validate_path_in_solution(
    solution_id: &str,
    path: &str,
    cx: &App,
) -> Result<std::path::PathBuf, PathValidationError> {
    let absolute = std::path::PathBuf::from(path);
    if !absolute.is_absolute() {
        // Relative paths require a cwd that we don't have here. Reject.
        return Err(PathValidationError::InvalidPath);
    }

    // Best-effort canonicalization. If the path doesn't exist yet (e.g.
    // create_file), we accept the absolute non-canonical form provided
    // its prefix is under a Solution member.
    let canonical = absolute.canonicalize().unwrap_or_else(|_| absolute.clone());

    let store = SolutionStore::try_global(cx).ok_or(PathValidationError::SolutionNotFound)?;
    let valid = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.0.to_string() == solution_id)
            .map(|sol| {
                sol.members.iter().any(|m| {
                    let canon_member = m
                        .local_path
                        .canonicalize()
                        .unwrap_or_else(|_| m.local_path.clone());
                    canonical.starts_with(&canon_member)
                }) || canonical.starts_with(&sol.root)
            })
    });

    match valid {
        Some(true) => Ok(canonical),
        Some(false) => Err(PathValidationError::PathOutsideSolution),
        None => Err(PathValidationError::SolutionNotFound),
    }
}

// =====================================================================
// Shared edit-location types (used by apply_edit + code navigation)
// =====================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EditSpec {
    pub range: EditRange,
    pub new_text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EditRange {
    pub start: EditPoint,
    pub end: EditPoint,
}

/// Zero-based `(line, col)` location. `col` is a UTF-8 byte offset
/// within the line, matching `language::Point`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EditPoint {
    pub line: u32,
    pub col: u32,
}

// Locate the `Project` whose worktrees back the named Solution. We walk
// every open `MultiWorkspace` window and return the first project whose
// visible worktrees include the Solution's root (or a member directory
// underneath it).
pub(crate) fn project_for_solution(solution_id: &str, cx: &mut App) -> Option<gpui::Entity<project::Project>> {
    let store = SolutionStore::try_global(cx)?;
    let root = store.read_with(cx, |s, _| {
        s.solutions()
            .iter()
            .find(|sol| sol.id.0.to_string() == solution_id)
            .map(|sol| sol.root.clone())
    })?;

    for handle in cx.windows() {
        let Some(window_handle) = handle.downcast::<workspace::MultiWorkspace>() else {
            continue;
        };
        let result = window_handle
            .update(cx, |multi, _window, cx| {
                for workspace_entity in multi.workspaces() {
                    let workspace = workspace_entity.read(cx);
                    let project = workspace.project();
                    let matches = project
                        .read(cx)
                        .visible_worktrees(cx)
                        .any(|tree| tree.read(cx).abs_path().starts_with(&root));
                    if matches {
                        return Some(project.clone());
                    }
                }
                None
            })
            .ok()
            .flatten();
        if let Some(project) = result {
            return Some(project);
        }
    }
    None
}

// Map an absolute path to a `ProjectPath` within one of the project's
// visible worktrees. Returns `path_not_in_worktree` if no worktree
// contains it.
fn resolve_project_path(
    project: &gpui::Entity<project::Project>,
    abs_path: &str,
    cx: &App,
) -> anyhow::Result<project::ProjectPath> {
    let abs = std::path::PathBuf::from(abs_path);
    let project_ref = project.read(cx);
    for tree_entity in project_ref.visible_worktrees(cx) {
        let tree = tree_entity.read(cx);
        let root = tree.abs_path();
        if abs.starts_with(root.as_ref()) {
            let rel = abs
                .strip_prefix(root.as_ref())
                .map_err(|err| anyhow::anyhow!("strip_prefix: {err}"))?;
            let rel_path = util::rel_path::RelPath::new(rel, tree.path_style())
                .map_err(|err| anyhow::anyhow!("rel_path: {err}"))?
                .into_owned()
                .into();
            return Ok(project::ProjectPath {
                worktree_id: tree.id(),
                path: rel_path,
            });
        }
    }
    anyhow::bail!("path_not_in_worktree: {abs_path}")
}
