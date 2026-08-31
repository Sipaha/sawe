//! A failed MCP-server bind must not release the single-instance flock.
//!
//! The regression: the lock guard used to be moved into `start_server`'s
//! spawned task and parked only at that task's very end, so any `?` inside it
//! dropped the guard. The process kept running and kept owning the editor's
//! other single-instance gate (`data_dir()/zed-<channel>.sock`), so from then
//! until restart every `sawe <path>` probed a free lock, decided no instance
//! was running, lost that gate to this very process, and exited having opened
//! nothing. Persistent, and silent about the cause.
//!
//! Isolation: pins the lock + socket to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test`, so this is safe to run alongside a
//! live `sawe` instance. That pin is process-global, which is why this test
//! has a `tests/*.rs` file of its own.

use gpui::TestAppContext;

#[gpui::test]
async fn a_failed_bind_keeps_the_single_instance_lock(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    // Force the bind to fail at a real `?`, not a simulated one. A *directory*
    // at the well-known socket path survives `start_server`'s `remove_file`
    // (EISDIR) and then makes the symlink step fail with EEXIST — which is
    // exactly the failure the live reproduction produced.
    std::fs::create_dir_all(editor_mcp::socket_path()).expect("plant a directory at mcp.sock");

    cx.update(|cx| {
        editor_mcp::init(cx);
        editor_mcp::start_server(cx).expect("start_server reports the bind failure out of band");
    });
    cx.run_until_parked();

    assert!(
        !editor_mcp::socket_path().is_symlink(),
        "the socket must not have been published — this test's premise is a FAILED bind"
    );

    // The whole point: the lock is still held, so a second `sawe` still sees
    // an instance and takes a branch that says so instead of the one that
    // silently drops the user's paths.
    let lock_file = std::fs::File::open(editor_mcp::lock_path()).expect("lock file exists");
    assert!(
        fs2::FileExt::try_lock_exclusive(&lock_file).is_err(),
        "the single-instance flock was released by a failed startup; from here every \
         `sawe <path>` sees a free lock and drops the user's paths until restart"
    );

    let recorded_pid: u32 = std::fs::read_to_string(editor_mcp::lock_path())
        .expect("read lock")
        .trim()
        .parse()
        .expect("lock body is this process's pid");
    assert_eq!(
        recorded_pid,
        std::process::id(),
        "the holder recorded in the lock file must still be us"
    );
}
