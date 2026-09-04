# Unified folder-name derivation (create == rename)

## The rule, in one sentence

A folder name is `crate::solutions::folder_name::derive(display_name)`: NFC-normalise,
drop control/illegal characters, collapse whitespace runs to a single `-`, insert a `-`
at every camelCase/PascalCase boundary, trim `.`/space/`-` off both edges, cut to 255
**bytes** on a character boundary, and reject the result if it is empty or a Windows
device name — case and script preserved throughout — and creation, rename and member
folders all go through exactly that function.

## Input → output, pinned

`crates/solutions/src/folder_name.rs::tests::unified_rule_table` (plus the pre-existing
tables in the same module, which still pass unchanged except for one — see "Contracts").

| input | folder | note |
|---|---|---|
| `UpdateDeps` | `Update-Deps` | the reported bug; creation produced `updatedeps`, then `update-deps` after 2fd305a924 |
| `updateDeps` | `update-Deps` | |
| `ECOSRecords` | `ECOS-Records` | acronym run splits before the word that ends it |
| `ECOS` | `ECOS` | a trailing acronym stays whole |
| `ecosV2` | `ecos-V2` | letter → digit is never a boundary |
| `v2Config` | `v2-Config` | digit → upper is, symmetric with lower → upper |
| `Мой Проект` | `Мой-Проект` | **old creation path produced `repo-{hash}`** — nothing ASCII survived |
| `МойПроект` | `Мой-Проект` | case is Unicode-wide, so Cyrillic humps split too |
| `update-deps` | `update-deps` | already hyphenated: unchanged, no doubled separator |
| `Update-Deps` | `Update-Deps` | idem, and not lowercased |
| `foo-Bar` | `foo-Bar` | the existing `-` sits at the hump; no second one added |
| `Sawe` / `sawe` | `Sawe` / `sawe` | two names that used to collapse onto one folder no longer do |
| `/\:*?"<>\|` | `Err(Empty)` | only illegal characters — refused, not hashed |
| `NUL` | `Err(Reserved)` | Windows device name |
| `"a" * 255` | unchanged | exactly at the byte cap |
| `"a" * 256` | `"a" * 255` | over the cap, cut on a char boundary |

The cross-path assertion is
`store::lifecycle::tests::create_and_rename_derive_the_same_folder`: for each of
`UpdateDeps`, `ECOSRecords`, `ecosV2`, `Мой Проект`, `update-deps`, `Sawe` it *creates* a
Solution with the name in one store and *renames* a seeded Solution to the same name in
another, then asserts the two folder names are equal **and** equal to `derive(name)`. It
needs two stores because the collision check spans every Solution in a store regardless
of which parent directory it lives in (pre-existing behaviour, unchanged) — one store
would report the created Solution as a collision for the rename.

## Collision handling: one predicate, two policies

`rename::ensure_folder_available(parent, folder, source, taken)` is the single
availability predicate — DB-owned names (case-insensitively), a real directory on disk,
and the compat symlink of an unfinished rename. On top of it:

- **Rename** keeps calling it directly and *fails* on a collision. A rename moves live
  data; silently landing in `Sawe-2` after the user typed `Sawe` would be a lie about
  where their Solution now is, and the error messages (`FolderNameError::{TakenBySolution,
  ExistsOnDisk, HeldByLink}`) already exist to explain it.
- **Creation** (`create_solution`, `add_empty_member`) calls the new
  `rename::first_available_folder`, which walks a `-2`, `-3`, … ladder over the *same*
  predicate and takes the first free name. A create has nothing to move and no reason to
  refuse, and this is what the old `slug::unique_slug` did — except `unique_slug`
  uniquified against the **in-memory folder list only** and then `create_dir_all`'d, so a
  leftover directory on disk (or an unfinished rename's symlink) was silently *adopted*
  as the new Solution's root. That hole is now closed; it is pinned by
  `create_solution_steps_around_an_existing_directory`.

The suffix ladder itself is `folder_name::uniquify(base, available)` — one mechanism,
several predicates — bounded at 1000 attempts so a predicate that always answers "taken"
cannot spin. It shortens the base to make room for `-N` rather than truncating the
candidate afterwards, which would defeat the uniquification at the byte cap.

`add_member_from_catalog` (the clone path) is the deliberate exception: it uniquifies
against the sibling members' folder names but **not** against disk, because the clone
step wipes a stale target left by a cancelled/failed add and a disk check would step
around that garbage into `-2` instead of reclaiming it. It had *no* dedupe at all before,
which was a live data-loss path: two catalog projects whose names derived to the same
folder meant the second add's "wipe the stale target" step deleted the first member's
checkout. The in-memory check closes that.

## What happened to `slug.rs`

Deleted (`git rm`), along with its 15 tests, and `mod slug;` dropped from
`solutions.rs`. After the change `slugify` and `unique_slug` had **zero** callers: the
three production ones moved to `derive`, and the two test helpers in `store.rs`
(`create_for_test_minimal`, `test_force_add_member`) now derive too. Nothing outside
`crates/solutions` ever referenced it (the many other `slugify` hits in the repo —
`agent_skills`, `run_config::file_format`, `util::markdown`, `channel_store` — are
unrelated functions with the same name and are untouched). The camelCase rule from
2fd305a924 was not rewritten: it moved into `folder_name::split_camel_case` with the
same two boundary rules and the same letter→digit carve-out, minus the lowercasing.

## Existing directories are not renamed — verified, not assumed

Both `Solution.root` and `SolutionMember.local_path` are stored columns
(`solutions.root`, `solution_members.local_path`) hydrated at startup, and every consumer
reads the stored value. Greps for a runtime re-derivation found none:

- The only `derive` call sites are `create_solution`, `rename_solution`, `rename_member`,
  `add_empty_member`, `add_member_from_catalog` and the two `#[cfg(test)]` store helpers.
- `solutions::derive_folder_name` (the crate's public re-export) has **no callers outside
  `crates/solutions`** — it is an unused public re-export, predating this change. Not
  removed here; flagging it rather than expanding the diff.
- The nearest thing to a re-derivation, `claude_native::worktree_hook::worktree_dir`,
  builds `<base>/<member>/<name>` from `repo_root.file_name()` — the actual on-disk
  directory name, not a display name. Safe.
- The per-solution MCP socket directory is keyed by the numeric `solution_id`;
  `editor_mcp::lifecycle` only *sweeps* legacy slug-named socket dirs, never mints one.

So the maintainer's `~/.spk/sawe/ss/updatedeps` keeps working: its `local_path` row is
untouched and nothing recomputes it.

## Contracts I changed, and why

Three existing assertions were wrong under the unified rule. None were quietly edited:

1. `folder_name::tests::never_changes_case` asserted `derive("MiXeD CaSe") ==
   "MiXeD-CaSe"`. Case *is* still preserved — the test's stated intent — but the
   camelCase rule now reads `MiXeD` as humps, so the answer is `Mi-Xe-D-Ca-Se`. I kept
   the input, updated the expectation with a comment saying what it used to read, and
   added `derive("MIXED case") == "MIXED-case"` so the case-preservation intent is still
   asserted without the humps. **If you think `MiXeD` should stay whole, the rule needs a
   "don't split a run shorter than N" clause and this is the test to argue over.**
2. `add_member::tests::add_empty_member_creates_directory_and_member` asserted the member
   folder for `Frontend` is `frontend` with the message `"slug from name"`. That message
   *is* the old rule; the new answer is `Frontend`.
3. `store::tests::paths_for_open_returns_member_paths_in_order` asserted the folders for
   catalog projects `A` / `B` end with `a` / `b`. Now `A` / `B`.

Two comments that documented the old split were corrected, not assertions:
`solutions_ui::window_helpers` (`create_solution` appends `"s"` → `"S"`, and the test's
path with it) and `editor_mcp/tests/rename_folder_move_e2e_test.rs` ("`solutions.create`
slugifies … only a *rename* derives" → both derive). The e2e test itself already read the
root back instead of assuming a spelling, so it passed unmodified.

## Mutation table

Each mutation was applied to the working tree, `cargo test -p solutions --lib` was run,
and the file was restored from a pristine copy afterwards. Final state: 234 passed, 0
failed.

| # | mutation | result | tests that caught it |
|---|---|---|---|
| M1 | `split_camel_case`: lower/digit → upper is no longer a boundary | **killed** | `folder_name::unified_rule_table`, `never_changes_case` |
| M2 | `split_camel_case`: upper → upper always a boundary (drop the lookahead) | **killed** | `unified_rule_table`, `never_changes_case`, `rejects_reserved_windows_names` |
| M3 | `create_solution`: `derive(name)?.to_lowercase()` — i.e. the original bug | **killed** | `create_and_rename_derive_the_same_folder`, `create_solution_steps_around_an_existing_directory` |
| M4 | `first_available_folder`: every candidate reported available (no disk check) | **killed** | `create_solution_steps_around_an_existing_directory` |
| M5 | `with_suffix`: stop reserving room for `-N` under the byte cap | **killed** | `uniquify_keeps_the_suffixed_name_under_the_byte_cap` |
| M6 | `uniquify`: always return the base, never climb the ladder | **killed** | `uniquify_walks_the_suffix_ladder`, `uniquify_keeps_the_suffixed_name_under_the_byte_cap`, `create_solution_steps_around_an_existing_directory` |

M1 is worth a note: the *agreement* test survives it, because creation and rename stay in
agreement no matter how wrong the shared rule is. Agreement and correctness needed
separate tests; the table is what pins the rule.

## Verification

- `CARGO_BUILD_JOBS=4 cargo build --bin sawe` — clean, 0 `^error`, 0 `^warning`.
- `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` — 0 errors, 0 warnings.
- `cargo test -p solutions` 234 passed · `-p solutions_ui` 50 · `-p solution_agent` 777 ·
  `-p editor_mcp` all suites green (incl. `rename_folder_move_e2e_test`,
  `solutions_add_empty_member_e2e_test`, `solutions_add_member_e2e_test`).

## Live probe

`script/run-mcp --debug --headless --runtime-dir /tmp/folder-naming-probe`, four
`solutions.create` calls, then `solutions.rename` (param is `new_name`, not `name`), then
one `solutions.add_empty_member`. Solutions root:
`/tmp/folder-naming-probe/.spk/sawe-dev/ss/`.

```
create "UpdateDeps"    -> ss/Update-Deps
create "ECOSRecords"   -> ss/ECOS-Records
create "Мой Проект"    -> ss/Мой-Проект
create "ecosV2"        -> ss/ecos-V2
rename  1 -> "BuildTools"   -> ss/Build-Tools
rename  2 -> "ECOSRecords"  -> ss/ECOS-Records   (unchanged: create already agreed)
rename  4 -> "ecosV2"       -> ss/ecos-V2        (unchanged: create already agreed)
add_empty_member 1 "FrontEnd Web" -> ss/Build-Tools/Front-End-Web
```

The two no-op renames are the point: on `main` before this change, renaming a
freshly-created `ECOSRecords` to its own name *moved* the directory (`ecos-records` →
`ECOS-Records`). Now the folder is already right and the rename short-circuits on
`old_root == new_root`.

Also observed, and expected: renaming `Build-Tools` back to `UpdateDeps` failed with
`Directory 'Update-Deps' is held by a link from an unfinished rename — restart the
editor`. That is the hot-rename compat symlink left at the old root, cleared by the cold
reconcile on the next start — pre-existing behaviour, not something this change
introduced.

## Sentence for FORK.md (not applied)

> One folder-name derivation for everything on disk: `solutions::folder_name::derive`
> case- and Unicode-preservingly sanitises a display name and splits camelCase humps
> (`UpdateDeps` → `Update-Deps`, `Мой Проект` → `Мой-Проект`), and creation, rename and
> member folders all use it — creation walks a `-2`, `-3` ladder over the same
> availability check a rename fails on, so a create can never adopt a directory that is
> already there. The old ASCII-lowercasing `slug::slugify` (which turned `Мой Проект`
> into `repo-{hash}`) is deleted; existing directories are not migrated, since every
> Solution and member stores its path in the DB.
