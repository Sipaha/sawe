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
//! in-editor preview modal — the same one images and full tool arguments use, so reading
//! a report the agent just wrote does not disturb the editor's tabs.

use std::io::Read as _;
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
        LinkTarget::File(path) => {
            crate::preview_window::open_preview(preview_content(&path, url), window, cx)
        }
        LinkTarget::Dead => {}
    }
}

/// Follow a document link in the same workspace modal; opening defers past the click update.
pub(crate) fn open_link_within_preview(
    url: &str,
    roots: &[PathBuf],
    window: &mut Window,
    cx: &mut App,
) {
    open_link(url, roots, window, cx);
}

/// `label` is the link as it was written, which is what the user recognises —
/// `docs/audit.md` rather than the absolute path it resolved to.
fn preview_content(path: &Path, label: &str) -> crate::preview_window::PreviewContent {
    let file_info = || crate::preview_window::PreviewContent::FileInfo {
        title: label.to_string().into(),
        path: path.to_path_buf(),
        details: file_details(path).into(),
    };
    // Formats that can begin with ASCII (PDF, archives, media, executables) are not text documents.
    if image_format(path).is_some() || is_binary_extension(path) {
        return file_info();
    }
    let body = match read_text_preview(path) {
        Ok(Some(body)) => body,
        Ok(None) => return file_info(),
        Err(err) => {
            return crate::preview_window::PreviewContent::Text {
                title: label.to_string().into(),
                body: format!("Could not read {}:\n\n{err:#}", path.display()).into(),
            };
        }
    };
    let title = SharedString::from(label.to_string());
    if is_markdown(path) {
        crate::preview_window::PreviewContent::Markdown {
            title,
            source: SharedString::from(body),
            base_dir: path.parent().map(Path::to_path_buf),
        }
    } else {
        crate::preview_window::PreviewContent::Text {
            title,
            body: SharedString::from(body),
        }
    }
}

fn is_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "pdf"
                    | "zip"
                    | "gz"
                    | "bz2"
                    | "xz"
                    | "7z"
                    | "rar"
                    | "tar"
                    | "zst"
                    | "exe"
                    | "dll"
                    | "so"
                    | "dylib"
                    | "o"
                    | "a"
                    | "wasm"
                    | "bin"
                    | "db"
                    | "sqlite"
                    | "sqlite3"
                    | "mp3"
                    | "wav"
                    | "flac"
                    | "ogg"
                    | "mp4"
                    | "mkv"
                    | "mov"
                    | "avi"
                    | "doc"
                    | "docx"
                    | "xls"
                    | "xlsx"
                    | "ppt"
                    | "pptx"
                    | "woff"
                    | "woff2"
                    | "ttf"
                    | "otf"
            )
        })
}

fn read_text_preview(path: &Path) -> anyhow::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut bytes = Vec::new();
    file.take((MAX_PREVIEW_BYTES + 4) as u64)
        .read_to_end(&mut bytes)?;
    // A clipped UTF-8 character is allowed only at the bounded sample's end.
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(err) if err.error_len().is_none() && size > bytes.len() as u64 => {
            std::str::from_utf8(&bytes[..err.valid_up_to()])?
        }
        Err(_) => return Ok(None),
    };
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}'))
    {
        return Ok(None);
    }
    if size <= MAX_PREVIEW_BYTES as u64 {
        return Ok(Some(text.to_owned()));
    }
    let mut end = MAX_PREVIEW_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(Some(format!(
        "{}\n\n… clipped: showing {end} of {size} bytes.",
        &text[..end]
    )))
}

fn file_details(path: &Path) -> String {
    let mut lines = vec![
        format!(
            "Name: {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        format!("Path: {}", path.display()),
    ];
    if let Ok(target) = std::fs::read_link(path) {
        lines.push(format!("Link target: {}", target.display()));
    }
    if let Ok(resolved) = path.canonicalize() {
        if resolved != path {
            lines.push(format!("Resolved path: {}", resolved.display()));
        }
    }
    match std::fs::metadata(path) {
        Ok(meta) => {
            lines.push(format!(
                "Type: {}",
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| format!(".{ext} file"))
                    .unwrap_or_else(|| "File".into())
            ));
            lines.push(format!("Size: {} bytes", meta.len()));
            for (label, time) in [
                ("Modified", meta.modified()),
                ("Created", meta.created()),
                ("Accessed", meta.accessed()),
            ] {
                let value = time
                    .map(|time| {
                        chrono::DateTime::<chrono::Local>::from(time)
                            .format("%Y-%m-%d %H:%M:%S %:z")
                            .to_string()
                    })
                    .unwrap_or_else(|_| "Unavailable".into());
                lines.push(format!("{label}: {value}"));
            }
            lines.push(format!("Read-only: {}", meta.permissions().readonly()));
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                lines.push(format!("Permissions: {:o}", meta.mode() & 0o7777));
                lines.push(format!(
                    "Owner UID: {}\nGroup GID: {}\nInode: {}\nHard links: {}",
                    meta.uid(),
                    meta.gid(),
                    meta.ino(),
                    meta.nlink()
                ));
            }
        }
        Err(err) => lines.push(format!("Metadata unavailable: {err}")),
    }
    lines.join("\n")
}

/// The image format a linked file is shown as, by extension. A screenshot the
/// agent links used to open as its raw bytes, a screen of mojibake.
fn image_format(path: &Path) -> Option<gpui::ImageFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => gpui::ImageFormat::Png,
        "jpg" | "jpeg" => gpui::ImageFormat::Jpeg,
        "webp" => gpui::ImageFormat::Webp,
        "gif" => gpui::ImageFormat::Gif,
        "svg" => gpui::ImageFormat::Svg,
        "bmp" => gpui::ImageFormat::Bmp,
        "tif" | "tiff" => gpui::ImageFormat::Tiff,
        "ico" => gpui::ImageFormat::Ico,
        "pbm" | "pgm" | "ppm" | "pnm" => gpui::ImageFormat::Pnm,
        _ => return None,
    })
}

/// Extensions the preview renders instead of showing as source.
///
/// Extension-only on purpose: the alternative is sniffing the bytes, and the
/// documents this exists for (a report the agent just wrote, a plan doc) are
/// indistinguishable from prose with the odd `#` in it. A wrong guess on a
/// non-markdown file would swallow its formatting; a wrong guess here can only
/// come from a misnamed file.
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown")
        })
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "отчёт\nline two\n").unwrap();
        let body = read_text_preview(&path).unwrap().unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.txt");
        std::fs::write(&path, text).unwrap();
        let body = read_text_preview(&path).unwrap().unwrap();
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

    #[test]
    fn non_text_file_links_show_metadata_while_text_stays_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let png = dir.path().join("shot.PNG");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").expect("write png");
        let log = dir.path().join("run.log");
        std::fs::write(&log, "plain text").expect("write log");

        match preview_content(&png, "shot.PNG") {
            crate::preview_window::PreviewContent::FileInfo { path, details, .. } => {
                assert_eq!(path, png);
                assert!(details.contains("Size: 8 bytes"));
            }
            _ => panic!("a non-text link must show file information"),
        }
        assert!(matches!(
            preview_content(&log, "run.log"),
            crate::preview_window::PreviewContent::Text { .. }
        ));
    }

    #[test]
    fn image_formats_are_recognised_by_extension() {
        assert_eq!(
            image_format(Path::new("a/b.jpg")),
            Some(gpui::ImageFormat::Jpeg)
        );
        assert_eq!(
            image_format(Path::new("b.JPEG")),
            Some(gpui::ImageFormat::Jpeg)
        );
        assert_eq!(
            image_format(Path::new("c.webp")),
            Some(gpui::ImageFormat::Webp)
        );
        assert_eq!(image_format(Path::new("d.md")), None);
        assert_eq!(image_format(Path::new("png")), None);
    }
    #[test]
    fn binary_detection_and_large_text_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("misnamed.txt");
        std::fs::write(&binary, b"header\0payload").unwrap();
        assert!(matches!(
            preview_content(&binary, "misnamed.txt"),
            crate::preview_window::PreviewContent::FileInfo { .. }
        ));
        let invalid = dir.path().join("unknown");
        std::fs::write(&invalid, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(matches!(
            preview_content(&invalid, "unknown"),
            crate::preview_window::PreviewContent::FileInfo { .. }
        ));
        let pdf = dir.path().join("document.pdf");
        std::fs::write(&pdf, b"%PDF-1.7 ASCII content").unwrap();
        assert!(matches!(
            preview_content(&pdf, "document.pdf"),
            crate::preview_window::PreviewContent::FileInfo { .. }
        ));
        let large = dir.path().join("large.log");
        let f = std::fs::File::create(&large).unwrap();
        f.set_len(200 * 1024 * 1024).unwrap();
        assert!(
            read_text_preview(&large).unwrap().is_none(),
            "sparse binary must not be loaded as text"
        );
        std::fs::write(&large, "я".repeat(MAX_PREVIEW_BYTES)).unwrap();
        let text = read_text_preview(&large).unwrap().unwrap();
        assert!(text.len() < MAX_PREVIEW_BYTES + 100);
        assert!(text.contains("1048576 bytes"));
        assert!(!text.contains('�'));
    }
}
