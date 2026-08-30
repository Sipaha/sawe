use std::path::Path;

use fs::FakeFs;
use futures::StreamExt;
use gpui::TestAppContext;
use language::{CodeLabel, FakeLspAdapter, HighlightId, rust_lang};
use lsp::Uri;
use project::{Project, lsp_store::*};
use serde_json::json;
use util::path;

use crate::init_test;

#[gpui::test]
async fn test_removing_invisible_worktree_cleans_reused_lsp_bookkeeping(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    fs.insert_tree(
        path!("/the-registry"),
        json!({ "dep": { "src": { "dep.rs": "pub fn dep() {}" } } }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (_visible_buffer, _visible_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });
    let external_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/the-registry/dep/src/dep.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let invisible_worktree_id =
        external_buffer.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
    project.read_with(cx, |project, cx| {
        let worktree = project.worktree_for_id(invisible_worktree_id, cx).unwrap();
        assert!(!worktree.read(cx).is_visible());
        assert!(
            project
                .lsp_store()
                .read(cx)
                .has_language_server_seed_for_worktree(invisible_worktree_id)
        );
    });

    project.update(cx, |project, cx| {
        project.remove_worktree(invisible_worktree_id, cx);
    });
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        let lsp_store = project.lsp_store();
        let lsp_store = lsp_store.read(cx);
        assert!(
            lsp_store
                .language_server_statuses()
                .any(|(status_server_id, _)| status_server_id == server_id)
        );
        assert!(!lsp_store.has_language_server_seed_for_worktree(invisible_worktree_id));
    });
}

#[test]
fn test_glob_literal_prefix() {
    assert_eq!(glob_literal_prefix(Path::new("**/*.js")), Path::new(""));
    assert_eq!(
        glob_literal_prefix(Path::new("node_modules/**/*.js")),
        Path::new("node_modules")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
        Path::new("foo")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/bar/baz.js")),
        Path::new("foo/bar/baz.js")
    );

    #[cfg(target_os = "windows")]
    {
        assert_eq!(glob_literal_prefix(Path::new("**\\*.js")), Path::new(""));
        assert_eq!(
            glob_literal_prefix(Path::new("node_modules\\**/*.js")),
            Path::new("node_modules")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
            Path::new("foo")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo\\bar\\baz.js")),
            Path::new("foo/bar/baz.js")
        );
    }
}

#[test]
fn test_multi_len_chars_normalization() {
    let mut label = CodeLabel::new(
        "myElˇ (parameter) myElˇ: {\n    foo: string;\n}".to_string(),
        0..6,
        vec![(0..6, HighlightId::new(1))],
    );
    ensure_uniform_list_compatible_label(&mut label);
    assert_eq!(
        label,
        CodeLabel::new(
            "myElˇ (parameter) myElˇ: { foo: string; }".to_string(),
            0..6,
            vec![(0..6, HighlightId::new(1))],
        )
    );
}

#[test]
fn test_trailing_newline_in_completion_documentation() {
    let doc =
        lsp::Documentation::String("Inappropriate argument value (of correct type).\n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(
        matches!(completion_doc, CompletionDocumentation::SingleLine(s) if s == "Inappropriate argument value (of correct type).")
    );

    let doc = lsp::Documentation::String("  some value  \n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(matches!(
        completion_doc,
        CompletionDocumentation::SingleLine(s) if s == "some value"
    ));
}

/// A language server that is still starting when the user quits must not be
/// awaited by the quit hook.
///
/// The test is only meaningful because `TestScheduler` models GPUI's real quit
/// contract: `App::shutdown` blocks the main thread on the collected quit
/// futures through the FOREGROUND executor's session, and `TestScheduler::block`
/// excludes runnables whose session is blocked — exactly as the production main
/// thread stops draining its dispatch channel. `LanguageServerState::Starting`
/// carries a `cx.spawn`ed `startup` task, i.e. foreground work, so awaiting it
/// from that hook can only burn `gpui::SHUTDOWN_TIMEOUT` and still never send a
/// shutdown request (FORK.md #103).
///
/// `set_block_on_ticks` pins the tick budget the scheduler draws for a timed
/// block. Every `TestAppContext` draws from `0..=1000` (hard-coded in
/// `TestDispatcher::new`) and THE FLOOR IS ZERO, i.e. `TestScheduler::block` can
/// return without polling the quit future at all — and it reports that as
/// `completed == false`, so an unpinned run of this test fails at random rather
/// than passing at random. A finite `N` is used rather than `usize::MAX` so a
/// genuinely parked hook ends the run instead of grinding the scheduler's
/// timer-advance loop.
#[gpui::test]
async fn test_quit_does_not_await_a_starting_language_server(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());

    lsp_store.update(cx, |lsp_store, cx| {
        // Stands in for a server whose `startup` has not resolved yet. The real one is
        // spawned the same way — on the foreground executor — so it is just as
        // unresolvable once `shutdown` has blocked that session.
        let startup = cx.spawn(async move |_, _| {
            futures::future::pending::<()>().await;
            None
        });
        lsp_store
            .as_local_mut()
            .expect("Project::test builds a local lsp store")
            .language_servers
            .insert(
                lsp::LanguageServerId(0),
                LanguageServerState::Starting {
                    startup,
                    pending_workspace_folders: Default::default(),
                },
            );
    });

    let quit_future = lsp_store.update(cx, |lsp_store, _| {
        lsp_store
            .as_local_mut()
            .expect("Project::test builds a local lsp store")
            .shutdown_language_servers_on_quit_for_test()
    });

    cx.executor().set_block_on_ticks(64..=64);
    let completed = cx
        .foreground_executor()
        .block_with_timeout(gpui::SHUTDOWN_TIMEOUT, quit_future)
        .is_ok();

    assert!(
        completed,
        "the quit hook parked on a still-starting language server, \
         which burns the whole shutdown budget and shuts nothing down"
    );
}

/// The counterpart to the test above: a `Starting` entry whose `startup` task has
/// ALREADY completed with a live server must still be shut down at quit. Skipping
/// every `Starting` entry unconditionally would orphan that server, because a
/// finished task resolves from its stored output with no scheduler involvement —
/// the blocked foreground session is irrelevant to it — and `shutdown()` itself is
/// background-driven.
///
/// The fixture uses a real `cx.spawn`ed task that is then run to completion, not
/// `Task::ready`, so it exercises `async_task::Task::is_finished` rather than the
/// `TaskState::Ready` shortcut that the production path can never produce.
#[gpui::test]
async fn test_quit_shuts_down_a_language_server_whose_startup_finished(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());

    let (server, _fake) = cx.update(|cx| {
        lsp::FakeLanguageServer::new(
            lsp::LanguageServerId(0),
            lsp::LanguageServerBinary {
                path: Path::new(path!("/the-fake-server")).to_path_buf(),
                arguments: vec![],
                env: None,
            },
            "the-fake-server".to_string(),
            Default::default(),
            &mut cx.to_async(),
        )
    });
    let server = std::sync::Arc::new(server);

    let startup = lsp_store.update(cx, |_, cx| {
        let server = server.clone();
        cx.spawn(async move |_, _| Some(server))
    });
    cx.run_until_parked();
    assert!(
        startup.is_ready(),
        "fixture: the startup task must have finished before the quit hook runs"
    );

    lsp_store.update(cx, |lsp_store, _| {
        lsp_store
            .as_local_mut()
            .expect("Project::test builds a local lsp store")
            .language_servers
            .insert(
                lsp::LanguageServerId(0),
                LanguageServerState::Starting {
                    startup,
                    pending_workspace_folders: Default::default(),
                },
            );
    });

    let quit_future = lsp_store.update(cx, |lsp_store, _| {
        lsp_store
            .as_local_mut()
            .expect("Project::test builds a local lsp store")
            .shutdown_language_servers_on_quit_for_test()
    });

    cx.executor().set_block_on_ticks(100_000..=100_000);
    let completed = cx
        .foreground_executor()
        .block_with_timeout(gpui::SHUTDOWN_TIMEOUT, quit_future)
        .is_ok();

    assert!(
        completed,
        "the quit hook did not finish shutting down an already-started language server"
    );
    // `LanguageServer::shutdown` TAKES `io_tasks`, so a second call can only return
    // `None` if the first one really happened. This is what distinguishes "the hook
    // shut the server down" from "the hook dropped the state and returned early".
    assert!(
        server.shutdown().is_none(),
        "the quit hook dropped an already-started language server instead of shutting it down"
    );
}
