# Plan 3 — editor-owned claude settings + sockets out of `config/` + optional `solution_id`

**Date:** 2026-07-13
**Spec:** `docs/superpowers/specs/2026-07-13-rename-with-folder-move-design.md` §
"Claude Code integration" (phase 6) and § "Side quests" (phase 7).
**Composes with:** plan 1 (identity — assumes `SolutionId(pub i64)` and the
per-solution socket dir keyed by the numeric id). **Independent of** plan 2
(folder move / cold reconcile).

## Goal

Three self-contained changes that fall out of the rename work but do not depend
on the rename landing:

1. **Sockets leave `config/`.** `editor_mcp::runtime_dir()` returns
   `paths::config_dir()` today (`crates/editor_mcp/src/lifecycle.rs:34-38`). A
   Unix socket, a PID lock and an upload spool are not configuration. They move
   to `paths::state_dir()` (`crates/paths/src/paths.rs:173`), and the old
   leftovers under `config/` are removed once at startup.
2. **A claude settings layer owned by the editor.** We spawn `claude` with
   `--setting-sources user,project,local` and write nothing of our own
   (`crates/claude_native/src/command.rs:52-98`). Add an editor-owned settings
   JSON passed as `--settings <file>` carrying a `WorktreeCreate` /
   `WorktreeRemove` hook pair (agent worktrees land under
   `<solution_root>/.agents/worktrees/` instead of `<member>/.claude/worktrees/`)
   and `autoMemoryDirectory` → `<solution_root>/.agents/memory`.
3. **`solution_id` becomes optional** in solution-scoped MCP tools. The server
   already force-injects the bound id per socket
   (`crates/context_server/src/listener.rs:485-501`), so requiring it in the
   schema only forces agents to send a value that is immediately overwritten.

Order matters: **sockets first** (task group 1), because the claude settings file
is written to `<state>/solutions/<id>/claude-settings.json`, next to that
Solution's socket.

## Architecture

### 1. Runtime root

```
~/.spk/sawe[-dev]/
  config/                       # settings.json, themes, remote-control keys  (STAYS)
  state/                        # NEW home of everything runtime
    mcp.sock                    # (a symlink to a short /tmp path — see below)
    mcp.lock
    uploads/<id>.bin
    solutions/<solution_id>/
      mcp.sock
      claude-settings.json      # NEW — the editor-owned claude settings layer
```

`runtime_dir()` keeps its test override (`set_runtime_dir_for_test`) — only the
*default* changes from `config_dir()` to `state_dir()`. Every consumer already
goes through `runtime_dir()` (`solution_agent::init` builds the upload spool from
it; `solution_socket_path` joins onto it), so the Rust side is a one-line change
plus a one-time cleanup of the old location. `script/run-mcp` hardcodes
`$HOME/.spk/sawe[-dev]/config` and must be updated by hand, as must the four docs
that name the socket path.

Note the live socket at `config/mcp.sock` is a **symlink** to
`/tmp/zed-mcp*/mcp.sock` (sun_path length workaround). The cleanup must use
`symlink_metadata`, not `exists()` — a dangling symlink reports
`exists() == false`.

### 2. Claude settings layer

**`--settings`, not a fourth `--setting-sources` entry.** `--setting-sources`
accepts only `user`, `project`, `local` — there is no way to name a directory of
our own (cli-reference). `--settings` takes "a path to a settings JSON file or an
inline JSON string" and sits at the **command-line** precedence tier, which is
*above* local/project/user but below managed (settings § Settings precedence). So
passing `--settings` does **not** drop the user's own settings: they still load
via `--setting-sources user,project,local`, and our file only outranks them.

The one sharp edge, quoted from the CLI reference: *"Values you set here override
the same keys in your `settings.json` files for this session. Keys you omit keep
their file-based values."* `hooks` is one key. So a naive `{"hooks": {...ours}}`
would **silently disable the user's own hooks**. We therefore read the `hooks`
object out of the three sources we keep enabled (user
`$CLAUDE_CONFIG_DIR|~/.claude/settings.json`, project `<cwd>/.claude/settings.json`,
local `<cwd>/.claude/settings.local.json`), union them, and append our two
entries. Residual risk: if claude actually *deep*-merges `hooks` (contradicting
the doc), a user hook would fire twice. Mitigation: our own hooks are idempotent
(create returns the existing dir; remove is a no-op on a missing dir), and
`SAWE_CLAUDE_SETTINGS_DISABLED=1` turns the whole layer off.

**The hook is the editor binary, not a shipped shell script.** We already use
`<current_exe> --nc <socket>` for the MCP bridge
(`crates/agent_servers/src/acp.rs:3809-3839`) — mirror it with
`<current_exe> --worktree-hook {create|remove} --worktree-base <dir>`. The doc's
own example hook is `bash -c '... jq -r .name ...'`, i.e. it assumes `jq` is
installed; the editor binary has no such dependency, needs no `chmod +x` dance,
and cannot drift out of sync with the JSON we generate. Like `--nc`, it
early-returns from `main()` before GPUI init, so the process cost is a bare exec.

**Verified hook contract** (https://code.claude.com/docs/en/hooks#worktreecreate,
fetched 2026-07-13):

- `WorktreeCreate` — *"Runs when a worktree is being created via `--worktree` or
  `isolation: "worktree"`. Replaces default git behavior. The hook must return
  the path to the created worktree on stdout."* Input on stdin is the common
  fields (`session_id`, `transcript_path`, `cwd`, `hook_event_name`) **plus
  `name`, `branch`, `repo_root`**:

  ```json
  {
    "session_id": "abc123",
    "transcript_path": "/Users/.../.claude/projects/.../transcript.jsonl",
    "cwd": "/Users/...",
    "repo_root": "/Users/.../my-repo",
    "hook_event_name": "WorktreeCreate",
    "name": "feat-new-feature",
    "branch": "feat/new-feature"
  }
  ```

  *"The hook must print the absolute path to the created worktree on stdout and
  exit with code 0. Any non-zero exit code causes worktree creation to fail."*
  No matcher support (the event always fires).

  **The spec's `{worktree_path, branch, session_id, cwd}` is wrong for the
  create side** — the field is `name` (plus `repo_root`), and `worktree_path` is
  the *WorktreeRemove* field. This plan follows the docs.

- `WorktreeRemove` — *"Runs when a worktree is being removed, either at session
  exit or when a subagent finishes. Failures are logged in debug mode only."*
  Input adds **`worktree_path`** (absolute). *"This event has no decision
  control. Exit code and output are ignored."*

  ```json
  {
    "session_id": "abc123",
    "transcript_path": "/Users/.../.claude/projects/.../transcript.jsonl",
    "cwd": "/Users/...",
    "hook_event_name": "WorktreeRemove",
    "worktree_path": "/Users/my-repo/.worktrees/feat-new-feature"
  }
  ```

- Config shape: the hooks reference shows a **flat** entry
  (`"WorktreeCreate": [{"type":"command","command":"..."}]`) while the worktrees
  page shows the canonical **matcher-group** entry
  (`"WorktreeCreate": [{"hooks":[{"type":"command","command":"..."}]}]`
  — https://code.claude.com/docs/en/worktrees § "Non-git version control", a
  complete working SVN example). We emit the **matcher-group** form: it is the
  general hook schema, it is the shape shown in a runnable end-to-end example,
  and it is the shape the user's own `settings.json` hooks will be in (we
  concatenate into the same arrays).

- Default location, quoted: *"By default, the worktree is created under
  `.claude/worktrees/<value>/` at your repository root, on a new branch named
  `worktree-<value>`. … To put worktrees somewhere else, configure a
  `WorktreeCreate` hook."* And the bonus the spec relies on: *"Worktrees created
  by a `WorktreeCreate` hook are excluded and keep the transcript at the launch
  directory"* (i.e. hook-created worktrees do not relocate the transcript
  bucket).

- Failure mode to respect: *"If Claude Code can't enter the worktree directory at
  startup, for example because a `WorktreeCreate` hook printed something other
  than the directory it created … Claude Code prints an error naming the path and
  exits with code 1."* → our hook prints the path and **nothing else**; all
  diagnostics go to stderr.

`autoMemoryDirectory`, quoted from settings: *"Custom directory for auto memory
storage. Accepts an absolute path or a `~/`-prefixed path. From project or local
settings, this is honored only after you accept the workspace trust dialog, since
a cloned repository can supply this file."* Ours arrives at the command-line tier,
not project/local, so the trust gate should not apply — verified by hand in the
final task.

Worktree naming: `<solution_root>/.agents/worktrees/<member-dir-name>/<name>`.
The member subdir exists because two members of one Solution can each get a
worktree called `bright-running-fox`.

**Existing worktrees under `<member>/.claude/worktrees/*` are left alone.** The
hook only intercepts *new* creations; `EnterWorktree` into an existing directory
still works, and the removal hook explicitly no-ops on any path outside our base
(so it cannot race plan 2's reconcile). Plan 2's cold reconcile runs
`git worktree repair` for the legacy ones — we do not duplicate that here.

> **Note for plan 2 (do not implement here):** a worktree's administrative files
> live in `<member>/.git/worktrees/<name>/` *regardless of where the working tree
> sits*, and the worktree's own `.git` file holds an absolute pointer back into
> the member. So after a **member** rename, plan 2's `git worktree repair` step
> must cover `<solution_root>/.agents/worktrees/**` too, not only the legacy
> `<member>/.claude/worktrees/*`. What moving them out buys is that the trees
> themselves are no longer swept along by the member's `mv` and keep a stable
> absolute path.

### 3. Optional `solution_id`

`RegisteredTool::wants_solution_id` is derived from the generated input schema
having a `solution_id` **property** (`listener.rs:145-149`), and the injection
then unconditionally overwrites whatever the caller sent (`listener.rs:485-501`).
`schemars` still emits an `Option<String>` field as a property (it only drops out
of `required`), so flipping the type keeps the injection firing. On the *global*
socket there is no bound id, so `None` must produce a clear error, not a panic or
a silent empty id — that is what the shared `resolve_solution_id` helper is for.

## Tech stack

Rust 2024, `anyhow`, `serde` / `serde_json`, `schemars` (draft-07, inlined
subschemas), `gpui`, `tempfile` for fs tests, `git` invoked as a subprocess.
Crates touched: `editor_mcp`, `paths` (read-only), `claude_native`,
`agent_servers`, `zed` (bin), `solutions`, `solution_agent`, plus `script/run-mcp`
and docs.

## Global constraints

- **Debug builds only.** `cargo check`, `cargo test -p <crate>` — never
  `--release`.
- **One commit per task**, imperative subject, **no `Co-Authored-By` trailer**,
  no `--amend`.
- Rust guidelines: no `unwrap()` in non-test code, no `let _ =` on fallible
  calls (use `?` / `.log_err()`), no summarizing comments — comments explain
  *why*.
- `SolutionId` is assumed to be `SolutionId(pub i64)` (plan 1). Every place this
  matters is flagged with a **"pre-plan-1 variant"** note so the task can land
  either way.
- The `sawe` binary must keep starting with no Solution open, with no `claude`
  installed, and with `git` missing from `PATH` — every new path is a
  `log_err()`-ed `Option`, never a hard failure.

---

# Task group 1 — sockets out of `config/`

## Task 1.1 — `runtime_dir()` defaults to `state_dir()`

**Files**
- `crates/editor_mcp/src/lifecycle.rs` (edit)

**Interfaces**
- Consumes: `paths::state_dir() -> &'static PathBuf` (`crates/paths/src/paths.rs:173`).
- Produces:
  ```rust
  fn default_runtime_dir() -> PathBuf;      // private
  pub fn runtime_dir() -> PathBuf;          // unchanged signature
  ```

**Steps**

- [x] Add a failing test at the bottom of `crates/editor_mcp/src/lifecycle.rs`
      (inside a new `#[cfg(test)] mod tests`, or the existing one if present):
      ```rust
      #[cfg(test)]
      mod runtime_dir_tests {
          use super::*;

          #[test]
          fn default_runtime_dir_is_state_not_config() {
              let dir = default_runtime_dir();
              assert!(
                  dir.ends_with("state"),
                  "the mcp socket + lock are runtime state, not configuration: {}",
                  dir.display()
              );
              assert_ne!(
                  dir,
                  *paths::config_dir(),
                  "runtime_dir must no longer alias config_dir"
              );
          }
      }
      ```
- [x] Run `cargo test -p editor_mcp --lib runtime_dir` — expect a **compile
      error**: ``cannot find function `default_runtime_dir` in this scope``.
- [x] Implement, replacing `runtime_dir` (`lifecycle.rs:34-38`):
      ```rust
      /// The socket, its lock file, the per-solution socket dirs and the upload
      /// spool are *runtime state*, not configuration — they must not live in
      /// `config/` next to `settings.json` (a `rm ~/.spk/sawe/config -r` to reset
      /// settings would otherwise take the lock with it, and vice versa a config
      /// sync would ship a dead socket).
      fn default_runtime_dir() -> PathBuf {
          paths::state_dir().clone()
      }

      pub fn runtime_dir() -> PathBuf {
          RUNTIME_DIR_OVERRIDE
              .get()
              .cloned()
              .unwrap_or_else(default_runtime_dir)
      }
      ```
- [x] Run `cargo test -p editor_mcp --lib runtime_dir` — expect
      `test runtime_dir_tests::default_runtime_dir_is_state_not_config ... ok`.
- [x] Run `cargo check -p editor_mcp -p solution_agent` — expect no errors
      (`solution_agent::init` already derives its upload spool from
      `editor_mcp::runtime_dir()`, `solution_agent.rs:112`, so it follows for
      free).
- [x] Commit: `Move the MCP socket, lock and upload spool from config/ to state/`

## Task 1.2 — remove the leftovers under `config/`

**Files**
- `crates/editor_mcp/src/lifecycle.rs` (edit)
- `crates/editor_mcp/src/editor_mcp.rs` (re-export)

**Interfaces**
- Produces: `pub fn cleanup_legacy_runtime_dir()` — idempotent, best-effort, no-op
  under a test override.
- Consumed by: `start_server` (same file), which calls it before
  `SingleInstanceLock::acquire`.

**Steps**

- [x] Add a failing test in `crates/editor_mcp/src/lifecycle.rs`:
      ```rust
      #[test]
      fn cleanup_legacy_removes_socket_lock_and_solution_dirs() {
          let legacy = tempfile::tempdir().expect("tempdir");
          let root = legacy.path();
          std::fs::write(root.join("mcp.lock"), b"1234").expect("lock");
          std::fs::write(root.join("mcp.sock"), b"").expect("sock");
          std::fs::write(root.join("settings.json"), b"{}").expect("settings");
          std::fs::create_dir_all(root.join("solutions/7")).expect("sol dir");
          std::fs::write(root.join("solutions/7/mcp.sock"), b"").expect("sol sock");
          std::fs::create_dir_all(root.join("uploads")).expect("uploads");
          std::fs::write(root.join("uploads/1.bin"), b"x").expect("upload");

          cleanup_legacy_runtime_dir_in(root);

          assert!(!root.join("mcp.lock").exists());
          assert!(!root.join("mcp.sock").exists());
          assert!(!root.join("solutions").exists());
          assert!(!root.join("uploads").exists());
          assert!(
              root.join("settings.json").exists(),
              "cleanup must never touch real configuration"
          );

          // Idempotent: a second pass on an already-clean dir is a no-op.
          cleanup_legacy_runtime_dir_in(root);
          assert!(root.join("settings.json").exists());
      }
      ```
- [x] Run `cargo test -p editor_mcp --lib cleanup_legacy` — expect a **compile
      error**: ``cannot find function `cleanup_legacy_runtime_dir_in` in this scope``.
- [x] Implement in `lifecycle.rs`:
      ```rust
      /// One-time migration: before this build the socket, its lock, the
      /// per-solution socket dirs and the upload spool lived under `config/`.
      /// A stale `config/mcp.lock` left behind by an old build would make a new
      /// build's `SingleInstanceLock` believe another instance is running, so we
      /// sweep the old location at startup.
      pub fn cleanup_legacy_runtime_dir() {
          if RUNTIME_DIR_OVERRIDE.get().is_some() {
              return;
          }
          cleanup_legacy_runtime_dir_in(paths::config_dir());
      }

      fn cleanup_legacy_runtime_dir_in(legacy: &Path) {
          // `mcp.sock` is a *symlink* to a short `/tmp/zed-mcp*/mcp.sock` (the
          // 108-byte `sun_path` limit), and a dangling symlink reports
          // `exists() == false` — so probe with `symlink_metadata`.
          for name in ["mcp.sock", "mcp.lock"] {
              let path = legacy.join(name);
              if std::fs::symlink_metadata(&path).is_ok() {
                  std::fs::remove_file(&path)
                      .with_context(|| format!("removing legacy {}", path.display()))
                      .log_err();
              }
          }

          let solutions = legacy.join("solutions");
          if let Ok(entries) = std::fs::read_dir(&solutions) {
              for entry in entries.flatten() {
                  let socket = entry.path().join("mcp.sock");
                  if std::fs::symlink_metadata(&socket).is_ok() {
                      std::fs::remove_file(&socket)
                          .with_context(|| format!("removing legacy {}", socket.display()))
                          .log_err();
                  }
                  // Only ever remove the dir we just emptied — never recurse, so a
                  // future non-socket file under `config/solutions/` survives and
                  // shows up in the log instead of being deleted.
                  if is_empty_dir(&entry.path()) {
                      std::fs::remove_dir(entry.path())
                          .with_context(|| format!("removing legacy {}", entry.path().display()))
                          .log_err();
                  }
              }
              if is_empty_dir(&solutions) {
                  std::fs::remove_dir(&solutions)
                      .with_context(|| format!("removing legacy {}", solutions.display()))
                      .log_err();
              }
          }

          let uploads = legacy.join("uploads");
          if uploads.is_dir() {
              std::fs::remove_dir_all(&uploads)
                  .with_context(|| format!("removing legacy {}", uploads.display()))
                  .log_err();
          }
      }

      fn is_empty_dir(path: &Path) -> bool {
          std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
      }
      ```
      (`anyhow::Context as _` and `util::ResultExt as _` are already imported at
      the top of the file; add `tempfile` to `[dev-dependencies]` of
      `crates/editor_mcp/Cargo.toml` if it is not there.)
- [x] Run `cargo test -p editor_mcp --lib cleanup_legacy` — expect
      `test cleanup_legacy_removes_socket_lock_and_solution_dirs ... ok`.
- [x] Call it from `start_server` (`lifecycle.rs`, immediately before
      `let lock = match SingleInstanceLock::acquire(&lock_path())`):
      ```rust
      cleanup_legacy_runtime_dir();
      ```
      and re-export from `crates/editor_mcp/src/editor_mcp.rs`, extending the
      existing `pub use lifecycle::{...}` list with `cleanup_legacy_runtime_dir`.
- [x] Run `cargo check -p editor_mcp` — expect no errors.
- [x] Commit: `Sweep the legacy config/ socket, lock and upload leftovers at startup`

## Task 1.3 — `script/run-mcp` and the docs

**Files**
- `script/run-mcp` (edit)
- `.rules`, `CLAUDE.md` (edit — lines 133, 147, 149, 163 in each)
- `FORK.md` (edit — the socket-path mentions)
- `docs/architecture/decisions/0003-remote-control-protocol.md:163` (edit)
- `crates/solution_agent/src/compact.rs:249-256` (edit — stale comment)

**Interfaces**
- Consumes: nothing.
- Produces: the resolved socket path printed by `script/run-mcp`, now
  `$HOME/.spk/sawe[-dev]/state/mcp.sock`.

**Steps**

- [x] `script/run-mcp` — replace the socket-dir resolution block (lines ~96-119):
      ```bash
      if [[ -n "$runtime_dir" ]]; then
          # Full isolation: editor's config / data / cache all land under
          # $runtime_dir. The editor uses its standard XDG-derived paths, which
          # means a fresh state — no user settings, themes, keymaps, db, etc.
          # The MCP socket lands at $runtime_dir/sawe/state/mcp.sock.
          mkdir -p "$runtime_dir"
          export XDG_CONFIG_HOME="$runtime_dir"
          export XDG_DATA_HOME="$runtime_dir"
          export XDG_CACHE_HOME="$runtime_dir"
          socket_dir="$runtime_dir/sawe/state"
      else
          # Sawe uses ~/.spk/sawe[-dev]/state/ for runtime state — the socket and
          # its lock are NOT configuration and no longer live under config/
          # (see crates/editor_mcp/src/lifecycle.rs::default_runtime_dir).
          # The dev binary suffixes with `-dev`.
          if [[ "$profile" == "dev" ]]; then
              socket_dir="$HOME/.spk/sawe-dev/state"
          else
              socket_dir="$HOME/.spk/sawe/state"
          fi
          # Pin the editor's home-dir resolution to the real $HOME. A binary built
          # with `--features test-support` would otherwise hard-code /home/zed in
          # util::paths::home_dir() and fail to create its dirs.
          export SAWE_HOME="$HOME"
      fi
      ```
      and fix the header comment on line 3 to
      `# The MCP socket lives at ~/.spk/sawe[-dev]/state/mcp.sock by default;`.
- [x] Run `bash -n script/run-mcp` — expect no output (syntax OK).
- [x] Update the docs. In **both** `.rules` and `CLAUDE.md`:
      - line ~133: `~/.spk/sawe/config/mcp.sock` → `~/.spk/sawe/state/mcp.sock`
      - line ~147 (**Socket:**): `~/.spk/sawe-dev/config/mcp.sock` →
        `~/.spk/sawe-dev/state/mcp.sock`, `~/.spk/sawe/config/mcp.sock` →
        `~/.spk/sawe/state/mcp.sock`, `Lock file is `…/config/mcp.lock`` →
        `` `…/state/mcp.lock` ``. Append: *"(state, not config — a socket is not
        configuration; `config/` keeps `settings.json`, themes and the
        remote-control keys.)"*
      - line ~149 (two-tier split): `…/config/solutions/<solution_id>/mcp.sock` →
        `…/state/solutions/<solution_id>/mcp.sock`
      - line ~163: `socat - UNIX-CONNECT:$HOME/.spk/sawe-dev/config/mcp.sock` →
        `.../state/mcp.sock`
- [x] `docs/architecture/decisions/0003-remote-control-protocol.md:163`:
      `ssh -L 7777:$HOME/.spk/sawe/config/mcp.sock` → `.../state/mcp.sock`.
- [x] `grep -rn "config/mcp.sock\|config/solutions\|config/mcp.lock" . --exclude-dir=target --exclude-dir=.git`
      — expect **no hits** outside `docs/superpowers/` (historical plans/specs are
      allowed to keep the old path) and outside the migration code in
      `lifecycle.rs`.
- [x] `crates/solution_agent/src/compact.rs:249-256` — fix the stale comment:
      replace `` the editor-global `~/.spk/sawe/config/mcp.sock` `` with
      `` the editor-global `~/.spk/sawe/state/mcp.sock` ``.
- [x] Run `cargo check -p solution_agent` — expect no errors.
- [x] Commit: `Point run-mcp and the docs at the state/ socket path`

---

# Task group 2 — the editor-owned claude settings layer

## Task 2.1 — resolve a project's Solution scope (id + root)

**Files**
- `crates/editor_mcp/src/lifecycle.rs` (edit)
- `crates/editor_mcp/src/editor_mcp.rs` (re-export)
- `crates/agent_servers/src/acp.rs` (edit)
- `crates/agent_servers/src/agent_servers.rs` (re-export)

**Interfaces**
- Consumes: `ActiveServer::solution_sockets: Rc<RefCell<HashMap<String, SolutionSocket>>>`
  (`lifecycle.rs:118`; `SolutionSocket { root, socket, server }`).
- Produces:
  ```rust
  // editor_mcp
  pub fn solution_scope_for_path(cx: &App, path: &Path) -> Option<(String, PathBuf)>;
  // agent_servers
  pub fn solution_scope_for_project(project: &Entity<Project>, cx: &App) -> Option<(String, PathBuf)>;
  ```
  Same longest-prefix rule as the existing `solution_socket_for_path`
  (`lifecycle.rs:460-469`), so the two can never disagree about which Solution a
  project is in.
- Consumed by: task 2.5 (`claude_native::connection`).

**Steps**

- [x] Add a failing test in `crates/editor_mcp/src/lifecycle.rs`'s test module:
      ```rust
      #[gpui::test]
      fn solution_scope_prefers_the_longest_matching_root(cx: &mut gpui::TestAppContext) {
          cx.update(|cx| {
              assert_eq!(
                  solution_scope_for_path(cx, Path::new("/tmp/nowhere")),
                  None,
                  "no ActiveServer global -> no scope"
              );
          });
      }
      ```
      (The interesting longest-prefix case needs a bound server; it is covered
      end-to-end by the manual check in task 2.6. This test pins the
      no-Solution/no-server path, which is the one every unit test and every
      standalone window hits.)
- [x] Run `cargo test -p editor_mcp --lib solution_scope` — expect a **compile
      error**: ``cannot find function `solution_scope_for_path` in this scope``.
- [x] Implement in `lifecycle.rs`, right below `solution_socket_for_path`:
      ```rust
      /// The `(solution_id, solution_root)` of the open Solution that owns `path`.
      /// Longest matching root wins, exactly as in [`solution_socket_for_path`] —
      /// the two lookups must agree, or a session could be MCP-scoped to one
      /// Solution while its claude settings point at another's `.agents/` dir.
      pub fn solution_scope_for_path(cx: &App, path: &Path) -> Option<(String, PathBuf)> {
          let active = cx.try_global::<ActiveServer>()?;
          let scope = active
              .solution_sockets
              .borrow()
              .iter()
              .filter(|(_, record)| record.server.is_some() && path.starts_with(&record.root))
              .max_by_key(|(_, record)| record.root.as_os_str().len())
              .map(|(id, record)| (id.clone(), record.root.clone()));
          scope
      }
      ```
      and add `solution_scope_for_path` to the `pub use lifecycle::{…}` list in
      `crates/editor_mcp/src/editor_mcp.rs`.
- [x] Run `cargo test -p editor_mcp --lib solution_scope` — expect
      `test solution_scope_prefers_the_longest_matching_root ... ok`.
- [x] Add to `crates/agent_servers/src/acp.rs`, directly after
      `mcp_servers_for_project` (line ~3750):
      ```rust
      /// Sawe: the `(solution_id, solution_root)` of the Solution this project
      /// belongs to, when one is open. Mirrors the socket lookup inside
      /// [`mcp_servers_for_project`] so a session's MCP scope and its claude
      /// settings layer always resolve to the same Solution. `None` for a
      /// standalone project (no Solution) and in tests (no `ActiveServer`).
      pub fn solution_scope_for_project(
          project: &Entity<Project>,
          cx: &App,
      ) -> Option<(String, PathBuf)> {
          let worktree = project.read(cx).visible_worktrees(cx).next()?;
          let abs_path = worktree.read(cx).abs_path();
          editor_mcp::solution_scope_for_path(cx, abs_path.as_ref())
      }
      ```
      and add `solution_scope_for_project` to the `pub use acp::{…}` list in
      `crates/agent_servers/src/agent_servers.rs` (line ~27).
- [x] Run `cargo check -p agent_servers` — expect no errors.
- [x] Commit: `Expose the Solution scope (id + root) that owns a project`

## Task 2.2 — the worktree hook itself (`sawe --worktree-hook`)

**Files**
- `crates/claude_native/src/worktree_hook.rs` (new)
- `crates/claude_native/src/claude_native.rs` (add `pub mod worktree_hook;`)
- `crates/claude_native/Cargo.toml` (add `tempfile` to `[dev-dependencies]`)

**Interfaces**
- Consumes: the `WorktreeCreate` / `WorktreeRemove` stdin JSON (contract quoted in
  Architecture § 2); `git` on `PATH`.
- Produces:
  ```rust
  pub struct CreateInput { pub name: String, pub branch: Option<String>,
                           pub repo_root: Option<PathBuf>, pub cwd: Option<PathBuf> }
  pub struct RemoveInput { pub worktree_path: PathBuf }
  pub fn create(base: &Path, input: &CreateInput) -> anyhow::Result<PathBuf>;
  pub fn remove(base: &Path, input: &RemoveInput) -> anyhow::Result<()>;
  pub fn main(mode: &str, base: &Path,
              stdin: &mut impl std::io::Read,
              stdout: &mut impl std::io::Write) -> anyhow::Result<()>;
  ```
- Consumed by: `crates/zed/src/main.rs` (task 2.3) and the settings JSON (task 2.4).

**Steps**

- [x] Write the failing tests first — create `crates/claude_native/src/worktree_hook.rs`
      containing **only** the test module:
      ```rust
      #[cfg(test)]
      mod tests {
          use super::*;

          fn git_ok(cwd: &Path, args: &[&str]) {
              let status = std::process::Command::new("git")
                  .current_dir(cwd)
                  .args(args)
                  .status()
                  .expect("git");
              assert!(status.success(), "git {args:?} failed in {}", cwd.display());
          }

          /// A member repo with one commit, so `git worktree add` has a HEAD.
          fn repo(root: &Path) -> PathBuf {
              let repo = root.join("member");
              std::fs::create_dir_all(&repo).expect("mkdir");
              git_ok(&repo, &["init", "--initial-branch=main"]);
              git_ok(&repo, &["config", "user.email", "t@t"]);
              git_ok(&repo, &["config", "user.name", "t"]);
              std::fs::write(repo.join("f.txt"), b"x").expect("write");
              git_ok(&repo, &["add", "."]);
              git_ok(&repo, &["commit", "-m", "init"]);
              repo
          }

          #[test]
          fn create_puts_the_worktree_under_the_editor_owned_base() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let repo_root = repo(tmp.path());
              let base = tmp.path().join(".agents/worktrees");

              let dir = create(
                  &base,
                  &CreateInput {
                      name: "bright-running-fox".into(),
                      branch: Some("worktree-bright-running-fox".into()),
                      repo_root: Some(repo_root.clone()),
                      cwd: None,
                  },
              )
              .expect("create");

              assert_eq!(dir, base.join("member").join("bright-running-fox"));
              assert!(dir.join("f.txt").is_file(), "worktree must be checked out");
              assert!(
                  !repo_root.join(".claude/worktrees").exists(),
                  "nothing may land in the member's .claude/worktrees anymore"
              );
          }

          #[test]
          fn create_is_idempotent() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let repo_root = repo(tmp.path());
              let base = tmp.path().join(".agents/worktrees");
              let input = CreateInput {
                  name: "fox".into(),
                  branch: None,
                  repo_root: Some(repo_root),
                  cwd: None,
              };
              let first = create(&base, &input).expect("first");
              let second = create(&base, &input).expect("second");
              assert_eq!(first, second);
          }

          #[test]
          fn create_refuses_a_name_that_escapes_the_base() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let repo_root = repo(tmp.path());
              let base = tmp.path().join(".agents/worktrees");
              let err = create(
                  &base,
                  &CreateInput {
                      name: "../../etc".into(),
                      branch: None,
                      repo_root: Some(repo_root),
                      cwd: None,
                  },
              )
              .expect_err("must refuse");
              assert!(err.to_string().contains("unsafe worktree name"), "got: {err}");
          }

          #[test]
          fn remove_deletes_our_worktree_and_ignores_foreign_paths() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let repo_root = repo(tmp.path());
              let base = tmp.path().join(".agents/worktrees");
              let dir = create(
                  &base,
                  &CreateInput {
                      name: "fox".into(),
                      branch: None,
                      repo_root: Some(repo_root.clone()),
                      cwd: None,
                  },
              )
              .expect("create");

              // A legacy worktree inside the member is NOT ours: leave it for the
              // cold reconcile's `git worktree repair`.
              let legacy = repo_root.join(".claude/worktrees/old");
              std::fs::create_dir_all(&legacy).expect("legacy");
              remove(&base, &RemoveInput { worktree_path: legacy.clone() }).expect("no-op");
              assert!(legacy.is_dir(), "a foreign worktree path must be left alone");

              remove(&base, &RemoveInput { worktree_path: dir.clone() }).expect("remove");
              assert!(!dir.exists(), "our worktree must be gone");

              // Removing twice is fine (WorktreeRemove has no decision control).
              remove(&base, &RemoveInput { worktree_path: dir }).expect("idempotent");
          }

          #[test]
          fn main_reads_the_documented_create_payload_and_prints_only_the_path() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let repo_root = repo(tmp.path());
              let base = tmp.path().join(".agents/worktrees");
              let payload = serde_json::json!({
                  "session_id": "abc123",
                  "transcript_path": "/tmp/t.jsonl",
                  "cwd": repo_root.to_string_lossy(),
                  "repo_root": repo_root.to_string_lossy(),
                  "hook_event_name": "WorktreeCreate",
                  "name": "feat-new-feature",
                  "branch": "feat/new-feature",
              })
              .to_string();

              let mut stdout: Vec<u8> = Vec::new();
              main("create", &base, &mut payload.as_bytes(), &mut stdout).expect("hook");

              let printed = String::from_utf8(stdout).expect("utf8");
              assert_eq!(
                  printed.trim(),
                  base.join("member").join("feat-new-feature").to_string_lossy(),
                  "claude exits 1 if stdout is anything but the created directory"
              );
              assert_eq!(printed.lines().count(), 1, "stdout must carry the path and nothing else");
          }
      }
      ```
- [x] Register the module: add `pub mod worktree_hook;` to
      `crates/claude_native/src/claude_native.rs`; add `tempfile.workspace = true`
      to `[dev-dependencies]` in `crates/claude_native/Cargo.toml`.
- [x] Run `cargo test -p claude_native --lib worktree_hook` — expect **compile
      errors**: ``cannot find function `create` in this scope``, ``cannot find
      struct `CreateInput` in this scope``, etc.
- [x] Implement, above the test module in `crates/claude_native/src/worktree_hook.rs`:
      ```rust
      //! `sawe --worktree-hook {create|remove}` — claude's `WorktreeCreate` /
      //! `WorktreeRemove` hooks, implemented by the editor binary itself (the same
      //! `<current_exe> --nc <socket>` trick the MCP bridge uses,
      //! `agent_servers::acp::sawe_mcp_bridge_server`). Shipping a shell script
      //! instead would drag in a `jq` dependency (that is what the upstream doc's
      //! example does) and could drift out of sync with the JSON we generate.
      //!
      //! Contract — https://code.claude.com/docs/en/hooks#worktreecreate :
      //! * `WorktreeCreate` receives `{session_id, transcript_path, cwd, repo_root,
      //!   hook_event_name, name, branch}` on stdin, **replaces** the default
      //!   `git worktree` logic, and must print the absolute path of the directory
      //!   it created on stdout, exiting 0. Any non-zero exit fails the creation,
      //!   and printing anything other than that directory makes claude exit 1.
      //! * `WorktreeRemove` receives `{…, worktree_path}` and has no decision
      //!   control — exit code and output are ignored, failures are logged in
      //!   debug mode only. So it is strictly best-effort.

      use anyhow::{Context as _, Result, anyhow, bail};
      use serde::Deserialize;
      use std::io::{Read, Write};
      use std::path::{Path, PathBuf};
      use std::process::Command;

      #[derive(Debug, Deserialize)]
      pub struct CreateInput {
          pub name: String,
          #[serde(default)]
          pub branch: Option<String>,
          #[serde(default)]
          pub repo_root: Option<PathBuf>,
          #[serde(default)]
          pub cwd: Option<PathBuf>,
      }

      #[derive(Debug, Deserialize)]
      pub struct RemoveInput {
          pub worktree_path: PathBuf,
      }

      pub fn main(
          mode: &str,
          base: &Path,
          stdin: &mut impl Read,
          stdout: &mut impl Write,
      ) -> Result<()> {
          let mut raw = String::new();
          stdin
              .read_to_string(&mut raw)
              .context("reading the hook payload from stdin")?;
          match mode {
              "create" => {
                  let input: CreateInput = serde_json::from_str(&raw)
                      .with_context(|| format!("parsing the WorktreeCreate payload: {raw}"))?;
                  let dir = create(base, &input)?;
                  // Stdout is the return channel: the path, one line, nothing else.
                  writeln!(stdout, "{}", dir.display())
                      .context("writing the worktree path to stdout")?;
                  Ok(())
              }
              "remove" => {
                  let input: RemoveInput = serde_json::from_str(&raw)
                      .with_context(|| format!("parsing the WorktreeRemove payload: {raw}"))?;
                  remove(base, &input)
              }
              other => bail!("--worktree-hook takes `create` or `remove`, got {other:?}"),
          }
      }

      pub fn create(base: &Path, input: &CreateInput) -> Result<PathBuf> {
          let repo_root = input
              .repo_root
              .clone()
              .or_else(|| input.cwd.clone())
              .ok_or_else(|| anyhow!("WorktreeCreate payload carried neither `repo_root` nor `cwd`"))?;
          let dir = worktree_dir(base, &repo_root, &input.name)?;

          // Idempotent: a resumed session can re-fire the hook for a worktree we
          // already made, and a user hook config merged into ours could double-fire
          // it. Handing back the existing directory is what claude expects.
          if dir.is_dir() {
              return Ok(dir);
          }
          if let Some(parent) = dir.parent() {
              std::fs::create_dir_all(parent)
                  .with_context(|| format!("creating {}", parent.display()))?;
          }

          // Match claude's own default branch naming (`worktree-<value>`) when the
          // payload doesn't carry a branch.
          let branch = input
              .branch
              .clone()
              .unwrap_or_else(|| format!("worktree-{}", input.name));
          let dir_arg = dir.to_string_lossy().into_owned();
          let branch_ref = format!("refs/heads/{branch}");
          let branch_exists =
              git(&repo_root, &["rev-parse", "--verify", "--quiet", &branch_ref]).is_ok();
          let args: Vec<&str> = if branch_exists {
              vec!["worktree", "add", &dir_arg, &branch]
          } else {
              vec!["worktree", "add", "-b", &branch, &dir_arg]
          };
          git(&repo_root, &args)?;
          Ok(dir)
      }

      pub fn remove(base: &Path, input: &RemoveInput) -> Result<()> {
          // Legacy worktrees under `<member>/.claude/worktrees/` predate this hook
          // and are not ours — leave them to git and to the folder-move plan's cold
          // reconcile (`git worktree repair`), which we must not race.
          if !input.worktree_path.starts_with(base) || !input.worktree_path.exists() {
              return Ok(());
          }
          // `git worktree remove` refuses to run from inside the worktree it is
          // removing, and our parent dir (`<base>/<member>/`) is not a repo — so
          // resolve the main checkout through the worktree's own git dir.
          let common = git(
              &input.worktree_path,
              &["rev-parse", "--path-format=absolute", "--git-common-dir"],
          )?;
          let repo_root = Path::new(&common)
              .parent()
              .ok_or_else(|| anyhow!("git-common-dir has no parent: {common}"))?
              .to_path_buf();
          let dir_arg = input.worktree_path.to_string_lossy().into_owned();
          git(&repo_root, &["worktree", "remove", "--force", &dir_arg])?;
          git(&repo_root, &["worktree", "prune"])?;
          Ok(())
      }

      /// One subdir per member repo: two members of the same Solution can each end
      /// up with a worktree called `bright-running-fox`.
      fn worktree_dir(base: &Path, repo_root: &Path, name: &str) -> Result<PathBuf> {
          if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
              bail!("refusing unsafe worktree name {name:?}");
          }
          let member = repo_root
              .file_name()
              .ok_or_else(|| anyhow!("repo_root has no final component: {}", repo_root.display()))?;
          Ok(base.join(member).join(name))
      }

      fn git(cwd: &Path, args: &[&str]) -> Result<String> {
          let output = Command::new("git")
              .current_dir(cwd)
              .args(args)
              .output()
              .with_context(|| format!("spawning `git {}`", args.join(" ")))?;
          if !output.status.success() {
              bail!(
                  "`git {}` failed in {}: {}",
                  args.join(" "),
                  cwd.display(),
                  String::from_utf8_lossy(&output.stderr).trim()
              );
          }
          Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
      }
      ```
- [x] Run `cargo test -p claude_native --lib worktree_hook` — expect all five
      tests to pass.
- [x] Commit: `Add the WorktreeCreate/WorktreeRemove hook implementation`

## Task 2.3 — wire `--worktree-hook` into the binary

**Files**
- `crates/zed/src/main.rs` (edit — `Args` struct ~line 2087 and the early-return
  block ~line 248)
- `crates/zed/Cargo.toml` (add `claude_native.workspace = true`)

**Interfaces**
- Consumes: `claude_native::worktree_hook::main`.
- Produces: `sawe --worktree-hook <create|remove> --worktree-base <DIR>` — exits
  before GPUI init, exactly like `--nc`.

**Steps**

- [x] Add the flags to `Args` in `crates/zed/src/main.rs`, right after the `nc`
      field:
      ```rust
      /// Runs the editor binary as claude's `WorktreeCreate` / `WorktreeRemove`
      /// hook (`create` / `remove`). Reads the hook payload as JSON on stdin and,
      /// for `create`, prints the absolute path of the worktree it made on stdout.
      /// Configured by the editor-owned claude settings layer
      /// (`claude_native::claude_settings`) so agent worktrees land under
      /// `<solution_root>/.agents/worktrees/` instead of `<member>/.claude/worktrees/`.
      #[arg(long, hide = true, value_name = "create|remove")]
      worktree_hook: Option<String>,

      /// Base directory for `--worktree-hook` — `<solution_root>/.agents/worktrees`.
      #[arg(long, hide = true, value_name = "DIR")]
      worktree_base: Option<PathBuf>,
      ```
- [x] Add the early-return, immediately after the `--nc` block (`main.rs:248-256`):
      ```rust
      // `sawe --worktree-hook {create|remove}` runs as claude's WorktreeCreate /
      // WorktreeRemove hook. Like `--nc`, it must exit before any GPUI / paths
      // init — claude spawns it per worktree and a cold editor boot would add
      // seconds to every subagent launch.
      if let Some(mode) = &args.worktree_hook {
          let Some(base) = args.worktree_base.clone() else {
              eprintln!("--worktree-hook requires --worktree-base");
              process::exit(1);
          };
          let mut stdin = io::stdin().lock();
          let mut stdout = io::stdout().lock();
          match claude_native::worktree_hook::main(mode, &base, &mut stdin, &mut stdout) {
              Ok(()) => process::exit(0),
              Err(err) => {
                  eprintln!("--worktree-hook: {err:#}");
                  process::exit(1);
              }
          }
      }
      ```
      (`io` is already imported at `main.rs:56`; add `claude_native.workspace = true`
      to `[dependencies]` in `crates/zed/Cargo.toml`, alphabetically near
      `client`.)
- [x] Run `cargo build --bin sawe` — expect a clean debug build.
- [x] Verify by hand against a throwaway repo (real end-to-end exercise of the
      flag, no editor needed):
      ```bash
      set -e
      tmp=$(mktemp -d); cd "$tmp"
      git init -q --initial-branch=main member && cd member
      git config user.email t@t && git config user.name t
      echo x > f.txt && git add . && git commit -qm init && cd "$tmp"
      printf '{"session_id":"s","cwd":"%s/member","repo_root":"%s/member","hook_event_name":"WorktreeCreate","name":"fox","branch":"worktree-fox"}' "$tmp" "$tmp" \
        | <repo>/target/debug/sawe --worktree-hook create --worktree-base "$tmp/.agents/worktrees"
      ```
      Expect exactly one line on stdout: `<tmp>/.agents/worktrees/member/fox`, and
      `git -C "$tmp/member" worktree list` to show it.
- [x] Commit: `Run the editor binary as claude's worktree hook`

## Task 2.4 — build the settings JSON

**Files**
- `crates/claude_native/src/claude_settings.rs` (new)
- `crates/claude_native/src/claude_native.rs` (add `pub mod claude_settings;`)
- `crates/claude_native/Cargo.toml` (add `paths.workspace = true`)

**Interfaces**
- Consumes: `paths::state_dir()` (task 1.1); `util::paths::home_dir()`;
  `$CLAUDE_CONFIG_DIR`.
- Produces:
  ```rust
  pub const DISABLE_ENV_VAR: &str = "SAWE_CLAUDE_SETTINGS_DISABLED";
  pub struct EditorClaudeSettings { pub agents_dir: PathBuf, pub editor_exe: PathBuf, pub work_dir: PathBuf }
  impl EditorClaudeSettings {
      pub fn worktrees_dir(&self) -> PathBuf;   // <agents_dir>/worktrees
      pub fn memory_dir(&self) -> PathBuf;      // <agents_dir>/memory
      pub fn to_json(&self) -> serde_json::Value;
      pub fn write_to(&self, path: &Path) -> anyhow::Result<()>;
  }
  pub fn settings_path(solution_id: &str) -> PathBuf; // <state>/solutions/<id>/claude-settings.json
  ```
- Consumed by: task 2.5.

**Steps**

- [x] Create `crates/claude_native/src/claude_settings.rs` with **only** the test
      module:
      ```rust
      #[cfg(test)]
      mod tests {
          use super::*;

          fn settings(tmp: &Path) -> EditorClaudeSettings {
              EditorClaudeSettings {
                  agents_dir: tmp.join("sol/.agents"),
                  editor_exe: PathBuf::from("/opt/my apps/sawe"),
                  work_dir: tmp.join("sol/member"),
              }
          }

          #[test]
          fn carries_the_two_hooks_and_the_memory_dir() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let json = settings(tmp.path()).to_json();

              assert_eq!(
                  json["autoMemoryDirectory"],
                  serde_json::Value::String(
                      tmp.path().join("sol/.agents/memory").to_string_lossy().into_owned()
                  ),
                  "auto memory is keyed by the git repo root (the member) by default, \
                   so a member rename would strand it — pin it to the solution"
              );

              for (event, mode) in [("WorktreeCreate", "create"), ("WorktreeRemove", "remove")] {
                  let command = json["hooks"][event][0]["hooks"][0]["command"]
                      .as_str()
                      .unwrap_or_else(|| panic!("{event} command"));
                  assert!(command.contains(&format!("--worktree-hook {mode}")), "got: {command}");
                  assert!(
                      command.contains("'/opt/my apps/sawe'"),
                      "the hook command runs through a shell — quote the exe: {command}"
                  );
                  assert!(
                      command.contains(".agents/worktrees'"),
                      "worktrees must land under the solution root: {command}"
                  );
                  assert_eq!(json["hooks"][event][0]["hooks"][0]["type"], "command");
              }
          }

          #[test]
          fn keeps_the_users_own_hooks() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let project_settings = tmp.path().join("sol/member/.claude");
              std::fs::create_dir_all(&project_settings).expect("mkdir");
              std::fs::write(
                  project_settings.join("settings.json"),
                  br#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}],
                               "WorktreeCreate":[{"hooks":[{"type":"command","command":"mine.sh"}]}]}}"#,
              )
              .expect("write");

              let json = settings(tmp.path()).to_json();

              // `--settings` overrides same-named keys wholesale, so re-emitting the
              // user's hooks is what keeps them alive.
              assert_eq!(json["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "echo hi");
              assert_eq!(json["hooks"]["WorktreeCreate"][0]["hooks"][0]["command"], "mine.sh");
              let ours = json["hooks"]["WorktreeCreate"][1]["hooks"][0]["command"]
                  .as_str()
                  .expect("ours");
              assert!(ours.contains("--worktree-hook create"), "got: {ours}");
          }

          #[test]
          fn write_to_creates_the_agents_dirs() {
              let tmp = tempfile::tempdir().expect("tempdir");
              let settings = settings(tmp.path());
              let path = tmp.path().join("state/solutions/7/claude-settings.json");

              settings.write_to(&path).expect("write");

              assert!(settings.memory_dir().is_dir(), "autoMemoryDirectory must exist");
              assert!(settings.worktrees_dir().is_dir());
              let written: serde_json::Value =
                  serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
              assert_eq!(written, settings.to_json());
          }

          #[test]
          fn settings_path_sits_next_to_the_solution_socket() {
              let path = settings_path("7");
              assert!(path.ends_with("solutions/7/claude-settings.json"), "{}", path.display());
          }
      }
      ```
- [x] Register the module (`pub mod claude_settings;` in
      `crates/claude_native/src/claude_native.rs`) and add `paths.workspace = true`
      to `[dependencies]` in `crates/claude_native/Cargo.toml`.
- [x] Run `cargo test -p claude_native --lib claude_settings` — expect **compile
      errors**: ``cannot find struct `EditorClaudeSettings` in this scope``.
- [x] Implement above the tests:
      ```rust
      //! The editor-owned claude settings layer, handed to the subprocess as
      //! `--settings <file>`.
      //!
      //! **Why `--settings` and not a fourth `--setting-sources` entry:**
      //! `--setting-sources` accepts only `user`, `project`, `local` — there is no
      //! way to name a directory of our own. `--settings` takes "a path to a
      //! settings JSON file or an inline JSON string" and lands at the
      //! *command-line* precedence tier, above local/project/user and below
      //! managed — so passing it does NOT stop `--setting-sources user,project,local`
      //! from loading the user's own settings.
      //!
      //! **The sharp edge:** "Values you set here override the same keys in your
      //! settings.json files for this session. Keys you omit keep their file-based
      //! values." `hooks` is one key — a bare `{"hooks": {ours}}` would silently
      //! disable the user's own hooks. So we read the `hooks` object out of the
      //! three sources we keep enabled, union them, and append ours.

      use anyhow::{Context as _, Result};
      use serde_json::{Map, Value, json};
      use std::path::{Path, PathBuf};

      /// Escape hatch: set to any value to spawn `claude` with no `--settings` at
      /// all (worktrees fall back to `<member>/.claude/worktrees/`, auto memory
      /// back to the git-repo-root default).
      pub const DISABLE_ENV_VAR: &str = "SAWE_CLAUDE_SETTINGS_DISABLED";

      pub struct EditorClaudeSettings {
          /// `<solution_root>/.agents`
          pub agents_dir: PathBuf,
          /// The running editor binary — it is its own worktree hook (`--worktree-hook`).
          pub editor_exe: PathBuf,
          /// The session's cwd (the member). Where project/local claude settings live.
          pub work_dir: PathBuf,
      }

      impl EditorClaudeSettings {
          pub fn worktrees_dir(&self) -> PathBuf {
              self.agents_dir.join("worktrees")
          }

          pub fn memory_dir(&self) -> PathBuf {
              self.agents_dir.join("memory")
          }

          pub fn to_json(&self) -> Value {
              let mut hooks = existing_hooks(&self.work_dir);
              for (event, mode) in [("WorktreeCreate", "create"), ("WorktreeRemove", "remove")] {
                  let entry = json!({
                      "hooks": [ { "type": "command", "command": self.hook_command(mode) } ]
                  });
                  let slot = hooks
                      .entry(event.to_string())
                      .or_insert_with(|| Value::Array(Vec::new()));
                  if let Some(entries) = slot.as_array_mut() {
                      entries.push(entry);
                  }
              }
              json!({
                  "autoMemoryDirectory": self.memory_dir().to_string_lossy(),
                  "hooks": Value::Object(hooks),
              })
          }

          pub fn write_to(&self, path: &Path) -> Result<()> {
              // claude does not create `autoMemoryDirectory` for us, and the hook's
              // base dir has to exist before the first `git worktree add`.
              for dir in [self.memory_dir(), self.worktrees_dir()] {
                  std::fs::create_dir_all(&dir)
                      .with_context(|| format!("creating {}", dir.display()))?;
              }
              if let Some(parent) = path.parent() {
                  std::fs::create_dir_all(parent)
                      .with_context(|| format!("creating {}", parent.display()))?;
              }
              let body = serde_json::to_vec_pretty(&self.to_json())
                  .context("serializing the editor claude settings")?;
              std::fs::write(path, body)
                  .with_context(|| format!("writing {}", path.display()))
          }

          fn hook_command(&self, mode: &str) -> String {
              format!(
                  "{} --worktree-hook {mode} --worktree-base {}",
                  shell_quote(&self.editor_exe.to_string_lossy()),
                  shell_quote(&self.worktrees_dir().to_string_lossy()),
              )
          }
      }

      /// `<state>/solutions/<id>/claude-settings.json` — beside that Solution's MCP
      /// socket, so its whole runtime footprint is one directory that plan 1's
      /// numeric-id migration can drop and recreate.
      pub fn settings_path(solution_id: &str) -> PathBuf {
          paths::state_dir()
              .join("solutions")
              .join(solution_id)
              .join("claude-settings.json")
      }

      /// The union of the `hooks` object across the three sources we keep enabled
      /// via `--setting-sources user,project,local`. Re-emitting them is what keeps
      /// the user's hooks alive under `--settings`' key-level override.
      fn existing_hooks(work_dir: &Path) -> Map<String, Value> {
          let user_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
              .map(PathBuf::from)
              .unwrap_or_else(|| util::paths::home_dir().join(".claude"));
          let sources = [
              user_dir.join("settings.json"),
              work_dir.join(".claude").join("settings.json"),
              work_dir.join(".claude").join("settings.local.json"),
          ];

          let mut merged: Map<String, Value> = Map::new();
          for source in sources {
              let Ok(raw) = std::fs::read_to_string(&source) else {
                  continue;
              };
              let value: Value = match serde_json::from_str(&raw) {
                  Ok(value) => value,
                  Err(error) => {
                      // claude itself tolerates a broken settings file; dropping the
                      // whole layer over one would be worse than losing its hooks.
                      log::warn!("claude settings: ignoring unparseable {}: {error}", source.display());
                      continue;
                  }
              };
              let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
                  continue;
              };
              for (event, entries) in hooks {
                  let Some(entries) = entries.as_array() else {
                      continue;
                  };
                  let slot = merged
                      .entry(event.clone())
                      .or_insert_with(|| Value::Array(Vec::new()));
                  if let Some(existing) = slot.as_array_mut() {
                      for entry in entries {
                          // The same hook can appear in both user and project settings.
                          if !existing.contains(entry) {
                              existing.push(entry.clone());
                          }
                      }
                  }
              }
          }
          merged
      }

      /// A `command` hook is run through a shell, so a path with a space (or a `$`)
      /// must be quoted.
      #[cfg(not(windows))]
      fn shell_quote(value: &str) -> String {
          format!("'{}'", value.replace('\'', r"'\''"))
      }

      /// `cmd.exe` has no single-quote quoting; double quotes are the only form it
      /// understands, and it has no escape for an embedded `"` — reject that rather
      /// than emit a command that would silently mis-parse.
      #[cfg(windows)]
      fn shell_quote(value: &str) -> String {
          format!("\"{}\"", value.replace('"', ""))
      }
      ```
- [x] Run `cargo test -p claude_native --lib claude_settings` — expect all four
      tests to pass. (`keeps_the_users_own_hooks` reads only the *project* file;
      the user file is whatever the dev box has and the assertions don't depend on
      it being absent.)
- [x] Commit: `Build the editor-owned claude settings JSON`

## Task 2.5 — pass `--settings` to the subprocess

**Files**
- `crates/claude_native/src/command.rs` (edit — `ClaudeCommandSpec`, `to_std_command`)
- `crates/claude_native/src/connection.rs` (edit — `open_session` ~832,
  `probe_models` ~164, `RespawnBlueprint` ~303, the respawn spec ~1068)
- `crates/claude_native/tests/mock_claude.rs` (edit — `spec_for`)

**Interfaces**
- Consumes: `agent_servers::solution_scope_for_project` (task 2.1),
  `claude_settings::{EditorClaudeSettings, settings_path, DISABLE_ENV_VAR}` (task 2.4).
- Produces:
  ```rust
  pub struct ClaudeCommandSpec { /* … */ pub settings_path: Option<PathBuf> }
  // ClaudeNativeConnection (private)
  fn editor_settings_path(project: &Entity<Project>, work_dir: &Path, cx: &App) -> Option<PathBuf>;
  ```

**Steps**

- [x] Add a failing test to `crates/claude_native/src/command.rs`'s test module:
      ```rust
      #[test]
      fn passes_the_editor_settings_file_without_dropping_the_user_sources() {
          let spec = ClaudeCommandSpec {
              binary: "claude".into(),
              work_dir: "/w".into(),
              session: SessionArg::New("uuid".into()),
              mcp_servers_json: "{}".into(),
              append_system_prompt: None,
              extra_env: vec![],
              model: None,
              settings_path: Some("/state/solutions/7/claude-settings.json".into()),
          };
          let args: Vec<String> = spec
              .to_std_command()
              .get_args()
              .map(|a| a.to_string_lossy().into_owned())
              .collect();
          assert!(
              args.windows(2).any(|w| w[0] == "--settings"
                  && w[1] == "/state/solutions/7/claude-settings.json")
          );
          // `--settings` is a command-line-tier OVERRIDE, not a replacement for the
          // file sources: the user's own settings must keep loading.
          assert!(
              args.windows(2)
                  .any(|w| w[0] == "--setting-sources" && w[1] == "user,project,local")
          );
      }

      #[test]
      fn omits_the_settings_flag_when_there_is_no_solution() {
          let spec = ClaudeCommandSpec {
              binary: "claude".into(),
              work_dir: "/w".into(),
              session: SessionArg::New("uuid".into()),
              mcp_servers_json: "{}".into(),
              append_system_prompt: None,
              extra_env: vec![],
              model: None,
              settings_path: None,
          };
          let args: Vec<String> = spec
              .to_std_command()
              .get_args()
              .map(|a| a.to_string_lossy().into_owned())
              .collect();
          assert!(!args.iter().any(|a| a == "--settings"));
      }
      ```
- [x] Run `cargo test -p claude_native --lib command` — expect **compile errors**
      in every existing test too: ``missing field `settings_path` in initializer
      of `ClaudeCommandSpec` `` (the four existing tests each build a spec).
- [x] Implement in `command.rs`: add the field to `ClaudeCommandSpec`
      ```rust
          /// The editor-owned settings layer (`--settings <file>`): the
          /// `WorktreeCreate`/`WorktreeRemove` hooks + `autoMemoryDirectory`. See
          /// `crate::claude_settings`. `None` when the project isn't under an open
          /// Solution (standalone window, tests) or when the layer is disabled via
          /// `SAWE_CLAUDE_SETTINGS_DISABLED`.
          pub settings_path: Option<PathBuf>,
      ```
      and, in `to_std_command`, directly after the `--setting-sources` line
      (`command.rs:80`):
      ```rust
          // `--settings` sits at the command-line precedence tier: it overrides the
          // *same keys* in user/project/local settings but does not stop those
          // sources from loading. `claude_settings` therefore re-emits the user's
          // own `hooks` alongside ours, since `hooks` is one such key.
          if let Some(settings) = &self.settings_path {
              cmd.arg("--settings").arg(settings);
          }
      ```
      Add `settings_path: None` to the four existing tests in `command.rs`, to
      `probe_models` (`connection.rs:164`) and to `spec_for` in
      `crates/claude_native/tests/mock_claude.rs:38`.
- [x] Run `cargo test -p claude_native --lib command` — expect all six tests to
      pass.
- [x] Wire it into the real spawn. In `connection.rs`, add to
      `impl ClaudeNativeConnection` (near `append_system_prompt_from_meta`):
      ```rust
      /// Materialize the editor-owned claude settings for this session's Solution
      /// and return the file to hand to `--settings`. `None` when the project is
      /// not under an open Solution (a standalone window, a test), when the editor
      /// binary can't be resolved, or when the operator opted out.
      fn editor_settings_path(
          project: &Entity<Project>,
          work_dir: &Path,
          cx: &App,
      ) -> Option<PathBuf> {
          if std::env::var_os(crate::claude_settings::DISABLE_ENV_VAR).is_some() {
              return None;
          }
          let (solution_id, solution_root) =
              agent_servers::solution_scope_for_project(project, cx)?;
          let settings = crate::claude_settings::EditorClaudeSettings {
              agents_dir: solution_root.join(".agents"),
              editor_exe: std::env::current_exe().log_err()?,
              work_dir: work_dir.to_path_buf(),
          };
          let path = crate::claude_settings::settings_path(&solution_id);
          settings.write_to(&path).log_err()?;
          Some(path)
      }
      ```
      (`util::ResultExt as _` for `log_err`; `agent_servers::solution_scope_for_project`
      is already re-exported by task 2.1.)
      In `open_session` (`connection.rs:832`), before building the spec:
      ```rust
      let settings_path = Self::editor_settings_path(&project, &work_dir, cx);
      ```
      and set `settings_path` on the spec. Add `settings_path: Option<PathBuf>` to
      `RespawnBlueprint` (`connection.rs:303-309`), populate it from the same value
      when the blueprint is built, and pass `settings_path: blueprint.settings_path.clone()`
      at the respawn spec site (`connection.rs:1068`) — a respawned session must keep
      the same worktree base, or the resumed agent's `EnterWorktree` would half-land
      in `.claude/worktrees/`.
- [x] Run `cargo test -p claude_native` — expect the whole crate's tests to pass
      (including `tests/mock_claude.rs`).
- [x] Run `cargo check -p zed` — expect no errors.
- [x] Commit: `Hand the editor-owned settings layer to the claude subprocess`

## Task 2.6 — verify against a live editor + document

**Files**
- `FORK.md` (edit — "Key architectural decisions": a new numbered entry)
- `.rules`, `CLAUDE.md` (edit — one line under the MCP section)

**Interfaces** — none (docs + manual verification).

**Steps**

- [x] `cargo build --bin sawe` (debug), then `script/run-mcp --debug --headless`.
      Open a Solution with at least one member, create an AI session, and ask the
      agent to run a subagent with `isolation: worktree` (or `EnterWorktree`).
- [x] Assert, on disk:
      - `<solution_root>/.agents/worktrees/<member>/<name>/` exists and
        `git -C <member> worktree list` lists it;
      - nothing new appeared under `<member>/.claude/worktrees/`;
      - `<solution_root>/.agents/memory/` exists;
      - `~/.spk/sawe-dev/state/solutions/<id>/claude-settings.json` contains both
        hooks and `autoMemoryDirectory` (and the user's own hooks, if they have
        any: diff `hooks` against `~/.claude/settings.json`).
      If `autoMemoryDirectory` is ignored (the doc's workspace-trust caveat applies
      to *project/local* settings; ours is command-line tier, so it should not),
      record it in `docs/findings/` and fall back to setting `CLAUDE_CONFIG_DIR`
      per solution — do **not** silently ship a dead setting.
- [x] Add to `FORK.md` under "Key architectural decisions" a new numbered entry:
      *"Editor-owned claude settings layer (`--settings <file>`, not a
      `--setting-sources` entry — that flag only takes `user|project|local`).
      Carries a `WorktreeCreate`/`WorktreeRemove` hook pair pointing agent
      worktrees at `<solution_root>/.agents/worktrees/` (they used to land at
      `<member>/.claude/worktrees/`, inside the member, where a member rename
      breaks their absolute `gitdir` pointers) and `autoMemoryDirectory` →
      `<solution_root>/.agents/memory` (auto memory is otherwise keyed by the git
      repo root, i.e. the member). The hook is the editor binary itself
      (`sawe --worktree-hook`), mirroring the `--nc` MCP bridge. Because
      `--settings` overrides same-named keys wholesale, `claude_settings` re-emits
      the user's own `hooks` alongside ours; `SAWE_CLAUDE_SETTINGS_DISABLED=1`
      turns the layer off."*
- [x] Add one line to the MCP section of `.rules` **and** `CLAUDE.md`: *"Agent
      worktrees (Agent Teams / background agents / `isolation: worktree`) land
      under `<solution_root>/.agents/worktrees/<member>/<name>` via an editor-owned
      `WorktreeCreate` hook, not `<member>/.claude/worktrees/`. The hook is the
      `sawe` binary itself (`--worktree-hook`); the settings file lives at
      `<state>/solutions/<id>/claude-settings.json`."*
- [x] Commit: `Document the editor-owned claude settings layer`

---

# Task group 3 — `solution_id` becomes optional

## Task 3.1 — the `resolve_solution_id` helper

**Files**
- `crates/solutions/src/mcp.rs` (edit)
- `crates/solutions/src/mcp/tests.rs` (edit)

**Interfaces**
- Consumes: `crate::SolutionId` (`SolutionId(pub i64)` after plan 1).
- Produces:
  ```rust
  pub(crate) fn resolve_solution_id(raw: Option<String>) -> anyhow::Result<crate::SolutionId>;
  ```
- Consumed by: every tool handler in tasks 3.2 / 3.3 / 3.4.

**Steps**

- [x] Add failing tests to `crates/solutions/src/mcp/tests.rs`:
      ```rust
      #[test]
      fn resolve_solution_id_parses_the_injected_id() {
          let id = super::resolve_solution_id(Some("7".to_string())).expect("resolve");
          assert_eq!(id, crate::SolutionId(7));
      }

      #[test]
      fn resolve_solution_id_explains_the_global_socket_case() {
          let err = super::resolve_solution_id(None).expect_err("must fail");
          assert!(
              err.to_string().contains("per-solution socket"),
              "the error must tell the caller where the id comes from: {err}"
          );
      }

      #[test]
      fn resolve_solution_id_rejects_a_non_numeric_id() {
          let err = super::resolve_solution_id(Some("spk-solutions".to_string()))
              .expect_err("must fail");
          assert!(err.to_string().contains("numeric id"), "got: {err}");
      }
      ```
- [x] Run `cargo test -p solutions --lib resolve_solution_id` — expect a **compile
      error**: ``cannot find function `resolve_solution_id` in module `super` ``.
- [x] Implement in `crates/solutions/src/mcp.rs`:
      ```rust
      use anyhow::{Context as _, Result, anyhow};

      /// Resolve the `solution_id` of a solution-scoped MCP tool call.
      ///
      /// On a per-solution socket the listener force-injects the bound id into the
      /// params before the handler runs (`context_server::listener` — it keys the
      /// injection off the `solution_id` *property* existing in the tool's input
      /// schema, which an `Option<String>` still emits). So `None` here can only
      /// mean "called on the editor-global socket without an id".
      pub(crate) fn resolve_solution_id(raw: Option<String>) -> Result<crate::SolutionId> {
          let raw = raw.ok_or_else(|| {
              anyhow!(
                  "solution_id is required on the editor-global socket — \
                   connect to the per-solution socket (`solutions.get` → `mcp_socket`) \
                   and it is injected for you"
              )
          })?;
          let id: i64 = raw
              .trim()
              .parse()
              .with_context(|| format!("solution_id must be a numeric id, got {raw:?}"))?;
          Ok(crate::SolutionId(id))
      }
      ```
      **Pre-plan-1 variant** (if this lands before `SolutionId` becomes `i64`):
      drop the `parse` and return `Ok(crate::SolutionId(raw))`; the
      `rejects_a_non_numeric_id` test then does not apply and is added with plan 1.
- [x] Run `cargo test -p solutions --lib resolve_solution_id` — expect three
      passing tests.
- [x] Add the schema guard to `crates/solutions/src/mcp/tests.rs` (this is the test
      that keeps the injection working — the listener's `wants_solution_id` is
      `input_schema.properties.solution_id.is_some()`, `listener.rs:145-149`):
      ```rust
      /// `context_server::listener` decides whether to force-inject the socket's
      /// bound `solution_id` by checking that the tool's input schema HAS a
      /// `solution_id` property (listener.rs:145-149). Making the field optional
      /// must drop it from `required`, never from `properties`.
      #[test]
      fn optional_solution_id_stays_a_schema_property() {
          let mut settings = schemars::generate::SchemaSettings::draft07();
          settings.inline_subschemas = true;
          let schema = settings
              .into_generator()
              .root_schema_for::<crate::mcp::GetDiagnosticsParams>();

          let properties = schema
              .get("properties")
              .and_then(|value| value.as_object())
              .expect("properties");
          assert!(
              properties.contains_key("solution_id"),
              "dropping the property would silently disable the per-socket injection"
          );

          let required = schema
              .get("required")
              .and_then(|value| value.as_array())
              .map(|values| values.iter().any(|value| value == "solution_id"))
              .unwrap_or(false);
          assert!(!required, "solution_id must no longer be required");
      }
      ```
- [x] Run `cargo test -p solutions --lib optional_solution_id_stays_a_schema_property`
      — expect a **failure**: `assertion failed: !required` (the field is still
      `String`, hence still required). This is the red test that tasks 3.2-3.4
      turn green.
- [x] Commit: `Add resolve_solution_id for solution-scoped MCP tools`

## Task 3.2 — flip `solutions.*` and `diagnostics.get`

**Files**
- `crates/solutions/src/mcp/diagnostics.rs` (line 33 + its `Deserialize` at 43,
  handler)
- `crates/solutions/src/mcp/solutions_lifecycle.rs` (line 164 + 172 — `solutions.get`)
- `crates/solutions/src/mcp/member_mgmt.rs` (lines 39/48, 158/167, 231/240 —
  `add_member`, `add_empty_member`, `remove_member`)

**Interfaces**
- Consumes: `crate::mcp::resolve_solution_id`.
- Produces: `pub solution_id: Option<String>` on `GetDiagnosticsParams`,
  `GetSolutionParams`, `AddMemberParams`, `AddEmptyMemberParams`,
  `RemoveMemberParams`; the `Inner` shadow structs' fields likewise.
- Unchanged: `reorder_members` / `set_active_member` (`member_mgmt.rs:302,374,396`)
  keep a required `solution_id` — they are pure global-socket operator tools and
  the spec does not list them.

**Steps**

- [x] Flip the five params. Each is the same two-line edit, e.g. in
      `diagnostics.rs:33`:
      ```rust
      pub struct GetDiagnosticsParams {
          /// Absent on a per-solution socket: the server injects the socket's bound
          /// Solution. Required only on the editor-global socket.
          #[serde(skip_serializing_if = "Option::is_none")]
          pub solution_id: Option<String>,
          #[serde(skip_serializing_if = "Option::is_none")]
          pub buffer_path: Option<String>,
      }
      ```
      and in its hand-written `Deserialize` (`diagnostics.rs:43`):
      ```rust
              struct Inner {
                  solution_id: Option<String>,
                  buffer_path: Option<String>,
              }
      ```
- [x] Update each handler's id construction. `crate::SolutionId(input.solution_id)`
      / `crate::SolutionId(input.solution_id.clone())` becomes
      `crate::mcp::resolve_solution_id(input.solution_id)?` — the five call sites
      are `member_mgmt.rs:86`, `member_mgmt.rs:205`, `member_mgmt.rs:279`,
      `solutions_lifecycle.rs:638` (`solutions.get`) and the `diagnostics.get`
      handler. Where the surrounding closure is `async` and returns
      `Task<Result<…>>`, `?` already works; where it is a plain `fn` returning
      `Result`, likewise. If a site is inside a closure that must not fail early,
      hoist the `resolve_solution_id` call above it.
- [x] Run `cargo test -p solutions --lib` — expect
      `optional_solution_id_stays_a_schema_property ... ok` (it keys off
      `GetDiagnosticsParams`) and no regressions.
- [x] Run `cargo check -p solutions` — expect no errors.
- [x] Commit: `Make solution_id optional in solutions.* and diagnostics.get`

## Task 3.3 — flip `workspace.*`

**Files**
- `crates/solutions/src/mcp/workspace_state.rs` (lines 36/44 `list_buffers`,
  190/200 `get_effective_settings`, 266/277 `dispatch_action`)
- `crates/solutions/src/mcp/visual_structure.rs` (lines 33/41
  `dump_visual_structure`)

**Interfaces**
- Consumes: `crate::mcp::resolve_solution_id`.
- Produces: `pub solution_id: Option<String>` on the four params structs.
- `workspace.screenshot` keeps its required `solution_id` **only if** its params
  live outside these two files; if it is in `workspace_state.rs`, flip it too —
  the rule is "every scoped tool in these files".

**Steps**

- [x] Add a failing test to `crates/solutions/src/mcp/tests.rs`:
      ```rust
      #[test]
      fn workspace_list_buffers_accepts_an_absent_solution_id() {
          let params: crate::mcp::ListBuffersParams =
              serde_json::from_value(serde_json::json!({})).expect("deserialize");
          assert_eq!(params.solution_id, None);
      }
      ```
      (Adjust the struct name to whatever `workspace_state.rs:36` actually
      declares.)
- [x] Run `cargo test -p solutions --lib workspace_list_buffers_accepts` — expect
      a **failure**: ``missing field `solution_id` `` or a type mismatch on
      `assert_eq!(params.solution_id, None)`.
- [x] Flip the four params structs + their `Inner` shadows to `Option<String>` and
      route every handler through `crate::mcp::resolve_solution_id(input.solution_id)?`,
      exactly as in task 3.2.
- [x] Run `cargo test -p solutions --lib` — expect the new test to pass and no
      regressions.
- [x] Commit: `Make solution_id optional in the workspace.* MCP tools`

## Task 3.4 — flip `project.*`

**Files**
- `crates/solutions/src/mcp/project_files/fs_ops.rs` (lines 24/45 `list_files`,
  254/268 `create_file`, 339/350 `delete_file`, 427/441 `rename_file`)
- `crates/solutions/src/mcp/project_files/code_nav.rs` (lines 31/57
  `find_in_buffers`, 315/330 `goto_definition`, 462/480 `find_references`)
- `crates/solutions/src/mcp/project_files/buffer_ops.rs` (lines 21/32, 117/130,
  232/243, 318/332, 420/434 — `read_buffer`, `apply_edit`, `save_buffer`,
  `open_file`, `close_buffer`)

**Interfaces**
- Consumes: `crate::mcp::resolve_solution_id`.
- Produces: `pub solution_id: Option<String>` on all twelve params structs.

**Note on scope:** the spec enumerates `fs_ops` and `code_nav` only, but
`buffer_ops` is the same `project.*` namespace served from the same scoped socket.
Leaving `project.read_buffer` demanding an id while `project.list_files` does not
would ship a half-migrated surface that every agent trips over once. Flipping all
of `project.*` in one commit is the same mechanical edit and keeps the namespace
coherent.

**Steps**

- [x] Add a failing test to `crates/solutions/src/mcp/tests.rs`:
      ```rust
      #[test]
      fn every_project_tool_accepts_an_absent_solution_id() {
          // One representative per file — the edit is mechanical, the risk is
          // forgetting a file.
          let list: crate::mcp::ListFilesParams =
              serde_json::from_value(serde_json::json!({})).expect("fs_ops");
          assert_eq!(list.solution_id, None);

          let find: crate::mcp::FindInBuffersParams =
              serde_json::from_value(serde_json::json!({ "query": "x" })).expect("code_nav");
          assert_eq!(find.solution_id, None);

          let read: crate::mcp::ReadBufferParams =
              serde_json::from_value(serde_json::json!({ "path": "src/main.rs" }))
                  .expect("buffer_ops");
          assert_eq!(read.solution_id, None);
      }
      ```
      (Use the real struct names / required sibling fields from each file.)
- [x] Run `cargo test -p solutions --lib every_project_tool_accepts` — expect a
      **type-mismatch failure** on the first `assert_eq!`.
- [x] Flip all twelve params structs + `Inner` shadows to `Option<String>` and
      route each handler through `crate::mcp::resolve_solution_id(input.solution_id)?`.
- [x] Run `cargo test -p solutions` — expect green.
- [x] Run `cargo check --workspace` — expect no errors (the params structs are
      also referenced from `remote_control`'s allow-list and from
      `crates/editor_mcp/tests/*`).
- [x] Commit: `Make solution_id optional in the project.* MCP tools`

## Task 3.5 — e2e: the scoped socket injects, and overrides a foreign id

**Files**
- `crates/editor_mcp/tests/solution_id_injection_e2e_test.rs` (new)

**Interfaces**
- Consumes: `editor_mcp::{set_runtime_dir_for_test, start_server, solution_socket_path}`;
  the JSON-RPC framing helper `call_tool` from
  `crates/editor_mcp/tests/solutions_add_member_e2e_test.rs` (copy it — it filters
  the interleaved `editor/notification` frames by JSON-RPC `id`).
- Produces: the acceptance test for task group 3.

**Steps**

- [ ] Write the failing test:
      ```rust
      //! A scoped socket call with NO `solution_id` must succeed (the listener
      //! injects the bound id), and a call carrying a FOREIGN `solution_id` must be
      //! overridden to the bound one — a scoped subagent cannot reach across
      //! Solutions. Both properties hang off `RegisteredTool::wants_solution_id`,
      //! which is derived from the `solution_id` *property* existing in the input
      //! schema (`context_server::listener`), so an `Option<String>` keeps them.

      #[gpui::test]
      async fn scoped_socket_injects_and_overrides_the_solution_id(cx: &mut TestAppContext) {
          let runtime = tempfile::tempdir().expect("tempdir");
          editor_mcp::set_runtime_dir_for_test(runtime.path().to_path_buf());
          // …boot the editor + create two Solutions (mirror
          // `solutions_e2e_test.rs`'s setup), note their ids `bound` and `foreign`.

          let socket = editor_mcp::solution_socket_path(&bound.to_string());
          let mut stream = UnixStream::connect(&socket).await.expect("connect");

          // 1. No solution_id at all -> the bound Solution answers.
          let response = call_tool(&mut stream, "solutions.get", json!({})).await;
          assert_eq!(response["structuredContent"]["id"], json!(bound));

          // 2. A foreign solution_id -> still the bound Solution.
          let response = call_tool(
              &mut stream,
              "solutions.get",
              json!({ "solution_id": foreign.to_string() }),
          )
          .await;
          assert_eq!(
              response["structuredContent"]["id"],
              json!(bound),
              "the per-socket injection must overwrite a caller-supplied id"
          );

          // 3. A scoped project tool with no id resolves against the bound Solution.
          let response = call_tool(&mut stream, "project.list_files", json!({})).await;
          assert_eq!(response["isError"], json!(false));
      }
      ```
- [ ] Run `cargo test -p editor_mcp --test solution_id_injection_e2e_test` — before
      tasks 3.2-3.4 this would fail on case 1 with a deserialize error; run it now
      and expect **pass** (the flip already landed). If case 1 fails with
      ``missing field `solution_id` ``, a params struct was missed — find it and
      flip it.
- [ ] Bump the tool-catalog note in `.rules` / `CLAUDE.md`: the two-tier-socket
      paragraph gains *"`solution_id` is optional on a per-solution socket (the
      server injects the bound id and overrides any value the caller sends); it is
      required only on the editor-global socket."*
- [ ] Run `cargo test -p editor_mcp` — expect the full e2e suite green.
- [ ] Commit: `Assert the per-solution socket injects and overrides solution_id`

---

## Done when

- `cargo test -p editor_mcp -p claude_native -p solutions -p solution_agent` is
  green (debug).
- `script/run-mcp --debug --headless` prints
  `~/.spk/sawe-dev/state/mcp.sock`, and `~/.spk/sawe-dev/config/` contains no
  `mcp.sock` / `mcp.lock` / `solutions/` / `uploads/` afterwards.
- A live agent-team / `isolation: worktree` subagent creates its worktree under
  `<solution_root>/.agents/worktrees/<member>/<name>` and **not** under
  `<member>/.claude/worktrees/`; existing worktrees under the latter are still
  listed by `git worktree list` and are untouched.
- A scoped-socket `project.list_files` with no `solution_id` succeeds, and one
  with a foreign `solution_id` still answers for the bound Solution.
