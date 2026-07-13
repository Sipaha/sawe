use crate::SolutionStore;
use anyhow::{Context as _, Result};
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

pub(crate) fn register_catalog(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ListCatalogTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(AddCatalogProjectTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RemoveCatalogProjectTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(MergeCatalogProjectTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(EditCatalogProjectTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(ClearCacheTool);
    });
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RefreshCacheTool);
    });
}

// =====================================================================
// catalog.list
// =====================================================================

/// List all catalog entries with their on-disk cache status.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ListCatalogParams {}

impl<'de> Deserialize<'de> for ListCatalogParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(de)?;
        Ok(ListCatalogParams {})
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CatalogProjectInfo {
    pub id: String,
    pub name: String,
    pub remote_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// `"absent"` when no cache directory exists, `"present"` when one does.
    pub cache_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_last_fetched: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListCatalogResult {
    pub projects: Vec<CatalogProjectInfo>,
}

#[derive(Clone)]
pub struct ListCatalogTool;

impl McpServerTool for ListCatalogTool {
    type Input = ListCatalogParams;
    type Output = ListCatalogResult;
    const NAME: &'static str = "catalog.list";

    async fn run(
        &self,
        _input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let projects: Vec<CatalogProjectInfo> = cx.update(|cx| {
            let store = SolutionStore::global(cx);
            let cache_root = crate::default_cache_root();
            store.read_with(cx, |s, _| {
                s.catalog()
                    .iter()
                    .map(|p| build_catalog_info(p, &cache_root))
                    .collect()
            })
        });
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} project(s)", projects.len()),
            }],
            structured_content: ListCatalogResult { projects },
        })
    }
}

fn build_catalog_info(
    p: &crate::CatalogProject,
    cache_root: &std::path::Path,
) -> CatalogProjectInfo {
    let entry_path = crate::cache::cache_path(cache_root, &p.remote_url);
    let exists = entry_path.exists();
    let cache_last_fetched = if exists {
        std::fs::metadata(&entry_path)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
    } else {
        None
    };
    CatalogProjectInfo {
        id: p.id.as_str().to_string(),
        name: p.name.clone(),
        remote_url: p.remote_url.clone(),
        default_branch: p.default_branch.clone(),
        cache_status: if exists { "present" } else { "absent" }.to_string(),
        cache_last_fetched,
    }
}

// =====================================================================
// catalog.add_project
// =====================================================================

/// Add a new catalog entry. The id is derived from `name` (slug) and is
/// returned in `catalog_id`. `remote_url` is immutable after creation.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct AddCatalogProjectParams {
    pub name: String,
    pub remote_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

impl<'de> Deserialize<'de> for AddCatalogProjectParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            name: String,
            remote_url: String,
            default_branch: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            name: inner.name,
            remote_url: inner.remote_url,
            default_branch: inner.default_branch,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AddCatalogProjectResult {
    pub catalog_id: String,
}

#[derive(Clone)]
pub struct AddCatalogProjectTool;

impl McpServerTool for AddCatalogProjectTool {
    type Input = AddCatalogProjectParams;
    type Output = AddCatalogProjectResult;
    const NAME: &'static str = "catalog.add_project";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.name.trim().is_empty(),
            "invalid_params: name is required"
        );
        anyhow::ensure!(
            !input.remote_url.trim().is_empty(),
            "invalid_params: remote_url is required"
        );
        let id = cx.update(|cx| -> Result<String> {
            let store = SolutionStore::global(cx);
            let id = store.update(cx, |s, cx| {
                s.add_catalog_project(
                    &input.name,
                    &input.remote_url,
                    input.default_branch.clone(),
                    cx,
                )
            })?;
            Ok(id.as_str().to_string())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("added: {id}"),
            }],
            structured_content: AddCatalogProjectResult { catalog_id: id },
        })
    }
}

// =====================================================================
// catalog.remove_project
// =====================================================================

/// Remove a catalog entry. Refused (with an error) if any Solution still
/// references it; remove the member from the Solution(s) first.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RemoveCatalogProjectParams {
    pub catalog_id: String,
}

impl<'de> Deserialize<'de> for RemoveCatalogProjectParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: String,
        }
        Ok(Self {
            catalog_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RemoveCatalogProjectResult {
    pub removed: bool,
}

#[derive(Clone)]
pub struct RemoveCatalogProjectTool;

impl McpServerTool for RemoveCatalogProjectTool {
    type Input = RemoveCatalogProjectParams;
    type Output = RemoveCatalogProjectResult;
    const NAME: &'static str = "catalog.remove_project";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let id = crate::CatalogId(input.catalog_id);
            store.update(cx, |s, cx| s.remove_catalog_project(&id, cx))?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: "removed".to_string(),
            }],
            structured_content: RemoveCatalogProjectResult { removed: true },
        })
    }
}

// =====================================================================
// catalog.merge_project
// =====================================================================

/// Fold a duplicate catalog entry into the canonical one: every solution member
/// referencing `from` is repointed at `into` (keeping its checked-out clone —
/// only the `catalog_id` link is rewritten), then the `from` row is deleted.
///
/// This is the cleanup path for duplicates created before `add_project` started
/// rejecting them. `remove_project` can't do it: both halves of such a pair are
/// usually referenced by different solutions, so it refuses.
///
/// Both entries must point at the SAME repository, and no single solution may
/// hold both (that would collapse two members into one and drop a working tree).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct MergeCatalogProjectParams {
    /// The duplicate to fold away — this row is deleted.
    pub from: String,
    /// The canonical entry to keep.
    pub into: String,
}

impl<'de> Deserialize<'de> for MergeCatalogProjectParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            from: String,
            into: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            from: inner.from,
            into: inner.into,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MergeCatalogProjectResult {
    /// How many solution members were repointed onto `into`.
    pub repointed_members: usize,
}

#[derive(Clone)]
pub struct MergeCatalogProjectTool;

impl McpServerTool for MergeCatalogProjectTool {
    type Input = MergeCatalogProjectParams;
    type Output = MergeCatalogProjectResult;
    const NAME: &'static str = "catalog.merge_project";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(!input.from.is_empty(), "invalid_params: from is required");
        anyhow::ensure!(!input.into.is_empty(), "invalid_params: into is required");
        let repointed = cx.update(|cx| -> Result<usize> {
            let store = SolutionStore::global(cx);
            let from = crate::CatalogId(input.from);
            let into = crate::CatalogId(input.into);
            store.update(cx, |s, cx| s.merge_catalog_project(&from, &into, cx))
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("merged; {repointed} member(s) repointed"),
            }],
            structured_content: MergeCatalogProjectResult {
                repointed_members: repointed,
            },
        })
    }
}

// =====================================================================
// catalog.edit_project
// =====================================================================

/// Edit `name` and/or `default_branch` of a catalog entry via the MCP
/// surface. The UI modal also lets the user change `remote_url` (which
/// rewrites every existing clone's `origin`); that capability is
/// intentionally not exposed here — agent-driven URL changes would need
/// separate plumbing to confirm-or-rollback the cascading remote-rewrite,
/// and no use case has come up yet. Use the UI modal for URL edits.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct EditCatalogProjectParams {
    pub catalog_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

impl<'de> Deserialize<'de> for EditCatalogProjectParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: String,
            name: Option<String>,
            default_branch: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            catalog_id: inner.catalog_id,
            name: inner.name,
            default_branch: inner.default_branch,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EditCatalogProjectResult {
    pub catalog_id: String,
}

#[derive(Clone)]
pub struct EditCatalogProjectTool;

impl McpServerTool for EditCatalogProjectTool {
    type Input = EditCatalogProjectParams;
    type Output = EditCatalogProjectResult;
    const NAME: &'static str = "catalog.edit_project";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        let catalog_id = input.catalog_id.clone();
        cx.update(|cx| -> Result<()> {
            let store = SolutionStore::global(cx);
            let id = crate::CatalogId(input.catalog_id);
            store.update(cx, |s, cx| {
                s.edit_catalog_project(&id, input.name, input.default_branch, None, cx)
            })?;
            Ok(())
        })?;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("edited: {catalog_id}"),
            }],
            structured_content: EditCatalogProjectResult { catalog_id },
        })
    }
}

// =====================================================================
// catalog.clear_cache
// =====================================================================

/// Delete the on-disk warm clone cache for one catalog entry (when
/// `catalog_id` is provided) or for every entry (when omitted). Useful
/// for autonomous test teardown and for forcing the next add_member /
/// refresh_cache to start from a fresh clone.
///
/// Synchronous: runs an `std::fs::remove_dir_all` per affected entry on
/// the calling thread. Returns the list of removed cache directories.
/// A missing directory is not an error — it counts as already cleared.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ClearCacheParams {
    /// Specific catalog entry to clear. If omitted, clears the cache for
    /// every catalog entry currently in the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<String>,
}

impl<'de> Deserialize<'de> for ClearCacheParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: Option<String>,
        }
        Ok(Self {
            catalog_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClearCacheResult {
    pub removed_paths: Vec<String>,
}

#[derive(Clone)]
pub struct ClearCacheTool;

impl McpServerTool for ClearCacheTool {
    type Input = ClearCacheParams;
    type Output = ClearCacheResult;
    const NAME: &'static str = "catalog.clear_cache";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let urls = cx.update(|cx| -> Result<Vec<String>> {
            let store = SolutionStore::global(cx);
            store.read_with(cx, |s, _| {
                if let Some(id) = input.catalog_id.as_deref() {
                    let url = s
                        .catalog()
                        .iter()
                        .find(|p| p.id.as_str() == id)
                        .map(|p| p.remote_url.clone())
                        .with_context(|| format!("catalog_not_found: {id}"))?;
                    Ok(vec![url])
                } else {
                    Ok(s.catalog().iter().map(|p| p.remote_url.clone()).collect())
                }
            })
        })?;

        let cache_root = crate::default_cache_root();
        let mut removed = Vec::new();
        for url in urls {
            let path = crate::cache::cache_path(&cache_root, &url);
            if path.exists() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
                removed.push(path.to_string_lossy().into_owned());
            }
        }

        let summary = match removed.len() {
            0 => "no cache directories to remove".to_string(),
            n => format!(
                "removed {n} cache director{}",
                if n == 1 { "y" } else { "ies" }
            ),
        };
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text { text: summary }],
            structured_content: ClearCacheResult {
                removed_paths: removed,
            },
        })
    }
}

// =====================================================================
// catalog.refresh_cache
// =====================================================================

/// Refresh the on-disk cache for a catalog entry by running `git fetch`
/// (or cloning if the cache is absent). Returns an `operation_id`
/// immediately; the work is spawned in the background and progress can be
/// polled via `editor.get_operation`.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RefreshCacheParams {
    pub catalog_id: String,
}

impl<'de> Deserialize<'de> for RefreshCacheParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            catalog_id: String,
        }
        Ok(Self {
            catalog_id: Option::<Inner>::deserialize(de)?
                .unwrap_or_default()
                .catalog_id,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefreshCacheResult {
    pub operation_id: String,
}

#[derive(Clone)]
pub struct RefreshCacheTool;

impl McpServerTool for RefreshCacheTool {
    type Input = RefreshCacheParams;
    type Output = RefreshCacheResult;
    const NAME: &'static str = "catalog.refresh_cache";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.catalog_id.is_empty(),
            "invalid_params: catalog_id is required"
        );
        let remote_url = cx.update(|cx| -> Result<String> {
            let store = SolutionStore::global(cx);
            let url = store.read_with(cx, |s, _| {
                s.catalog()
                    .iter()
                    .find(|p| p.id.as_str() == input.catalog_id)
                    .map(|p| p.remote_url.clone())
            });
            url.with_context(|| format!("catalog_not_found: {}", input.catalog_id))
        })?;

        let operation_id = cx.update(|cx| editor_mcp::op_start("catalog.refresh_cache", cx));

        let op_id_for_task = operation_id.clone();
        let catalog_id_for_log = input.catalog_id.clone();
        let cache_root = crate::default_cache_root();

        cx.spawn(async move |cx| {
            cx.update(|cx| {
                editor_mcp::op_record_progress(
                    &op_id_for_task,
                    "fetching".to_string(),
                    Some(0),
                    cx,
                );
            });

            // Note: the progress callback runs synchronously inside the future
            // and has no App handle, so intermediate progress updates can't
            // call op_record_progress here. We only record the initial state.
            let result = crate::cache::refresh_cache(&cache_root, &remote_url, |_| {}).await;

            cx.update(|cx| match result {
                Ok(_) => {
                    editor_mcp::op_complete_ok(
                        &op_id_for_task,
                        serde_json::json!({ "catalog_id": catalog_id_for_log }),
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
                text: format!("queued refresh_cache: {}", input.catalog_id),
            }],
            structured_content: RefreshCacheResult { operation_id },
        })
    }
}

