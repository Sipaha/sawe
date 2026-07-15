use gpui::{
    actions, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, Styled, Window,
};
use project::{Project, search::SearchQuery};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use ui::prelude::*;
use util::paths::PathMatcher;
use workspace::{ModalView, Workspace};

use crate::SearchOptions;

#[cfg(test)]
#[path = "find_in_path_tests.rs"]
mod find_in_path_tests;

/// Opens the Find in Path modal (project-wide search overlay).
#[derive(Clone, PartialEq, Debug, Deserialize, JsonSchema, Default, gpui::Action)]
#[action(namespace = find_in_path)]
#[serde(deny_unknown_fields)]
pub struct Toggle {
    #[serde(default)]
    pub replace_enabled: bool,
}

actions!(
    find_in_path,
    [
        /// Opens the Find in Path modal with the replace field revealed.
        ToggleReplace
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(
    workspace: &mut Workspace,
    _window: Option<&mut Window>,
    _cx: &mut Context<Workspace>,
) {
    workspace.register_action(|workspace, action: &Toggle, window, cx| {
        FindInPath::toggle(workspace, action.replace_enabled, window, cx);
    });
    workspace.register_action(|workspace, _: &ToggleReplace, window, cx| {
        FindInPath::toggle(workspace, true, window, cx);
    });
}

/// What part of the Solution a Find in Path search is restricted to.
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    /// No restriction — search every visible worktree.
    Solution,
    /// Restrict to the active member's worktree (`member_root`, resolved by the caller).
    Project,
    /// Restrict to one directory (and everything below it).
    Directory(PathBuf),
}

/// Resolve the active member's root path for the Solution that owns `workspace`'s project, if any.
///
/// Unused until the modal is constructed with a `&Workspace` (Task 4), which stores the result on
/// `FindInPath` for `Scope::Project` to consume via `include_patterns_for_scope`'s `member_root`.
#[allow(dead_code)]
fn active_member_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let store = solutions::SolutionStore::global(cx);
    let project = workspace.project().read(cx);
    let first_root = project.visible_worktrees(cx).next()?.read(cx).abs_path();
    let solution = store.read(cx).solution_for_path(&first_root)?;
    store.read(cx).active_member_path(solution.id)
}

/// Build include globs restricting a search to `scope`. Empty ⇒ whole Solution.
///
/// A Solution is one `project::Project` with each member mounted as a separate
/// worktree, so `In Project` / `Directory` restrictions are expressed as
/// worktree-relative (or, when the project has multiple visible worktrees,
/// root-name-prefixed) recursive globs rather than as a different project.
fn include_patterns_for_scope(
    scope: &Scope,
    member_root: Option<&Path>,
    project: &Entity<Project>,
    cx: &App,
) -> Vec<String> {
    let project = project.read(cx);
    let match_full_paths = project.visible_worktrees(cx).count() > 1;
    let root_glob = |abs: &Path| -> Option<String> {
        for worktree in project.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let worktree_abs_path = worktree.abs_path();
            let Ok(relative) = abs.strip_prefix(&*worktree_abs_path) else {
                continue;
            };
            let mut glob = if match_full_paths {
                format!("{}/", worktree.root_name_str())
            } else {
                String::new()
            };
            if relative.as_os_str().is_empty() {
                glob.push_str("**");
            } else {
                glob.push_str(&relative.to_string_lossy());
                glob.push_str("/**");
            }
            return Some(glob);
        }
        None
    };

    match scope {
        Scope::Solution => Vec::new(),
        Scope::Project => {
            let owned_root = match member_root {
                Some(root) => Some(root.to_path_buf()),
                None => project
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| worktree.read(cx).abs_path().to_path_buf()),
            };
            owned_root.as_deref().and_then(root_glob).into_iter().collect()
        }
        Scope::Directory(dir) => root_glob(dir).into_iter().collect(),
    }
}

/// Split a comma-separated glob list into individual pattern strings, respecting `{...}` brace groups.
///
/// Copied from `project_search::split_glob_patterns` (kept private to each module — sharing it
/// would mean threading a new public export through `project_search` for one helper function).
fn split_glob_patterns(text: &str) -> Vec<&str> {
    let mut patterns = Vec::new();
    let mut pattern_start = 0;
    let mut brace_depth: usize = 0;
    let mut escaped = false;

    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ',' if brace_depth == 0 => {
                patterns.push(&text[pattern_start..index]);
                pattern_start = index + 1;
            }
            _ => {}
        }
    }
    patterns.push(&text[pattern_start..]);
    patterns
}

fn parse_glob_patterns(text: &str) -> Vec<String> {
    split_glob_patterns(text)
        .into_iter()
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Build a `SearchQuery` from raw editor text plus a `Scope` restriction.
///
/// `scope`'s include patterns are merged in front of `include_text`'s user-typed patterns so an
/// empty `Scope::Solution` leaves the user's own include filter untouched. Returns `None` when the
/// query text is empty or when either glob list fails to parse.
fn build_query(
    query_text: &str,
    options: SearchOptions,
    include_text: &str,
    exclude_text: &str,
    scope: &Scope,
    member_root: Option<&Path>,
    project: &Entity<Project>,
    cx: &App,
) -> Option<SearchQuery> {
    if query_text.is_empty() {
        return None;
    }

    let path_style = project.read(cx).path_style(cx);

    let mut include_patterns = include_patterns_for_scope(scope, member_root, project, cx);
    include_patterns.extend(parse_glob_patterns(include_text));
    let included_files = PathMatcher::new(&include_patterns, path_style).ok()?;

    let exclude_patterns = parse_glob_patterns(exclude_text);
    let excluded_files = PathMatcher::new(&exclude_patterns, path_style).ok()?;

    let match_full_paths = project.read(cx).visible_worktrees(cx).count() > 1;

    let query = if options.contains(SearchOptions::REGEX) {
        SearchQuery::regex(
            query_text,
            options.contains(SearchOptions::WHOLE_WORD),
            options.contains(SearchOptions::CASE_SENSITIVE),
            options.contains(SearchOptions::INCLUDE_IGNORED),
            options.contains(SearchOptions::ONE_MATCH_PER_LINE),
            included_files,
            excluded_files,
            match_full_paths,
            None,
        )
        .ok()?
    } else {
        SearchQuery::text(
            query_text,
            options.contains(SearchOptions::WHOLE_WORD),
            options.contains(SearchOptions::CASE_SENSITIVE),
            options.contains(SearchOptions::INCLUDE_IGNORED),
            included_files,
            excluded_files,
            match_full_paths,
            None,
        )
        .ok()?
    };

    if query.is_empty() { None } else { Some(query) }
}

pub struct FindInPath {
    focus_handle: FocusHandle,
    replace_enabled: bool,
}

impl FindInPath {
    fn toggle(
        workspace: &mut Workspace,
        replace_enabled: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if let Some(existing) = workspace.active_modal::<Self>(cx) {
            existing.update(cx, |this, cx| {
                this.replace_enabled |= replace_enabled;
                this.focus_handle.focus(window, cx);
                cx.notify();
            });
            return;
        }
        workspace.toggle_modal(window, cx, |_window, cx| Self {
            focus_handle: cx.focus_handle(),
            replace_enabled,
        });
    }
}

impl Focusable for FindInPath {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FindInPath {}

impl ModalView for FindInPath {
    fn fade_out_background(&self) -> bool {
        true
    }

    fn debug_kind(&self) -> &'static str {
        "FindInPath"
    }
}

impl Render for FindInPath {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Placeholder shell — replaced in Task 4 with the real header/results/preview.
        v_flex()
            .key_context("FindInPath")
            .track_focus(&self.focus_handle)
            .w(rems(60.))
            .h(rems(30.))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_lg()
            .child("Find in Path")
    }
}
