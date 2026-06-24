//! `editor.handle_cli_args` MCP tool — single-instance handoff endpoint.
//!
//! When a second `spk-editor` process launches, it connects to the existing
//! instance's socket and calls this tool with the CLI paths. The existing
//! instance opens them in (or as) a workspace and returns metadata.
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::AsyncApp;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;

/// Forward CLI args from a second editor process to the existing instance.
/// The existing instance opens any provided paths and focuses the relevant window.
//
// `new_window` and `focus` are accepted (and round-trip through the schema) so
// Phase 7 can wire them to focus / new-window behaviour without a schema
// change. Until then, the run path ignores them — `#[allow(dead_code)]` on the
// fields keeps clippy quiet.
#[derive(Debug, Clone, Default, JsonSchema)]
pub struct HandleCliArgsParams {
    pub paths: Vec<String>,
    pub cwd: Option<String>,
    #[allow(dead_code)]
    pub new_window: Option<bool>,
    #[allow(dead_code)]
    pub focus: Option<bool>,
}

// Custom deserializer accepts JSON null, missing, or `{}` — the dispatcher in
// `context_server::listener` converts a missing `arguments` field to
// `Value::Null`, which serde would otherwise reject for a struct. When a
// concrete object is provided, fields are populated normally; absent fields
// fall back to `Default`.
impl<'de> Deserialize<'de> for HandleCliArgsParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Inner {
            paths: Vec<String>,
            cwd: Option<String>,
            new_window: Option<bool>,
            focus: Option<bool>,
        }

        let opt = Option::<Inner>::deserialize(de)?;
        let inner = opt.unwrap_or_default();
        Ok(HandleCliArgsParams {
            paths: inner.paths,
            cwd: inner.cwd,
            new_window: inner.new_window,
            focus: inner.focus,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HandleCliArgsResult {
    pub handled: bool,
    pub opened_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_window_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct HandleCliArgsTool;

impl McpServerTool for HandleCliArgsTool {
    type Input = HandleCliArgsParams;
    type Output = HandleCliArgsResult;
    const NAME: &'static str = "editor.handle_cli_args";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        let (resolved, opened_paths) = match resolve_paths(&input.paths, input.cwd.as_deref()) {
            Ok(value) => value,
            Err(err) => {
                return Ok(refused(format!("path resolution failed: {err}")));
            }
        };

        if resolved.is_empty() {
            // Empty paths means a duplicate launch with no args — bring an
            // existing window to the foreground so the user gets feedback that
            // the editor is already running, instead of having the new
            // process exit silently while the original window stays buried
            // under whatever else is on screen.
            let activated: Option<String> = cx.update(|cx| {
                cx.activate(false);
                let handle = cx.windows().into_iter().next()?;
                handle
                    .update(cx, |_, window, _| {
                        window.activate_window();
                        editor_mcp::format_window_id(handle.window_id())
                    })
                    .ok()
            });
            return Ok(success(opened_paths, activated));
        }

        let task = cx.update(|cx| {
            let app_state = crate::AppState::global(cx);
            crate::open_paths(&resolved, app_state, crate::OpenOptions::default(), cx)
        });
        match task.await {
            Ok(open_result) => {
                let window_id = editor_mcp::format_window_id(open_result.window.window_id());
                Ok(success(opened_paths, Some(window_id)))
            }
            Err(err) => Ok(refused(format!("open_paths failed: {err}"))),
        }

        // TODO Phase 7: emit `cli_args_received` notification with payload
        // { paths, source_pid, opened_window_id }.
    }
}

fn resolve_paths(
    paths: &[String],
    cwd: Option<&str>,
) -> anyhow::Result<(Vec<PathBuf>, Vec<String>)> {
    let mut resolved = Vec::with_capacity(paths.len());
    let mut display = Vec::with_capacity(paths.len());
    for path in paths {
        let pb = PathBuf::from(path);
        let abs = if pb.is_absolute() {
            pb
        } else if let Some(cwd) = cwd {
            PathBuf::from(cwd).join(path)
        } else {
            anyhow::bail!("relative path {path:?} requires cwd, but none was provided");
        };
        display.push(abs.to_string_lossy().into_owned());
        resolved.push(abs);
    }
    Ok((resolved, display))
}

fn success(
    opened_paths: Vec<String>,
    focused_window_id: Option<String>,
) -> ToolResponse<HandleCliArgsResult> {
    ToolResponse {
        content: vec![ToolResponseContent::Text {
            text: format!("opened {} path(s)", opened_paths.len()),
        }],
        structured_content: HandleCliArgsResult {
            handled: true,
            opened_paths,
            focused_window_id,
            error: None,
        },
    }
}

fn refused(error: String) -> ToolResponse<HandleCliArgsResult> {
    ToolResponse {
        content: vec![ToolResponseContent::Text {
            text: format!("refused: {error}"),
        }],
        structured_content: HandleCliArgsResult {
            handled: false,
            opened_paths: Vec::new(),
            focused_window_id: None,
            error: Some(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_paths_absolute_passes_through() {
        let (resolved, display) = resolve_paths(&["/tmp/foo".into()], None).expect("ok");
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].is_absolute());
        assert_eq!(display, vec!["/tmp/foo".to_string()]);
    }

    #[test]
    fn resolve_paths_relative_with_cwd() {
        let (resolved, _) = resolve_paths(&["sub/file.rs".into()], Some("/work")).expect("ok");
        assert_eq!(resolved[0], PathBuf::from("/work/sub/file.rs"));
    }

    #[test]
    fn resolve_paths_relative_without_cwd_errors() {
        let result = resolve_paths(&["sub/file.rs".into()], None);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_paths_empty_input() {
        let (resolved, display) = resolve_paths(&[], None).expect("ok");
        assert!(resolved.is_empty());
        assert!(display.is_empty());
    }

    #[test]
    fn params_deserialize_from_null() {
        let _: HandleCliArgsParams =
            serde_json::from_value(serde_json::Value::Null).expect("null accepted");
    }

    #[test]
    fn params_deserialize_from_empty_object() {
        let _: HandleCliArgsParams =
            serde_json::from_value(serde_json::json!({})).expect("empty object accepted");
    }

    #[test]
    fn params_deserialize_from_paths_only() {
        let p: HandleCliArgsParams = serde_json::from_value(serde_json::json!({
            "paths": ["/tmp/foo", "/tmp/bar"]
        }))
        .expect("parse");
        assert_eq!(p.paths.len(), 2);
        assert_eq!(p.paths[0], "/tmp/foo");
    }
}
