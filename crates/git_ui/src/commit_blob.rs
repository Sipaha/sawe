//! Loading one file of a git commit as a pair of in-memory buffers.
//!
//! A commit's file arrives from git as "text at the parent revision" plus
//! "text at this revision". Turning that into something an editor can show
//! means a synthetic [`language::File`] (there is nothing on disk to point
//! at), a [`Buffer`] holding the new text, a [`BufferDiff`] against the old
//! text, and the excerpt ranges the multibuffer should reveal.
//!
//! This lives apart from `commit_view` because more than one view needs it:
//! the commit view batches the resulting excerpts across every file of the
//! commit, while a single-file diff view wants exactly one file's worth and
//! batches differently. The batching is therefore the caller's job — this
//! module stops at [`LoadedBlob`].

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use git::repository::{CommitFile, RepoPath, is_binary_content};
use git::status::{FileStatus, StatusCode, TrackedStatus};
use gpui::{App, AppContext as _, AsyncWindowContext, Entity};
use language::{
    Buffer, Capability, DiskState, File, LanguageRegistry, LineEnding, OffsetRangeExt as _,
    ReplicaId, Rope, TextBuffer,
};
use multi_buffer::PathKey;
use project::{WorktreeId, git_store::Repository};
use std::{ops::Range, path::PathBuf, sync::Arc};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath};

/// Sort prefix for the file excerpts of a commit. The commit-message
/// excerpt sorts ahead of them on prefix 0, so every file namespace shares
/// this one and orders among itself by path.
pub(crate) const FILE_NAMESPACE_SORT_PREFIX: u64 = 1;

/// A file inside a commit: it has a repo-relative path and a revision, but
/// no location on disk, so it reports [`DiskState::Historic`].
pub(crate) struct GitBlob {
    pub(crate) path: RepoPath,
    pub(crate) worktree_id: WorktreeId,
    pub(crate) is_deleted: bool,
    pub(crate) is_binary: bool,
    pub(crate) display_name: String,
}

impl language::File for GitBlob {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        self.path.as_ref()
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        // Synthetic commit buffers never travel over the collab wire —
        // collab is disabled in this fork (.rules § "What's disabled"), so
        // `to_proto` is unreachable. If collab is ever re-enabled, these
        // read-only synthetic blobs would need a real serialization shape;
        // until then `unreachable!` is correct.
        unreachable!("CommitView synthetic File never serializes — collab disabled")
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

/// One commit file, ready to be handed to a multibuffer.
pub(crate) struct LoadedBlob {
    pub(crate) buffer: Entity<Buffer>,
    pub(crate) diff: Entity<BufferDiff>,
    pub(crate) status: FileStatus,
    pub(crate) excerpt_ranges: Vec<Range<language::Point>>,
    pub(crate) path_key: PathKey,
    pub(crate) is_binary: bool,
}

/// Turn one [`CommitFile`] into its buffer, diff, status and excerpt ranges.
///
/// `fallback_worktree_id` is used when the repo path resolves to no project
/// path (a file that no longer exists in the working tree); passing `None`
/// for a project with no worktrees at all makes this fail rather than
/// silently attach the buffer to a worktree that isn't there.
pub(crate) async fn load_commit_file_blob(
    file: CommitFile,
    commit_sha: &str,
    repository: &Entity<Repository>,
    fallback_worktree_id: Option<WorktreeId>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<LoadedBlob> {
    let is_created = file.old_text.is_none();
    let is_deleted = file.new_text.is_none();
    let raw_new_text = file.new_text.unwrap_or_default();
    let raw_old_text = file.old_text;

    let is_binary = file.is_binary
        || is_binary_content(raw_new_text.as_bytes())
        || raw_old_text
            .as_ref()
            .is_some_and(|text| is_binary_content(text.as_bytes()));

    let new_text = if is_binary {
        "(binary file not shown)".to_string()
    } else {
        raw_new_text
    };
    let old_text = if is_binary { None } else { raw_old_text };

    let worktree_id = repository
        .update(cx, |repository, cx| {
            repository
                .repo_path_to_project_path(&file.path, cx)
                .map(|path| path.worktree_id)
                .or(fallback_worktree_id)
        })
        .context("project has no worktrees")?;

    let short_sha = commit_sha
        .get(0..git::SHORT_SHA_LENGTH)
        .unwrap_or(commit_sha);
    let file_name = file
        .path
        .file_name()
        .map(|name| name.to_string())
        .unwrap_or_else(|| file.path.display(PathStyle::local()).to_string());
    let display_name = format!("{short_sha} - {file_name}");

    let path = file.path.clone();
    let blob = Arc::new(GitBlob {
        path: path.clone(),
        is_deleted,
        is_binary,
        worktree_id,
        display_name,
    }) as Arc<dyn language::File>;

    let buffer = build_buffer(new_text, blob, language_registry, cx).await?;

    let status_code = if is_created {
        StatusCode::Added
    } else if is_deleted {
        StatusCode::Deleted
    } else {
        StatusCode::Modified
    };
    let status = FileStatus::Tracked(TrackedStatus {
        index_status: status_code,
        worktree_status: StatusCode::Unmodified,
    });

    let diff = if is_binary {
        cx.update(|_, cx| {
            let snapshot = buffer.read(cx).snapshot();
            cx.new(|cx| {
                BufferDiff::new_unchanged(
                    &snapshot,
                    snapshot.language().cloned(),
                    Some(language_registry.clone()),
                    cx,
                )
            })
        })?
    } else {
        build_buffer_diff(old_text, &buffer, language_registry, cx).await?
    };

    // The buffer's file is the `GitBlob` built above, so its path is `path`
    // — read it back from there rather than unwrapping `snapshot.file()`.
    let path_key = PathKey::with_sort_prefix(FILE_NAMESPACE_SORT_PREFIX, path.as_ref().clone());

    let excerpt_ranges = cx.update(|_, cx| {
        let snapshot = buffer.read(cx).snapshot();
        if is_binary {
            vec![language::Point::zero()..snapshot.max_point()]
        } else {
            let diff_snapshot = diff.read(cx).snapshot(cx);
            let mut hunks = diff_snapshot.hunks(&snapshot).peekable();
            if hunks.peek().is_none() {
                vec![language::Point::zero()..snapshot.max_point()]
            } else {
                hunks
                    .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                    .collect::<Vec<_>>()
            }
        }
    })?;

    Ok(LoadedBlob {
        buffer,
        diff,
        status,
        excerpt_ranges,
        path_key,
        is_binary,
    })
}

pub(crate) async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<language::LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language =
        cx.update(|_, cx| language_registry.language_for_file(&blob, Some(&text), cx))?;
    let language = if let Some(language) = language {
        language_registry
            .load_language(&language)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite);
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

pub(crate) async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|_, cx| buffer.read(cx).language().cloned())?;
    let buffer = cx.update(|_, cx| buffer.read(cx).snapshot())?;

    let diff =
        cx.new(|cx| BufferDiff::new(&buffer.text, language, Some(language_registry.clone()), cx));

    diff.update(cx, |diff, cx| {
        diff.set_base_text(
            old_text.map(|old_text| Arc::from(old_text.as_str())),
            buffer.text.clone(),
            cx,
        )
    })
    .await;

    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::repo_path;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) {
        zlog::init_test();
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    struct BlobTestContext {
        repository: Entity<Repository>,
        language_registry: Arc<LanguageRegistry>,
        first_worktree_id: Option<WorktreeId>,
    }

    /// A one-repository project plus a live window — `load_commit_file_blob`
    /// takes an `AsyncWindowContext`, so a windowed test context is required.
    async fn blob_test_context(cx: &mut TestAppContext) -> (BlobTestContext, VisualTestContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            util::path!("/project"),
            serde_json::json!({
                ".git": {},
                "a.rs": "a\n",
            }),
        )
        .await;

        let project = Project::test(
            fs.clone(),
            [std::path::Path::new(util::path!("/project"))],
            cx,
        )
        .await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());

        let window_handle = cx.add_window(|_window, _cx| gpui::Empty);
        let cx = VisualTestContext::from_window(window_handle.into(), cx);
        cx.run_until_parked();

        let repository = project
            .read_with(&cx, |project, cx| project.active_repository(cx))
            .expect("the fake project exposes its repository");
        let first_worktree_id = project.read_with(&cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
        });

        (
            BlobTestContext {
                repository,
                language_registry,
                first_worktree_id,
            },
            cx,
        )
    }

    fn commit_file(path: &str, old_text: Option<&str>, new_text: Option<&str>) -> CommitFile {
        CommitFile {
            path: repo_path(path),
            old_text: old_text.map(str::to_string),
            new_text: new_text.map(str::to_string),
            is_binary: false,
        }
    }

    async fn load(
        file: CommitFile,
        context: &BlobTestContext,
        cx: &mut VisualTestContext,
    ) -> LoadedBlob {
        let mut async_cx = cx.update(|window, cx| window.to_async(cx));
        let load = load_commit_file_blob(
            file,
            "0123456789abcdef0123456789abcdef01234567",
            &context.repository,
            context.first_worktree_id,
            &context.language_registry,
            &mut async_cx,
        );
        load.await.expect("the blob loads")
    }

    fn tracked(index_status: StatusCode) -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status,
            worktree_status: StatusCode::Unmodified,
        })
    }

    #[gpui::test]
    async fn test_modified_file_yields_one_excerpt_per_hunk(cx: &mut TestAppContext) {
        let (context, mut cx) = blob_test_context(cx).await;

        let blob = load(
            commit_file(
                "a.rs",
                Some("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n"),
                Some("1 changed\n2\n3\n4\n5\n6\n7\n8\n9\n10 changed\n"),
            ),
            &context,
            &mut cx,
        )
        .await;

        assert_eq!(blob.status, tracked(StatusCode::Modified));
        assert!(!blob.is_binary);
        assert_eq!(
            blob.path_key,
            PathKey::with_sort_prefix(1, repo_path("a.rs").as_ref().clone())
        );
        let starting_rows = blob
            .excerpt_ranges
            .iter()
            .map(|range| range.start.row)
            .collect::<Vec<_>>();
        assert_eq!(starting_rows, vec![0, 9], "one excerpt per hunk");
        let text = blob.buffer.read_with(&cx, |buffer, _| buffer.text());
        assert_eq!(text, "1 changed\n2\n3\n4\n5\n6\n7\n8\n9\n10 changed\n");
    }

    #[gpui::test]
    async fn test_created_file_is_added(cx: &mut TestAppContext) {
        let (context, mut cx) = blob_test_context(cx).await;

        let blob = load(
            commit_file("new.rs", None, Some("fresh\n")),
            &context,
            &mut cx,
        )
        .await;

        assert_eq!(blob.status, tracked(StatusCode::Added));
        let text = blob.buffer.read_with(&cx, |buffer, _| buffer.text());
        assert_eq!(text, "fresh\n");
    }

    #[gpui::test]
    async fn test_deleted_file_is_deleted_and_empty(cx: &mut TestAppContext) {
        let (context, mut cx) = blob_test_context(cx).await;

        let blob = load(
            commit_file("gone.rs", Some("was here\n"), None),
            &context,
            &mut cx,
        )
        .await;

        assert_eq!(blob.status, tracked(StatusCode::Deleted));
        let text = blob.buffer.read_with(&cx, |buffer, _| buffer.text());
        assert_eq!(text, "");
        let was_deleted = blob.buffer.read_with(&cx, |buffer, _| {
            buffer.file().map(|file| file.disk_state())
        });
        assert_eq!(was_deleted, Some(DiskState::Historic { was_deleted: true }));
    }

    #[gpui::test]
    async fn test_binary_file_is_placeheld_and_undiffed(cx: &mut TestAppContext) {
        let (context, mut cx) = blob_test_context(cx).await;

        let blob = load(
            commit_file("image.png", Some("old\0bytes"), Some("new\0bytes")),
            &context,
            &mut cx,
        )
        .await;

        assert!(blob.is_binary);
        let text = blob.buffer.read_with(&cx, |buffer, _| buffer.text());
        assert_eq!(text, "(binary file not shown)");

        let (snapshot, max_point) = blob
            .buffer
            .read_with(&cx, |buffer, _| (buffer.snapshot(), buffer.max_point()));
        assert_eq!(
            blob.excerpt_ranges,
            vec![language::Point::zero()..max_point],
            "a binary file gets one full-file excerpt"
        );
        let hunk_count = blob
            .diff
            .read_with(&cx, |diff, cx| diff.snapshot(cx).hunks(&snapshot).count());
        assert_eq!(hunk_count, 0, "a binary file reports no hunks");
    }

    #[gpui::test]
    async fn test_unchanged_file_falls_back_to_one_full_file_excerpt(cx: &mut TestAppContext) {
        let (context, mut cx) = blob_test_context(cx).await;

        let blob = load(
            commit_file("same.rs", Some("identical\n"), Some("identical\n")),
            &context,
            &mut cx,
        )
        .await;

        let max_point = blob.buffer.read_with(&cx, |buffer, _| buffer.max_point());
        assert_eq!(
            blob.excerpt_ranges,
            vec![language::Point::zero()..max_point],
            "zero hunks must still yield one full-file excerpt"
        );
    }

    #[gpui::test]
    async fn test_crlf_input_is_normalized(cx: &mut TestAppContext) {
        let (context, mut cx) = blob_test_context(cx).await;

        let blob = load(
            commit_file("crlf.rs", Some("one\r\ntwo\r\n"), Some("one\r\nTWO\r\n")),
            &context,
            &mut cx,
        )
        .await;

        let (text, line_ending) = blob
            .buffer
            .read_with(&cx, |buffer, _| (buffer.text(), buffer.line_ending()));
        assert_eq!(text, "one\nTWO\n");
        assert_eq!(line_ending, LineEnding::Windows);

        // The base text is normalized too, so the CRLF pair does not read as
        // a difference on every single line.
        let snapshot = blob.buffer.read_with(&cx, |buffer, _| buffer.snapshot());
        let hunk_rows = blob.diff.read_with(&cx, |diff, cx| {
            diff.snapshot(cx)
                .hunks(&snapshot)
                .map(|hunk| hunk.buffer_range.to_point(&snapshot).start.row)
                .collect::<Vec<_>>()
        });
        assert_eq!(hunk_rows, vec![1]);
    }
}
