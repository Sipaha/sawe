//! The building blocks of a *commit's* detail surface: the changed-files tree,
//! the commit-message split, the markdown identity line and the client-side
//! +/− totals.
//!
//! These were written for the git graph's inline detail sidebar and lived in
//! `git_graph.rs`; they are relocated here verbatim so the git panel's Commit
//! tab can host the same surface. `git_graph` depends on `git_ui` and never
//! the reverse, so down is the only direction they could move.
//!
//! The row renderers are the one thing that could not come across unchanged:
//! they were typed against `Context<GitGraph>` and are now hosted by two
//! unrelated views, so their host behaviour arrives as
//! [`ChangedFileRowHandlers`] — plain `&mut App` closures the host builds from
//! its own weak handle. Nothing about what they paint changed.

use super::*;

use git::repository::{CommitDiff, CommitFile};
use git::status::{StatusCode, TrackedStatus};
use gpui::{AnyElement, StyleRefinement, TextStyleRefinement, UnderlineStyle};
use language::line_diff;
use markdown::MarkdownStyle;
use std::rc::Rc;
use std::sync::OnceLock;
use time::{UtcOffset, format_description::BorrowedFormatItem};

/// What a changed-files row does when the user acts on it. The tree is hosted
/// by two views that share no type, so each host supplies its own behaviour as
/// `&mut App` closures built from its own weak handle rather than the rows
/// naming a concrete view.
#[derive(Clone)]
pub struct ChangedFileRowHandlers {
    /// Left click on a file row — marks it as the tree's selected file.
    pub select_file: Rc<dyn Fn(&RepoPath, &mut Window, &mut App)>,
    /// Right click on a file row, at the click position.
    pub deploy_file_context_menu: Rc<dyn Fn(&RepoPath, Point<Pixels>, &mut Window, &mut App)>,
    /// Click on a directory header row — collapses or expands the group.
    pub toggle_directory: Rc<dyn Fn(&SharedString, &mut Window, &mut App)>,
}

#[derive(Clone)]
pub struct ChangedFileEntry {
    pub status: FileStatus,
    pub file_name: SharedString,
    pub dir_path: SharedString,
    pub repo_path: RepoPath,
}

impl ChangedFileEntry {
    pub fn from_commit_file(file: &CommitFile) -> Self {
        let file_name: SharedString = file
            .path
            .file_name()
            .map(|n| n.to_string())
            .unwrap_or_default()
            .into();
        let dir_path: SharedString = file
            .path
            .parent()
            .map(|p| p.as_unix_str().to_string())
            .unwrap_or_default()
            .into();

        let status_code = match (&file.old_text, &file.new_text) {
            (None, Some(_)) => StatusCode::Added,
            (Some(_), None) => StatusCode::Deleted,
            _ => StatusCode::Modified,
        };

        let status = FileStatus::Tracked(TrackedStatus {
            index_status: status_code,
            worktree_status: StatusCode::Unmodified,
        });

        Self {
            status,
            file_name,
            dir_path,
            repo_path: file.path.clone(),
        }
    }

    fn open_file_diff(
        &self,
        commit_sha: &SharedString,
        repository: &WeakEntity<Repository>,
        workspace: &WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        CommitView::open_file_diff(
            commit_sha.to_string(),
            repository.clone(),
            workspace.clone(),
            self.repo_path.clone(),
            window,
            cx,
        );
    }

    /// Full repo-relative path, used for tooltips and the copy-path menu.
    fn display_path(&self) -> SharedString {
        if self.dir_path.is_empty() {
            self.file_name.clone()
        } else {
            format!("{}/{}", self.dir_path, self.file_name).into()
        }
    }

    /// A file row of the changed-files tree. The directory is carried by the
    /// header row above, so the row itself only shows the file name — the
    /// panel is narrow and IDEA's tree does the same.
    /// `indent` is the row's left padding: one step in from the directory
    /// header. It is passed in rather than read here because it comes from
    /// `ProjectPanelSettings`, and `project_panel` depends on `git_ui` — the
    /// host reads the setting and hands the result down.
    pub fn render(
        &self,
        ix: usize,
        indent: Pixels,
        commit_sha: SharedString,
        repository: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        handlers: ChangedFileRowHandlers,
        is_selected: bool,
    ) -> AnyElement {
        let file_name = self.file_name.clone();
        let full_path = self.display_path();

        let handlers_for_click = handlers.clone();

        div()
            .w_full()
            .pl(indent)
            .on_mouse_down(MouseButton::Right, {
                let repo_path = self.repo_path.clone();
                move |event: &MouseDownEvent, window, cx| {
                    (handlers.deploy_file_context_menu)(&repo_path, event.position, window, cx);
                    cx.stop_propagation();
                }
            })
            .child(
                ButtonLike::new(("changed-file", ix))
                    .toggle_state(is_selected)
                    .child(
                        h_flex()
                            .min_w_0()
                            .w_full()
                            .gap_1()
                            .overflow_hidden()
                            .child(git_status_icon(self.status))
                            .child(Label::new(file_name).size(LabelSize::Small).truncate()),
                    )
                    .tooltip({
                        let meta = full_path;
                        move |_, cx| Tooltip::with_meta("Open Diff", None, meta.clone(), cx)
                    })
                    // Single click only selects; the diff opens on double
                    // click, so walking the file list with the mouse does not
                    // spray tabs across the pane.
                    .on_click({
                        let entry = self.clone();
                        let handlers = handlers_for_click;
                        move |event: &ClickEvent, window, cx| {
                            (handlers.select_file)(&entry.repo_path, window, cx);
                            if event.click_count() >= 2 {
                                entry.open_file_diff(
                                    &commit_sha,
                                    &repository,
                                    &workspace,
                                    window,
                                    cx,
                                );
                            }
                        }
                    }),
            )
            .into_any_element()
    }
}

/// One row of the sidebar's changed-files tree. IDEA renders the commit's
/// files grouped under their directory rather than as a flat list of
/// `name  dir/path` pairs; a directory chain with a single child is compacted
/// into one header (`docs/plans/completed`) instead of nesting a row per path
/// component.
#[derive(Clone)]
pub enum ChangedFileRow {
    Directory {
        /// Raw directory path — the key into `collapsed_changed_dirs`. Empty
        /// for files sitting at the repository root.
        key: SharedString,
        /// What the row shows: the directory path, or the repository name for
        /// the root group.
        label: SharedString,
        file_count: usize,
        collapsed: bool,
    },
    File(ChangedFileEntry),
}

/// Flatten a commit's changed files into directory-grouped rows. Files under a
/// collapsed directory are dropped, but its header keeps the full count so the
/// user can see how much is hidden.
pub fn build_changed_file_rows(
    entries: &[ChangedFileEntry],
    root_label: &SharedString,
    collapsed: &HashSet<SharedString>,
) -> Vec<ChangedFileRow> {
    let mut by_directory: BTreeMap<SharedString, Vec<&ChangedFileEntry>> = BTreeMap::new();
    for entry in entries {
        by_directory
            .entry(entry.dir_path.clone())
            .or_default()
            .push(entry);
    }

    let mut rows = Vec::new();
    for (directory, mut files) in by_directory {
        files.sort_by(|left, right| left.file_name.cmp(&right.file_name));
        let is_collapsed = collapsed.contains(&directory);
        rows.push(ChangedFileRow::Directory {
            key: directory.clone(),
            label: if directory.is_empty() {
                root_label.clone()
            } else {
                directory
            },
            file_count: files.len(),
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            rows.extend(files.into_iter().cloned().map(ChangedFileRow::File));
        }
    }
    rows
}

/// Header row of one directory group in the changed-files tree. Clicking it
/// collapses / expands the group.
pub fn render_changed_directory_row(
    ix: usize,
    key: SharedString,
    label: SharedString,
    file_count: usize,
    collapsed: bool,
    handlers: ChangedFileRowHandlers,
) -> AnyElement {
    let tooltip_label = label.clone();
    ButtonLike::new(("changed-dir", ix))
        .child(
            h_flex()
                .min_w_0()
                .w_full()
                .gap_1()
                .overflow_hidden()
                // A plain chevron rather than a `Disclosure`: that renders as
                // an `IconButton`, and a nested button inside the row's own
                // `ButtonLike` both muddies the click target and makes the row
                // taller than a file row — `uniform_list` sizes every row from
                // the first one, so the two must match exactly.
                .child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(IconSize::Small)
                    .color(Color::Muted),
                )
                // Default (16px), matching the project panel's folder glyph and
                // the 16px status glyph on the file rows below it.
                .child(Icon::new(IconName::Folder).color(Color::Muted))
                .child(Label::new(label).size(LabelSize::Small).truncate_start())
                .child(
                    Label::new(format!(
                        "{file_count} {}",
                        if file_count == 1 { "file" } else { "files" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
        .tooltip(move |_, cx| Tooltip::simple(tooltip_label.clone(), cx))
        .on_click(move |_, window, cx| {
            (handlers.toggle_directory)(&key, window, cx);
        })
        .into_any_element()
}

/// Split a raw commit message into its subject (first line) and body. The
/// blank line git puts between them is dropped, as is trailing whitespace —
/// otherwise the sidebar renders a run of empty lines under short messages.
pub fn split_commit_message(message: &str) -> (SharedString, SharedString) {
    let mut lines = message.lines();
    let subject = lines.next().unwrap_or_default().trim_end().to_string();
    let body = lines
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches(|c: char| c == '\n' || c == '\r')
        .trim_end()
        .to_string();
    (subject.into(), body.into())
}

/// Escape the CommonMark inline markers that could turn an author name into
/// formatting. Only the markers that can *start* an inline construct are
/// escaped — over-escaping would show up verbatim in the text the user copies
/// out of the sidebar, since `markdown::Copy` copies from the source.
fn escape_markdown_inline(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// The markdown source of IDEA's identity line:
/// `550e4c28 antivanov <anton.ivanov@citeck.ru> on 14 Aug 2026 at 06:24`.
/// The email is a real `mailto:` link; the rest is plain text so the whole
/// line stays selectable as one run.
pub fn commit_identity_source(
    short_sha: &str,
    author_name: &str,
    author_email: &str,
    timestamp: Option<i64>,
) -> SharedString {
    let mut source = escape_markdown_inline(short_sha);
    if !author_name.is_empty() {
        source.push(' ');
        source.push_str(&escape_markdown_inline(author_name));
    }
    if !author_email.is_empty() {
        // The angle brackets are escaped and sit *outside* the link so the
        // parser cannot read them as an HTML tag or a nested autolink.
        source.push_str(&format!(
            " \\<[{}](mailto:{})\\>",
            escape_markdown_inline(author_email),
            author_email
        ));
    }
    if let Some(timestamp) = timestamp {
        source.push_str(" on ");
        source.push_str(&escape_markdown_inline(&format_detail_timestamp(timestamp)));
    }
    source.into()
}

/// `on <date> at <time>` reads better with the two halves separated, so the
/// detail sidebar spells the time out instead of reusing the log column's
/// compact `[day] [month] [year] [hour]:[minute]`.
fn detail_timestamp_format() -> &'static [BorrowedFormatItem<'static>] {
    static FORMAT: OnceLock<Vec<BorrowedFormatItem<'static>>> = OnceLock::new();
    FORMAT.get_or_init(|| {
        time::format_description::parse("[day] [month repr:short] [year] at [hour]:[minute]")
            .unwrap_or_default()
    })
}

fn format_detail_timestamp(timestamp: i64) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    datetime
        .to_offset(local_offset)
        .format(detail_timestamp_format())
        .unwrap_or_default()
}

/// Style for the selectable single-line text fields of the commit-details
/// sidebar, matching what the equivalent [`Label`] rendered before.
pub fn detail_text_style(
    text_size: TextSize,
    color: Color,
    weight: Option<gpui::FontWeight>,
    window: &Window,
    cx: &App,
) -> MarkdownStyle {
    let refinement = TextStyleRefinement {
        font_size: Some(text_size.rems(cx).into()),
        color: Some(color.color(cx)),
        font_weight: weight,
        ..Default::default()
    };
    let mut base_text_style = window.text_style();
    base_text_style.refine(&refinement);

    let container_style = StyleRefinement::default();

    MarkdownStyle {
        base_text_style,
        // `base_text_style` alone is NOT enough: markdown's text runs carry
        // `HighlightStyle`, which has no font size, so the glyphs are laid out
        // at whatever size the containing div inherits — the window's UI size.
        // Setting the size on the container too is what actually shrinks the
        // text, and is what `MarkdownStyle::with_preview_overrides` does.
        container_style,
        selection_background_color: cx.theme().colors().element_selection_background,
        link: TextStyleRefinement {
            color: Some(cx.theme().colors().link_text_hover),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(cx.theme().colors().link_text_hover),
                wavy: false,
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn compute_diff_stats(diff: &CommitDiff) -> (usize, usize) {
    diff.files.iter().fold((0, 0), |(added, removed), file| {
        let old_text = file.old_text.as_deref().unwrap_or("");
        let new_text = file.new_text.as_deref().unwrap_or("");
        let hunks = line_diff(old_text, new_text);
        hunks
            .iter()
            .fold((added, removed), |(a, r), (old_range, new_range)| {
                (
                    a + (new_range.end - new_range.start) as usize,
                    r + (old_range.end - old_range.start) as usize,
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_commit_message() {
        let (subject, body) = split_commit_message("Just a subject");
        assert_eq!(subject, "Just a subject");
        assert_eq!(body, "");

        let (subject, body) =
            split_commit_message("Subject line\n\nFirst paragraph.\n\nSecond paragraph.\n\n");
        assert_eq!(subject, "Subject line");
        assert_eq!(
            body, "First paragraph.\n\nSecond paragraph.",
            "the blank line git puts after the subject and the trailing newlines are dropped, \
             but blank lines inside the body are kept"
        );

        let (subject, body) = split_commit_message("Subject\nBody starts immediately");
        assert_eq!(subject, "Subject");
        assert_eq!(body, "Body starts immediately");

        let (subject, body) = split_commit_message("");
        assert_eq!(subject, "");
        assert_eq!(body, "");
    }

    /// The identity line is the one place the detail surface parses real
    /// markdown, so the author name has to be escaped and the email has to
    /// come out as a `mailto:` link with the angle brackets outside it.
    #[test]
    fn test_commit_identity_source() {
        assert_eq!(
            commit_identity_source("550e4c28", "antivanov", "anton@citeck.ru", None).as_ref(),
            "550e4c28 antivanov \\<[anton@citeck.ru](mailto:anton@citeck.ru)\\>"
        );

        assert_eq!(
            commit_identity_source("550e4c28", "a_b*c", "", None).as_ref(),
            "550e4c28 a\\_b\\*c",
            "markdown inline markers in an author name must not become formatting"
        );

        assert_eq!(
            commit_identity_source("550e4c28", "", "", None).as_ref(),
            "550e4c28"
        );

        let with_date = commit_identity_source("550e4c28", "Ada", "ada@example.com", Some(0));
        assert!(
            with_date.contains(" on "),
            "a resolved timestamp gets an `on <date> at <time>` suffix: {with_date}"
        );
    }

    fn changed_file_entry(path: &str) -> ChangedFileEntry {
        ChangedFileEntry::from_commit_file(&CommitFile {
            path: RepoPath::new(path).expect("valid repo path"),
            old_text: Some("old".to_string()),
            new_text: Some("new".to_string()),
            is_binary: false,
        })
    }

    fn describe_changed_file_row(row: &ChangedFileRow) -> String {
        match row {
            ChangedFileRow::Directory {
                key,
                label,
                file_count,
                ..
            } => format!("dir[key={key}, label={label}, {file_count}]"),
            ChangedFileRow::File(entry) => format!("file[{}]", entry.file_name),
        }
    }

    #[test]
    fn test_build_changed_file_rows_groups_by_directory() {
        let entries = vec![
            changed_file_entry("docs/plans/b.md"),
            changed_file_entry("README.md"),
            changed_file_entry("docs/plans/a.md"),
        ];
        let root: SharedString = "my-repo".into();

        let rows = build_changed_file_rows(&entries, &root, &HashSet::default());
        let described: Vec<String> = rows.iter().map(describe_changed_file_row).collect();
        assert_eq!(
            described,
            vec![
                "dir[key=, label=my-repo, 1]".to_string(),
                "file[README.md]".to_string(),
                "dir[key=docs/plans, label=docs/plans, 2]".to_string(),
                "file[a.md]".to_string(),
                "file[b.md]".to_string(),
            ],
            "root-level files sit under a header named after the repository, \
             and each directory group is sorted by file name"
        );

        let mut collapsed = HashSet::default();
        collapsed.insert(SharedString::from("docs/plans"));
        let rows = build_changed_file_rows(&entries, &root, &collapsed);
        let described: Vec<String> = rows.iter().map(describe_changed_file_row).collect();
        assert_eq!(
            described,
            vec![
                "dir[key=, label=my-repo, 1]".to_string(),
                "file[README.md]".to_string(),
                "dir[key=docs/plans, label=docs/plans, 2]".to_string(),
            ],
            "a collapsed directory hides its files but keeps its count"
        );
    }
}
