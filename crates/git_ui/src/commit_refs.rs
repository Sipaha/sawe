//! The ref-decoration chips a commit carries — `main`, `origin/main`,
//! `HEAD -> feature/x`, `tag: 2.41.0` — as git reports them in a `%D`
//! decoration.
//!
//! Two surfaces paint them and they have to agree, because the user reads
//! them side by side: the git graph's Description column, and the git panel's
//! Commit tab, which describes whichever row the graph has selected. The chip
//! itself therefore lives here rather than in either one. `git_graph` depends
//! on `git_ui` and never the reverse, so `git_ui` is the only place both can
//! reach.
//!
//! There is deliberately no per-ref-kind icon or colour: git's own decoration
//! text is what distinguishes the three kinds — a bare name is a local branch,
//! `<remote>/<name>` is a remote-tracking one, `tag: <name>` is a tag — and
//! inventing a second, parallel encoding would only give the two surfaces
//! something new to disagree about. The one glyph a chip can carry says
//! something git's text does not: a check for the checked-out branch, a lock
//! for a branch this Solution's policy protects.

use std::path::Path;

use gpui::{Hsla, SharedString};
use ui::{Chip, LabelSize, Tooltip, prelude::*};

/// Strip the prefixes git can emit in `%D` decorations
/// (`HEAD -> `, `tag: `, `refs/heads/`, `refs/remotes/<remote>/`) so
/// the bare branch name is what reaches downstream callers — matters
/// for branch-protection glob matching, where `release/*` should
/// match the branch `release/v1` regardless of upstream-tracking
/// shape.
pub fn strip_ref_namespace(name: &str) -> &str {
    let s = name.trim();
    if let Some(after) = s.strip_prefix("HEAD -> ") {
        return strip_ref_namespace(after);
    }
    if let Some(after) = s.strip_prefix("tag: ") {
        return after;
    }
    if let Some(after) = s.strip_prefix("refs/heads/") {
        return after;
    }
    if let Some(after) = s.strip_prefix("refs/remotes/") {
        // refs/remotes/<remote>/<branch> — drop the remote segment so
        // the policy match is on the branch portion alone.
        if let Some((_remote, rest)) = after.split_once('/') {
            return rest;
        }
        return after;
    }
    s
}

/// Whether a `%D` decoration names the currently checked-out branch.
///
/// Git writes the checked-out branch as `HEAD -> <name>` when it decorates the
/// commit HEAD is on, so that prefix alone settles it; the bare comparison
/// covers a caller whose decorations were produced without the arrow.
pub fn is_head_ref(name: &str, head_branch_name: Option<&SharedString>) -> bool {
    if name.starts_with("HEAD -> ") {
        return true;
    }
    head_branch_name.is_some_and(|head| name == head.as_ref())
}

/// The tag names among a commit's `%D` decorations, with git's `tag: ` prefix
/// removed — i.e. the tags that *point at* the commit, in git's order.
pub fn tag_names(ref_names: &[SharedString]) -> impl Iterator<Item = &str> {
    ref_names
        .iter()
        .filter_map(|name| name.as_ref().strip_prefix("tag: "))
        .filter(|name| !name.is_empty())
}

/// One decoration's chip.
///
/// `truncate` belongs to the caller, not the chip. [`Chip::truncate`] sets
/// `min_w_0`, which is right in a single-line row that must give a long ref
/// back to the text beside it, and wrong in a row that wraps: there every chip
/// would collapse to a bare ellipsis.
pub fn ref_chip(
    name: &SharedString,
    accent_color: Hsla,
    is_head: bool,
    work_dir: Option<&Path>,
    truncate: bool,
) -> Chip {
    // S-SOL-PRT — render protected refs with a lock glyph in place of the
    // standard chip icon. The ref-namespace prefixes git emits in `%D` are
    // stripped first so the policy's globs match the bare branch name.
    let bare = strip_ref_namespace(name.as_ref());
    let is_protected = work_dir.is_some_and(|work_dir| {
        matches!(
            solutions::branch_protection::check(work_dir, bare, "delete_branch"),
            solutions::branch_protection::Decision::Forbidden { .. }
        )
    });
    Chip::new(name.clone())
        .label_size(LabelSize::Small)
        .map(|chip| if truncate { chip.truncate() } else { chip })
        .map(|chip| {
            if is_head {
                chip.icon(IconName::Check)
                    .bg_color(accent_color.opacity(0.25))
                    .border_color(accent_color.opacity(0.5))
            } else if is_protected {
                chip.icon(IconName::LockOutlined)
                    .bg_color(accent_color.opacity(0.12))
                    .border_color(accent_color.opacity(0.5))
            } else {
                chip.bg_color(accent_color.opacity(0.08))
                    .border_color(accent_color.opacity(0.25))
            }
        })
}

/// The `+N` chip that stands in for the decorations a row had no width for.
///
/// The hidden names are on its tooltip rather than dropped: a count on its own
/// tells the user something is missing and gives them no way to see it.
pub fn overflow_chip(hidden: &[SharedString], accent_color: Hsla) -> Chip {
    let names = hidden
        .iter()
        .map(|name| name.as_ref())
        .collect::<Vec<_>>()
        .join(", ");
    Chip::new(format!("+{}", hidden.len()))
        .label_size(LabelSize::Small)
        .bg_color(accent_color.opacity(0.08))
        .border_color(accent_color.opacity(0.25))
        .tooltip(Tooltip::text(SharedString::from(names)))
}

/// The graph lane colour at `accent_idx`, falling back to the theme's first
/// accent so a commit whose lane index outruns a short accent list still gets
/// a chip that reads as a chip.
pub fn accent_color(accent_idx: usize, cx: &App) -> Hsla {
    let accents = cx.theme().accents();
    accents
        .0
        .get(accent_idx)
        .copied()
        .unwrap_or_else(|| accents.0.first().copied().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ref_namespace() {
        assert_eq!(strip_ref_namespace("main"), "main");
        assert_eq!(strip_ref_namespace("HEAD -> release/v1"), "release/v1");
        assert_eq!(strip_ref_namespace("tag: 2.41.0"), "2.41.0");
        assert_eq!(strip_ref_namespace("refs/heads/release/v1"), "release/v1");
        assert_eq!(
            strip_ref_namespace("refs/remotes/origin/release/v1"),
            "release/v1"
        );
    }

    #[test]
    fn test_is_head_ref() {
        let head = SharedString::from("main");
        assert!(is_head_ref("HEAD -> main", None));
        assert!(is_head_ref("main", Some(&head)));
        assert!(
            !is_head_ref("origin/main", Some(&head)),
            "a remote-tracking ref that happens to end in the head branch's \
             name is not the checked-out branch"
        );
    }

    #[test]
    fn test_tag_names_are_the_decorations_git_prefixed_with_tag() {
        let refs: Vec<SharedString> = vec![
            "HEAD -> main".into(),
            "tag: 2.41.0".into(),
            "origin/main".into(),
            "tag: ".into(),
        ];
        assert_eq!(
            tag_names(&refs).collect::<Vec<_>>(),
            vec!["2.41.0"],
            "only `tag: ` decorations are tags, and an empty one names nothing"
        );
    }
}
