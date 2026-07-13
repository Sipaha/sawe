//! Cold reconcile end to end: seed the app DB, the agent DB and a claude
//! transcript bucket at an old path, run one recorded migration through
//! `apply_one_with_connections`, and assert every row is rewritten, the bucket
//! is merged, the compat symlink is gone and the window's `workspace_id` (its
//! whole layout) is preserved.
//!
//! The schemas here mirror the *shipped* tables, not the plan's sketch: the
//! terminal table is `terminals` (with both the BLOB `working_directory` and
//! its TEXT twin `working_directory_path`) and `editors.buffer_path` is TEXT.

use crate::path_migrations::{PathRewrite, apply_one_with_connections, encode_claude_bucket};
use db::sqlez::connection::Connection;
use std::path::Path;

fn create_app_schema(connection: &Connection) {
    connection
        .exec(
            "CREATE TABLE solutions (id INTEGER PRIMARY KEY, name TEXT, root TEXT, last_opened_at INTEGER);
             CREATE TABLE solution_members (id INTEGER PRIMARY KEY, solution_id INTEGER, name TEXT, local_path TEXT, position INTEGER, origin_catalog_id INTEGER);
             CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT, paths_order TEXT, identity_paths TEXT, identity_paths_order TEXT, remote_connection_id INTEGER);
             CREATE TABLE console_panel_state (workspace_id INTEGER, tab_index INTEGER, cwd TEXT);
             CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, buffer_path TEXT);
             CREATE TABLE terminals (workspace_id INTEGER, item_id INTEGER, working_directory BLOB, working_directory_path TEXT);
             CREATE TABLE breakpoints (workspace_id INTEGER, path TEXT, breakpoint_location INTEGER);
             CREATE TABLE bookmarks (workspace_id INTEGER, path TEXT, row INTEGER);
             CREATE TABLE trusted_worktrees (trust_id INTEGER PRIMARY KEY, absolute_path TEXT, user_name TEXT, host_name TEXT);
             CREATE TABLE toolchains (workspace_id INTEGER, worktree_root_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT, relative_worktree_path TEXT);
             CREATE TABLE user_toolchains (remote_connection_id INTEGER, workspace_id INTEGER, worktree_root_path TEXT, relative_worktree_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT);",
        )
        .expect("prepare app schema")()
    .expect("create app schema");

    // A statement that depends on a table created by an earlier statement cannot
    // be prepared in the same batch (sqlez prepares them all up front).
    connection
        .exec(
            "CREATE UNIQUE INDEX ix_workspaces_location
                 ON workspaces(remote_connection_id, paths);",
        )
        .expect("prepare index")()
    .expect("create index");
}

fn text(connection: &Connection, query: &str) -> Vec<String> {
    connection.select::<String>(query).expect("prepare")().expect("select")
}

fn integers(connection: &Connection, query: &str) -> Vec<i64> {
    connection.select::<i64>(query).expect("prepare")().expect("select")
}

#[test]
fn cold_reconcile_rewrites_all_three_databases_and_merges_the_bucket() {
    let base = tempfile::tempdir().expect("tempdir");
    let old_root = base.path().join("spk-solutions");
    let new_root = base.path().join("Sawe");
    let old_member = old_root.join("sawe");
    let new_member = new_root.join("sawe");
    std::fs::create_dir_all(&new_member).expect("mkdir new member");
    // What the hot rename left behind.
    std::os::unix::fs::symlink(&new_root, &old_root).expect("compat symlink");

    let old_root_text = old_root.to_string_lossy().into_owned();
    let old_member_text = old_member.to_string_lossy().into_owned();
    let new_root_text = new_root.to_string_lossy().into_owned();
    let new_member_text = new_member.to_string_lossy().into_owned();
    let old_source_text = old_member.join("src/main.rs").to_string_lossy().into_owned();
    let new_source_text = new_member.join("src/main.rs").to_string_lossy().into_owned();

    let app = Connection::open_memory(Some("cold_reconcile_app"));
    create_app_schema(&app);

    // `exec_bound` prepares exactly one statement, so seed row by row.
    app.exec_bound::<String>("INSERT INTO solutions VALUES (1, 'Sawe', ?, NULL)")
        .expect("prepare solutions insert")(old_root_text)
    .expect("insert solution");
    app.exec_bound::<String>("INSERT INTO solution_members VALUES (1, 1, 'sawe', ?, 0, NULL)")
        .expect("prepare members insert")(old_member_text.clone())
    .expect("insert member");
    app.exec_bound::<(String, String)>("INSERT INTO workspaces VALUES (42, ?1, '0', ?2, '0', NULL)")
        .expect("prepare workspace insert")((
        old_member_text.clone(),
        old_member_text.clone(),
    ))
    .expect("insert workspace");
    app.exec_bound::<String>("INSERT INTO console_panel_state VALUES (42, 0, ?)")
        .expect("prepare console insert")(old_member_text.clone())
    .expect("insert console tab");
    app.exec_bound::<(Vec<u8>, String)>("INSERT INTO editors VALUES (1, 42, ?1, ?2)")
        .expect("prepare editor insert")((
        old_source_text.as_bytes().to_vec(),
        old_source_text.clone(),
    ))
    .expect("insert editor");
    app.exec_bound::<(Vec<u8>, String)>("INSERT INTO terminals VALUES (42, 1, ?1, ?2)")
        .expect("prepare terminal insert")((
        old_member_text.as_bytes().to_vec(),
        old_member_text.clone(),
    ))
    .expect("insert terminal");
    app.exec_bound::<String>("INSERT INTO breakpoints VALUES (42, ?, 3)")
        .expect("prepare breakpoint insert")(old_source_text.clone())
    .expect("insert breakpoint");
    app.exec_bound::<String>("INSERT INTO bookmarks VALUES (42, ?, 9)")
        .expect("prepare bookmark insert")(old_source_text)
    .expect("insert bookmark");
    app.exec_bound::<String>("INSERT INTO trusted_worktrees VALUES (1, ?, NULL, NULL)")
        .expect("prepare trust insert")(old_member_text.clone())
    .expect("insert trusted worktree");
    app.exec_bound::<String>(
        "INSERT INTO toolchains VALUES (42, ?, 'Rust', 'stable', '/usr/bin/cargo', '{}', '')",
    )
    .expect("prepare toolchain insert")(old_member_text.clone())
    .expect("insert toolchain");
    app.exec_bound::<String>(
        "INSERT INTO user_toolchains VALUES (NULL, 42, ?, '', 'Rust', 'stable', '/usr/bin/cargo', '{}')",
    )
    .expect("prepare user toolchain insert")(old_member_text.clone())
    .expect("insert user toolchain");

    let agent = Connection::open_memory(Some("cold_reconcile_agent"));
    agent
        .exec(
            "CREATE TABLE solution_sessions (id TEXT PRIMARY KEY, solution_id TEXT, cwd TEXT);
             CREATE TABLE solution_session_background_agent (solution_session_id TEXT, agent_id TEXT, jsonl_path TEXT, PRIMARY KEY (solution_session_id, agent_id));
             CREATE TABLE solution_session_attachment (session_id TEXT, solution_id TEXT, path TEXT, created_at_ms INTEGER, PRIMARY KEY (session_id, path));",
        )
        .expect("prepare agent schema")()
    .expect("create agent schema");
    agent
        .exec_bound::<String>("INSERT INTO solution_sessions VALUES ('s1', '1', ?)")
        .expect("prepare session insert")(old_member_text)
    .expect("insert session");

    let projects = tempfile::tempdir().expect("projects tempdir");
    let old_bucket = projects.path().join(encode_claude_bucket(&old_member));
    let new_bucket = projects.path().join(encode_claude_bucket(&new_member));
    std::fs::create_dir_all(&old_bucket).expect("mkdir old bucket");
    std::fs::write(old_bucket.join("s1.jsonl"), b"old").expect("write old transcript");
    // A bucket already sitting at the target: the two must be merged, never
    // renamed over.
    std::fs::create_dir_all(&new_bucket).expect("mkdir new bucket");
    std::fs::write(new_bucket.join("s2.jsonl"), b"new").expect("write new transcript");

    let rewrite = PathRewrite {
        old: old_root.clone(),
        new: new_root,
    };
    apply_one_with_connections(&app, Some(&agent), Some(projects.path()), &rewrite)
        .expect("reconcile");

    assert_eq!(text(&app, "SELECT root FROM solutions"), vec![new_root_text]);
    assert_eq!(
        text(&app, "SELECT local_path FROM solution_members"),
        vec![new_member_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT paths FROM workspaces"),
        vec![new_member_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT identity_paths FROM workspaces"),
        vec![new_member_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT cwd FROM console_panel_state"),
        vec![new_member_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT buffer_path FROM editors"),
        vec![new_source_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT CAST(path AS TEXT) FROM editors"),
        vec![new_source_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT working_directory_path FROM terminals"),
        vec![new_member_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT CAST(working_directory AS TEXT) FROM terminals"),
        vec![new_member_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT path FROM breakpoints"),
        vec![new_source_text.clone()]
    );
    assert_eq!(
        text(&app, "SELECT path FROM bookmarks"),
        vec![new_source_text]
    );
    assert_eq!(
        text(&app, "SELECT absolute_path FROM trusted_worktrees"),
        vec![new_member_text.clone()]
    );
    assert_eq!(
        integers(&app, "SELECT COUNT(*) FROM toolchains"),
        vec![0],
        "a toolchain keyed on the stale worktree root is dropped, not rewritten"
    );
    assert_eq!(
        integers(&app, "SELECT COUNT(*) FROM user_toolchains"),
        vec![0]
    );
    assert_eq!(
        text(&agent, "SELECT cwd FROM solution_sessions"),
        vec![new_member_text]
    );

    assert_eq!(
        integers(&app, "SELECT workspace_id FROM workspaces"),
        vec![42],
        "the window keeps its workspace_id — and with it every pane, tab and dock FK'd on it"
    );

    assert_eq!(
        std::fs::read(new_bucket.join("s1.jsonl")).expect("merged"),
        b"old"
    );
    assert_eq!(
        std::fs::read(new_bucket.join("s2.jsonl")).expect("kept"),
        b"new"
    );
    assert!(!old_bucket.exists(), "the source bucket is drained");
    assert!(!old_root.exists(), "the compat symlink is removed");
}

/// The workspace row is the window's identity: if the reconcile *inserted* a new
/// row instead of rewriting the existing one, the window would come back with a
/// fresh `workspace_id` and lose its whole layout. The nasty case is a second row
/// already sitting at the target path set (the user had opened that directory
/// before): the UNIQUE `ix_workspaces_location` makes a blind UPDATE fail.
#[test]
fn cold_reconcile_keeps_the_workspace_id_even_when_a_row_squats_on_the_target() {
    let base = tempfile::tempdir().expect("tempdir");
    let old_root = base.path().join("spk-solutions");
    let new_root = base.path().join("Sawe");
    std::fs::create_dir_all(&new_root).expect("mkdir new root");
    std::os::unix::fs::symlink(&new_root, &old_root).expect("compat symlink");

    let app = Connection::open_memory(Some("cold_reconcile_workspace_identity"));
    create_app_schema(&app);
    app.exec_bound::<(String, String)>("INSERT INTO workspaces VALUES (42, ?1, '0', ?2, '0', NULL)")
        .expect("prepare workspace insert")((
        old_root.to_string_lossy().into_owned(),
        old_root.to_string_lossy().into_owned(),
    ))
    .expect("insert workspace");
    app.exec_bound::<String>("INSERT INTO workspaces VALUES (77, ?, '0', NULL, NULL, NULL)")
        .expect("prepare squatter insert")(new_root.to_string_lossy().into_owned())
    .expect("insert squatter");
    // Children of the migrating window: they are FK'd on workspace_id, so they
    // only survive if the row keeps its id.
    app.exec_bound::<String>("INSERT INTO console_panel_state VALUES (42, 0, ?)")
        .expect("prepare console insert")(old_root.to_string_lossy().into_owned())
    .expect("insert console tab");

    apply_one_with_connections(
        &app,
        None,
        None,
        &PathRewrite {
            old: old_root,
            new: new_root.clone(),
        },
    )
    .expect("reconcile");

    assert_eq!(
        integers(&app, "SELECT workspace_id FROM workspaces"),
        vec![42],
        "the migrating row keeps its id; the row squatting on the target is merged away"
    );
    assert_eq!(
        text(&app, "SELECT paths FROM workspaces WHERE workspace_id = 42"),
        vec![new_root.to_string_lossy().into_owned()]
    );
    assert_eq!(
        integers(
            &app,
            "SELECT workspace_id FROM console_panel_state WHERE tab_index = 0"
        ),
        vec![42],
        "the window's children still point at the same window"
    );
}

/// A **member** rename moves the repo — and with it the worktree admin dirs at
/// `<member>/.git/worktrees/<name>/` — while the relocated agent worktree itself
/// stays at `<solution_root>/.agents/worktrees/<member-dir>/<name>` and keeps
/// pointing at the *old* admin path. Without a targeted `git worktree repair
/// <tree>` the tree shows up as missing/prunable.
#[test]
fn cold_reconcile_repairs_relocated_agent_worktrees() {
    let base = tempfile::tempdir().expect("tempdir");
    let solution_root = base.path().join("sol");
    let old_member = solution_root.join("old-project");
    let new_member = solution_root.join("New-Project");
    std::fs::create_dir_all(&old_member).expect("mkdir member");

    git(&["init", "-q"], &old_member);
    git(&["config", "user.email", "t@example.com"], &old_member);
    git(&["config", "user.name", "Test"], &old_member);
    std::fs::write(old_member.join("README.md"), b"hi").expect("write");
    git(&["add", "README.md"], &old_member);
    git(&["commit", "-qm", "init"], &old_member);

    // The relocated agent worktree, exactly where plan 3's `WorktreeCreate` hook
    // puts it — outside the member repo.
    let tree = solution_root
        .join(".agents")
        .join("worktrees")
        .join("old-project")
        .join("wt-1");
    git(
        &["worktree", "add", "-q", "-b", "wt-1", &tree.to_string_lossy()],
        &old_member,
    );

    // The hot rename: move the member, leave the compat symlink.
    std::fs::rename(&old_member, &new_member).expect("rename member");
    std::os::unix::fs::symlink(&new_member, &old_member).expect("compat symlink");

    let app = Connection::open_memory(Some("cold_reconcile_worktrees"));
    create_app_schema(&app);
    app.exec_bound::<String>("INSERT INTO solutions VALUES (1, 'Sol', ?, NULL)")
        .expect("prepare solutions insert")(solution_root.to_string_lossy().into_owned())
    .expect("insert solution");
    app.exec_bound::<String>(
        "INSERT INTO solution_members VALUES (1, 1, 'New Project', ?, 0, NULL)",
    )
    .expect("prepare members insert")(new_member.to_string_lossy().into_owned())
    .expect("insert member");

    apply_one_with_connections(
        &app,
        None,
        None,
        &PathRewrite {
            old: old_member.clone(),
            new: new_member.clone(),
        },
    )
    .expect("reconcile");

    let listed = git(&["worktree", "list", "--porcelain"], &new_member);
    assert!(
        listed.contains(&format!("worktree {}", tree.display())),
        "the relocated worktree must resolve at its path: {listed}"
    );
    assert!(
        !listed.contains("prunable"),
        "the worktree must not be prunable after the repair: {listed}"
    );
    // The tree's own `.git` pointer now names the *moved* admin dir.
    let pointer = std::fs::read_to_string(tree.join(".git")).expect("read .git");
    assert!(
        pointer.contains(
            &new_member
                .join(".git/worktrees/wt-1")
                .to_string_lossy()
                .into_owned()
        ),
        "{pointer}"
    );
    assert!(!old_member.exists(), "the compat symlink is removed");
}

// A synchronous one-shot `git` in a test, not in a task that could block the
// executor — the `disallowed_methods` lint's concern does not apply here.
#[allow(clippy::disallowed_methods)]
fn git(args: &[&str], cwd: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
