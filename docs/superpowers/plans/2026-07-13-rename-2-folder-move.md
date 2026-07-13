# Rename Moves The Folder — Implementation Plan (phases 2–5)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Renaming a solution or a member project also renames its directory on disk, without breaking the running editor, the live `claude` subprocesses, the terminals, or the window layout.

**Architecture:** A rename is split in two halves. The **hot half** (runs while the editor is live) is deliberately cheap: derive a folder name from the display name, check for collisions, verify source and target are on the same filesystem, `rename(2)` the directory, drop a `symlink(new → old)` compatibility shim so every *string* path still in flight (a live subprocess's spawn cwd, claude's transcript-bucket key, git `gitdir` pointers, absolute paths in model context) keeps resolving, update the owning DB rows, and record the move in a new `pending_path_migrations` table. Live sessions and terminals are **not** touched: on Linux a process's cwd is an *inode*, so after a same-filesystem `mv` the running `claude` subprocess and shell keep working. The **cold half** (`path_migrations::drain_and_apply`, runs inside `SolutionStore::init_global` before any window opens, when nothing is live) drains that table and rewrites every path-bearing row across all three databases, moves and merges the claude transcript bucket, repairs legacy git worktrees, removes the compat symlink and deletes the row. It is idempotent and crash-safe: a half-applied migration resumes correctly on the next start because every rewrite is a no-op when re-run.

**Tech Stack:** Rust, GPUI, `sqlez`/`db` (SQLite), `unicode-normalization` (new workspace dep), `smol`, `editor_mcp` (MCP over a Unix socket), `solutions` / `solutions_ui` / `solution_agent` / `workspace` / `worktree` crates.

## Global Constraints

- **This plan assumes plan 1 ("identity") is merged.** The shared contract, used verbatim everywhere below:
  - `SolutionId(pub i64)`, `MemberId(pub i64)`, `CatalogId(pub i64)`
  - `Solution { id: SolutionId, name: String, root: PathBuf, members: Vec<SolutionMember>, last_opened_at: Option<DateTime<Utc>> }`
  - `SolutionMember { id: MemberId, name: String, local_path: PathBuf, origin_catalog_id: Option<CatalogId> }`
  - `SolutionStore::find_solution(&self, id: SolutionId) -> Option<&Solution>`, `SolutionStore::find_member(&self, id: MemberId) -> Option<&SolutionMember>`
  - The MCP tool params and the `solutions_ui` actions carry **`i64`** ids (`solution_id: i64`, `member_id: i64`). If plan 1 landed them as `String`, parse with `.parse::<i64>()` at the boundary and change nothing else.
- **Produced by this plan** (plan 3 and later work depend on these exact names):
  - `solutions::folder_name::derive(name: &str) -> Result<String, FolderNameError>`
  - `SolutionStore::rename_solution(&mut self, id: SolutionId, new_name: &str, cx: &mut Context<Self>) -> Result<()>` (now also moves the folder)
  - `SolutionStore::rename_member(&mut self, id: MemberId, new_name: &str, cx: &mut Context<Self>) -> Result<()>`
  - `solutions::path_migrations::drain_and_apply(cx: &mut App) -> Task<Result<()>>`
- **Debug builds only.** `cargo check`, `cargo test -p <crate>` — never `--release`, never `script/bundle-*`.
- **Commit after every task.** Imperative commit messages (`solutions: Derive folder names from display names`). **Never** add a `Co-Authored-By` trailer. Never `git commit --amend`.
- **`INSERT OR REPLACE` on an FK parent with `ON DELETE CASCADE` is BANNED.** `solutions` is the parent of `solution_members` and `active_member`, both `ON DELETE CASCADE`, and `PRAGMA foreign_keys=TRUE` is global (`crates/db/src/db.rs:126`). SQLite's REPLACE *deletes* the conflicting parent row, firing the cascade and wiping the members — this is a real data-loss bug we already shipped once (`docs/findings/2026-07-13-rename-solution-cascade-data-loss.md`, fixed in `132e89f5a7`). Every write in this plan is a plain `UPDATE`, a plain `INSERT`, or an explicit UPSERT (`ON CONFLICT … DO UPDATE`).
- **Cross-device move is a hard error, no copy fallback.** A move across filesystems changes the inode and orphans every live process; the whole "hot rename is safe" premise dies with it.
- **Never remove/recreate the worktree on rename.** The worktree heals itself: the scanner holds the root by an inode handle, detects the rename and emits `ScanState::RootUpdated` → `update_abs_path_and_refresh` (`crates/worktree/src/worktree.rs:4356-4381` → `2050-2066`), which swaps `abs_path`/`root_name` and restarts the scanners. Open buffers survive.
- **`workspaces.paths` is the workspace identity key** (`crates/workspace/src/persistence.rs:890-893`, `CREATE UNIQUE INDEX ix_workspaces_location ON workspaces(remote_connection_id, paths)` at `:960`). If it is not rewritten, the window gets a fresh `workspace_id` on the next launch and every FK'd child (panes, tabs, docks, console state, editors, terminals, breakpoints, bookmarks) is lost. The merge case must never create a second row.
- **Two databases, not three files.** `SolutionsDb`, `WorkspaceDb`, `EditorDb`, `TerminalDb` are all *Domains on the same* `AppDatabase` sqlite file (`db::static_connection!` → `AppDatabase::global(cx)`), so `solutions`, `solution_members`, `pending_path_migrations`, `workspaces`, `editors`, `terminals2`, `breakpoints`, `bookmarks`, `trusted_worktrees`, `toolchains`, `user_toolchains`, `console_panel_state` share one connection. `solution_agent` has its **own** file at `paths::data_dir().join("solution_agent/solution_agent.db")` (`crates/solution_agent/src/db.rs:86`). The `solutions` crate must **not** depend on `solution_agent` (that would be a dependency cycle) — open the agent DB by path with `db::sqlez::connection::Connection::open_file`.
- **`editor_mcp` tests MUST call `editor_mcp::set_runtime_dir_for_test(tempdir)` before `editor_mcp::start_server`**, or they corrupt the user's real `mcp.sock`/`mcp.lock` (CLAUDE.md).
- Rust guidelines from CLAUDE.md apply: no `unwrap()` in non-test code, no `let _ =` on fallible calls, no `mod.rs`, comments explain *why* only.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/solutions/src/folder_name.rs` (**new**) | Pure derivation `display name → folder name`; `FolderNameError` with the user-facing messages. No fs, no DB. |
| `crates/solutions/src/rename.rs` (**new**) | Filesystem-level rename primitives: collision check (DB list + disk + symlink), same-filesystem check, `rename(2)` (+ two-step case-only), compat symlink. |
| `crates/solutions/src/path_migrations.rs` (**new**) | Cold reconcile: `PathRewrite`, per-DB rewriters, transcript-bucket merge, `git worktree repair`, `drain_and_apply`. |
| `crates/solutions/src/db.rs` (modify) | New `pending_path_migrations` migration + its queries; `update_solution_row`, `update_member_row`. |
| `crates/solutions/src/store/lifecycle.rs` (modify) | `rename_solution` — now moves the root and rewrites member paths. |
| `crates/solutions/src/store/members.rs` (modify) | `rename_member` — new. |
| `crates/solutions/src/store.rs` (modify) | Run the cold reconcile at the top of `init_with_db`. |
| `crates/solutions/src/mcp/solutions_lifecycle.rs` (modify) | `solutions.rename` (unchanged shape, new semantics) + new `solutions.rename_member`. |
| `crates/solutions_ui/src/modals/rename_member.rs` (**new**) | Rename-a-member modal + `open_rename_member`. |
| `crates/solutions_ui/src/modals/rename_solution.rs` (modify) | Show the derivation/collision error inline instead of swallowing it. |
| `crates/solutions_ui/src/project_tab.rs` (modify) | "Rename…" context-menu entry. |
| `crates/solutions_ui/src/actions.rs`, `solutions_ui.rs` (modify) | `RenameMember` action + its workspace handler. |
| `crates/editor_mcp/src/lifecycle.rs` (modify) | `solutions.rename_member` added to `GLOBAL_TOOLS`. |

---

### Task 1: Folder-name derivation

**Files:**
- Create: `crates/solutions/src/folder_name.rs`
- Modify: `crates/solutions/src/solutions.rs` (module list + re-export)
- Modify: `Cargo.toml` (root, `[workspace.dependencies]`), `crates/solutions/Cargo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `solutions::folder_name::derive(name: &str) -> Result<String, FolderNameError>`
  - `solutions::folder_name::MAX_FOLDER_NAME_BYTES: usize` (= 255)
  - `pub enum FolderNameError { Empty { name: String }, Reserved { folder: String }, TakenBySolution { folder: String, owner: String }, ExistsOnDisk { folder: String }, HeldByLink { folder: String } }` — implements `Display` + `std::error::Error`. (The three collision variants are constructed in Task 2; they live here so every rename error is one type.)

`unicode-normalization` **0.1.24 is already in `Cargo.lock`** (transitively, via `idna`) but is **not** in `[workspace.dependencies]` — add it there; no lockfile churn results.

- [x] **Step 1: Add the dependency**

In `Cargo.toml`, inside `[workspace.dependencies]` (keep the list alphabetical — it sits between `unicase` and `unindent` if present, otherwise place it alphabetically):

```toml
unicode-normalization = "0.1"
```

In `crates/solutions/Cargo.toml`, under `[dependencies]`, after `thiserror.workspace = true`:

```toml
unicode-normalization.workspace = true
```

- [x] **Step 2: Write the failing test**

Create `crates/solutions/src/folder_name.rs` containing **only** the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_folder_names() {
        let cases: &[(&str, &str)] = &[
            ("Citeck Forge", "Citeck-Forge"),
            ("Sawe", "Sawe"),
            ("sawe", "sawe"),
            ("Мой Проект", "Мой-Проект"),
            ("项目一", "项目一"),
            ("مشروع جديد", "مشروع-جديد"),
            ("rocket 🚀 ship", "rocket-🚀-ship"),
            ("  padded  name  ", "padded-name"),
            ("a\t\n b", "a-b"),
            ("a/b:c*d?e\"f<g>h|i\\j", "abcdefghij"),
            ("...dots...", "dots"),
            (" . mixed . ", "mixed"),
            ("Sawe (fork)", "Sawe-(fork)"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                derive(input).as_deref(),
                Ok(*expected),
                "derive({input:?})"
            );
        }
    }

    #[test]
    fn normalizes_to_nfc() {
        // "й" as U+0438 + U+0306 (decomposed) must derive to the composed form.
        let decomposed = "\u{0438}\u{0306}";
        let composed = "\u{0439}";
        assert_eq!(derive(decomposed).as_deref(), Ok(composed));
        assert_eq!(derive(composed).as_deref(), Ok(composed));
    }

    #[test]
    fn rejects_empty_derivations() {
        for input in ["", "   ", "...", " . . ", "/", "\u{0}", "\u{7}"] {
            assert_eq!(
                derive(input),
                Err(FolderNameError::Empty {
                    name: input.to_string()
                }),
                "derive({input:?})"
            );
        }
    }

    #[test]
    fn rejects_reserved_windows_names() {
        for input in ["CON", "con", "PRN", "AUX", "NUL", "COM1", "com9", "LPT1", "LPT9", "nul.txt"] {
            let derived = derive(input);
            assert!(
                matches!(derived, Err(FolderNameError::Reserved { .. })),
                "derive({input:?}) = {derived:?}"
            );
        }
        // COM0 / LPT0 are NOT reserved.
        assert_eq!(derive("COM0").as_deref(), Ok("COM0"));
        assert_eq!(derive("LPT0").as_deref(), Ok("LPT0"));
    }

    #[test]
    fn truncates_to_255_bytes_on_a_char_boundary() {
        // 128 Cyrillic chars = 256 bytes; the 128th char must be dropped whole.
        let input = "я".repeat(128);
        let derived = derive(&input).expect("derives");
        assert_eq!(derived.len(), 254);
        assert_eq!(derived.chars().count(), 127);

        // Exactly 255 ASCII bytes survives untouched.
        let ascii = "a".repeat(255);
        assert_eq!(derive(&ascii).as_deref(), Ok(ascii.as_str()));

        // 256 ASCII bytes truncates to 255.
        let long = "a".repeat(256);
        assert_eq!(derive(&long).expect("derives").len(), MAX_FOLDER_NAME_BYTES);
    }

    #[test]
    fn truncation_never_leaves_a_trailing_dot() {
        let input = format!("{}.x", "a".repeat(254));
        let derived = derive(&input).expect("derives");
        assert!(!derived.ends_with('.'), "{derived:?}");
        assert_eq!(derived, "a".repeat(254));
    }

    #[test]
    fn never_changes_case() {
        assert_eq!(derive("MiXeD CaSe").as_deref(), Ok("MiXeD-CaSe"));
    }

    #[test]
    fn error_messages_match_the_spec() {
        assert_eq!(
            FolderNameError::Empty { name: "...".into() }.to_string(),
            "Cannot derive a folder name from '...' — use at least one ordinary character"
        );
        assert_eq!(
            FolderNameError::TakenBySolution {
                folder: "citeck-forge".into(),
                owner: "Citeck Forge".into()
            }
            .to_string(),
            "Directory 'citeck-forge' is already taken by solution 'Citeck Forge'"
        );
        assert_eq!(
            FolderNameError::ExistsOnDisk { folder: "citeck-forge".into() }.to_string(),
            "Directory 'citeck-forge' already exists on disk (not owned by any solution)"
        );
        assert_eq!(
            FolderNameError::HeldByLink { folder: "citeck-forge".into() }.to_string(),
            "Directory 'citeck-forge' is held by a link from an unfinished rename — restart the editor"
        );
        assert_eq!(
            FolderNameError::Reserved { folder: "CON".into() }.to_string(),
            "'CON' is a reserved device name on Windows — choose another name"
        );
    }
}
```

Register the module in `crates/solutions/src/solutions.rs` — add `pub mod folder_name;` after `mod event_sources;` and extend the re-exports with:

```rust
pub use folder_name::{FolderNameError, derive_folder_name};
```

…where `derive_folder_name` is a re-export alias:

```rust
pub use folder_name::derive as derive_folder_name;
```

(Keep `pub mod folder_name;` so the contract path `solutions::folder_name::derive` also resolves.)

- [x] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p solutions folder_name`
Expected: FAIL — `error[E0425]: cannot find function 'derive' in this scope` (and `cannot find type 'FolderNameError'`).

- [x] **Step 4: Write the implementation**

Prepend to `crates/solutions/src/folder_name.rs` (above the test module):

```rust
//! Derivation of an on-disk folder name from a user-visible display name.
//!
//! Unicode-preserving sanitization, **not** transliteration: `Мой Проект`
//! becomes `Мой-Проект`, not `moy-proekt`. Nothing here touches the
//! filesystem or the database — collision checks live in `crate::rename`.

use std::fmt;
use unicode_normalization::UnicodeNormalization as _;

/// ext4 / APFS cap a single path component at 255 **bytes**, not characters
/// (a Cyrillic character is 2 bytes in UTF-8, a CJK one is 3).
pub const MAX_FOLDER_NAME_BYTES: usize = 255;

/// Everything that can stop a rename. The three collision variants are
/// produced by `crate::rename::ensure_folder_available`; they live here so a
/// caller has a single error type to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderNameError {
    Empty { name: String },
    Reserved { folder: String },
    TakenBySolution { folder: String, owner: String },
    ExistsOnDisk { folder: String },
    HeldByLink { folder: String },
}

impl fmt::Display for FolderNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { name } => write!(
                f,
                "Cannot derive a folder name from '{name}' — use at least one ordinary character"
            ),
            Self::Reserved { folder } => write!(
                f,
                "'{folder}' is a reserved device name on Windows — choose another name"
            ),
            Self::TakenBySolution { folder, owner } => write!(
                f,
                "Directory '{folder}' is already taken by solution '{owner}'"
            ),
            Self::ExistsOnDisk { folder } => write!(
                f,
                "Directory '{folder}' already exists on disk (not owned by any solution)"
            ),
            Self::HeldByLink { folder } => write!(
                f,
                "Directory '{folder}' is held by a link from an unfinished rename — restart the editor"
            ),
        }
    }
}

impl std::error::Error for FolderNameError {}

/// Characters that are illegal or non-portable in a path component.
/// `/` and NUL are illegal on POSIX; the rest are the Windows set.
const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

const RESERVED_WINDOWS_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn derive(name: &str) -> Result<String, FolderNameError> {
    // NFC first: otherwise `й` can exist as two different byte sequences and
    // two "identical" folder names differ on disk.
    let normalized: String = name.nfc().collect();

    let mut out = String::with_capacity(normalized.len());
    let mut pending_separator = false;
    for ch in normalized.chars() {
        if ch.is_whitespace() {
            pending_separator = true;
            continue;
        }
        if ch == '\u{0}' || ch.is_control() || ILLEGAL.contains(&ch) {
            continue;
        }
        if pending_separator && !out.is_empty() {
            out.push('-');
        }
        pending_separator = false;
        out.push(ch);
    }

    let trimmed = out.trim_matches(|ch: char| ch == '.' || ch == ' ');
    // Truncating can expose a trailing dot that was legal mid-name, so trim
    // again after the cut.
    let folder = truncate_to_bytes(trimmed, MAX_FOLDER_NAME_BYTES)
        .trim_end_matches(['.', ' '])
        .to_string();

    if folder.is_empty() {
        return Err(FolderNameError::Empty {
            name: name.to_string(),
        });
    }
    if is_reserved(&folder) {
        return Err(FolderNameError::Reserved { folder });
    }
    Ok(folder)
}

fn truncate_to_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Windows reserves these names with *any* extension (`NUL.txt` is still the
/// null device), so the check is on the stem.
fn is_reserved(folder: &str) -> bool {
    let stem = folder.split('.').next().unwrap_or(folder).to_uppercase();
    RESERVED_WINDOWS_NAMES.contains(&stem.as_str())
}
```

- [x] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p solutions folder_name`
Expected: PASS — 8 tests (`derives_folder_names`, `normalizes_to_nfc`, `rejects_empty_derivations`, `rejects_reserved_windows_names`, `truncates_to_255_bytes_on_a_char_boundary`, `truncation_never_leaves_a_trailing_dot`, `never_changes_case`, `error_messages_match_the_spec`).

- [x] **Step 6: Commit**

```bash
git add Cargo.toml crates/solutions/Cargo.toml crates/solutions/src/folder_name.rs crates/solutions/src/solutions.rs
git commit -m "solutions: Derive on-disk folder names from display names"
```

---

### Task 2: Collision checks and filesystem primitives

**Files:**
- Create: `crates/solutions/src/rename.rs`
- Modify: `crates/solutions/src/solutions.rs` (module list)

**Interfaces:**
- Consumes: `solutions::folder_name::{derive, FolderNameError}` (Task 1).
- Produces:
  - `pub struct TakenFolder { pub folder: String, pub owner: String }`
  - `pub fn ensure_folder_available(parent: &Path, folder: &str, source: Option<&Path>, taken: &[TakenFolder]) -> Result<PathBuf, FolderNameError>` — returns the absolute target path. `source` is the directory being renamed (`None` when creating): a target that *is* the source (case-only rename on a case-insensitive fs) is allowed.
  - `pub fn same_filesystem(source: &Path, target_parent: &Path) -> std::io::Result<bool>`
  - `pub fn move_dir_with_compat_link(source: &Path, target: &Path) -> anyhow::Result<()>`

- [x] **Step 1: Write the failing tests**

Create `crates/solutions/src/rename.rs` containing **only** the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_name_is_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = ensure_folder_available(dir.path(), "Sawe", None, &[]).expect("available");
        assert_eq!(target, dir.path().join("Sawe"));
    }

    #[test]
    fn db_owned_name_collides_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let taken = [TakenFolder {
            folder: "citeck-forge".into(),
            owner: "Citeck Forge".into(),
        }];
        let err = ensure_folder_available(dir.path(), "Citeck-Forge", None, &taken)
            .expect_err("collides");
        assert_eq!(
            err,
            FolderNameError::TakenBySolution {
                folder: "Citeck-Forge".into(),
                owner: "Citeck Forge".into()
            }
        );
    }

    #[test]
    fn plain_directory_on_disk_collides() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("citeck-forge")).expect("mkdir");
        let err =
            ensure_folder_available(dir.path(), "citeck-forge", None, &[]).expect_err("collides");
        assert_eq!(
            err,
            FolderNameError::ExistsOnDisk {
                folder: "citeck-forge".into()
            }
        );
    }

    #[test]
    fn symlink_from_an_unfinished_rename_collides_with_its_own_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("new-name");
        std::fs::create_dir(&real).expect("mkdir");
        std::os::unix::fs::symlink(&real, dir.path().join("citeck-forge")).expect("symlink");
        let err =
            ensure_folder_available(dir.path(), "citeck-forge", None, &[]).expect_err("collides");
        assert_eq!(
            err,
            FolderNameError::HeldByLink {
                folder: "citeck-forge".into()
            }
        );
    }

    #[test]
    fn renaming_a_directory_to_itself_is_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("sawe");
        std::fs::create_dir(&source).expect("mkdir");
        // On a case-sensitive fs `Sawe` simply doesn't exist; on a
        // case-insensitive one it resolves to `source` — both are fine.
        let target =
            ensure_folder_available(dir.path(), "Sawe", Some(&source), &[]).expect("available");
        assert_eq!(target, dir.path().join("Sawe"));
    }

    #[test]
    fn same_filesystem_is_true_within_one_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("a");
        std::fs::create_dir(&source).expect("mkdir");
        assert!(same_filesystem(&source, dir.path()).expect("stat"));
    }

    #[test]
    fn move_leaves_a_symlink_behind_and_old_paths_resolve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("old");
        std::fs::create_dir(&source).expect("mkdir");
        std::fs::write(source.join("file.txt"), b"hello").expect("write");
        let target = dir.path().join("new");

        move_dir_with_compat_link(&source, &target).expect("move");

        assert!(target.join("file.txt").is_file());
        assert!(
            std::fs::symlink_metadata(&source)
                .expect("stat old")
                .file_type()
                .is_symlink()
        );
        // The old path — and every path *under* it — still resolves.
        assert_eq!(
            std::fs::read(source.join("file.txt")).expect("read via link"),
            b"hello"
        );
        std::fs::write(source.join("file.txt"), b"written via link").expect("write via link");
        assert_eq!(
            std::fs::read(target.join("file.txt")).expect("read new"),
            b"written via link"
        );
    }

    #[test]
    fn case_only_move_goes_through_a_temp_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("sawe");
        std::fs::create_dir(&source).expect("mkdir");
        std::fs::write(source.join("file.txt"), b"x").expect("write");
        let target = dir.path().join("Sawe");

        move_dir_with_compat_link(&source, &target).expect("move");

        assert!(target.join("file.txt").is_file());
        // No leftover temp directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rename-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }
}
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p solutions rename::`
Expected: FAIL — `error[E0425]: cannot find function 'ensure_folder_available' in this scope` (plus `TakenFolder`, `same_filesystem`, `move_dir_with_compat_link` unresolved).

- [x] **Step 3: Write the implementation**

Prepend to `crates/solutions/src/rename.rs`:

```rust
//! Filesystem-level rename primitives shared by `rename_solution` and
//! `rename_member`: collision checks (DB + disk + the symlink an unfinished
//! rename leaves behind), the same-filesystem preflight, and the
//! `rename(2)` + compat-symlink move itself.

use crate::folder_name::FolderNameError;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// A folder name already spoken for by a row in the DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenFolder {
    pub folder: String,
    pub owner: String,
}

/// Case-insensitive on purpose: on macOS/Windows `Sawe` and `sawe` are the
/// same directory, so allowing both would produce a rename that silently
/// works on Linux and destroys data elsewhere.
fn same_folder_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

pub fn ensure_folder_available(
    parent: &Path,
    folder: &str,
    source: Option<&Path>,
    taken: &[TakenFolder],
) -> Result<PathBuf, FolderNameError> {
    let target = parent.join(folder);

    if let Some(conflict) = taken
        .iter()
        .find(|candidate| same_folder_name(&candidate.folder, folder))
    {
        return Err(FolderNameError::TakenBySolution {
            folder: folder.to_string(),
            owner: conflict.owner.clone(),
        });
    }

    match std::fs::symlink_metadata(&target) {
        Err(_) => Ok(target),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(FolderNameError::HeldByLink {
                    folder: folder.to_string(),
                });
            }
            // A case-only rename on a case-insensitive filesystem "finds" the
            // source directory at the target path. Same inode ⇒ same
            // directory ⇒ not a collision.
            if let Some(source) = source
                && is_same_dir(source, &target)
            {
                return Ok(target);
            }
            Err(FolderNameError::ExistsOnDisk {
                folder: folder.to_string(),
            })
        }
    }
}

#[cfg(unix)]
fn is_same_dir(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A cross-device move changes the inode, which orphans every live process
/// holding the directory as its cwd — the premise of the whole hot-rename
/// design. Callers treat `false` as a hard error, never as "copy instead".
#[cfg(unix)]
pub fn same_filesystem(source: &Path, target_parent: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(source)?.dev() == std::fs::metadata(target_parent)?.dev())
}

#[cfg(not(unix))]
pub fn same_filesystem(_source: &Path, _target_parent: &Path) -> std::io::Result<bool> {
    // Windows: `rename` across volumes fails with ERROR_NOT_SAME_DEVICE, which
    // surfaces as an error from `move_dir_with_compat_link` anyway.
    Ok(true)
}

/// `rename(2)` + a `symlink(target → source)` compatibility shim. The shim is
/// what keeps the *strings* held by live processes valid (a spawned `claude`'s
/// cwd string, its `~/.claude/projects/<enc(cwd)>` bucket key, git `gitdir`
/// pointers, absolute paths in model context). It is deleted by the cold
/// reconcile on the next start.
pub fn move_dir_with_compat_link(source: &Path, target: &Path) -> Result<()> {
    let case_only = source != target && is_same_dir_name_ignoring_case(source, target);
    if case_only {
        // A direct rename is a no-op (or an error) on a case-insensitive
        // filesystem, so go through a temporary name.
        let temp = temp_sibling(target)?;
        std::fs::rename(source, &temp)
            .with_context(|| format!("renaming {} to {}", source.display(), temp.display()))?;
        if let Err(err) = std::fs::rename(&temp, target) {
            std::fs::rename(&temp, source).ok();
            return Err(err).with_context(|| {
                format!("renaming {} to {}", temp.display(), target.display())
            });
        }
    } else {
        std::fs::rename(source, target)
            .with_context(|| format!("renaming {} to {}", source.display(), target.display()))?;
    }

    if let Err(err) = symlink_dir(target, source) {
        // Roll the move back so the caller's DB stays consistent with disk.
        std::fs::rename(target, source).ok();
        return Err(err).with_context(|| {
            format!(
                "linking {} back to {}",
                source.display(),
                target.display()
            )
        });
    }
    Ok(())
}

fn is_same_dir_name_ignoring_case(source: &Path, target: &Path) -> bool {
    match (source.file_name(), target.file_name()) {
        (Some(a), Some(b)) => {
            source.parent() == target.parent()
                && same_folder_name(&a.to_string_lossy(), &b.to_string_lossy())
        }
        _ => false,
    }
}

fn temp_sibling(target: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .context("target has no file name")?
        .to_string_lossy()
        .into_owned();
    let parent = target.parent().context("target has no parent")?;
    for attempt in 0..1_000u32 {
        let candidate = parent.join(format!(".{name}.rename-tmp.{attempt}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("no free temporary name next to {}", target.display())
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}
```

Register the module in `crates/solutions/src/solutions.rs`: add `pub mod rename;` after `mod persistence;`.

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p solutions rename::`
Expected: PASS — 8 tests.

- [x] **Step 5: Commit**

```bash
git add crates/solutions/src/rename.rs crates/solutions/src/solutions.rs
git commit -m "solutions: Add collision checks and the rename+symlink move primitive"
```

---

### Task 3: `pending_path_migrations` table and the row-update queries

**Files:**
- Modify: `crates/solutions/src/db.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (on `SolutionsDb`):
  - `pub async fn insert_pending_path_migration(old_path: String, new_path: String, created_at: i64) -> Result<()>`
  - `pub fn load_pending_path_migrations() -> Result<Vec<(i64, String, String)>>` — `(id, old_path, new_path)`, ordered by `id`.
  - `pub async fn delete_pending_path_migration(id: i64) -> Result<()>`
  - `pub async fn update_solution_row(id: i64, name: String, root: String) -> Result<()>`
  - `pub async fn update_member_row(id: i64, name: String, local_path: String) -> Result<()>`
- Table (new migration appended to `SolutionsDb::MIGRATIONS` — never edit an existing entry, migrations are content-hashed):

```sql
CREATE TABLE pending_path_migrations (
    id         INTEGER PRIMARY KEY,
    old_path   TEXT    NOT NULL,
    new_path   TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);
```

- [x] **Step 1: Write the failing test**

Append to the `#[cfg(test)] mod tests` block at the bottom of `crates/solutions/src/db.rs` (create the block if the file has none):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn pending_path_migrations_round_trip() {
        let db = SolutionsDb::open_test_db("pending_path_migrations_round_trip").await;

        db.insert_pending_path_migration("/sol/old".into(), "/sol/new".into(), 17)
            .await
            .expect("insert");
        db.insert_pending_path_migration("/sol/a".into(), "/sol/b".into(), 18)
            .await
            .expect("insert");

        let rows = db.load_pending_path_migrations().expect("load");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "/sol/old");
        assert_eq!(rows[0].2, "/sol/new");
        assert_eq!(rows[1].1, "/sol/a");

        db.delete_pending_path_migration(rows[0].0)
            .await
            .expect("delete");
        let rows = db.load_pending_path_migrations().expect("load");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "/sol/a");
    }

    #[gpui::test]
    async fn update_solution_row_keeps_members() {
        let db = SolutionsDb::open_test_db("update_solution_row_keeps_members").await;
        db.save_solution(1, "Old".into(), "/sol/old".into(), None)
            .await
            .expect("save solution");
        db.save_member(1, 1, "member".into(), "/sol/old/member".into(), 0, None)
            .await
            .expect("save member");

        db.update_solution_row(1, "New".into(), "/sol/new".into())
            .await
            .expect("update solution");
        db.update_member_row(1, "member".into(), "/sol/new/member".into())
            .await
            .expect("update member");

        let solutions = db.load_all_solutions_with_members().expect("load");
        assert_eq!(solutions.len(), 1);
        assert_eq!(solutions[0].name, "New");
        assert_eq!(solutions[0].root, std::path::PathBuf::from("/sol/new"));
        assert_eq!(solutions[0].members.len(), 1, "members must survive a rename");
        assert_eq!(
            solutions[0].members[0].local_path,
            std::path::PathBuf::from("/sol/new/member")
        );
    }
}
```

Note: `save_solution` / `save_member` / `load_all_solutions_with_members` are plan 1's helpers — if their argument lists differ, adapt the *call sites in this test only*; the assertions are what matters.

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p solutions db::tests`
Expected: FAIL — `no method named 'insert_pending_path_migration' found for struct 'SolutionsDb'`.

- [x] **Step 3: Write the implementation**

In `crates/solutions/src/db.rs`, append a new entry at the **end** of `const MIGRATIONS`:

```rust
        sql!(
            CREATE TABLE pending_path_migrations (
                id         INTEGER PRIMARY KEY,
                old_path   TEXT    NOT NULL,
                new_path   TEXT    NOT NULL,
                created_at INTEGER NOT NULL
            );
        ),
```

And inside `impl SolutionsDb`, add:

```rust
    query! {
        pub async fn insert_pending_path_migration(
            old_path: String,
            new_path: String,
            created_at: i64
        ) -> Result<()> {
            INSERT INTO pending_path_migrations (old_path, new_path, created_at)
            VALUES (?, ?, ?)
        }
    }

    query! {
        pub fn load_pending_path_migrations() -> Result<Vec<(i64, String, String)>> {
            SELECT id, old_path, new_path FROM pending_path_migrations ORDER BY id ASC
        }
    }

    query! {
        pub async fn delete_pending_path_migration(id: i64) -> Result<()> {
            DELETE FROM pending_path_migrations WHERE id = ?
        }
    }

    // Plain UPDATEs. `INSERT OR REPLACE` on `solutions` deletes the parent row
    // first, and both `solution_members` and `active_member` are
    // `ON DELETE CASCADE` — that wiped every member the last time a rename
    // shipped (docs/findings/2026-07-13-rename-solution-cascade-data-loss.md).
    query! {
        pub async fn update_solution_row(id: i64, name: String, root: String) -> Result<()> {
            UPDATE solutions SET name = ?2, root = ?3 WHERE id = ?1
        }
    }

    query! {
        pub async fn update_member_row(id: i64, name: String, local_path: String) -> Result<()> {
            UPDATE solution_members SET name = ?2, local_path = ?3 WHERE id = ?1
        }
    }
```

- [x] **Step 4: Run the test to verify it passes**

Run: `cargo test -p solutions db::tests`
Expected: PASS — 2 tests.

- [x] **Step 5: Commit**

```bash
git add crates/solutions/src/db.rs
git commit -m "solutions: Add the pending_path_migrations table and row-update queries"
```

---

### Task 4: `SolutionStore::rename_member` (hot path)

**Files:**
- Modify: `crates/solutions/src/store/members.rs`
- Test: `crates/solutions/src/store/members.rs` (its `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `folder_name::derive`, `rename::{ensure_folder_available, TakenFolder, same_filesystem, move_dir_with_compat_link}`, `SolutionsDb::{update_member_row, insert_pending_path_migration}`.
- Produces: `SolutionStore::rename_member(&mut self, id: MemberId, new_name: &str, cx: &mut Context<Self>) -> Result<()>`.

Behavior: derive the folder from `new_name`; collide against the *sibling members of the same solution* (DB) and against the filesystem; require the same filesystem; move + link; update the in-memory member (`name`, `local_path`) and its DB row; insert the `pending_path_migrations` row; emit `SolutionStoreEvent::Changed`. Live sessions/terminals are untouched by design.

- [x] **Step 1: Write the failing test**

Add to the test module at the bottom of `crates/solutions/src/store/members.rs`:

```rust
    #[gpui::test]
    async fn rename_member_moves_the_folder_and_leaves_a_link(cx: &mut gpui::TestAppContext) {
        let root = tempfile::tempdir().expect("tempdir");
        let solution_root = root.path().join("my-solution");
        let member_path = solution_root.join("old-project");
        std::fs::create_dir_all(&member_path).expect("mkdir member");
        std::fs::write(member_path.join("marker.txt"), b"m").expect("write marker");

        let store = cx.update(|cx| crate::store::for_test_with_solution(cx, &solution_root, &member_path));

        let member_id = store.read_with(cx, |store, _| {
            store.solutions()[0].members[0].id
        });

        store
            .update(cx, |store, cx| {
                store.rename_member(member_id, "New Project", cx)
            })
            .expect("rename");

        let new_path = solution_root.join("New-Project");
        assert!(new_path.join("marker.txt").is_file(), "folder moved");
        assert!(
            std::fs::symlink_metadata(&member_path)
                .expect("stat old path")
                .file_type()
                .is_symlink(),
            "compat symlink left behind"
        );

        store.read_with(cx, |store, _| {
            let member = store.find_member(member_id).expect("member");
            assert_eq!(member.name, "New Project");
            assert_eq!(member.local_path, new_path);
        });
    }

    #[gpui::test]
    async fn rename_member_rejects_a_sibling_collision(cx: &mut gpui::TestAppContext) {
        let root = tempfile::tempdir().expect("tempdir");
        let solution_root = root.path().join("my-solution");
        let member_path = solution_root.join("old-project");
        std::fs::create_dir_all(&member_path).expect("mkdir member");
        std::fs::create_dir_all(solution_root.join("Taken")).expect("mkdir sibling");

        let store = cx.update(|cx| crate::store::for_test_with_solution(cx, &solution_root, &member_path));
        let member_id = store.read_with(cx, |store, _| store.solutions()[0].members[0].id);

        let err = store
            .update(cx, |store, cx| store.rename_member(member_id, "taken", cx))
            .expect_err("collides");
        assert!(
            err.to_string()
                .contains("already exists on disk (not owned by any solution)"),
            "{err}"
        );
        // Nothing moved.
        assert!(member_path.join("..").exists());
        assert!(member_path.is_dir());
    }
```

Add the test-only fixture used above at the bottom of `crates/solutions/src/store.rs`, inside the existing `#[cfg(any(test, feature = "test-support"))]` region:

```rust
#[cfg(test)]
pub(crate) fn for_test_with_solution(
    cx: &mut App,
    solution_root: &std::path::Path,
    member_path: &std::path::Path,
) -> Entity<SolutionStore> {
    cx.new(|_| SolutionStore {
        config: SolutionsConfig {
            version: CURRENT_VERSION,
            solutions: vec![Solution {
                id: SolutionId(1),
                name: "My Solution".into(),
                root: solution_root.to_path_buf(),
                members: vec![SolutionMember {
                    id: MemberId(1),
                    name: "Old Project".into(),
                    local_path: member_path.to_path_buf(),
                    origin_catalog_id: None,
                }],
                last_opened_at: None,
            }],
            ..Default::default()
        },
        db: None,
        fs_lock: Arc::new(smol::lock::Mutex::new(())),
        in_flight_adds: HashMap::default(),
        tab_snapshots: TabSnapshots::default(),
        active_member: HashMap::default(),
        open_solutions: HashSet::default(),
    })
}
```

(`db: None` means the DB writes are skipped — the same escape hatch `db_save_solution` already uses. Adjust the field list if plan 1 added fields.)

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p solutions rename_member`
Expected: FAIL — `no method named 'rename_member' found for struct 'SolutionStore'`.

- [x] **Step 3: Write the implementation**

Add to `impl SolutionStore` in `crates/solutions/src/store/members.rs`:

```rust
    /// Rename a member project **and its directory**. Live sessions and
    /// terminals are deliberately left alone: their cwd is an inode, so a
    /// same-filesystem `rename(2)` does not disturb them, and the compat
    /// symlink keeps the path *strings* they still hold valid until the cold
    /// reconcile runs.
    pub fn rename_member(
        &mut self,
        id: MemberId,
        new_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> anyhow::Result<()> {
        let folder = crate::folder_name::derive(new_name)?;

        let (solution_index, member_index) = self
            .config
            .solutions
            .iter()
            .enumerate()
            .find_map(|(solution_index, solution)| {
                solution
                    .members
                    .iter()
                    .position(|member| member.id == id)
                    .map(|member_index| (solution_index, member_index))
            })
            .with_context(|| format!("member not found: {}", id.0))?;

        let solution = &self.config.solutions[solution_index];
        let old_path = solution.members[member_index].local_path.clone();
        let parent = old_path
            .parent()
            .context("member path has no parent")?
            .to_path_buf();

        let taken: Vec<crate::rename::TakenFolder> = solution
            .members
            .iter()
            .filter(|member| member.id != id)
            .filter_map(|member| {
                Some(crate::rename::TakenFolder {
                    folder: member.local_path.file_name()?.to_string_lossy().into_owned(),
                    owner: member.name.clone(),
                })
            })
            .collect();

        let new_path =
            crate::rename::ensure_folder_available(&parent, &folder, Some(&old_path), &taken)?;
        if old_path == new_path {
            // Display-name-only change (the folder already has this name).
            let member = &mut self.config.solutions[solution_index].members[member_index];
            member.name = new_name.to_string();
            let member = member.clone();
            self.db_update_member(&member)?;
            cx.emit(SolutionStoreEvent::Changed);
            cx.notify();
            return Ok(());
        }

        anyhow::ensure!(
            crate::rename::same_filesystem(&old_path, &parent)?,
            "{} and {} are on different filesystems — a cross-device move would orphan every running process in this project",
            old_path.display(),
            parent.display(),
        );

        crate::rename::move_dir_with_compat_link(&old_path, &new_path)?;

        let member = &mut self.config.solutions[solution_index].members[member_index];
        member.name = new_name.to_string();
        member.local_path = new_path.clone();
        let member = member.clone();
        self.db_update_member(&member)?;
        self.db_insert_pending_path_migration(&old_path, &new_path)?;

        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    pub(crate) fn db_update_member(&self, member: &SolutionMember) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_member_row(
            member.id.0,
            member.name.clone(),
            member.local_path.to_string_lossy().into_owned(),
        ))
    }

    pub(crate) fn db_insert_pending_path_migration(
        &self,
        old_path: &std::path::Path,
        new_path: &std::path::Path,
    ) -> anyhow::Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.insert_pending_path_migration(
            old_path.to_string_lossy().into_owned(),
            new_path.to_string_lossy().into_owned(),
            chrono::Utc::now().timestamp_millis(),
        ))
    }
```

Make sure the file's imports cover `anyhow::{Context as _, Result}`, `crate::model::{MemberId, SolutionMember}` and `super::{SolutionStore, SolutionStoreEvent}`.

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p solutions rename_member`
Expected: PASS — `rename_member_moves_the_folder_and_leaves_a_link`, `rename_member_rejects_a_sibling_collision`.

- [x] **Step 5: Commit**

```bash
git add crates/solutions/src/store/members.rs crates/solutions/src/store.rs
git commit -m "solutions: Move the member folder when a member is renamed"
```

---

### Task 5: `SolutionStore::rename_solution` moves the root

**Files:**
- Modify: `crates/solutions/src/store/lifecycle.rs:39-58` (the existing name-only `rename_solution`)
- Test: `crates/solutions/src/store/lifecycle.rs` (its `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `folder_name::derive`, `rename::{ensure_folder_available, TakenFolder, same_filesystem, move_dir_with_compat_link}`, `SolutionStore::db_update_member` / `db_insert_pending_path_migration` (Task 4), `SolutionsDb::update_solution_row` (Task 3).
- Produces: `SolutionStore::rename_solution(&mut self, id: SolutionId, new_name: &str, cx: &mut Context<Self>) -> Result<()>` — **note the signature change**: `id` is now taken by value (`SolutionId` is `Copy`-cheap `i64`), matching the shared contract. Every caller (`solutions_ui`, MCP) is updated in Tasks 9–10.

One `rename(2)` of the root moves every member with it, and one symlink at the old root covers every descendant path — so member `local_path`s are rewritten in memory/DB but **not** moved individually, and exactly one `pending_path_migrations` row is written.

- [x] **Step 1: Write the failing test**

Add to the test module at the bottom of `crates/solutions/src/store/lifecycle.rs`:

```rust
    #[gpui::test]
    async fn rename_solution_moves_the_root_and_rewrites_member_paths(
        cx: &mut gpui::TestAppContext,
    ) {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("spk-solutions");
        let member_path = old_root.join("sawe");
        std::fs::create_dir_all(&member_path).expect("mkdir member");
        std::fs::write(member_path.join("marker.txt"), b"m").expect("write marker");

        let store =
            cx.update(|cx| crate::store::for_test_with_solution(cx, &old_root, &member_path));
        let solution_id = store.read_with(cx, |store, _| store.solutions()[0].id);

        store
            .update(cx, |store, cx| store.rename_solution(solution_id, "Sawe", cx))
            .expect("rename");

        let new_root = base.path().join("Sawe");
        assert!(new_root.join("sawe/marker.txt").is_file(), "root moved");
        assert!(
            std::fs::symlink_metadata(&old_root)
                .expect("stat old root")
                .file_type()
                .is_symlink(),
            "compat symlink left at the old root"
        );
        // The single link at the root covers every descendant path.
        assert_eq!(
            std::fs::read(old_root.join("sawe/marker.txt")).expect("read through link"),
            b"m"
        );

        store.read_with(cx, |store, _| {
            let solution = store.find_solution(solution_id).expect("solution");
            assert_eq!(solution.name, "Sawe");
            assert_eq!(solution.root, new_root);
            assert_eq!(solution.members.len(), 1, "members survive the rename");
            assert_eq!(solution.members[0].local_path, new_root.join("sawe"));
        });
    }

    #[gpui::test]
    async fn rename_solution_rejects_a_name_taken_by_another_solution(
        cx: &mut gpui::TestAppContext,
    ) {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("spk-solutions");
        let member_path = old_root.join("sawe");
        std::fs::create_dir_all(&member_path).expect("mkdir member");

        let store =
            cx.update(|cx| crate::store::for_test_with_solution(cx, &old_root, &member_path));
        store.update(cx, |store, _| {
            store.config.solutions.push(crate::model::Solution {
                id: crate::model::SolutionId(2),
                name: "Citeck Forge".into(),
                root: base.path().join("Citeck-Forge"),
                members: vec![],
                last_opened_at: None,
            });
        });
        let solution_id = store.read_with(cx, |store, _| store.solutions()[0].id);

        let err = store
            .update(cx, |store, cx| {
                store.rename_solution(solution_id, "citeck forge", cx)
            })
            .expect_err("collides");
        assert_eq!(
            err.to_string(),
            "Directory 'citeck-forge' is already taken by solution 'Citeck Forge'"
        );
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p solutions rename_solution`
Expected: FAIL — `rename_solution_moves_the_root_and_rewrites_member_paths` fails at `assert!(new_root.join("sawe/marker.txt").is_file())` (today's rename only rewrites the name), and `rename_solution_rejects_a_name_taken_by_another_solution` fails with `expect_err("collides")` panicking on an `Ok`.

- [x] **Step 3: Write the implementation**

Replace `rename_solution` in `crates/solutions/src/store/lifecycle.rs` with:

```rust
    /// Rename a solution **and move its directory**. One `rename(2)` of the
    /// root carries every member with it, and one compat symlink at the old
    /// root keeps every descendant path string resolving (live `claude`
    /// subprocesses, terminals, git `gitdir` pointers) until the cold
    /// reconcile removes it. Live sessions and terminals are not touched.
    pub fn rename_solution(
        &mut self,
        id: SolutionId,
        new_name: &str,
        cx: &mut gpui::Context<Self>,
    ) -> Result<()> {
        let folder = crate::folder_name::derive(new_name)?;

        let index = self
            .config
            .solutions
            .iter()
            .position(|solution| solution.id == id)
            .with_context(|| format!("solution not found: {}", id.0))?;

        let old_root = self.config.solutions[index].root.clone();
        let parent = old_root
            .parent()
            .context("solution root has no parent")?
            .to_path_buf();

        let taken: Vec<crate::rename::TakenFolder> = self
            .config
            .solutions
            .iter()
            .filter(|solution| solution.id != id)
            .filter_map(|solution| {
                Some(crate::rename::TakenFolder {
                    folder: solution.root.file_name()?.to_string_lossy().into_owned(),
                    owner: solution.name.clone(),
                })
            })
            .collect();

        let new_root =
            crate::rename::ensure_folder_available(&parent, &folder, Some(&old_root), &taken)?;

        if old_root == new_root {
            let solution = &mut self.config.solutions[index];
            solution.name = new_name.to_string();
            let solution = solution.clone();
            self.db_update_solution(&solution)?;
            cx.emit(SolutionStoreEvent::Changed);
            cx.notify();
            return Ok(());
        }

        anyhow::ensure!(
            crate::rename::same_filesystem(&old_root, &parent)?,
            "{} and {} are on different filesystems — a cross-device move would orphan every running process in this solution",
            old_root.display(),
            parent.display(),
        );

        crate::rename::move_dir_with_compat_link(&old_root, &new_root)?;

        let solution = &mut self.config.solutions[index];
        solution.name = new_name.to_string();
        solution.root = new_root.clone();
        for member in solution.members.iter_mut() {
            if let Ok(relative) = member.local_path.strip_prefix(&old_root) {
                member.local_path = new_root.join(relative);
            }
        }
        let solution = solution.clone();

        self.db_update_solution(&solution)?;
        for member in &solution.members {
            self.db_update_member(member)?;
        }
        self.db_insert_pending_path_migration(&old_root, &new_root)?;

        cx.emit(SolutionStoreEvent::Changed);
        cx.notify();
        Ok(())
    }

    /// Plain UPDATE — see the cascade data-loss finding.
    pub(crate) fn db_update_solution(&self, solution: &Solution) -> Result<()> {
        let Some(db) = self.db.as_ref() else {
            return Ok(());
        };
        gpui::block_on(db.update_solution_row(
            solution.id.0,
            solution.name.clone(),
            solution.root.to_string_lossy().into_owned(),
        ))
    }
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p solutions rename_solution`
Expected: PASS — both tests.

- [x] **Step 5: Check the crate still compiles for its callers**

Run: `cargo check -p solutions`
Expected: PASS. (`solutions_ui` and the MCP tool still pass `&SolutionId` — they are fixed in Tasks 9 and 10. If `cargo check -p solutions_ui` is run now it will fail; that is expected and fixed there.)

- [x] **Step 6: Commit**

```bash
git add crates/solutions/src/store/lifecycle.rs
git commit -m "solutions: Move the solution root when a solution is renamed"
```

---

### Task 6: Cold reconcile — the path-rewriting engine for the app database

**Files:**
- Create: `crates/solutions/src/path_migrations.rs`
- Modify: `crates/solutions/src/solutions.rs` (module list)

**Interfaces:**
- Consumes: nothing from earlier tasks except the table from Task 3.
- Produces:
  - `pub struct PathRewrite { pub old: PathBuf, pub new: PathBuf }` with `pub fn apply_str(&self, value: &str) -> Option<String>` and `pub fn apply_bytes(&self, value: &[u8]) -> Option<Vec<u8>>` (both return `None` when the value is not the old path nor under it).
  - `pub fn rewrite_app_db(connection: &db::sqlez::connection::Connection, rewrite: &PathRewrite) -> anyhow::Result<()>`

`rewrite_app_db` covers every path-bearing row in the shared `AppDatabase` file:
`solutions.root`, `solution_members.local_path`, `workspaces.paths`/`paths_order`/`identity_paths`/`identity_paths_order` (with the `ix_workspaces_location` merge case), `console_panel_state.cwd`, `editors.path` and `editors.buffer_path` (BLOB), `terminals2.working_directory` (BLOB), `breakpoints.path`, `bookmarks.path`, `trusted_worktrees.absolute_path`, and **deletes** stale `toolchains` / `user_toolchains` rows (path is in the PK; the toolchain is re-detected).

- [x] **Step 1: Write the failing tests**

Create `crates/solutions/src/path_migrations.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use db::sqlez::connection::Connection;

    fn seed(connection: &Connection) {
        connection
            .exec(
                "CREATE TABLE solutions (id INTEGER PRIMARY KEY, name TEXT, root TEXT, last_opened_at INTEGER);
                 CREATE TABLE solution_members (id INTEGER PRIMARY KEY, solution_id INTEGER, name TEXT, local_path TEXT, position INTEGER, origin_catalog_id INTEGER);
                 CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT, paths_order TEXT, identity_paths TEXT, identity_paths_order TEXT, remote_connection_id INTEGER);
                 CREATE UNIQUE INDEX ix_workspaces_location ON workspaces(remote_connection_id, paths);
                 CREATE TABLE console_panel_state (workspace_id INTEGER, tab_index INTEGER, cwd TEXT);
                 CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, buffer_path BLOB);
                 CREATE TABLE terminals2 (workspace_id INTEGER, item_id INTEGER, working_directory BLOB);
                 CREATE TABLE breakpoints (workspace_id INTEGER, path TEXT, breakpoint_location INTEGER);
                 CREATE TABLE bookmarks (workspace_id INTEGER, path TEXT, row INTEGER);
                 CREATE TABLE trusted_worktrees (trust_id INTEGER PRIMARY KEY, absolute_path TEXT);
                 CREATE TABLE toolchains (workspace_id INTEGER, worktree_root_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT, relative_worktree_path TEXT);
                 CREATE TABLE user_toolchains (remote_connection_id INTEGER, workspace_id INTEGER, worktree_root_path TEXT, relative_worktree_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT);",
            )
            .expect("prepare schema")()
        .expect("create schema");

        connection
            .exec(
                "INSERT INTO solutions VALUES (1, 'Sol', '/base/old', NULL);
                 INSERT INTO solution_members VALUES (1, 1, 'Member', '/base/old/member', 0, NULL);
                 INSERT INTO workspaces VALUES (7, '/base/old/member', '0', '/base/old/member', '0', NULL);
                 INSERT INTO console_panel_state VALUES (7, 0, '/base/old/member');
                 INSERT INTO breakpoints VALUES (7, '/base/old/member/src/main.rs', 3);
                 INSERT INTO bookmarks VALUES (7, '/base/old/member/src/main.rs', 9);
                 INSERT INTO trusted_worktrees VALUES (1, '/base/old/member');
                 INSERT INTO toolchains VALUES (7, '/base/old/member', 'Rust', 'stable', '/usr/bin/cargo', '{}', '');
                 INSERT INTO user_toolchains VALUES (NULL, 7, '/base/old/member', '', 'Rust', 'stable', '/usr/bin/cargo', '{}');",
            )
            .expect("prepare seed")()
        .expect("seed rows");

        let mut insert_editor = connection
            .exec_bound::<(i64, i64, Vec<u8>, Vec<u8>)>(
                "INSERT INTO editors (item_id, workspace_id, path, buffer_path) VALUES (?, ?, ?, ?)",
            )
            .expect("prepare editors");
        insert_editor((
            1,
            7,
            b"/base/old/member/src/main.rs".to_vec(),
            b"/base/old/member/src/main.rs".to_vec(),
        ))
        .expect("insert editor");

        let mut insert_terminal = connection
            .exec_bound::<(i64, i64, Vec<u8>)>(
                "INSERT INTO terminals2 (workspace_id, item_id, working_directory) VALUES (?, ?, ?)",
            )
            .expect("prepare terminals");
        insert_terminal((7, 1, b"/base/old/member".to_vec())).expect("insert terminal");
    }

    fn rewrite() -> PathRewrite {
        PathRewrite {
            old: PathBuf::from("/base/old"),
            new: PathBuf::from("/base/new"),
        }
    }

    #[test]
    fn apply_str_rewrites_the_path_and_its_descendants_only() {
        let rewrite = rewrite();
        assert_eq!(rewrite.apply_str("/base/old").as_deref(), Some("/base/new"));
        assert_eq!(
            rewrite.apply_str("/base/old/member/src/main.rs").as_deref(),
            Some("/base/new/member/src/main.rs")
        );
        assert_eq!(rewrite.apply_str("/base/older"), None);
        assert_eq!(rewrite.apply_str("/base/other"), None);
        assert_eq!(rewrite.apply_str("/base/new/member"), None);
    }

    #[test]
    fn rewrites_every_path_bearing_row() {
        let connection = Connection::open_memory(Some("rewrites_every_path_bearing_row"));
        seed(&connection);

        rewrite_app_db(&connection, &rewrite()).expect("rewrite");

        let text = |query: &str| -> Vec<String> {
            connection.select::<String>(query).expect("prepare")().expect("select")
        };
        assert_eq!(text("SELECT root FROM solutions"), vec!["/base/new"]);
        assert_eq!(
            text("SELECT local_path FROM solution_members"),
            vec!["/base/new/member"]
        );
        assert_eq!(text("SELECT paths FROM workspaces"), vec!["/base/new/member"]);
        assert_eq!(
            text("SELECT identity_paths FROM workspaces"),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text("SELECT cwd FROM console_panel_state"),
            vec!["/base/new/member"]
        );
        assert_eq!(
            text("SELECT path FROM breakpoints"),
            vec!["/base/new/member/src/main.rs"]
        );
        assert_eq!(
            text("SELECT path FROM bookmarks"),
            vec!["/base/new/member/src/main.rs"]
        );
        assert_eq!(
            text("SELECT absolute_path FROM trusted_worktrees"),
            vec!["/base/new/member"]
        );

        let blobs = |query: &str| -> Vec<Vec<u8>> {
            connection.select::<Vec<u8>>(query).expect("prepare")().expect("select")
        };
        assert_eq!(
            blobs("SELECT path FROM editors"),
            vec![b"/base/new/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            blobs("SELECT buffer_path FROM editors"),
            vec![b"/base/new/member/src/main.rs".to_vec()]
        );
        assert_eq!(
            blobs("SELECT working_directory FROM terminals2"),
            vec![b"/base/new/member".to_vec()]
        );

        let counts = |query: &str| -> Vec<i64> {
            connection.select::<i64>(query).expect("prepare")().expect("select")
        };
        assert_eq!(
            counts("SELECT COUNT(*) FROM toolchains"),
            vec![0],
            "stale toolchain rows are deleted, not rewritten"
        );
        assert_eq!(counts("SELECT COUNT(*) FROM user_toolchains"), vec![0]);
    }

    #[test]
    fn workspace_identity_row_is_preserved_and_the_squatter_is_merged_away() {
        let connection =
            Connection::open_memory(Some("workspace_identity_row_is_preserved_and_merged"));
        seed(&connection);
        // A second workspace row already sits at the *target* path set — the
        // UNIQUE ix_workspaces_location would reject a blind UPDATE.
        connection
            .exec("INSERT INTO workspaces VALUES (9, '/base/new/member', '0', NULL, NULL, NULL);")
            .expect("prepare")()
        .expect("insert squatter");

        rewrite_app_db(&connection, &rewrite()).expect("rewrite");

        let ids: Vec<i64> = connection
            .select::<i64>("SELECT workspace_id FROM workspaces")
            .expect("prepare")()
        .expect("select");
        assert_eq!(
            ids,
            vec![7],
            "the migrating row keeps its workspace_id; the row already at the target is merged away"
        );
    }

    #[test]
    fn rewriting_twice_is_a_no_op() {
        let connection = Connection::open_memory(Some("rewriting_twice_is_a_no_op"));
        seed(&connection);

        rewrite_app_db(&connection, &rewrite()).expect("first");
        rewrite_app_db(&connection, &rewrite()).expect("second");

        let paths: Vec<String> = connection
            .select::<String>("SELECT paths FROM workspaces")
            .expect("prepare")()
        .expect("select");
        assert_eq!(paths, vec!["/base/new/member"]);
        let roots: Vec<String> = connection
            .select::<String>("SELECT root FROM solutions")
            .expect("prepare")()
        .expect("select");
        assert_eq!(roots, vec!["/base/new"]);
    }
}
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p solutions path_migrations`
Expected: FAIL — `cannot find type 'PathRewrite' in this scope`, `cannot find function 'rewrite_app_db'`.

- [x] **Step 3: Write the implementation**

Prepend to `crates/solutions/src/path_migrations.rs`:

```rust
//! Cold reconcile of a folder move: the heavy path rewiring that the hot
//! rename deliberately skips. Runs at startup, before any window opens, when
//! nothing holds the old paths any more.
//!
//! Every step is idempotent — re-running a partially applied migration is a
//! no-op for the parts that already landed — so a crash mid-reconcile is
//! recovered by simply running it again on the next start.

use anyhow::{Context as _, Result};
use db::sqlez::connection::Connection;
use std::path::{Path, PathBuf};
use util::path_list::{PathList, SerializedPathList};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRewrite {
    pub old: PathBuf,
    pub new: PathBuf,
}

impl PathRewrite {
    /// `Some(rewritten)` when `value` is the old path or lives under it;
    /// `None` otherwise. A sibling with a longer name (`/base/older` vs
    /// `/base/old`) must not match — hence the explicit separator check.
    pub fn apply_str(&self, value: &str) -> Option<String> {
        let old = self.old.to_string_lossy();
        let new = self.new.to_string_lossy();
        if value == old {
            return Some(new.into_owned());
        }
        let prefix = format!("{old}/");
        let suffix = value.strip_prefix(&prefix)?;
        Some(format!("{new}/{suffix}"))
    }

    /// The blob columns (`editors.path`, `terminals2.working_directory`) hold
    /// raw OS-string bytes, which are not guaranteed to be UTF-8.
    pub fn apply_bytes(&self, value: &[u8]) -> Option<Vec<u8>> {
        let old = self.old.to_string_lossy();
        let new = self.new.to_string_lossy();
        let (old, new) = (old.as_bytes(), new.as_bytes());
        if value == old {
            return Some(new.to_vec());
        }
        if value.len() > old.len() && value.starts_with(old) && value[old.len()] == b'/' {
            let mut out = new.to_vec();
            out.extend_from_slice(&value[old.len()..]);
            return Some(out);
        }
        None
    }
}

pub fn rewrite_app_db(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    rewrite_text_column(connection, "solutions", "id", "root", rewrite)?;
    rewrite_text_column(connection, "solution_members", "id", "local_path", rewrite)?;
    rewrite_workspaces(connection, rewrite)?;
    rewrite_console_panel_state(connection, rewrite)?;
    rewrite_blob_column(connection, "editors", "path", rewrite)?;
    rewrite_blob_column(connection, "editors", "buffer_path", rewrite)?;
    rewrite_blob_column(connection, "terminals2", "working_directory", rewrite)?;
    rewrite_keyless_text_column(connection, "breakpoints", "path", rewrite)?;
    rewrite_keyless_text_column(connection, "bookmarks", "path", rewrite)?;
    rewrite_text_column(
        connection,
        "trusted_worktrees",
        "trust_id",
        "absolute_path",
        rewrite,
    )?;
    delete_stale_toolchains(connection, rewrite)?;
    Ok(())
}

/// For tables with an INTEGER primary key we can UPDATE row by row.
fn rewrite_text_column(
    connection: &Connection,
    table: &str,
    key_column: &str,
    path_column: &str,
    rewrite: &PathRewrite,
) -> Result<()> {
    let rows: Vec<(i64, String)> = connection
        .select::<(i64, String)>(&format!(
            "SELECT {key_column}, {path_column} FROM {table} WHERE {path_column} IS NOT NULL"
        ))
        .with_context(|| format!("preparing select on {table}"))?()
    .with_context(|| format!("selecting from {table}"))?;

    let mut update = connection
        .exec_bound::<(String, i64)>(&format!(
            "UPDATE {table} SET {path_column} = ?1 WHERE {key_column} = ?2"
        ))
        .with_context(|| format!("preparing update on {table}"))?;
    for (key, value) in rows {
        if let Some(rewritten) = rewrite.apply_str(&value) {
            update((rewritten, key)).with_context(|| format!("updating {table}"))?;
        }
    }
    Ok(())
}

/// `breakpoints` and `bookmarks` have no primary key, so match on the old
/// value itself. Safe because the old path no longer exists as a real
/// directory once the reconcile runs.
fn rewrite_keyless_text_column(
    connection: &Connection,
    table: &str,
    path_column: &str,
    rewrite: &PathRewrite,
) -> Result<()> {
    let rows: Vec<String> = connection
        .select::<String>(&format!(
            "SELECT DISTINCT {path_column} FROM {table} WHERE {path_column} IS NOT NULL"
        ))
        .with_context(|| format!("preparing select on {table}"))?()
    .with_context(|| format!("selecting from {table}"))?;

    let mut update = connection
        .exec_bound::<(String, String)>(&format!(
            "UPDATE {table} SET {path_column} = ?1 WHERE {path_column} = ?2"
        ))
        .with_context(|| format!("preparing update on {table}"))?;
    for value in rows {
        if let Some(rewritten) = rewrite.apply_str(&value) {
            update((rewritten, value)).with_context(|| format!("updating {table}"))?;
        }
    }
    Ok(())
}

fn rewrite_blob_column(
    connection: &Connection,
    table: &str,
    path_column: &str,
    rewrite: &PathRewrite,
) -> Result<()> {
    let rows: Vec<Vec<u8>> = connection
        .select::<Vec<u8>>(&format!(
            "SELECT DISTINCT {path_column} FROM {table} WHERE {path_column} IS NOT NULL"
        ))
        .with_context(|| format!("preparing select on {table}"))?()
    .with_context(|| format!("selecting from {table}"))?;

    let mut update = connection
        .exec_bound::<(Vec<u8>, Vec<u8>)>(&format!(
            "UPDATE {table} SET {path_column} = ?1 WHERE {path_column} = ?2"
        ))
        .with_context(|| format!("preparing update on {table}"))?;
    for value in rows {
        if let Some(rewritten) = rewrite.apply_bytes(&value) {
            update((rewritten, value)).with_context(|| format!("updating {table}"))?;
        }
    }
    Ok(())
}

fn rewrite_console_panel_state(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    let rows: Vec<(i64, i64, String)> = connection
        .select::<(i64, i64, String)>(
            "SELECT workspace_id, tab_index, cwd FROM console_panel_state WHERE cwd IS NOT NULL",
        )
        .context("preparing select on console_panel_state")?()
    .context("selecting from console_panel_state")?;

    let mut update = connection
        .exec_bound::<(String, i64, i64)>(
            "UPDATE console_panel_state SET cwd = ?1 WHERE workspace_id = ?2 AND tab_index = ?3",
        )
        .context("preparing update on console_panel_state")?;
    for (workspace_id, tab_index, cwd) in rows {
        if let Some(rewritten) = rewrite.apply_str(&cwd) {
            update((rewritten, workspace_id, tab_index))
                .context("updating console_panel_state")?;
        }
    }
    Ok(())
}

/// `workspaces.paths` is the window's identity key (UNIQUE
/// `ix_workspaces_location`): every pane, tab, dock, editor, terminal and
/// console tab is FK'd on `workspace_id`, so losing the row loses the whole
/// layout. Two subtleties:
///   * `paths` is a `\n`-joined, *lexicographically sorted* list and
///     `paths_order` is the permutation back to the user's order — a rename
///     can change the sort, so round-trip through `PathList` rather than
///     patching the string.
///   * a row may already exist at the target path set (the user opened that
///     directory before). Blindly updating would violate the unique index, so
///     the squatter is deleted (its children cascade) and the *migrating* row
///     keeps its `workspace_id`.
fn rewrite_workspaces(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    type Row = (i64, Option<String>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = connection
        .select::<Row>(
            "SELECT workspace_id, paths, paths_order, identity_paths, identity_paths_order
             FROM workspaces",
        )
        .context("preparing select on workspaces")?()
    .context("selecting from workspaces")?;

    let mut delete_conflict = connection
        .exec_bound::<i64>("DELETE FROM workspaces WHERE workspace_id = ?")
        .context("preparing workspace delete")?;
    let mut update = connection
        .exec_bound::<(Option<String>, Option<String>, Option<String>, Option<String>, i64)>(
            "UPDATE workspaces
             SET paths = ?1, paths_order = ?2, identity_paths = ?3, identity_paths_order = ?4
             WHERE workspace_id = ?5",
        )
        .context("preparing workspace update")?;
    let mut select_conflict = connection
        .select_bound::<String, i64>(
            "SELECT workspace_id FROM workspaces
             WHERE paths = ? AND remote_connection_id IS NULL",
        )
        .context("preparing workspace conflict select")?;

    for (workspace_id, paths, paths_order, identity_paths, identity_paths_order) in rows {
        let rewritten = rewrite_path_list(paths.as_deref(), paths_order.as_deref(), rewrite);
        let rewritten_identity = rewrite_path_list(
            identity_paths.as_deref(),
            identity_paths_order.as_deref(),
            rewrite,
        );
        if rewritten.is_none() && rewritten_identity.is_none() {
            continue;
        }

        let (new_paths, new_order) = rewritten
            .clone()
            .map_or((paths.clone(), paths_order.clone()), |(paths, order)| {
                (Some(paths), Some(order))
            });
        let (new_identity, new_identity_order) = rewritten_identity.map_or(
            (identity_paths.clone(), identity_paths_order.clone()),
            |(paths, order)| (Some(paths), Some(order)),
        );

        if let Some(new_paths) = new_paths.as_deref() {
            for conflicting in select_conflict(new_paths.to_string())
                .context("selecting conflicting workspace")?
            {
                if conflicting != workspace_id {
                    delete_conflict(conflicting).context("deleting conflicting workspace")?;
                }
            }
        }

        update((
            new_paths,
            new_order,
            new_identity,
            new_identity_order,
            workspace_id,
        ))
        .context("updating workspace")?;
    }
    Ok(())
}

/// `None` when nothing in the list is under the old path.
fn rewrite_path_list(
    paths: Option<&str>,
    order: Option<&str>,
    rewrite: &PathRewrite,
) -> Option<(String, String)> {
    let paths = paths?;
    if paths.is_empty() {
        return None;
    }
    let list = PathList::deserialize(&SerializedPathList {
        paths: paths.to_string(),
        order: order.unwrap_or_default().to_string(),
    });
    let mut changed = false;
    let rewritten: Vec<PathBuf> = list
        .ordered_paths()
        .map(|path| {
            match rewrite.apply_str(&path.to_string_lossy()) {
                Some(new) => {
                    changed = true;
                    PathBuf::from(new)
                }
                None => path.clone(),
            }
        })
        .collect();
    if !changed {
        return None;
    }
    let serialized = PathList::new(&rewritten).serialize();
    Some((serialized.paths, serialized.order))
}

/// The toolchain path is part of the primary key and the toolchain itself is
/// re-detected on the next open, so a stale row is deleted rather than
/// rewritten.
fn delete_stale_toolchains(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    let old = rewrite.old.to_string_lossy().into_owned();
    let prefix = format!("{old}/%");
    for table in ["toolchains", "user_toolchains"] {
        let mut delete = connection
            .exec_bound::<(String, String)>(&format!(
                "DELETE FROM {table} WHERE worktree_root_path = ?1 OR worktree_root_path LIKE ?2"
            ))
            .with_context(|| format!("preparing delete on {table}"))?;
        delete((old.clone(), prefix.clone()))
            .with_context(|| format!("deleting stale rows from {table}"))?;
    }
    Ok(())
}
```

Register the module in `crates/solutions/src/solutions.rs`: add `pub mod path_migrations;` after `pub mod migrate;`.

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p solutions path_migrations`
Expected: PASS — 4 tests (`apply_str_rewrites_the_path_and_its_descendants_only`, `rewrites_every_path_bearing_row`, `workspace_identity_row_is_preserved_and_the_squatter_is_merged_away`, `rewriting_twice_is_a_no_op`).

- [x] **Step 5: Commit**

```bash
git add crates/solutions/src/path_migrations.rs crates/solutions/src/solutions.rs
git commit -m "solutions: Rewrite every path-bearing app-db row on a cold reconcile"
```

---

### Task 7: Cold reconcile — agent DB, transcript bucket, worktree repair

**Files:**
- Modify: `crates/solutions/src/path_migrations.rs`
- Modify: `crates/solutions/Cargo.toml` (dev-dependency `tempfile` is already present; no new deps)

**Interfaces:**
- Consumes: `PathRewrite` (Task 6).
- Produces:
  - `pub fn rewrite_agent_db(connection: &Connection, rewrite: &PathRewrite) -> Result<()>` — `solution_sessions.cwd`, `solution_session_background_agent.jsonl_path`, `solution_session_attachment.path` (path is in the PK → delete + reinsert).
  - `pub fn encode_claude_bucket(path: &Path) -> String` — claude's own encoding: every `/` and `.` becomes `-` (mirrors `crates/solution_agent/src/store/teammate_reconciler.rs:22-39`).
  - `pub fn move_transcript_bucket(claude_projects_dir: &Path, rewrite: &PathRewrite) -> Result<()>` — moves `<enc(old)>` → `<enc(new)>`, merging file-by-file when the target bucket exists; never renames over an existing bucket.
  - `pub fn repair_git_worktrees(members: &[(PathBuf, PathBuf)], rewrite: &PathRewrite) -> Result<()>` — `(member_root, solution_root)` pairs. Runs `git -C <member_root> worktree repair <tree paths…>` for every claude agent worktree owned by that member, in **both** locations: the legacy `<member_root>/.claude/worktrees/*` and the relocated `<solution_root>/.agents/worktrees/<member-dir>/*` (plan 3's `WorktreeCreate` hook puts them there). The tree paths are passed as **arguments** — see the doc comment in the code for why a bare `git worktree repair` is not enough, and why ownership is decided from the tree's `.git` pointer rather than from the `<member-dir>` folder name.

- [x] **Step 1: Write the failing tests**

Append to the `mod tests` in `crates/solutions/src/path_migrations.rs`:

```rust
    fn seed_agent_db(connection: &Connection) {
        connection
            .exec(
                "CREATE TABLE solution_sessions (id TEXT PRIMARY KEY, solution_id TEXT, cwd TEXT);
                 CREATE TABLE solution_session_background_agent (solution_session_id TEXT, agent_id TEXT, jsonl_path TEXT, PRIMARY KEY (solution_session_id, agent_id));
                 CREATE TABLE solution_session_attachment (session_id TEXT, solution_id TEXT, path TEXT, created_at_ms INTEGER, PRIMARY KEY (session_id, path));",
            )
            .expect("prepare schema")()
        .expect("create schema");
        connection
            .exec(
                "INSERT INTO solution_sessions VALUES ('s1', '1', '/base/old/member');
                 INSERT INTO solution_session_background_agent VALUES ('s1', 'a1', '/base/old/member/.claude/x.jsonl');
                 INSERT INTO solution_session_attachment VALUES ('s1', '1', '/base/old/member/inbox/a.png', 5);",
            )
            .expect("prepare seed")()
        .expect("seed rows");
    }

    #[test]
    fn rewrites_agent_db_rows_including_the_pk_path() {
        let connection = Connection::open_memory(Some("rewrites_agent_db_rows"));
        seed_agent_db(&connection);

        rewrite_agent_db(&connection, &rewrite()).expect("rewrite");
        // Idempotent.
        rewrite_agent_db(&connection, &rewrite()).expect("rewrite again");

        let text = |query: &str| -> Vec<String> {
            connection.select::<String>(query).expect("prepare")().expect("select")
        };
        assert_eq!(text("SELECT cwd FROM solution_sessions"), vec!["/base/new/member"]);
        assert_eq!(
            text("SELECT jsonl_path FROM solution_session_background_agent"),
            vec!["/base/new/member/.claude/x.jsonl"]
        );
        assert_eq!(
            text("SELECT path FROM solution_session_attachment"),
            vec!["/base/new/member/inbox/a.png"]
        );
        let count: Vec<i64> = connection
            .select::<i64>("SELECT COUNT(*) FROM solution_session_attachment")
            .expect("prepare")()
        .expect("select");
        assert_eq!(count, vec![1], "delete+reinsert must not duplicate the row");
    }

    #[test]
    fn encodes_a_claude_bucket_name_like_claude_does() {
        assert_eq!(
            encode_claude_bucket(Path::new("/home/spk/.spk/sawe/ss/spk-solutions")),
            "-home-spk--spk-sawe-ss-spk-solutions"
        );
    }

    #[test]
    fn moves_the_transcript_bucket() {
        let projects = tempfile::tempdir().expect("tempdir");
        let rewrite = rewrite();
        let old_bucket = projects.path().join(encode_claude_bucket(&rewrite.old));
        std::fs::create_dir_all(&old_bucket).expect("mkdir bucket");
        std::fs::write(old_bucket.join("session.jsonl"), b"{}").expect("write");

        move_transcript_bucket(projects.path(), &rewrite).expect("move");

        let new_bucket = projects.path().join(encode_claude_bucket(&rewrite.new));
        assert!(new_bucket.join("session.jsonl").is_file());
        assert!(!old_bucket.exists());

        // Idempotent: a second run with no source bucket is a no-op.
        move_transcript_bucket(projects.path(), &rewrite).expect("move again");
        assert!(new_bucket.join("session.jsonl").is_file());
    }

    #[test]
    fn merges_into_an_existing_transcript_bucket() {
        let projects = tempfile::tempdir().expect("tempdir");
        let rewrite = rewrite();
        let old_bucket = projects.path().join(encode_claude_bucket(&rewrite.old));
        let new_bucket = projects.path().join(encode_claude_bucket(&rewrite.new));
        std::fs::create_dir_all(old_bucket.join("subagents")).expect("mkdir old");
        std::fs::write(old_bucket.join("a.jsonl"), b"a").expect("write a");
        std::fs::write(old_bucket.join("subagents/s.jsonl"), b"s").expect("write s");
        std::fs::create_dir_all(&new_bucket).expect("mkdir new");
        std::fs::write(new_bucket.join("b.jsonl"), b"b").expect("write b");
        std::fs::write(new_bucket.join("a.jsonl"), b"keep").expect("write existing a");

        move_transcript_bucket(projects.path(), &rewrite).expect("merge");

        assert_eq!(std::fs::read(new_bucket.join("b.jsonl")).expect("b"), b"b");
        assert_eq!(
            std::fs::read(new_bucket.join("a.jsonl")).expect("a"),
            b"keep",
            "an existing file in the target bucket is never overwritten"
        );
        assert_eq!(
            std::fs::read(new_bucket.join("subagents/s.jsonl")).expect("s"),
            b"s"
        );
        assert!(!old_bucket.exists(), "the source bucket is drained and removed");
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p solutions path_migrations`
Expected: FAIL — `cannot find function 'rewrite_agent_db'`, `cannot find function 'encode_claude_bucket'`, `cannot find function 'move_transcript_bucket'`.

- [x] **Step 3: Write the implementation**

Append to the non-test part of `crates/solutions/src/path_migrations.rs`:

```rust
pub fn rewrite_agent_db(connection: &Connection, rewrite: &PathRewrite) -> Result<()> {
    let sessions: Vec<(String, String)> = connection
        .select::<(String, String)>(
            "SELECT id, cwd FROM solution_sessions WHERE cwd IS NOT NULL",
        )
        .context("preparing select on solution_sessions")?()
    .context("selecting from solution_sessions")?;
    let mut update_session = connection
        .exec_bound::<(String, String)>("UPDATE solution_sessions SET cwd = ?1 WHERE id = ?2")
        .context("preparing update on solution_sessions")?;
    for (id, cwd) in sessions {
        if let Some(rewritten) = rewrite.apply_str(&cwd) {
            update_session((rewritten, id)).context("updating solution_sessions")?;
        }
    }

    let agents: Vec<(String, String, String)> = connection
        .select::<(String, String, String)>(
            "SELECT solution_session_id, agent_id, jsonl_path FROM solution_session_background_agent",
        )
        .context("preparing select on solution_session_background_agent")?()
    .context("selecting from solution_session_background_agent")?;
    let mut update_agent = connection
        .exec_bound::<(String, String, String)>(
            "UPDATE solution_session_background_agent SET jsonl_path = ?1
             WHERE solution_session_id = ?2 AND agent_id = ?3",
        )
        .context("preparing update on solution_session_background_agent")?;
    for (session_id, agent_id, jsonl_path) in agents {
        if let Some(rewritten) = rewrite.apply_str(&jsonl_path) {
            update_agent((rewritten, session_id, agent_id))
                .context("updating solution_session_background_agent")?;
        }
    }

    // `path` is part of the attachment PK, so an UPDATE would have to move the
    // key — delete + reinsert instead.
    let attachments: Vec<(String, String, String, i64)> = connection
        .select::<(String, String, String, i64)>(
            "SELECT session_id, solution_id, path, created_at_ms FROM solution_session_attachment",
        )
        .context("preparing select on solution_session_attachment")?()
    .context("selecting from solution_session_attachment")?;
    let mut delete_attachment = connection
        .exec_bound::<(String, String)>(
            "DELETE FROM solution_session_attachment WHERE session_id = ?1 AND path = ?2",
        )
        .context("preparing delete on solution_session_attachment")?;
    let mut insert_attachment = connection
        .exec_bound::<(String, String, String, i64)>(
            "INSERT OR IGNORE INTO solution_session_attachment
                 (session_id, solution_id, path, created_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .context("preparing insert on solution_session_attachment")?;
    for (session_id, solution_id, path, created_at_ms) in attachments {
        if let Some(rewritten) = rewrite.apply_str(&path) {
            delete_attachment((session_id.clone(), path))
                .context("deleting solution_session_attachment")?;
            insert_attachment((session_id, solution_id, rewritten, created_at_ms))
                .context("reinserting solution_session_attachment")?;
        }
    }
    Ok(())
}

/// claude keys its transcript bucket by the session cwd with every `/` and
/// `.` replaced by `-` (`<CLAUDE_CONFIG_DIR|~/.claude>/projects/<enc(cwd)>`).
/// Mirrors `solution_agent/src/store/teammate_reconciler.rs:22-39`; there is
/// no setting that overrides the location.
pub fn encode_claude_bucket(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut encoded = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '/' | '.' => encoded.push('-'),
            other => encoded.push(other),
        }
    }
    encoded
}

pub fn move_transcript_bucket(claude_projects_dir: &Path, rewrite: &PathRewrite) -> Result<()> {
    let source = claude_projects_dir.join(encode_claude_bucket(&rewrite.old));
    if !source.exists() {
        return Ok(());
    }
    let target = claude_projects_dir.join(encode_claude_bucket(&rewrite.new));
    if !target.exists() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&source, &target).with_context(|| {
            format!("moving {} to {}", source.display(), target.display())
        })?;
        return Ok(());
    }
    merge_dir(&source, &target)?;
    std::fs::remove_dir_all(&source)
        .with_context(|| format!("removing drained bucket {}", source.display()))?;
    Ok(())
}

/// Copy every entry of `source` into `target`, never overwriting an existing
/// file. A transcript file we already have at the target is by definition the
/// same session (the id is in the name), so keeping the target's copy is safe
/// and preserves anything written since the rename.
fn merge_dir(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    for entry in
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading an entry of {}", source.display()))?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", from.display()))?;
        if file_type.is_dir() {
            merge_dir(&from, &to)?;
        } else if !to.exists() {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// A claude agent worktree is a *linked* git worktree: the tree lives in one
/// place, but its **admin directory always lives inside the member repo** at
/// `<member>/.git/worktrees/<name>/`, and the two point at each other with
/// **absolute** paths. So a rename breaks it in one of two directions:
///
///   * a **member** rename moves the repo (and with it the admin dir) — the
///     tree's `.git` file still names the old admin dir;
///   * a **solution** rename moves the *trees* — the admin dir's `gitdir` file
///     still names the old tree location. (Plan 3's `WorktreeCreate` hook
///     relocates new trees to `<solution_root>/.agents/worktrees/<member-dir>/<name>`;
///     legacy trees are still at `<member>/.claude/worktrees/<name>`. Both are
///     scanned here.)
///
/// `git -C <member> worktree repair <tree paths…>` fixes **both** directions —
/// but only when the moved tree paths are passed as **arguments**. A bare
/// `git worktree repair` can only repair trees it can still reach through the
/// (possibly stale) admin entries, so it silently misses the trees whose
/// location changed.
pub fn repair_git_worktrees(members: &[(PathBuf, PathBuf)], rewrite: &PathRewrite) -> Result<()> {
    for (member_root, solution_root) in members {
        // Candidate trees, both locations. The `<member-dir>` level under
        // `.agents/worktrees` is itself named after the member's *old* folder
        // after a member rename, so do NOT filter by that name — collect every
        // tree and decide ownership from the tree's own `.git` pointer.
        let mut candidates = Vec::new();
        collect_dirs(&member_root.join(".claude").join("worktrees"), &mut candidates);
        let relocated = solution_root.join(".agents").join("worktrees");
        let mut member_dirs = Vec::new();
        collect_dirs(&relocated, &mut member_dirs);
        for member_dir in &member_dirs {
            collect_dirs(member_dir, &mut candidates);
        }

        let trees: Vec<PathBuf> = candidates
            .into_iter()
            .filter(|tree| owning_repo(tree, rewrite).as_deref() == Some(member_root.as_path()))
            .collect();
        if trees.is_empty() {
            continue;
        }
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(member_root)
            .arg("worktree")
            .arg("repair")
            .args(&trees)
            .output();
        match output {
            Ok(output) if !output.status.success() => log::warn!(
                "path_migrations: `git worktree repair` in {} failed: {}",
                member_root.display(),
                String::from_utf8_lossy(&output.stderr).trim(),
            ),
            Err(err) => log::warn!(
                "path_migrations: running `git worktree repair` in {} failed: {err}",
                member_root.display(),
            ),
            Ok(_) => {}
        }
    }
    Ok(())
}

fn collect_dirs(parent: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            out.push(entry.path());
        }
    }
}

/// A linked worktree's `.git` is a file reading
/// `gitdir: <repo>/.git/worktrees/<name>` — an **absolute** path that may still
/// name the pre-rename location, hence the rewrite before matching. Returns the
/// repo (member) the tree belongs to.
fn owning_repo(tree: &Path, rewrite: &PathRewrite) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(tree.join(".git")).ok()?;
    let pointer = contents.trim().strip_prefix("gitdir:")?.trim();
    let pointer = rewrite
        .apply_str(pointer)
        .unwrap_or_else(|| pointer.to_string());
    // <repo>/.git/worktrees/<name> → <repo>
    Path::new(&pointer)
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
}
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p solutions path_migrations`
Expected: PASS — 8 tests total in the module.

- [x] **Step 5: Commit**

```bash
git add crates/solutions/src/path_migrations.rs
git commit -m "solutions: Reconcile agent-db rows, transcript buckets and legacy worktrees"
```

---

### Task 8: `drain_and_apply` and the startup wiring

**Files:**
- Modify: `crates/solutions/src/path_migrations.rs`
- Modify: `crates/solutions/src/store.rs:129-165` (`init_with_db`)
- Test: `crates/solutions/src/path_migrations.rs` (integration-style test in the same module)

**Interfaces:**
- Consumes: `rewrite_app_db`, `rewrite_agent_db`, `move_transcript_bucket`, `repair_git_worktrees`, `PathRewrite`, `SolutionsDb::{load_pending_path_migrations, delete_pending_path_migration}`.
- Produces:
  - `pub struct ReconcileContext { pub app_db: ThreadSafeConnection, pub agent_db_path: Option<PathBuf>, pub claude_projects_dir: Option<PathBuf> }`
  - `pub fn apply_one(context: &ReconcileContext, rewrite: &PathRewrite) -> Result<()>` — the whole per-migration sequence (app db → agent db → bucket → worktree repair → drop the symlink). Idempotent.
  - `pub fn drain_and_apply(cx: &mut App) -> Task<Result<()>>` — the contract entry point. Reads the globals synchronously, then does the work on the background executor.

`init_with_db` blocks on the returned task **before** hydrating the store, so the store loads already-rewritten rows and no window can open on a stale path. `gpui::block_on` on a *background* task is the pattern already used in `init_with_db` (`gpui::block_on(db.load_all_active_members())`).

- [x] **Step 1: Write the failing test**

Append to `mod tests` in `crates/solutions/src/path_migrations.rs`:

```rust
    #[test]
    fn apply_one_removes_the_compat_symlink_and_is_crash_safe() {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("old");
        let new_root = base.path().join("new");
        std::fs::create_dir_all(&new_root).expect("mkdir new");
        std::os::unix::fs::symlink(&new_root, &old_root).expect("symlink");

        let projects = tempfile::tempdir().expect("projects tempdir");
        let rewrite = PathRewrite {
            old: old_root.clone(),
            new: new_root.clone(),
        };
        let old_bucket = projects.path().join(encode_claude_bucket(&old_root));
        std::fs::create_dir_all(&old_bucket).expect("mkdir bucket");
        std::fs::write(old_bucket.join("s.jsonl"), b"{}").expect("write");

        let app = Connection::open_memory(Some("apply_one_removes_the_compat_symlink"));
        seed(&app);
        let agent = Connection::open_memory(Some("apply_one_agent"));
        seed_agent_db(&agent);

        apply_one_with_connections(&app, Some(&agent), Some(projects.path()), &rewrite)
            .expect("apply");
        assert!(!old_root.exists(), "the compat symlink is gone");
        assert!(
            projects
                .path()
                .join(encode_claude_bucket(&new_root))
                .join("s.jsonl")
                .is_file()
        );

        // Crash-safe: re-running a fully applied migration is a clean no-op.
        apply_one_with_connections(&app, Some(&agent), Some(projects.path()), &rewrite)
            .expect("apply again");
        assert!(!old_root.exists());
    }

    #[test]
    fn apply_one_refuses_to_delete_a_real_directory_at_the_old_path() {
        let base = tempfile::tempdir().expect("tempdir");
        let old_root = base.path().join("old");
        let new_root = base.path().join("new");
        std::fs::create_dir_all(&new_root).expect("mkdir new");
        // The user re-created a *real* directory at the old path after the
        // rename — it is not our symlink and must never be removed.
        std::fs::create_dir_all(&old_root).expect("mkdir old");
        std::fs::write(old_root.join("user-file.txt"), b"precious").expect("write");

        let app = Connection::open_memory(Some("apply_one_refuses_to_delete"));
        seed(&app);
        apply_one_with_connections(
            &app,
            None,
            None,
            &PathRewrite {
                old: old_root.clone(),
                new: new_root.clone(),
            },
        )
        .expect("apply");

        assert!(
            old_root.join("user-file.txt").is_file(),
            "a real directory at the old path is left alone"
        );
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p solutions path_migrations::tests::apply_one`
Expected: FAIL — `cannot find function 'apply_one_with_connections' in this scope`.

- [x] **Step 3: Write the implementation**

Append to the non-test part of `crates/solutions/src/path_migrations.rs`:

```rust
use db::sqlez::thread_safe_connection::ThreadSafeConnection;
use gpui::{App, Task};

pub struct ReconcileContext {
    pub app_db: ThreadSafeConnection,
    /// `None` when the agent DB file does not exist yet (fresh install).
    pub agent_db_path: Option<PathBuf>,
    /// `<CLAUDE_CONFIG_DIR|~/.claude>/projects`. `None` when the home
    /// directory cannot be resolved.
    pub claude_projects_dir: Option<PathBuf>,
}

pub fn apply_one(context: &ReconcileContext, rewrite: &PathRewrite) -> Result<()> {
    let agent = match context.agent_db_path.as_ref() {
        Some(path) if path.exists() => Some(
            Connection::open_file(&path.to_string_lossy())
                .with_context(|| format!("opening {}", path.display()))?,
        ),
        _ => None,
    };
    apply_one_with_connections(
        &context.app_db,
        agent.as_ref(),
        context.claude_projects_dir.as_deref(),
        rewrite,
    )
}

/// The whole per-migration sequence, on plain connections so it is testable
/// without an `App`. Every step is a no-op when it has already been applied,
/// which is what makes a crash mid-reconcile recoverable by re-running.
pub(crate) fn apply_one_with_connections(
    app_db: &Connection,
    agent_db: Option<&Connection>,
    claude_projects_dir: Option<&Path>,
    rewrite: &PathRewrite,
) -> Result<()> {
    rewrite_app_db(app_db, rewrite)?;
    if let Some(agent_db) = agent_db {
        rewrite_agent_db(agent_db, rewrite)?;
    }
    if let Some(projects) = claude_projects_dir {
        move_transcript_bucket(projects, rewrite)?;
    }

    // `(member_root, solution_root)` for every member that now lives under the
    // moved path. The solution root is needed because the relocated agent
    // worktrees sit at `<solution_root>/.agents/worktrees/<member-dir>/*`,
    // outside the member itself.
    let members: Vec<(PathBuf, PathBuf)> = app_db
        .select::<(String, String)>(
            "SELECT solution_members.local_path, solutions.root
             FROM solution_members
             JOIN solutions ON solutions.id = solution_members.solution_id",
        )
        .context("preparing member select")?()
    .context("selecting members")?
    .into_iter()
    .map(|(member, solution)| (PathBuf::from(member), PathBuf::from(solution)))
    .filter(|(member, _)| member.starts_with(&rewrite.new))
    .collect();
    repair_git_worktrees(&members, rewrite)?;

    remove_compat_link(&rewrite.old)?;
    Ok(())
}

/// Only ever removes a *symlink*. If the user re-created a real directory at
/// the old path after the rename, it is theirs and must survive.
fn remove_compat_link(old: &Path) -> Result<()> {
    match std::fs::symlink_metadata(old) {
        Err(_) => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::remove_file(old)
            .with_context(|| format!("removing the compat link at {}", old.display())),
        Ok(_) => {
            log::warn!(
                "path_migrations: {} is a real directory, not our compat link — leaving it alone",
                old.display()
            );
            Ok(())
        }
    }
}

/// Drain `pending_path_migrations` and apply every recorded move. Called from
/// `SolutionStore::init_with_db` **before the store is hydrated and before any
/// window opens** — nothing is live, so this is the moment to rewrite the
/// paths that the hot rename deliberately left stale.
pub fn drain_and_apply(cx: &mut App) -> Task<Result<()>> {
    let solutions_db = crate::db::SolutionsDb::global(cx);
    let context = ReconcileContext {
        app_db: db::AppDatabase::global(cx).clone(),
        agent_db_path: Some(
            paths::data_dir()
                .join("solution_agent")
                .join("solution_agent.db"),
        ),
        claude_projects_dir: dirs::home_dir().map(|home| home.join(".claude").join("projects")),
    };
    cx.background_spawn(async move {
        let pending = solutions_db
            .load_pending_path_migrations()
            .context("loading pending_path_migrations")?;
        for (id, old_path, new_path) in pending {
            let rewrite = PathRewrite {
                old: PathBuf::from(old_path),
                new: PathBuf::from(new_path),
            };
            if let Err(err) = apply_one(&context, &rewrite) {
                // Leave the row in place: the next start retries. Every step is
                // idempotent, so a partially applied migration resumes cleanly.
                log::error!(
                    "path_migrations: reconciling {} → {} failed: {err:#}. Will retry on the next start.",
                    rewrite.old.display(),
                    rewrite.new.display(),
                );
                continue;
            }
            solutions_db
                .delete_pending_path_migration(id)
                .await
                .context("deleting the drained pending_path_migrations row")?;
        }
        Ok(())
    })
}
```

`dirs` and `paths` must be dependencies of `solutions`: `paths.workspace = true` is already there; add `dirs.workspace = true` to `crates/solutions/Cargo.toml` under `[dependencies]` if it is missing (it is the crate `solution_agent` already uses for `home_dir()`).

Then wire the drain into `crates/solutions/src/store.rs::init_with_db`, as the **first** thing after the legacy import, before `load_from_db_blocking`:

```rust
    fn init_with_db(db: SolutionsDb, cx: &mut App) {
        let json_path = paths::config_dir().join("solutions.json");
        if let Err(err) = crate::migrate::run_one_time_migration(&db, &json_path) {
            log::error!("solutions::store: legacy import failed: {err}. Continuing with empty DB.");
        }
        // Cold reconcile of any folder move recorded by a rename in a previous
        // run. MUST complete before the store is hydrated and before any window
        // opens — it rewrites `solutions.root`, `solution_members.local_path`
        // and the workspace identity rows the window layout is keyed on.
        if let Err(err) = gpui::block_on(crate::path_migrations::drain_and_apply(cx)) {
            log::error!("solutions::store: path-migration reconcile failed: {err:#}");
        }
        let config = match Self::load_from_db_blocking(&db) {
            // … unchanged …
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p solutions path_migrations`
Expected: PASS — 10 tests, including `apply_one_removes_the_compat_symlink_and_is_crash_safe` and `apply_one_refuses_to_delete_a_real_directory_at_the_old_path`.

- [x] **Step 5: Verify the whole crate still builds**

Run: `cargo check -p solutions`
Expected: PASS, no warnings about the new module.

- [x] **Step 6: Commit**

```bash
git add crates/solutions/src/path_migrations.rs crates/solutions/src/store.rs crates/solutions/Cargo.toml
git commit -m "solutions: Drain pending path migrations at startup before any window opens"
```

---

### Task 9: UI — rename modals and the project-tab entry

**Files:**
- Create: `crates/solutions_ui/src/modals/rename_member.rs`
- Modify: `crates/solutions_ui/src/modals.rs` (module list + re-export)
- Modify: `crates/solutions_ui/src/modals/rename_solution.rs` (pass `SolutionId` by value; render the error instead of swallowing it)
- Modify: `crates/solutions_ui/src/actions.rs:114-119` (add `RenameMember`)
- Modify: `crates/solutions_ui/src/project_tab.rs:177-199` (context-menu entry)
- Modify: `crates/solutions_ui/src/solutions_ui.rs:366-445` (register the action, mirroring `RemoveMember`)

**Interfaces:**
- Consumes: `SolutionStore::rename_member(MemberId, &str, cx)`, `SolutionStore::rename_solution(SolutionId, &str, cx)`, `SolutionStore::find_member(MemberId)`.
- Produces: `solutions_ui::actions::RenameMember { member_id: i64 }`; `modals::open_rename_member(workspace: &mut Workspace, id: MemberId, window: &mut Window, cx: &mut Context<Workspace>)`.

A rename can now fail (bad name, collision, cross-device), so the modal must **show** the error and stay open instead of `log_err()`-ing it away.

- [x] **Step 1: Write the failing test**

Add to the bottom of `crates/solutions_ui/src/modals/rename_member.rs` (create the file with only this test):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn confirm_reports_a_collision_and_keeps_the_modal_open(cx: &mut gpui::TestAppContext) {
        let error = RenameMemberModal::describe_error(&anyhow::anyhow!(
            solutions::FolderNameError::ExistsOnDisk {
                folder: "taken".into()
            }
        ));
        assert_eq!(
            error,
            "Directory 'taken' already exists on disk (not owned by any solution)"
        );
        // A blank name never reaches the store.
        assert!(RenameMemberModal::sanitize("   ").is_none());
        assert_eq!(RenameMemberModal::sanitize("  New Project "), Some("New Project".to_string()));
        let _ = cx;
    }
}
```

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p solutions_ui rename_member`
Expected: FAIL — `cannot find type 'RenameMemberModal' in this scope`.

- [x] **Step 3: Write the modal**

Prepend to `crates/solutions_ui/src/modals/rename_member.rs`:

```rust
use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use solutions::{MemberId, SolutionStore};
use ui::prelude::*;
use workspace::{ModalView, Workspace};

/// Single-field modal for renaming a member project. Renaming now also moves
/// the member's directory, so — unlike the old name-only rename — it can fail
/// (empty derivation, collision, cross-device move). The error is rendered in
/// the modal and the modal stays open.
pub struct RenameMemberModal {
    id: MemberId,
    name_editor: Entity<Editor>,
    focus_handle: FocusHandle,
    error: Option<SharedString>,
}

impl RenameMemberModal {
    fn new(id: MemberId, current_name: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_editor = cx.new(|cx| Editor::single_line(window, cx));
        name_editor.update(cx, |editor, cx| {
            editor.set_text(current_name, window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
        });
        let focus_handle = cx.focus_handle();
        Self {
            id,
            name_editor,
            focus_handle,
            error: None,
        }
    }

    fn sanitize(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    fn describe_error(error: &anyhow::Error) -> String {
        error.to_string()
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(new_name) = Self::sanitize(&self.name_editor.read(cx).text(cx)) else {
            return;
        };
        let id = self.id;
        let result = SolutionStore::global(cx).update(cx, |store, cx| store.rename_member(id, &new_name, cx));
        match result {
            Ok(()) => {
                self.error = None;
                cx.emit(DismissEvent);
            }
            Err(error) => {
                self.error = Some(Self::describe_error(&error).into());
                cx.notify();
            }
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for RenameMemberModal {}

impl Focusable for RenameMemberModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name_editor.focus_handle(cx)
    }
}

impl ModalView for RenameMemberModal {
    fn debug_kind(&self) -> &'static str {
        "RenameMember"
    }
}

impl Render for RenameMemberModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RenameMemberModal")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .track_focus(&self.focus_handle)
            .w(rems(28.))
            .p_4()
            .gap_3()
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Label::new("Rename Project").size(LabelSize::Large))
            .child(
                Label::new("Renaming also renames the project's folder on disk.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(self.name_editor.clone())
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("rename-member-cancel", "Cancel").on_click(cx.listener(
                        |this, _, window, cx| {
                            this.cancel(&menu::Cancel, window, cx);
                        },
                    )))
                    .child(
                        Button::new("rename-member-save", "Save")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm(&menu::Confirm, window, cx);
                            })),
                    ),
            )
    }
}

/// Entry point for the `RenameMember` action. No-op if the id is unknown
/// (stale action targeting an already-removed member).
pub fn open_rename_member(
    workspace: &mut Workspace,
    id: MemberId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let store = SolutionStore::global(cx);
    let Some(current_name) =
        store.read_with(cx, |store, _| store.find_member(id).map(|member| member.name.clone()))
    else {
        return;
    };
    workspace.toggle_modal(window, cx, move |window, cx| {
        RenameMemberModal::new(id, &current_name, window, cx)
    });
}
```

In `crates/solutions_ui/src/modals.rs`, add `mod rename_member;` to the module list and `pub(crate) use rename_member::open_rename_member;` to the re-exports.

- [x] **Step 4: Run the test to verify it passes**

Run: `cargo test -p solutions_ui rename_member`
Expected: PASS — `confirm_reports_a_collision_and_keeps_the_modal_open`.

- [x] **Step 5: Add the action and the context-menu entry**

In `crates/solutions_ui/src/actions.rs`, after `RemoveMember`:

```rust
/// Open the rename modal for a member project. Dispatched from the project
/// tab's right-click menu. Renaming also renames the member's folder on disk.
#[derive(PartialEq, Clone, Debug, Deserialize, Serialize, JsonSchema, Action)]
#[action(namespace = solutions)]
pub struct RenameMember {
    pub member_id: i64,
}
```

In `crates/solutions_ui/src/project_tab.rs`, extend the right-click menu built at `:177-199` — add the entry **above** the destructive one, and capture the member id alongside the ids already captured for the menu closure:

```rust
            .menu(move |window, cx| {
                let solution_id = solution_for_menu;
                let member_id = member_for_menu;
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.action(
                        "Rename…",
                        Box::new(RenameMember {
                            member_id: member_id.0,
                        }),
                    )
                    .separator()
                    .action(
                        "Remove from Solution…",
                        Box::new(RemoveMember {
                            solution_id: solution_id.0,
                            member_id: member_id.0,
                        }),
                    )
                })
            })
```

(`member_for_menu` is the `MemberId` copy of the tab's member, cloned next to the existing `solution_for_menu` / `catalog_for_menu` bindings; import `RenameMember` in the file's `use crate::actions::{…}` list.)

In `crates/solutions_ui/src/solutions_ui.rs`, next to the `RemoveMember` handler (`:366`), register:

```rust
    workspace.register_action(|workspace, action: &RenameMember, window, cx| {
        crate::modals::open_rename_member(workspace, solutions::MemberId(action.member_id), window, cx);
    });
```

…and add `RenameMember` to the `use crate::actions::{…}` import list at `:41`.

- [x] **Step 6: Surface rename errors in the solution modal too**

In `crates/solutions_ui/src/modals/rename_solution.rs`: add an `error: Option<SharedString>` field (initialised to `None` in `new`), replace `confirm` with:

```rust
    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let new_name = self.name_editor.read(cx).text(cx).trim().to_string();
        if new_name.is_empty() {
            return;
        }
        let id = self.id;
        let result = SolutionStore::global(cx)
            .update(cx, |store, cx| store.rename_solution(id, &new_name, cx));
        match result {
            Ok(()) => {
                self.error = None;
                cx.emit(DismissEvent);
            }
            Err(error) => {
                self.error = Some(error.to_string().into());
                cx.notify();
            }
        }
    }
```

…drop the now-unused `use util::ResultExt as _;`, and in `render` insert, right after `.child(self.name_editor.clone())`:

```rust
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
```

Also update `open_rename_solution` to look the solution up through the contract API:

```rust
    let Some(current_name) = store.read_with(cx, |store, _| {
        store.find_solution(id).map(|solution| solution.name.clone())
    }) else {
        return;
    };
```

- [x] **Step 7: Build and run the crate's tests**

Run: `cargo test -p solutions_ui`
Expected: PASS (existing tests plus the new one). `cargo check -p solutions_ui` must be clean — this is where the `rename_solution(id, …)`-by-value signature change from Task 5 lands.

- [x] **Step 8: Commit**

```bash
git add crates/solutions_ui/src/modals.rs crates/solutions_ui/src/modals/rename_member.rs crates/solutions_ui/src/modals/rename_solution.rs crates/solutions_ui/src/actions.rs crates/solutions_ui/src/project_tab.rs crates/solutions_ui/src/solutions_ui.rs
git commit -m "solutions_ui: Add a member rename modal and surface rename failures"
```

---

### Task 10: MCP — `solutions.rename` semantics and `solutions.rename_member`

**Files:**
- Modify: `crates/solutions/src/mcp/solutions_lifecycle.rs:415-482` (the `solutions.rename` block) and `:10-25` (registration)
- Modify: `crates/editor_mcp/src/lifecycle.rs:139-160` (`GLOBAL_TOOLS`)
- Modify: `CLAUDE.md` (tool catalog count 87 → 88, and the `solutions.*` list gains `rename_member`)
- Test: `crates/solutions/src/mcp/tests.rs`

**Interfaces:**
- Consumes: `SolutionStore::rename_solution`, `SolutionStore::rename_member`, `SolutionStore::find_member`.
- Produces:
  - `solutions.rename` — `{ solution_id: i64, new_name: String }` → `{ solution_id: i64, root: String }` (the **new** root; the tool now moves the folder).
  - `solutions.rename_member` — `{ member_id: i64, new_name: String }` → `{ member_id: i64, local_path: String }`.
  - `RenameMemberTool`, `RenameMemberParams`, `RenameMemberResult` (exported from `mcp::solutions_lifecycle`, re-exported by `mcp.rs`'s `pub use solutions_lifecycle::*;`).

- [x] **Step 1: Write the failing test**

Append to `crates/solutions/src/mcp/tests.rs`:

```rust
#[gpui::test]
async fn rename_member_tool_moves_the_folder(cx: &mut gpui::TestAppContext) {
    let base = tempfile::tempdir().expect("tempdir");
    let solution_root = base.path().join("sol");
    let member_path = solution_root.join("old-project");
    std::fs::create_dir_all(&member_path).expect("mkdir");

    let store = cx.update(|cx| crate::store::install_global_for_test(cx));
    let member_id = store.update(cx, |store, cx| {
        let solution_id = store
            .create_solution("Sol", base.path().to_path_buf(), cx)
            .expect("create");
        let _ = solution_id;
        store.solutions()[0].members[0].id
    });

    let response = RenameMemberTool
        .run(
            RenameMemberParams {
                member_id: member_id.0,
                new_name: "New Project".into(),
            },
            &mut cx.to_async(),
        )
        .await
        .expect("rename");

    assert_eq!(
        response.structured_content.local_path,
        solution_root.join("New-Project").to_string_lossy()
    );
    let _ = member_path;
}
```

If `install_global_for_test` + `create_solution` do not let the test seed a member directly, seed the store with `crate::store::for_test_with_solution` (Task 4's fixture) and install it as the global instead — the assertion (the tool returns the **new** `local_path` and the folder moved) is what matters.

- [x] **Step 2: Run the test to verify it fails**

Run: `cargo test -p solutions mcp::tests::rename_member_tool`
Expected: FAIL — `cannot find struct 'RenameMemberTool' in this scope`.

- [x] **Step 3: Write the implementation**

In `crates/solutions/src/mcp/solutions_lifecycle.rs`, replace the `solutions.rename` doc comment and result type, and add the new tool below it:

```rust
// =====================================================================
// solutions.rename
// =====================================================================

/// Rename a Solution. Also renames its directory on disk (the folder name is
/// derived from the display name) and records a pending path migration that
/// the next cold start reconciles. `solution_id` is stable across a rename.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RenameSolutionParams {
    pub solution_id: i64,
    pub new_name: String,
}

impl<'de> Deserialize<'de> for RenameSolutionParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            solution_id: i64,
            new_name: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            solution_id: inner.solution_id,
            new_name: inner.new_name,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenameSolutionResult {
    pub solution_id: i64,
    /// The solution's root **after** the rename.
    pub root: String,
}

#[derive(Clone)]
pub struct RenameSolutionTool;

impl McpServerTool for RenameSolutionTool {
    type Input = RenameSolutionParams;
    type Output = RenameSolutionResult;
    const NAME: &'static str = "solutions.rename";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(
            input.solution_id != 0,
            "invalid_params: solution_id is required"
        );
        anyhow::ensure!(
            !input.new_name.trim().is_empty(),
            "invalid_params: new_name is required"
        );
        let id = crate::SolutionId(input.solution_id);
        let root = cx.update(|cx| -> anyhow::Result<String> {
            let store = SolutionStore::global(cx);
            store.update(cx, |store, cx| store.rename_solution(id, &input.new_name, cx))?;
            let root = store
                .read(cx)
                .find_solution(id)
                .context("solution vanished during rename")?
                .root
                .to_string_lossy()
                .into_owned();
            Ok(root)
        })??;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("renamed: {} → {root}", input.solution_id),
            }],
            structured_content: RenameSolutionResult {
                solution_id: input.solution_id,
                root,
            },
        })
    }
}

// =====================================================================
// solutions.rename_member
// =====================================================================

/// Rename a member project. Also renames its directory on disk. `member_id`
/// is stable across a rename.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RenameMemberParams {
    pub member_id: i64,
    pub new_name: String,
}

impl<'de> Deserialize<'de> for RenameMemberParams {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Inner {
            member_id: i64,
            new_name: String,
        }
        let inner = Option::<Inner>::deserialize(de)?.unwrap_or_default();
        Ok(Self {
            member_id: inner.member_id,
            new_name: inner.new_name,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RenameMemberResult {
    pub member_id: i64,
    /// The member's path **after** the rename.
    pub local_path: String,
}

#[derive(Clone)]
pub struct RenameMemberTool;

impl McpServerTool for RenameMemberTool {
    type Input = RenameMemberParams;
    type Output = RenameMemberResult;
    const NAME: &'static str = "solutions.rename_member";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<ToolResponse<Self::Output>> {
        anyhow::ensure!(input.member_id != 0, "invalid_params: member_id is required");
        anyhow::ensure!(
            !input.new_name.trim().is_empty(),
            "invalid_params: new_name is required"
        );
        let id = crate::MemberId(input.member_id);
        let local_path = cx.update(|cx| -> anyhow::Result<String> {
            let store = SolutionStore::global(cx);
            store.update(cx, |store, cx| store.rename_member(id, &input.new_name, cx))?;
            let path = store
                .read(cx)
                .find_member(id)
                .context("member vanished during rename")?
                .local_path
                .to_string_lossy()
                .into_owned();
            Ok(path)
        })??;
        Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: format!("renamed: {} → {local_path}", input.member_id),
            }],
            structured_content: RenameMemberResult {
                member_id: input.member_id,
                local_path,
            },
        })
    }
}
```

Register it in `register_solutions_lifecycle` (after the `RenameSolutionTool` block):

```rust
    editor_mcp::register_tool(cx, |server| {
        server.add_tool(RenameMemberTool);
    });
```

Add `"solutions.rename_member",` to `GLOBAL_TOOLS` in `crates/editor_mcp/src/lifecycle.rs`, immediately after `"solutions.rename",` — **a new tool defaults to solution-scoped and would silently not appear on the global socket otherwise** (CLAUDE.md).

In `CLAUDE.md`, bump the catalog count (`**Tool catalog** (87 tools` → `88 tools`) and add `rename_member` to the `solutions.{…}` list.

- [x] **Step 4: Run the test to verify it passes**

Run: `cargo test -p solutions mcp::tests::rename_member_tool`
Expected: PASS.

- [x] **Step 5: Verify the workspace still builds**

Run: `cargo check -p solutions -p solutions_ui -p editor_mcp`
Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add crates/solutions/src/mcp/solutions_lifecycle.rs crates/solutions/src/mcp/tests.rs crates/editor_mcp/src/lifecycle.rs CLAUDE.md
git commit -m "solutions: Expose solutions.rename_member over MCP and move folders on rename"
```

---

### Task 11: Integration test — hot rename with a live worktree

**Files:**
- Create: `crates/solutions/src/tests/rename_hot_worktree.rs`
- Modify: `crates/solutions/src/solutions.rs` (`mod tests { mod persistence_e2e; mod rename_hot_worktree; }`)
- Modify: `crates/solutions/Cargo.toml` (`[dev-dependencies]`: `project = { workspace = true, features = ["test-support"] }`, `fs.workspace = true`, `language = { workspace = true, features = ["test-support"] }`, `settings = { workspace = true, features = ["test-support"] }`)

**Interfaces:**
- Consumes: `SolutionStore::rename_solution`, `crate::store::for_test_with_solution` (Task 4).
- Produces: nothing.

Proves the three claims the design rests on: the worktree's `abs_path` follows the move (via `ScanState::RootUpdated` → `update_abs_path_and_refresh`, `crates/worktree/src/worktree.rs:4356-4381` → `2050-2066`), open buffers survive, and the old path still resolves through the compat symlink.

- [ ] **Step 1: Write the failing test**

Create `crates/solutions/src/tests/rename_hot_worktree.rs`:

```rust
//! Hot-rename integration test against a **real** filesystem and a **live**
//! worktree: the worktree heals itself (the scanner holds the root by an inode
//! handle and emits `ScanState::RootUpdated`), open buffers survive, and the
//! old path keeps resolving through the compat symlink.

use gpui::TestAppContext;
use project::Project;
use settings::SettingsStore;
use std::sync::Arc;

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
        language::init(cx);
        Project::init_settings(cx);
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
    let text_before = buffer.read_with(cx, |buffer, _| buffer.text());
    assert_eq!(text_before, "fn main() {}");

    let store = cx.update(|cx| crate::store::for_test_with_solution(cx, &old_root, &member_path));
    let solution_id = store.read_with(cx, |store, _| store.solutions()[0].id);

    store
        .update(cx, |store, cx| store.rename_solution(solution_id, "Sawe", cx))
        .expect("rename");

    let new_root = base.path().join("Sawe");
    let new_member_path = new_root.join("sawe");
    assert!(new_member_path.join("src/main.rs").is_file());

    // The scanner notices the rename and repoints the worktree. Give it a
    // moment of real time — this is a real fs watcher, not FakeFs.
    for _ in 0..50 {
        cx.background_executor
            .timer(std::time::Duration::from_millis(100))
            .await;
        cx.run_until_parked();
        let followed = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == new_member_path.as_path())
        });
        if followed {
            break;
        }
    }
    project.read_with(cx, |project, cx| {
        let paths: Vec<_> = project
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect();
        assert_eq!(
            paths,
            vec![new_member_path.clone()],
            "the worktree's abs_path follows the move via ScanState::RootUpdated"
        );
    });

    // The open buffer survived — same entity, same text.
    assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "fn main() {}");

    // The old path still resolves through the compat symlink, which is what
    // keeps a live `claude` subprocess (holding the old cwd *string*) working.
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
}
```

Register the module: in `crates/solutions/src/solutions.rs`, change the test module to

```rust
#[cfg(test)]
mod tests {
    mod persistence_e2e;
    mod rename_hot_worktree;
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p solutions renaming_a_solution_moves_the_folder_under_a_live_worktree`
Expected: FAIL — first a compile error (`use of undeclared crate 'project'`) until the dev-dependencies are added; after adding them the test must run and pass (the implementation from Task 5 is already in place). If it fails on the worktree assertion, that is a **real** bug in the rename path, not a test artifact — do not paper over it by removing the assertion.

- [ ] **Step 3: Add the dev-dependencies**

In `crates/solutions/Cargo.toml`, under `[dev-dependencies]`:

```toml
fs.workspace = true
language = { workspace = true, features = ["test-support"] }
project = { workspace = true, features = ["test-support"] }
settings = { workspace = true, features = ["test-support"] }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p solutions renaming_a_solution_moves_the_folder_under_a_live_worktree`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/solutions/Cargo.toml crates/solutions/src/solutions.rs crates/solutions/src/tests/rename_hot_worktree.rs
git commit -m "solutions: Cover the hot rename with a live-worktree integration test"
```

---

### Task 12: Integration test — cold reconcile across all three databases, and the MCP e2e

**Files:**
- Create: `crates/solutions/src/tests/cold_reconcile.rs`
- Create: `crates/editor_mcp/tests/rename_folder_move_e2e_test.rs`
- Modify: `crates/solutions/src/solutions.rs` (`mod tests { … mod cold_reconcile; }`)

**Interfaces:**
- Consumes: everything from Tasks 1–10.
- Produces: nothing.

- [ ] **Step 1: Write the failing cold-reconcile test**

Create `crates/solutions/src/tests/cold_reconcile.rs`:

```rust
//! Cold reconcile end to end: seed the app DB, the agent DB and a claude
//! transcript bucket at an old path, run the drain, and assert every row is
//! rewritten, the bucket is merged, the compat symlink is gone and the
//! window's `workspace_id` (its whole layout) is preserved.

use crate::path_migrations::{PathRewrite, apply_one_with_connections, encode_claude_bucket};
use db::sqlez::connection::Connection;

#[test]
fn cold_reconcile_rewrites_all_three_databases_and_merges_the_bucket() {
    let base = tempfile::tempdir().expect("tempdir");
    let old_root = base.path().join("spk-solutions");
    let new_root = base.path().join("Sawe");
    let old_member = old_root.join("sawe");
    let new_member = new_root.join("sawe");
    std::fs::create_dir_all(&new_member).expect("mkdir new member");
    std::os::unix::fs::symlink(&new_root, &old_root).expect("compat symlink");

    let app = Connection::open_memory(Some("cold_reconcile_app"));
    app.exec(
        "CREATE TABLE solutions (id INTEGER PRIMARY KEY, name TEXT, root TEXT, last_opened_at INTEGER);
         CREATE TABLE solution_members (id INTEGER PRIMARY KEY, solution_id INTEGER, name TEXT, local_path TEXT, position INTEGER, origin_catalog_id INTEGER);
         CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT, paths_order TEXT, identity_paths TEXT, identity_paths_order TEXT, remote_connection_id INTEGER);
         CREATE UNIQUE INDEX ix_workspaces_location ON workspaces(remote_connection_id, paths);
         CREATE TABLE console_panel_state (workspace_id INTEGER, tab_index INTEGER, cwd TEXT);
         CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, buffer_path BLOB);
         CREATE TABLE terminals2 (workspace_id INTEGER, item_id INTEGER, working_directory BLOB);
         CREATE TABLE breakpoints (workspace_id INTEGER, path TEXT, breakpoint_location INTEGER);
         CREATE TABLE bookmarks (workspace_id INTEGER, path TEXT, row INTEGER);
         CREATE TABLE trusted_worktrees (trust_id INTEGER PRIMARY KEY, absolute_path TEXT);
         CREATE TABLE toolchains (workspace_id INTEGER, worktree_root_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT, relative_worktree_path TEXT);
         CREATE TABLE user_toolchains (remote_connection_id INTEGER, workspace_id INTEGER, worktree_root_path TEXT, relative_worktree_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT);",
    )
    .expect("prepare app schema")()
    .expect("create app schema");

    // `exec_bound` prepares exactly one statement, so seed row by row.
    app.exec_bound::<String>("INSERT INTO solutions VALUES (1, 'Sawe', ?, NULL)")
        .expect("prepare solutions insert")(old_root.to_string_lossy().into_owned())
    .expect("insert solution");
    app.exec_bound::<String>(
        "INSERT INTO solution_members VALUES (1, 1, 'sawe', ?, 0, NULL)",
    )
    .expect("prepare members insert")(old_member.to_string_lossy().into_owned())
    .expect("insert member");
    app.exec_bound::<(String, String)>(
        "INSERT INTO workspaces VALUES (42, ?1, '0', ?2, '0', NULL)",
    )
    .expect("prepare workspace insert")((
        old_member.to_string_lossy().into_owned(),
        old_member.to_string_lossy().into_owned(),
    ))
    .expect("insert workspace");
    app.exec_bound::<String>("INSERT INTO console_panel_state VALUES (42, 0, ?)")
        .expect("prepare console insert")(old_member.to_string_lossy().into_owned())
    .expect("insert console tab");
    app.exec_bound::<Vec<u8>>("INSERT INTO editors VALUES (1, 42, ?1, ?1)")
        .expect("prepare editor insert")(
        old_member.join("src/main.rs").to_string_lossy().as_bytes().to_vec(),
    )
    .expect("insert editor");
    app.exec_bound::<Vec<u8>>("INSERT INTO terminals2 VALUES (42, 1, ?)")
        .expect("prepare terminal insert")(
        old_member.to_string_lossy().as_bytes().to_vec()
    )
    .expect("insert terminal");

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
        .expect("prepare session insert")(old_member.to_string_lossy().into_owned())
    .expect("insert session");

    let projects = tempfile::tempdir().expect("projects tempdir");
    let old_bucket = projects.path().join(encode_claude_bucket(&old_member));
    let new_bucket = projects.path().join(encode_claude_bucket(&new_member));
    std::fs::create_dir_all(&old_bucket).expect("mkdir old bucket");
    std::fs::write(old_bucket.join("s1.jsonl"), b"old").expect("write old transcript");
    std::fs::create_dir_all(&new_bucket).expect("mkdir new bucket");
    std::fs::write(new_bucket.join("s2.jsonl"), b"new").expect("write new transcript");

    let rewrite = PathRewrite {
        old: old_root.clone(),
        new: new_root.clone(),
    };
    apply_one_with_connections(&app, Some(&agent), Some(projects.path()), &rewrite)
        .expect("reconcile");

    let text = |connection: &Connection, query: &str| -> Vec<String> {
        connection.select::<String>(query).expect("prepare")().expect("select")
    };
    assert_eq!(text(&app, "SELECT root FROM solutions"), vec![new_root.to_string_lossy()]);
    assert_eq!(
        text(&app, "SELECT local_path FROM solution_members"),
        vec![new_member.to_string_lossy()]
    );
    assert_eq!(
        text(&app, "SELECT paths FROM workspaces"),
        vec![new_member.to_string_lossy()]
    );
    assert_eq!(
        text(&app, "SELECT cwd FROM console_panel_state"),
        vec![new_member.to_string_lossy()]
    );
    assert_eq!(
        text(&agent, "SELECT cwd FROM solution_sessions"),
        vec![new_member.to_string_lossy()]
    );

    let ids: Vec<i64> = app
        .select::<i64>("SELECT workspace_id FROM workspaces")
        .expect("prepare")()
    .expect("select");
    assert_eq!(ids, vec![42], "the window keeps its workspace_id — and its layout");

    assert_eq!(std::fs::read(new_bucket.join("s1.jsonl")).expect("merged"), b"old");
    assert_eq!(std::fs::read(new_bucket.join("s2.jsonl")).expect("kept"), b"new");
    assert!(!old_bucket.exists(), "the source bucket is drained");
    assert!(!old_root.exists(), "the compat symlink is removed");
}

/// A **member** rename moves the repo — and with it the worktree admin dirs at
/// `<member>/.git/worktrees/<name>/` — while the relocated agent worktree
/// itself stays at `<solution_root>/.agents/worktrees/<member-dir>/<name>` and
/// keeps pointing at the *old* admin path. Without a targeted
/// `git worktree repair <tree>` the tree shows up as missing/prunable.
#[test]
fn cold_reconcile_repairs_relocated_agent_worktrees() {
    let base = tempfile::tempdir().expect("tempdir");
    let solution_root = base.path().join("sol");
    let old_member = solution_root.join("old-project");
    let new_member = solution_root.join("New-Project");
    std::fs::create_dir_all(&old_member).expect("mkdir member");

    let git = |args: &[&str], cwd: &std::path::Path| {
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
    };

    git(&["init", "-q"], &old_member);
    git(&["config", "user.email", "t@example.com"], &old_member);
    git(&["config", "user.name", "Test"], &old_member);
    std::fs::write(old_member.join("README.md"), b"hi").expect("write");
    git(&["add", "README.md"], &old_member);
    git(&["commit", "-qm", "init"], &old_member);

    // The relocated agent worktree, exactly where plan 3's `WorktreeCreate`
    // hook puts it.
    let tree = solution_root
        .join(".agents")
        .join("worktrees")
        .join("old-project")
        .join("wt-1");
    git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt-1",
            &tree.to_string_lossy(),
        ],
        &old_member,
    );

    // The hot rename: move the member, leave the compat symlink.
    std::fs::rename(&old_member, &new_member).expect("rename member");
    std::os::unix::fs::symlink(&new_member, &old_member).expect("compat symlink");

    let app = Connection::open_memory(Some("cold_reconcile_worktrees"));
    app.exec(
        "CREATE TABLE solutions (id INTEGER PRIMARY KEY, name TEXT, root TEXT, last_opened_at INTEGER);
         CREATE TABLE solution_members (id INTEGER PRIMARY KEY, solution_id INTEGER, name TEXT, local_path TEXT, position INTEGER, origin_catalog_id INTEGER);
         CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY, paths TEXT, paths_order TEXT, identity_paths TEXT, identity_paths_order TEXT, remote_connection_id INTEGER);
         CREATE UNIQUE INDEX ix_workspaces_location ON workspaces(remote_connection_id, paths);
         CREATE TABLE console_panel_state (workspace_id INTEGER, tab_index INTEGER, cwd TEXT);
         CREATE TABLE editors (item_id INTEGER, workspace_id INTEGER, path BLOB, buffer_path BLOB);
         CREATE TABLE terminals2 (workspace_id INTEGER, item_id INTEGER, working_directory BLOB);
         CREATE TABLE breakpoints (workspace_id INTEGER, path TEXT, breakpoint_location INTEGER);
         CREATE TABLE bookmarks (workspace_id INTEGER, path TEXT, row INTEGER);
         CREATE TABLE trusted_worktrees (trust_id INTEGER PRIMARY KEY, absolute_path TEXT);
         CREATE TABLE toolchains (workspace_id INTEGER, worktree_root_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT, relative_worktree_path TEXT);
         CREATE TABLE user_toolchains (remote_connection_id INTEGER, workspace_id INTEGER, worktree_root_path TEXT, relative_worktree_path TEXT, language_name TEXT, name TEXT, path TEXT, raw_json TEXT);",
    )
    .expect("prepare app schema")()
    .expect("create app schema");
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

    let listed = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(&new_member)
        .output()
        .expect("git worktree list");
    let listed = String::from_utf8_lossy(&listed.stdout);
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
        pointer.contains(&new_member.join(".git/worktrees/wt-1").to_string_lossy().to_string()),
        "{pointer}"
    );
}
```

Register it: `mod cold_reconcile;` inside `mod tests` in `crates/solutions/src/solutions.rs`.

- [ ] **Step 2: Run it to verify it fails, then passes**

Run: `cargo test -p solutions cold_reconcile`
Expected: both `cold_reconcile_rewrites_all_three_databases_and_merges_the_bucket` and `cold_reconcile_repairs_relocated_agent_worktrees` PASS with the Task 6–8 implementation. (If the seed statements need adjusting to sqlez's one-statement-per-`exec_bound` rule, fix the *test*.) If an assertion fails, fix the implementation, not the assertion — in particular a "prunable" entry in `git worktree list` means `repair_git_worktrees` did not pass the moved tree path as an argument.

- [ ] **Step 3: Write the MCP e2e test**

Create `crates/editor_mcp/tests/rename_folder_move_e2e_test.rs`:

```rust
//! End-to-end acceptance test for the folder-move rename, over the real MCP
//! socket: create a solution → add an empty member → rename both → restart the
//! store (cold reconcile) → the solution still has its member, at the new path.
//!
//! Isolation: pins the lock + socket to a tempdir via
//! `editor_mcp::set_runtime_dir_for_test` — mandatory, or this corrupts the
//! live editor's socket (CLAUDE.md).

use gpui::TestAppContext;
use serde_json::{Value, json};
use smol::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use smol::net::unix::UnixStream;

async fn call_tool(stream: &mut BufReader<UnixStream>, id: i64, name: &str, args: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": args }
    });
    let mut line = format!("{request}\n");
    stream
        .get_mut()
        .write_all(line.as_bytes())
        .await
        .expect("write request");
    loop {
        line.clear();
        stream.read_line(&mut line).await.expect("read frame");
        let frame: Value = serde_json::from_str(line.trim()).expect("parse frame");
        // Notifications interleave with responses on the same socket — skip
        // any frame without our id.
        if frame.get("id").and_then(Value::as_i64) == Some(id) {
            return frame;
        }
    }
}

#[gpui::test]
async fn rename_solution_and_member_over_mcp_survives_a_restart(cx: &mut TestAppContext) {
    cx.executor().allow_parking();

    let runtime_dir = tempfile::tempdir().expect("runtime tempdir");
    editor_mcp::set_runtime_dir_for_test(runtime_dir.path().to_path_buf());

    let work_dir = tempfile::tempdir().expect("work tempdir");
    let solutions_root = work_dir.path().join("sol-root");
    std::fs::create_dir_all(&solutions_root).expect("mkdir sol-root");

    // Bring up the editor globals exactly as `solutions_add_member_e2e_test`
    // does (settings store + `solutions::init` with `settings.root` pointed at
    // `solutions_root`), then start the MCP server.
    // … (copy the setup block from
    //    crates/editor_mcp/tests/solutions_add_member_e2e_test.rs) …

    let socket = runtime_dir.path().join("mcp.sock");
    let mut stream = BufReader::new(UnixStream::connect(&socket).await.expect("connect"));

    let created = call_tool(
        &mut stream,
        1,
        "solutions.create",
        json!({ "name": "Old Solution" }),
    )
    .await;
    let solution_id = created["result"]["structuredContent"]["solution_id"]
        .as_i64()
        .expect("solution_id");

    let member = call_tool(
        &mut stream,
        2,
        "solutions.add_empty_member",
        json!({ "solution_id": solution_id, "name": "Old Project" }),
    )
    .await;
    let member_id = member["result"]["structuredContent"]["member_id"]
        .as_i64()
        .expect("member_id");

    let renamed_member = call_tool(
        &mut stream,
        3,
        "solutions.rename_member",
        json!({ "member_id": member_id, "new_name": "New Project" }),
    )
    .await;
    assert!(
        renamed_member["result"]["structuredContent"]["local_path"]
            .as_str()
            .expect("local_path")
            .ends_with("/New-Project"),
        "{renamed_member}"
    );

    let renamed_solution = call_tool(
        &mut stream,
        4,
        "solutions.rename",
        json!({ "solution_id": solution_id, "new_name": "New Solution" }),
    )
    .await;
    let new_root = renamed_solution["result"]["structuredContent"]["root"]
        .as_str()
        .expect("root")
        .to_string();
    assert!(new_root.ends_with("/New-Solution"), "{new_root}");
    assert!(std::path::Path::new(&new_root).join("New-Project").is_dir());

    // "Restart": re-run the store's init, which drains
    // `pending_path_migrations` before hydrating.
    cx.update(|cx| {
        solutions::SolutionStore::init_global(cx);
    });
    cx.run_until_parked();

    let solution = call_tool(
        &mut stream,
        5,
        "solutions.get",
        json!({ "solution_id": solution_id }),
    )
    .await;
    let structured = &solution["result"]["structuredContent"];
    assert_eq!(structured["name"], "New Solution");
    assert_eq!(
        structured["members"]
            .as_array()
            .expect("members")
            .len(),
        1,
        "the member survives the rename + restart: {solution}"
    );
    assert_eq!(structured["members"][0]["name"], "New Project");
    assert!(
        structured["members"][0]["local_path"]
            .as_str()
            .expect("local_path")
            .ends_with("/New-Solution/New-Project")
    );
    // The compat symlink is gone after the cold reconcile.
    assert!(!work_dir.path().join("sol-root/Old-Solution").exists());
}
```

Copy the settings/`solutions::init`/`start_server` setup block verbatim from `crates/editor_mcp/tests/solutions_add_member_e2e_test.rs:24-70` — it already does exactly what this test needs (a `SettingsStore`, `solutions::init`, `editor_mcp::start_server`), and the field names there are the source of truth for `settings.root`.

- [ ] **Step 4: Run the e2e test**

Run: `cargo test -p editor_mcp --test rename_folder_move_e2e_test`
Expected: PASS. If `solutions.add_empty_member`'s result field is not `member_id` (plan 1 may name it differently), read the actual field name off the tool's `…Result` struct and fix the test.

- [ ] **Step 5: Full check**

Run: `cargo test -p solutions && cargo test -p solutions_ui && cargo test -p editor_mcp`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/solutions/src/solutions.rs crates/solutions/src/tests/cold_reconcile.rs crates/editor_mcp/tests/rename_folder_move_e2e_test.rs
git commit -m "solutions: Cover the cold reconcile and the rename MCP flow end to end"
```

---

## Out of scope for this plan

Phases 6 and 7 of the spec are **not** in here and must not be attempted:

- Phase 6 — the claude settings layer (`WorktreeCreate` / `WorktreeRemove` hooks → `<root>/.agents/worktrees`, `autoMemoryDirectory` → `<root>/.agents/memory`).
- Phase 7 — the side quests (sockets move from `config_dir()` to `state_dir()`; `solution_id` becomes optional in solution-scoped MCP tools).

Also deliberately accepted, per the spec: the `"cwd":"<old path>"` fields and old absolute paths **inside** a moved `*.jsonl` transcript stay stale. The resumed process's real cwd is correct; only the replayed context carries old paths. Rewriting claude's transcript format is out of scope.
