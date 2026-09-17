//! Where a link in the conversation goes when it is clicked.
//!
//! Links in the agent's own text used to go nowhere at all: the hook was wired
//! on the user-message renderer only, so `[Подробный отчёт](docs/audit.md)` in
//! an answer rendered underlined and in the accent colour and did nothing —
//! the dead-control trap this fork bans elsewhere.
//!
//! Two kinds of target, and the difference matters. A link with a scheme of its
//! own is the system browser's business. A **path** is not: the agent writes
//! them relative to the project it is working in, and `cx.open_url` on
//! `docs/audit.md` does nothing useful. Those open in the conversation's shared
//! preview window — the same one images and full tool arguments use, so reading
//! a report the agent just wrote does not disturb the editor's tabs.

use std::path::{Path, PathBuf};

use gpui::{App, SharedString, WeakEntity, Window};
use workspace::Workspace;

/// A file bigger than this is shown clipped. The preview window holds the whole
/// body in memory and in a read-only editor; a link to a 200 MB log should not
/// freeze the conversation it was clicked from.
const MAX_PREVIEW_BYTES: usize = 512 * 1024;

/// What a markdown URL in the conversation means.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    /// Carries a scheme of its own — hand it to the system browser.
    External(String),
    /// A path that resolved to a file that is actually there.
    File(PathBuf),
    /// Nothing sensible to do: an in-document anchor, or a path that does not
    /// exist under any of the roots. Silence beats opening the wrong thing.
    Dead,
}

/// Decide what `url` means, given the directories a relative path could be
/// written against.
///
/// `exists` is injected so the decision is testable without a filesystem, and
/// so the caller decides what "is there" means (a directory is not a preview).
pub(crate) fn resolve_link(
    url: &str,
    roots: &[PathBuf],
    exists: &dyn Fn(&Path) -> bool,
) -> LinkTarget {
    let url = url.trim();
    if url.is_empty() {
        return LinkTarget::Dead;
    }
    // An in-document anchor. We render no heading ids to jump to, so this is
    // honestly dead rather than dead by omission.
    if url.starts_with('#') {
        return LinkTarget::Dead;
    }

    // `file://` is a path wearing a scheme. Everything else with a scheme —
    // `https:`, `mailto:`, `ssh:` — belongs to the browser.
    let path_part = if let Some(rest) = strip_file_scheme(url) {
        rest.to_string()
    } else if has_scheme(url) {
        return LinkTarget::External(url.to_string());
    } else {
        url.to_string()
    };

    // `docs/audit.md#findings` is a link to the file; the fragment is for a
    // renderer that has anchors, which the preview window does not.
    let path_part = path_part
        .split_once('#')
        .map_or(path_part.as_str(), |(before, _)| before);
    if path_part.is_empty() {
        return LinkTarget::Dead;
    }

    let candidate = Path::new(path_part);
    if candidate.is_absolute() {
        return if exists(candidate) {
            LinkTarget::File(candidate.to_path_buf())
        } else {
            LinkTarget::Dead
        };
    }

    // Relative: the agent wrote it against the directory it was working in,
    // which we cannot know for certain, so try every root the project has and
    // take the first that actually holds the file. In a Solution with several
    // members this is what makes `docs/audit.md` resolve to the member that
    // has it rather than to the first one listed.
    for root in roots {
        let joined = root.join(candidate);
        if exists(&joined) {
            return LinkTarget::File(joined);
        }
    }
    LinkTarget::Dead
}

/// `file:///a/b` and `file://localhost/a/b` both mean `/a/b`.
fn strip_file_scheme(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("file://")?;
    Some(rest.strip_prefix("localhost").unwrap_or(rest))
}

/// A scheme per RFC 3986: a letter followed by letters, digits, `+`, `-`, `.`,
/// then a colon. Checked before any path handling so a Windows drive letter
/// (`C:\…`, one character) is not mistaken for one.
fn has_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    scheme.len() > 1
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The body to show for `path`, and whether it had to be clipped.
///
/// Lossy on purpose: a link into a file with one invalid byte in it should
/// still show the other 99 %, the same way the editor opens it.
pub(crate) fn preview_body(bytes: Vec<u8>) -> String {
    if bytes.len() <= MAX_PREVIEW_BYTES {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    // Cut on a character boundary — `from_utf8_lossy` would otherwise render
    // the split multi-byte character as a replacement glyph.
    let mut end = MAX_PREVIEW_BYTES;
    while end > 0 && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    let shown = String::from_utf8_lossy(&bytes[..end]);
    format!(
        "{shown}\n\n… clipped: showing {} of {} bytes.",
        end,
        bytes.len()
    )
}

/// The directories a relative link is resolved against: the worktrees of the
/// project this conversation is shown in.
///
/// Deliberately the **workspace's** project and not the `AcpThread`'s. A
/// thread exists only while an agent is connected, and a conversation reopened
/// from the database has none — which is exactly when a user scrolls back to a
/// report the agent wrote last week and clicks the link to it. Resolving
/// through the thread made every link in a cold session dead, measured in a
/// seeded cold session before this was written: `roots=[] -> Dead`.
pub(crate) fn project_roots(workspace: &WeakEntity<Workspace>, cx: &App) -> Vec<PathBuf> {
    let Some(workspace) = workspace.upgrade() else {
        return Vec::new();
    };
    workspace
        .read(cx)
        .project()
        .read(cx)
        .worktrees(cx)
        .map(|tree| tree.read(cx).abs_path().to_path_buf())
        .collect()
}

/// Act on a click on `url`.
pub(crate) fn open_link(url: &str, roots: &[PathBuf], window: &mut Window, cx: &mut App) {
    match resolve_link(url, roots, &|path| path.is_file()) {
        LinkTarget::External(url) => cx.open_url(&url),
        LinkTarget::File(path) => open_file_preview(&path, url, window, cx),
        LinkTarget::Dead => {}
    }
}

/// `label` is the link as it was written, which is what the user recognises —
/// `docs/audit.md` rather than the absolute path it resolved to.
fn open_file_preview(path: &Path, label: &str, window: &mut Window, cx: &mut App) {
    let body = match std::fs::read(path) {
        Ok(bytes) => preview_body(bytes),
        Err(err) => {
            // It existed a microsecond ago; something is wrong with it rather
            // than missing. Say so in the window instead of doing nothing,
            // which would look exactly like the dead link we just fixed.
            log::warn!("failed to read {} for preview: {err:#}", path.display());
            format!("Could not read {}:\n\n{err:#}", path.display())
        }
    };
    crate::preview_window::open_preview(
        crate::preview_window::PreviewContent::Text {
            title: SharedString::from(label.to_string()),
            body: SharedString::from(body),
        },
        window,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn roots(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn present(paths: &[&str]) -> impl Fn(&Path) -> bool + use<> {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |path: &Path| set.contains(path)
    }

    #[test]
    fn a_url_with_a_scheme_goes_to_the_browser() {
        for url in [
            "https://example.com/x",
            "http://example.com",
            "mailto:someone@example.com",
        ] {
            assert_eq!(
                resolve_link(url, &roots(&["/p"]), &present(&[])),
                LinkTarget::External(url.to_string()),
                "{url}"
            );
        }
    }

    #[test]
    fn a_relative_path_resolves_against_the_first_root_that_has_it() {
        let target = resolve_link(
            "docs/audit.md",
            &roots(&["/a", "/b"]),
            &present(&["/b/docs/audit.md"]),
        );
        assert_eq!(target, LinkTarget::File(PathBuf::from("/b/docs/audit.md")));
    }

    #[test]
    fn a_relative_path_that_is_nowhere_is_dead_rather_than_guessed() {
        assert_eq!(
            resolve_link("docs/audit.md", &roots(&["/a", "/b"]), &present(&[])),
            LinkTarget::Dead
        );
    }

    #[test]
    fn an_absolute_path_does_not_consult_the_roots() {
        assert_eq!(
            resolve_link("/elsewhere/x.md", &roots(&["/a"]), &present(&["/a/x.md"])),
            LinkTarget::Dead
        );
        assert_eq!(
            resolve_link(
                "/elsewhere/x.md",
                &roots(&["/a"]),
                &present(&["/elsewhere/x.md"])
            ),
            LinkTarget::File(PathBuf::from("/elsewhere/x.md"))
        );
    }

    #[test]
    fn file_scheme_is_a_path_not_a_browser_url() {
        assert_eq!(
            resolve_link("file:///a/x.md", &roots(&[]), &present(&["/a/x.md"])),
            LinkTarget::File(PathBuf::from("/a/x.md"))
        );
        assert_eq!(
            resolve_link(
                "file://localhost/a/x.md",
                &roots(&[]),
                &present(&["/a/x.md"])
            ),
            LinkTarget::File(PathBuf::from("/a/x.md"))
        );
    }

    #[test]
    fn a_fragment_is_dropped_before_the_path_is_looked_up() {
        assert_eq!(
            resolve_link(
                "docs/audit.md#findings",
                &roots(&["/a"]),
                &present(&["/a/docs/audit.md"])
            ),
            LinkTarget::File(PathBuf::from("/a/docs/audit.md"))
        );
    }

    #[test]
    fn an_in_document_anchor_is_dead() {
        assert_eq!(
            resolve_link("#findings", &roots(&["/a"]), &present(&[])),
            LinkTarget::Dead
        );
        assert_eq!(
            resolve_link("", &roots(&["/a"]), &present(&[])),
            LinkTarget::Dead
        );
    }

    #[test]
    fn a_body_within_the_cap_is_passed_through_untouched() {
        let body = preview_body("отчёт\nline two\n".as_bytes().to_vec());
        assert_eq!(body, "отчёт\nline two\n");
    }

    #[test]
    fn an_oversized_body_is_clipped_on_a_character_boundary() {
        // Three bytes per character, and the cap is not a multiple of three, so
        // the cut lands INSIDE a character and the walk back is what saves it.
        // A two-byte character would not do: the cap is even, so a naive cut
        // would land on a boundary by luck and the test would prove nothing.
        assert_ne!(
            MAX_PREVIEW_BYTES % 3,
            0,
            "pick a cap the cut can land inside of"
        );
        let text = "€".repeat(MAX_PREVIEW_BYTES);
        let body = preview_body(text.into_bytes());
        assert!(
            !body.contains('\u{FFFD}'),
            "clipping split a character and produced a replacement glyph"
        );
        assert!(body.contains("… clipped: showing"));
        assert!(
            body.len() < MAX_PREVIEW_BYTES + 200,
            "the clipped body should be about the cap, not the whole input"
        );
    }
}
