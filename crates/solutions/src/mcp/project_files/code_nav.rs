use crate::SolutionStore;
use anyhow::Result;
use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::{App, AsyncApp};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::{EditPoint, project_for_solution, resolve_project_path, validate_path_in_solution};

// =====================================================================
// project.find_in_buffers
// =====================================================================

/// Search across files of a Solution. Defaults to a case-insensitive
/// substring match; opt into `regex` for a regex match (in either case
/// `case_sensitive: true` makes the match case-sensitive). The `scope`
/// parameter is reserved for future "open buffers only" behaviour and is
/// currently ignored — all searchable files are searched. Pagination via
/// an opaque cursor (`worktree_root|path:line` of the last match
/// returned); the cursor is advisory and not perfectly stable across
/// calls because the order of results from `Project::search` depends on
/// scan timing, so callers should treat it as a coarse "resume from
/// here" hint.
///
/// Backed by `Project::search`, so gitignore is respected and unsaved
/// open-buffer state is reflected. Files outside the Solution's root
/// (when the project owns extra worktrees) are filtered out post-hoc.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FindInBuffersParams {
    pub solution_id: String,
    /// Substring or regex pattern.
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub case_sensitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<bool>,
    /// `"all_files"` (default) or `"open"`. v1: ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Optional glob pattern matched against the worktree-relative path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_glob: Option<String>,
    /// Opaque cursor returned from the previous response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum number of matches in this page. Default 100, max 1000.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

impl<'de> Deserialize<'de> for FindInBuffersParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            query: String,
            case_sensitive: Option<bool>,
            regex: Option<bool>,
            scope: Option<String>,
            file_glob: Option<String>,
            cursor: Option<String>,
            max: Option<usize>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            query: inner.query,
            case_sensitive: inner.case_sensitive,
            regex: inner.regex,
            scope: inner.scope,
            file_glob: inner.file_glob,
            cursor: inner.cursor,
            max: inner.max,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchMatch {
    /// Path relative to `worktree_root`, in unix form.
    pub path: String,
    /// Absolute worktree root containing the file.
    pub worktree_root: String,
    /// Zero-based line index.
    pub line: u32,
    /// Zero-based UTF-8 byte column where the match starts.
    pub col: u32,
    /// Full text of the line containing the match (untruncated).
    pub line_text: String,
    /// `[start, end)` UTF-8 byte offsets within `line_text`.
    pub match_range: [u32; 2],
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindInBuffersResult {
    pub matches: Vec<SearchMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct FindInBuffersTool;

impl McpServerTool for FindInBuffersTool {
    type Input = FindInBuffersParams;
    type Output = FindInBuffersResult;
    const NAME: &'static str = "project.find_in_buffers";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(!input.query.is_empty(), "invalid_params: query is required");
        let max = input.max.unwrap_or(100).clamp(1, 1000);
        let case_sensitive = input.case_sensitive.unwrap_or(false);
        let use_regex = input.regex.unwrap_or(false);
        let start_after = input.cursor.clone().unwrap_or_default();

        let solution_root = cx
            .update(|cx| {
                SolutionStore::try_global(cx).and_then(|store| {
                    store.read_with(cx, |s, _| {
                        s.solutions()
                            .iter()
                            .find(|sol| sol.id.0.to_string() == input.solution_id)
                            .map(|sol| sol.root.clone())
                    })
                })
            })
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        // Build the SearchQuery. We pass the optional file_glob through
        // the project search engine's own include matcher so gitignore
        // and the include/exclude pipeline stay consistent with
        // upstream's project search.
        let path_style = cx.update(|cx| project.read(cx).path_style(cx));
        let include_matcher = match input.file_glob.as_deref() {
            Some(glob) => util::paths::PathMatcher::new([glob], path_style)
                .map_err(|err| anyhow::anyhow!("invalid_glob: {err}"))?,
            None => util::paths::PathMatcher::default(),
        };
        let exclude_matcher = util::paths::PathMatcher::default();

        let query = if use_regex {
            project::search::SearchQuery::regex(
                &input.query,
                false,
                case_sensitive,
                false,
                false,
                include_matcher,
                exclude_matcher,
                false,
                None,
            )
        } else {
            project::search::SearchQuery::text(
                &input.query,
                false,
                case_sensitive,
                false,
                include_matcher,
                exclude_matcher,
                false,
                None,
            )
        };
        let query = query.map_err(|err| anyhow::anyhow!("invalid_query: {err}"))?;

        let results = cx.update(|cx| project.update(cx, |proj, cx| proj.search(query, cx)));
        let project::SearchResults { rx, _task_handle } = results;

        let mut all_matches: Vec<SearchMatch> = Vec::new();
        let mut hit_limit = false;
        // Pull matches from the search stream; bail out once we've
        // accumulated `max` matches so the caller can resume via the
        // returned cursor.
        loop {
            let Ok(result) = rx.recv().await else {
                break;
            };
            match result {
                project::search::SearchResult::Buffer { buffer, ranges } => {
                    if ranges.is_empty() {
                        continue;
                    }
                    let collected = cx.update(|cx| {
                        let buffer_ref = buffer.read(cx);
                        let snapshot = buffer_ref.snapshot();
                        let Some(file) = buffer_ref.file() else {
                            return Vec::new();
                        };
                        let Some(local) = file.as_local() else {
                            return Vec::new();
                        };
                        let abs_path = local.abs_path(cx);
                        // Filter out files that fall outside the
                        // Solution root, e.g. when the project owns
                        // extra worktrees added after the Solution was
                        // opened.
                        if !abs_path.starts_with(&solution_root) {
                            return Vec::new();
                        }
                        let worktree_id = file.worktree_id(cx);
                        let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx)
                        else {
                            return Vec::new();
                        };
                        let worktree_root =
                            worktree.read(cx).abs_path().to_string_lossy().into_owned();
                        let rel_path = file.path().as_unix_str().to_string();

                        let mut local_matches = Vec::new();
                        for range in ranges.iter() {
                            use language::OffsetRangeExt as _;
                            let point_range = range.to_point(&snapshot);
                            let line = point_range.start.row;
                            // Restrict the match to the start line
                            // (multi-line matches are rare for typical
                            // text searches and the response shape is
                            // single-line).
                            let line_len = snapshot.line_len(line);
                            let line_start = language::Point::new(line, 0);
                            let line_end = language::Point::new(line, line_len);
                            let line_text: String =
                                snapshot.text_for_range(line_start..line_end).collect();
                            let start_col = point_range.start.column;
                            let end_col = if point_range.end.row == line {
                                point_range.end.column
                            } else {
                                line_len
                            };
                            local_matches.push(SearchMatch {
                                path: rel_path.clone(),
                                worktree_root: worktree_root.clone(),
                                line,
                                col: start_col,
                                line_text,
                                match_range: [start_col, end_col],
                            });
                        }
                        local_matches
                    });

                    // Apply the cursor filter (advisory: skip matches
                    // whose `worktree_root|rel_path:line` ordering is
                    // <= start_after).
                    for m in collected {
                        if !start_after.is_empty() {
                            let key = format!("{}|{}:{}", m.worktree_root, m.path, m.line);
                            if key.as_str() <= start_after.as_str() {
                                continue;
                            }
                        }
                        if all_matches.len() >= max {
                            hit_limit = true;
                            break;
                        }
                        all_matches.push(m);
                    }
                    if hit_limit {
                        break;
                    }
                }
                project::search::SearchResult::LimitReached => {
                    hit_limit = true;
                    break;
                }
                project::search::SearchResult::WaitingForScan
                | project::search::SearchResult::Searching => continue,
            }
        }

        let next_cursor = if hit_limit {
            all_matches
                .last()
                .map(|m| format!("{}|{}:{}", m.worktree_root, m.path, m.line))
        } else {
            None
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} match(es)", all_matches.len()),
            }],
            structured_content: FindInBuffersResult {
                matches: all_matches,
                next_cursor,
            },
        })
    }
}

// =====================================================================
// project.goto_definition
// =====================================================================

/// Resolve LSP "goto definition" for a position in a file. Opens the
/// buffer (without surfacing a tab) so the language server is engaged,
/// then awaits the LSP query. Returns an empty list when no language
/// server provides definitions for the file (not an error).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct GotoDefinitionParams {
    pub solution_id: String,
    /// Absolute path of the file. Must lie under one of the Solution's
    /// worktrees.
    pub path: String,
    /// Zero-based line index.
    pub line: u32,
    /// Zero-based UTF-8 byte column.
    pub col: u32,
}

impl<'de> Deserialize<'de> for GotoDefinitionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            line: u32,
            col: u32,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            line: inner.line,
            col: inner.col,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LocationRef {
    /// Absolute path of the target file (the buffer's `abs_path`). Empty
    /// when the target buffer has no on-disk file (e.g. a scratch
    /// buffer).
    pub path: String,
    pub start: EditPoint,
    pub end: EditPoint,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GotoDefinitionResult {
    pub definitions: Vec<LocationRef>,
}

#[derive(Clone)]
pub struct GotoDefinitionTool;

impl McpServerTool for GotoDefinitionTool {
    type Input = GotoDefinitionParams;
    type Output = GotoDefinitionResult;
    const NAME: &'static str = "project.goto_definition";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(!input.path.is_empty(), "invalid_params: path is required");

        cx.update(|cx| validate_path_in_solution(&input.solution_id, &input.path, cx))
            .map_err(|err| anyhow::anyhow!("{err}"))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project_path = cx.update(|cx| resolve_project_path(&project, &input.path, cx))?;

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        let position = language::Point::new(input.line, input.col);
        let task = project.update(cx, |project, cx| project.definitions(&buffer, position, cx));

        let definitions = match task.await {
            Ok(Some(links)) => cx.update(|cx| location_links_to_refs(&links, cx)),
            Ok(None) | Err(_) => Vec::new(),
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} definition(s)", definitions.len()),
            }],
            structured_content: GotoDefinitionResult { definitions },
        })
    }
}

fn location_links_to_refs(links: &[project::LocationLink], cx: &App) -> Vec<LocationRef> {
    links
        .iter()
        .map(|link| location_to_ref(&link.target, cx))
        .collect()
}

fn locations_to_refs(locations: &[language::Location], cx: &App) -> Vec<LocationRef> {
    locations
        .iter()
        .map(|location| location_to_ref(location, cx))
        .collect()
}

fn location_to_ref(location: &language::Location, cx: &App) -> LocationRef {
    use language::ToPoint as _;

    let buffer = location.buffer.read(cx);
    let path = project::File::from_dyn(buffer.file())
        .map(|file| {
            <project::File as language::LocalFile>::abs_path(file, cx)
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_default();
    let snapshot = buffer.snapshot();
    let start_point = location.range.start.to_point(&snapshot);
    let end_point = location.range.end.to_point(&snapshot);
    LocationRef {
        path,
        start: EditPoint {
            line: start_point.row,
            col: start_point.column,
        },
        end: EditPoint {
            line: end_point.row,
            col: end_point.column,
        },
    }
}

// =====================================================================
// project.find_references
// =====================================================================

/// Resolve LSP "find references" for a position in a file. Opens the
/// buffer (without surfacing a tab) so the language server is engaged.
/// `include_declaration` is forwarded to the language server's
/// preference where applicable; v1 simply returns whatever set the
/// server reports. Returns an empty list when no language server
/// provides references for the file (not an error).
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FindReferencesParams {
    pub solution_id: String,
    /// Absolute path of the file. Must lie under one of the Solution's
    /// worktrees.
    pub path: String,
    pub line: u32,
    pub col: u32,
    /// Reserved for forwarding to LSP `includeDeclaration`. Currently
    /// the editor's `Project::references` does not expose this knob, so
    /// the parameter is accepted but ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_declaration: Option<bool>,
}

impl<'de> Deserialize<'de> for FindReferencesParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: String,
            path: String,
            line: u32,
            col: u32,
            include_declaration: Option<bool>,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            path: inner.path,
            line: inner.line,
            col: inner.col,
            include_declaration: inner.include_declaration,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FindReferencesResult {
    pub references: Vec<LocationRef>,
}

#[derive(Clone)]
pub struct FindReferencesTool;

impl McpServerTool for FindReferencesTool {
    type Input = FindReferencesParams;
    type Output = FindReferencesResult;
    const NAME: &'static str = "project.find_references";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            !input.solution_id.is_empty(),
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(!input.path.is_empty(), "invalid_params: path is required");

        cx.update(|cx| validate_path_in_solution(&input.solution_id, &input.path, cx))
            .map_err(|err| anyhow::anyhow!("{err}"))?;

        let project = cx
            .update(|cx| project_for_solution(&input.solution_id, cx))
            .ok_or_else(|| anyhow::anyhow!("solution_not_open: {}", input.solution_id))?;

        let project_path = cx.update(|cx| resolve_project_path(&project, &input.path, cx))?;

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))
            .await?;

        let position = language::Point::new(input.line, input.col);
        let task = project.update(cx, |project, cx| project.references(&buffer, position, cx));

        let references = match task.await {
            Ok(Some(locations)) => cx.update(|cx| locations_to_refs(&locations, cx)),
            Ok(None) | Err(_) => Vec::new(),
        };

        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("{} reference(s)", references.len()),
            }],
            structured_content: FindReferencesResult { references },
        })
    }
}
