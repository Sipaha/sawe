//! Hot-rename integration test against a **real** filesystem and a **live**
//! worktree.
//!
//! Two things the design rests on are proved here:
//!
//!   * the rename never removes/recreates the worktree — the same worktree
//!     entity keeps scanning the directory it has always held (by inode), so
//!     open buffers survive and new files under the moved directory are still
//!     picked up. While the compat symlink is in place the worktree keeps its
//!     *old* `abs_path`, which still resolves — that is exactly what keeps a
//!     live `claude` subprocess and its path *strings* working;
//!   * once the symlink is gone (what the cold reconcile does, at startup),
//!     the worktree heals itself: `canonicalize(abs_path)` fails, the scanner
//!     falls back to the root file handle, emits `ScanState::RootUpdated` and
//!     `update_abs_path_and_refresh` repoints the *same* worktree at the new
//!     path.

use gpui::TestAppContext;
use project::Project;
use settings::SettingsStore;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use util::rel_path::rel_path;

/// The fs watcher is a real OS watcher on a real directory, so waiting on it
/// means waiting in wall-clock time — `cx.executor().timer()` under
/// `allow_parking` does park the thread, and `run_until_parked` then pumps
/// whatever the watcher delivered.
async fn wait_until(
    cx: &mut TestAppContext,
    mut condition: impl FnMut(&mut TestAppContext) -> bool,
) -> bool {
    for _ in 0..100 {
        if condition(cx) {
            return true;
        }
        cx.executor().timer(Duration::from_millis(100)).await;
        cx.run_until_parked();
    }
    condition(cx)
}

#[gpui::test]
async fn renaming_a_solution_moves_the_folder_under_a_live_worktree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let base = tempfile::tempdir().expect("tempdir");
    let old_root = base.path().join("spk-solutions");
    let member_path = old_root.join("sawe");
    std::fs::create_dir_all(member_path.join("src")).expect("mkdir member");
    std::fs::write(member_path.join("src/main.rs"), b"fn main() {}").expect("write source");

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
    });

    let fs = Arc::new(fs::RealFs::new(None, cx.executor()));
    let project = Project::test(fs, [member_path.as_path()], cx).await;

    // An open buffer must survive the move.
    let buffer = project
        .update(cx, |project, cx| {
            let path = project
                .find_project_path(member_path.join("src/main.rs"), cx)
                .expect("project path");
            project.open_buffer(path, cx)
        })
        .await
        .expect("open buffer");
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "fn main() {}"
    );

    let worktree = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().expect("one worktree")
    });
    let worktree_entity_id = worktree.entity_id();

    let store = cx.update(|cx| crate::store::for_test_with_solution(cx, &old_root, &member_path));
    let solution_id = store.read_with(cx, |store, _| store.solutions()[0].id);

    store
        .update(cx, |store, cx| {
            store.rename_solution(solution_id, "Sawe", cx)
        })
        .expect("rename");

    let new_root = base.path().join("Sawe");
    let new_member_path = new_root.join("sawe");
    assert!(new_member_path.join("src/main.rs").is_file());
    assert!(
        std::fs::symlink_metadata(&old_root)
            .expect("stat the old root")
            .file_type()
            .is_symlink(),
        "the hot rename leaves a compat symlink at the old root"
    );

    // The worktree is never torn down: it is the same entity, still scanning the
    // same inode, so a file created under the *new* path shows up in it.
    std::fs::write(new_member_path.join("src/added.rs"), b"// added").expect("write added file");
    let sees_the_new_file = wait_until(cx, |cx| {
        worktree.read_with(cx, |worktree, _| {
            worktree.entry_for_path(rel_path("src/added.rs")).is_some()
        })
    })
    .await;
    assert!(
        sees_the_new_file,
        "the live worktree keeps scanning the directory across the move"
    );
    project.read_with(cx, |project, cx| {
        let worktrees: Vec<_> = project.worktrees(cx).collect();
        assert_eq!(worktrees.len(), 1, "the rename must not add a worktree");
        assert_eq!(
            worktrees[0].entity_id(),
            worktree_entity_id,
            "the rename must not remove and re-create the worktree"
        );
    });

    // The old path still resolves through the compat symlink — which is what
    // keeps a live `claude` subprocess (holding the old cwd *string*) working —
    // and that is also why the worktree has no reason to repoint yet.
    assert_eq!(
        std::fs::read(member_path.join("src/main.rs")).expect("read through the link"),
        b"fn main() {}"
    );
    std::fs::write(member_path.join("src/main.rs"), b"fn main() { /* edited */ }")
        .expect("write through the link");
    assert_eq!(
        std::fs::read(new_member_path.join("src/main.rs")).expect("read the moved file"),
        b"fn main() { /* edited */ }"
    );
    worktree.read_with(cx, |worktree, _| {
        assert_eq!(
            worktree.abs_path().as_ref(),
            member_path.as_path(),
            "with the compat link in place the old abs_path still resolves, so the scanner keeps it"
        );
    });

    // Now drop the compat link — exactly what the cold reconcile does — and the
    // worktree heals: ScanState::RootUpdated → update_abs_path_and_refresh.
    std::fs::remove_file(&old_root).expect("remove the compat link");
    // The scanner only re-canonicalizes when it processes an event, so give it
    // one under the moved root.
    std::fs::write(new_member_path.join("src/poke.rs"), b"// poke").expect("poke");
    let followed = wait_until(cx, |cx| {
        worktree.read_with(cx, |worktree, _| {
            worktree.abs_path().as_ref() == new_member_path.as_path()
        })
    })
    .await;
    assert!(
        followed,
        "the worktree's abs_path follows the move via ScanState::RootUpdated once the link is gone"
    );
    project.read_with(cx, |project, cx| {
        let worktrees: Vec<_> = project.worktrees(cx).collect();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(
            worktrees[0].entity_id(),
            worktree_entity_id,
            "the worktree heals in place — it is never removed and re-added"
        );
    });

    // The open buffer survived the whole thing — it is the same entity, and it
    // is still tracking the same file, which it re-read through the link after
    // the write above (a clean buffer reloads on an external change).
    assert_eq!(
        buffer.read_with(cx, |buffer, _| buffer.text()),
        "fn main() { /* edited */ }",
        "the buffer stayed live on the moved file"
    );
    let buffer_path = buffer.read_with(cx, |buffer, cx| {
        buffer
            .file()
            .map(|file| file.as_local().expect("local file").abs_path(cx))
    });
    assert_eq!(
        buffer_path.as_deref(),
        Some(new_member_path.join("src/main.rs").as_path() as &Path),
        "the buffer's file now resolves under the new root"
    );
}
