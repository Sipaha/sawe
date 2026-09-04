# 2026-09-04 — slug camelCase separator fix

## Rule implemented

Insert a `-` at every lower/digit→upper transition, and before the last
capital of an uppercase run when a lowercase letter follows it — letter→digit
transitions never split, so digits stay glued to the word they belong to.

## Inputs → outputs pinned

| Input | Output | Note |
|---|---|---|
| `UpdateDeps` | `update-deps` | reported case, lower→upper boundary |
| `ECOSRecords` | `ecos-records` | acronym run, split before its last capital |
| `ECOS` | `ecos` | lone trailing acronym stays whole (no lowercase follows) |
| `ecosV2` | `ecos-v2` | letter→digit never splits (`2` stays with `v`) |
| `v2Config` | `v2-config` | digit→upper is symmetric with lower→upper |
| `foo-Bar` | `foo-bar` | already-separated input doesn't get a doubled `-` |
| `foo Bar` | `foo-bar` | same, via whitespace |
| `ECOS Records` (existing test) | `ecos-records` | unchanged |
| `foo  bar/baz` (existing test) | `foo-bar-baz` | unchanged |
| `--foo--` (existing test) | `foo` | unchanged |
| `ecos v2 module` (existing test) | `ecos-v2-module` | unchanged, explicit contract per task |
| `漢字` (existing test) | `repo-<hash>` | unchanged hash fallback |

All pre-existing tests in `crates/solutions/src/slug.rs` pass unmodified;
7 new tests were added to pin the cases above (`splits_camel_case_boundary`,
`splits_acronym_run_before_trailing_word`, `keeps_lone_leading_acronym_whole`,
`does_not_split_letter_to_digit`, `splits_digit_to_upper_boundary`,
`does_not_double_separator_after_explicit_hyphen`,
`does_not_double_separator_after_space`).

## Mutation table

Each mutation was applied directly to the boundary-decision `match` in
`slugify`, run against `cargo test -p solutions slug`, confirmed to fail,
then reverted to the shipped code before moving to the next.

| # | Mutation | Command | Result | Reverted |
|---|---|---|---|---|
| 1 | `(Lower, Upper) => true` → `false` | `cargo test -p solutions slug` | 2 failed: `splits_camel_case_boundary`, `does_not_split_letter_to_digit` | yes |
| 2 | `(Digit, Upper) => true` → `false` | `cargo test -p solutions slug` | 1 failed: `splits_digit_to_upper_boundary` | yes |
| 3 | `(Upper, Upper) => <lookahead-lowercase>` → `(Upper, Upper) => true` (unconditional acronym split) | `cargo test -p solutions slug` | 3 failed: `slugifies_simple_name`, `splits_acronym_run_before_trailing_word`, `keeps_lone_leading_acronym_whole` | yes |

Post-revert: `cargo test -p solutions slug` → 16 passed, 0 failed. Full crate
suite `cargo test -p solutions` → 243 passed, 0 failed, 2 ignored (real-DB
rehearsal tests, unaffected). `cargo build --bin sawe` and
`cargo check --workspace --all-targets` both clean (0 errors, 0 warnings).

## Runtime path re-derivation

No bug found. `slug::slugify` / `slug::unique_slug` are called only at
creation time — `store/lifecycle.rs::create_solution`,
`add_member.rs::add_member_from_catalog` (line 148),
`add_member.rs::add_empty_member` (line 377), plus two `#[cfg(test)]`-only
helpers in `store.rs` (lines 340, 376: `create_for_test_minimal` and
`test_force_add_member`). Every one of those immediately persists the
computed folder into `Solution.root` / `SolutionMember.local_path`
(and the DB), and all downstream code reads those stored fields rather than
recomputing them.

The one place that *does* recompute a folder name from a display name at a
time other than initial creation is rename: `store/lifecycle.rs::rename_solution`
and `store/members.rs::rename_member` both call `crate::folder_name::derive`
— but that is a separate, deliberately Unicode-preserving, **non-lowercasing**
module (`crates/solutions/src/folder_name.rs`), not `slug::slugify`. It is by
design (a rename is explicitly asked to produce a new folder from the new
name) and it never had the camelCase-lowercasing bug this task fixes, so it
is unaffected either way and out of scope here.
