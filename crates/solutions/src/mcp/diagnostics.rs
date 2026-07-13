use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::project_files::{EditPoint, EditRange, project_for_solution};

pub(crate) fn register_diagnostics(cx: &mut App) {
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(GetDiagnosticsTool);
    });
}

// =====================================================================
// diagnostics.get
// =====================================================================

/// Get LSP diagnostics for files in a Solution. Returns both per-path
/// summary counts (`error_count` / `warning_count` aggregated across all
/// language servers reporting on that file) and detailed per-diagnostic
/// items (path, range, severity, message, source, code). Optional
/// `buffer_path` filters results to a single project-relative path.
///
/// `info_count` / `hint_count` are intentionally absent from the
/// summary: the underlying `project::DiagnosticSummary` only tracks
/// errors and warnings today. Use the `items` array for full severity
/// detail (`"info"`, `"hint"`).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GetDiagnosticsParams {
    /// Absent on a per-solution socket: the server injects the socket's bound
    /// Solution and overrides any value sent here. Required only on the
    /// editor-global socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub solution_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_path: Option<String>,
}

impl<'de> Deserialize<'de> for GetDiagnosticsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: Option<i64>,
            buffer_path: Option<String>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            buffer_path: inner.buffer_path,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticPathSummary {
    pub path: String,
    pub error_count: usize,
    pub warning_count: usize,
}

/// A single diagnostic emitted by an LSP server, resolved to
/// zero-based `(line, col)` byte coordinates within its buffer.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiagnosticItem {
    /// Project-relative path of the buffer that owns this diagnostic.
    pub path: String,
    pub range: EditRange,
    /// Lower-cased severity: `"error"`, `"warning"`, `"info"`, or `"hint"`.
    pub severity: String,
    pub message: String,
    /// LSP `source` field (e.g. `"rust-analyzer"`, `"clippy"`), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// LSP `code` field rendered to a string, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GetDiagnosticsResult {
    /// Per-file summary counts (always present).
    pub summaries: Vec<DiagnosticPathSummary>,
    /// Detailed per-diagnostic items, one entry per LSP diagnostic.
    pub items: Vec<DiagnosticItem>,
}

#[derive(Clone)]
pub struct GetDiagnosticsTool;

impl McpServerTool for GetDiagnosticsTool {
    type Input = GetDiagnosticsParams;
    type Output = GetDiagnosticsResult;
    const NAME: &'static str = "diagnostics.get";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let solution_id = crate::mcp::resolve_solution_id(input.solution_id)?.0;
        let summaries = cx.update(|cx| collect_diagnostic_summaries(solution_id, &input, cx));
        let items = collect_diagnostic_items(solution_id, &input, cx).await;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!(
                    "{} file(s) with {} diagnostic(s)",
                    summaries.len(),
                    items.len(),
                ),
            }],
            structured_content: GetDiagnosticsResult { summaries, items },
        })
    }
}

fn collect_diagnostic_summaries(
    solution_id: i64,
    input: &GetDiagnosticsParams,
    cx: &App,
) -> Vec<DiagnosticPathSummary> {
    // The Solution's own project, not the hosting window's active one — the
    // window may be presenting a sibling Solution (see `workspaces_for_solution`).
    let Some(project) = project_for_solution(solution_id, cx) else {
        return Vec::new();
    };
    let project = project.read(cx);

    // A path may have multiple language servers reporting on it (e.g.
    // rust-analyzer + clippy). Aggregate counts across all servers for a single
    // per-path summary, matching the rollup shown in the editor's diagnostics
    // panel.
    let mut by_path: std::collections::BTreeMap<String, DiagnosticPathSummary> =
        std::collections::BTreeMap::new();

    for (project_path, _server_id, summary) in project.diagnostic_summaries(false, cx) {
        let path_str = project_path.path.as_unix_str().to_string();
        if let Some(filter) = input.buffer_path.as_deref()
            && path_str != filter
        {
            continue;
        }
        let entry = by_path
            .entry(path_str.clone())
            .or_insert(DiagnosticPathSummary {
                path: path_str,
                error_count: 0,
                warning_count: 0,
            });
        entry.error_count += summary.error_count;
        entry.warning_count += summary.warning_count;
    }

    by_path.into_values().collect()
}

async fn collect_diagnostic_items(
    solution_id: i64,
    input: &GetDiagnosticsParams,
    cx: &mut AsyncApp,
) -> Vec<DiagnosticItem> {
    let Some(project) = cx.update(|cx| project_for_solution(solution_id, cx)) else {
        return Vec::new();
    };

    // Snapshot the project paths that have any diagnostic summary.
    // Multiple language servers may report on the same file, so dedupe by
    // (worktree, path) before opening buffers — `Buffer::diagnostics_in_range`
    // already aggregates entries across all servers for that buffer.
    let project_paths: Vec<project::ProjectPath> = cx.update(|cx| {
        let project_ref = project.read(cx);
        let mut seen = collections::HashSet::default();
        let mut paths = Vec::new();
        for (project_path, _server_id, _summary) in project_ref.diagnostic_summaries(false, cx) {
            let path_str = project_path.path.as_unix_str().to_string();
            if let Some(filter) = input.buffer_path.as_deref() {
                if path_str != filter {
                    continue;
                }
            }
            let key = (project_path.worktree_id, project_path.path.clone());
            if seen.insert(key) {
                paths.push(project_path);
            }
        }
        paths
    });

    let mut items = Vec::new();
    for project_path in project_paths {
        let path_str = project_path.path.as_unix_str().to_string();
        let buffer_task = project.update(cx, |project, cx| project.open_buffer(project_path, cx));
        let buffer = match buffer_task.await {
            Ok(buffer) => buffer,
            Err(err) => {
                log::debug!("diagnostics.get: open_buffer failed for {path_str}: {err}");
                continue;
            }
        };
        let entries = cx.update(|cx| {
            use language::OffsetRangeExt as _;
            let snapshot = buffer.read(cx).snapshot();
            let max_point = snapshot.max_point();
            snapshot
                .diagnostics_in_range::<_, language::Anchor>(
                    language::Point::zero()..max_point,
                    false,
                )
                .map(|entry| {
                    let point_range = entry.range.to_point(&snapshot);
                    DiagnosticItem {
                        path: path_str.clone(),
                        range: EditRange {
                            start: EditPoint {
                                line: point_range.start.row,
                                col: point_range.start.column,
                            },
                            end: EditPoint {
                                line: point_range.end.row,
                                col: point_range.end.column,
                            },
                        },
                        severity: severity_to_string(entry.diagnostic.severity).to_string(),
                        message: entry.diagnostic.message.clone(),
                        source: entry.diagnostic.source.clone(),
                        code: entry.diagnostic.code.as_ref().map(|code| match code {
                            lsp::NumberOrString::Number(n) => n.to_string(),
                            lsp::NumberOrString::String(s) => s.clone(),
                        }),
                    }
                })
                .collect::<Vec<_>>()
        });
        items.extend(entries);
    }

    items
}

fn severity_to_string(severity: language::DiagnosticSeverity) -> &'static str {
    match severity {
        language::DiagnosticSeverity::ERROR => "error",
        language::DiagnosticSeverity::WARNING => "warning",
        language::DiagnosticSeverity::INFORMATION => "info",
        language::DiagnosticSeverity::HINT => "hint",
        _ => "info",
    }
}
