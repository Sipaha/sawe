# Failed `add_member` — the way out, the catalog leftover, and the URL typo

**Date:** 2026-09-04 · **Crates:** `solutions`, `solutions_ui`

## 1. What I found vs. what the brief claimed

Every claim in the brief verified, plus two the brief did not mention.

| Claim | Verdict |
|---|---|
| Failed add is a *pending entry*, not a member | **Confirmed.** `sqlite3 ~/.spk/sawe/data/db/0-stable/db.sqlite` → `solution_members` has exactly one hazelcast row, `133 | solution 33 | origin_catalog_id 84` (the good SSH one). No row for the typo. |
| `add_member.rs:334` parks `stage = "failed"`, with a test | **Confirmed** (`add_member_records_failure_in_pending`). |
| `clear_failed_add` has zero callers outside its module | **Confirmed.** `grep -rn clear_failed_add crates/ --include=*.rs` → 2 hits, both in `add_member.rs`, one of them the test. |
| Right-click does nothing on the failed tab | **Confirmed.** `ProjectTab` (`project_tab.rs:182`) wraps its row in `right_click_menu`; `PendingProjectTab` returned a bare `div()` with only a tooltip. |
| Catalog kept both rows | **Confirmed.** `catalog_projects` → `83 | citeck-hazelcast# | …/citeck-hazelcast#` and `84 | citeck-hazelcast | git@…`. |

Two things the brief did not say, both of which change the answer to item 2:

- **`cancel_add_member` is also caller-less.** The in-flight (spinner) tab is just as inert as the failed one — a slow clone could not be cancelled from the UI either. Same root cause, so it got the same fix.
- **`EditCatalogProject` and `DeleteCatalogProject` are registered workspace actions with *no dispatch site anywhere*.** `grep` for a construction of either struct outside `actions.rs` / `modals.rs` returns nothing. Their doc comments still say "triggered from the failed in-flight add row in the Solutions panel" — that panel was retired and took the only entry points with it. So there is currently **no UI at all** that can edit or delete a catalog project, while `AddProjectPicker` happily keeps offering it. That is why catalog row 83 is not merely untidy: it is permanently unreachable.

## 2. Decisions

### (1) The way out: right-click menu on the ghost tab — no new chrome

Right-click, matching `ProjectTab` / `SolutionTab` / the AI session tabs (`0c569d6c95`). I considered an always-visible `×` or a "retry" button and rejected it: the maintainer has just been *removing* per-tab affordances in favour of the context menu, and an error tab is not different enough to reverse that — it is transient, it already carries a warning glyph and a `Clone failed: …` tooltip, and right-click is the gesture the maintainer actually reached for ("ПКМ на нем не работает" — the complaint is that the habit did not pay off, not that a button was missing).

The discoverability gap is closed in the **tooltip** instead, which is free: `Tooltip::with_meta` now adds a second line — `Right-click to retry, edit or dismiss` (failed) / `Right-click to cancel` (in flight).

Menu, failed:

```
Retry Clone                    → SolutionStore::retry_failed_add
Edit Project…                  → EditCatalogProject { id }   (its first-ever dispatch site)
────────
Dismiss                        → clear_failed_add            (its first-ever caller)
Remove Project from Catalog    → clear_failed_add + remove_catalog_project   (only when unreferenced)
```

Menu, still cloning: a single `Cancel Clone` → `cancel_add_member` (also its first caller).

`retry_failed_add` is a new store method rather than "dismiss then re-add from the picker", because `add_member` explicitly refuses to start while an entry for the same `(solution, catalog)` is in the map — a caller that forgot the clear would get `add already in progress`.

One extra fix fell out: `pending_adds_for` now reports the catalog project's **current** name rather than the one snapshotted when the add began. The recovery flow is *edit, then retry*, and a tab still labelled `citeck-hazelcast#` after the user fixed the name reads as a second stuck entry.

### (2) The catalog leftover: no rollback — offer the deletion in the same gesture

**Not** auto-rolled-back. Three reasons: the catalog is shared across Solutions, so a failed clone in one Solution is not evidence the project is junk; the user's most likely intent is to *fix the URL and retry*, which needs the row to still exist; and silently deleting a row the user just asked for is precisely the "hidden magic" `add_catalog_project`'s own comment rejects ("Say no and let the user decide").

Instead the menu offers **both** outcomes explicitly, one click each:

- `Dismiss` — drops the failed row only. The project stays in the catalog (it may be fine and merely unreachable right now).
- `Remove Project from Catalog` — drops the failed row **and** deletes the catalog entry, in one gesture.

The second entry is rendered only when `SolutionStore::catalog_project_is_unreferenced(catalog_id)` — the row exists and **no** Solution has a member cloned from it. A project other Solutions already use is not leftover from this typo, and `remove_catalog_project` would refuse it anyway. Given the finding above (no other UI can delete a catalog project at all), this menu is currently the *only* way to remove one, which is an argument for it, not against.

### (3) The URL: normalise what is unambiguous, refuse what is not, at the single choke point

New `crates/solutions/src/remote_url.rs::normalize_remote_url`, called from **`add_catalog_project` and `edit_catalog_project`** — one place, so both modals and the MCP `catalog.add_project` / `catalog.edit_project` tools get it. Normalisation runs *before* the duplicate checks, so `…/repo#` now collides with `…/repo` instead of minting the second row.

Normalised silently: outer whitespace; a URL **fragment** (`#`, `#L42`) on URL-shaped input — git has no use for one in any transport it supports.
Refused, tagged `invalid_remote:` (stripped for humans by `humanize_catalog_error`): empty input; any control character; a space inside a URL-shaped input (split paste); a bare `#`; and a **forge browse URL** (`…/-/tree/main`, `…/blob/main/…`) — refused rather than rewritten, but the message *names the clone URL it should have been*, e.g. `that is a web page URL, not a clone URL — try https://gitlab.citeck.ru/citeck-projects/citeck-hazelcast instead`.

Deliberately left alone, and documented in the module header: no scheme allow-list (a bare filesystem path is a legitimate remote — the crate's own tests clone from a temp dir, and a `C:\…` path must not be mistaken for scp-like `host:path`); no host/DNS/reachability probe (that is what the clone is for); `?query` untouched; trailing `/` and `.git` untouched (`same_remote` already folds them).

Two knock-ons in `AddCatalogProjectModal`:

- The Name auto-fill now derives from the **normalised** URL, and derives **nothing** from a URL the store would refuse. Otherwise `…hazelcast#` became the project *name* too, where nothing would ever remove it, and a pasted browse URL named the project `master`.
- Because Name can now legitimately stay empty, `confirm` checks the URL **before** the empty-name guard. Without that reorder, pressing Enter on a browse URL was a silent no-op — I introduced that regression, saw it in the live probe, and pinned it with `confirm_reports_a_refused_url_even_with_an_empty_name`.

## 3. Mutation table

Every mutation applied to the working tree, run, and reverted. All seven were killed.

| # | Mutation | Test(s) that failed |
|---|---|---|
| M1 | `PendingProjectTab` returns the bare row (pre-fix dead end) | all 3 `pending_tab_paint_tests` |
| M2 | `.when(catalog_removable, …)` → `.when(true \|\| …)` | `a_referenced_catalog_project_is_not_offered_for_deletion` |
| M3 | drop the fragment strip in `normalize_remote_url` | `strips_a_trailing_fragment_…`, `add_catalog_project_normalizes_or_refuses_the_url`, `edit_catalog_project_normalizes_or_refuses_the_url` |
| M4 | `retry_failed_add` no longer clears the failed entry | `retry_failed_add_clears_the_failure_and_clones_the_fixed_url` |
| M5 | `pending_adds_for` keeps the stale snapshot name | `retry_failed_add_clears_the_failure_and_clones_the_fixed_url` |
| M6 | empty-name guard back ahead of the URL check in `confirm` | `confirm_reports_a_refused_url_even_with_an_empty_name` |
| M7 | derive a project name from a refused URL | `a_typo_never_reaches_the_project_name`, `confirm_reports_a_refused_url_even_with_an_empty_name` |

The three UI tests assert the **painted** tree (`VisualTestContext::debug_bounds` after a real frame), both sides, per repo-root `.rules` and `docs/findings/2026-09-02-paint-tests-with-debug-bounds.md`. `ContextMenu` already registers `MENU_ITEM-{label}`; the ghost tab itself gained explicit `PENDING-PROJECT-TAB-FAILED` / `PENDING-PROJECT-TAB-CLONING` selectors, because its two states differ only by which `Icon` is drawn and `Icon` (unlike `IconButton`) registers nothing. `right_click_menu` only fires when its hitbox is hovered, so the helper rests the pointer and draws a frame before pressing the button — a bare mouse-down opens nothing and every assertion after it would be a false negative.

## 4. Verification

- `CARGO_BUILD_JOBS=4 cargo build --bin sawe` — clean, zero `^error` / `^warning`.
- `CARGO_BUILD_JOBS=4 cargo check --workspace --all-targets` — clean, zero `^error` / `^warning`.
- `CARGO_BUILD_JOBS=4 cargo test -p solutions -p solutions_ui` — 236 + 50 passed, 0 failed (was 232 + 43).
- `cargo fmt -p solutions -p solutions_ui -- --check` — clean. Two untouched files (`add_project_picker.rs`, `solution_picker_dropdown.rs`) had pre-existing rustfmt drift; committed separately.

### Live probe — `script/run-mcp --debug --headless --runtime-dir /tmp/failed-add-probe`

Isolated instance only; the maintainer's editor was never touched. All four screenshots are from the **final** binary.

1. `.agents/reports/2026-09-04-failed-add-url-rejected.png` — the Add-Project modal after Enter on `https://gitlab.citeck.ru/citeck-projects/citeck-hazelcast/-/tree/master`. The modal **stays open**, Name is still the empty placeholder (`Project name (e.g. ECOS Records)`) — proof the browse URL seeded no name — and a red inline line reads *"that is a web page URL, not a clone URL — try https://gitlab.citeck.ru/citeck-projects/citeck-hazelcast instead"*, with the `invalid_remote:` tag stripped.
2. `.agents/reports/2026-09-04-failed-add-tab.png` — after a deliberately failing add (`…/citeck-hazelcast-nope.git`, `fatal: could not read Username`): the project strip shows `scratch` and, beside it, a muted `Hazelcast Probe` with the red warning triangle. Exactly the maintainer's wedged state.
3. `.agents/reports/2026-09-04-failed-add-menu.png` — right-clicking that tab opens the menu: `Retry Clone`, `Edit Project…`, separator, `Dismiss`, `Remove Project from Catalog`.
4. `.agents/reports/2026-09-04-failed-add-cleared.png` — after clicking `Dismiss`: the strip is back to `scratch` alone, the warning tab gone. `catalog.list` over MCP still shows the project, i.e. Dismiss kept the catalog entry as designed.

Then, re-triggering the same failed add and choosing **`Remove Project from Catalog`** instead: the tab disappeared **and** `catalog.list` returned `{"projects": []}` — the junk row removed in one gesture.

Also checked over the wire on the running instance: `catalog.add_project` with `remote_url` ending in `#` succeeds and stores the URL **without** the `#`; the browse URL is rejected with the suggestion.

Probe wrinkle worth knowing: `windows.click_at {button:"right"}` **toggles** the context menu, and `workspace.screenshot` renders the retained scene, so a naive "click, screenshot" loop alternates open/closed. I gated on the PNG byte length (menu open ≈ 64.6 KB vs ≈ 56.9 KB) before clicking a menu row.

## 5. Sentence for `FORK.md` (not applied)

> A failed `add_member` parks a ghost tab in the project strip; that tab's right-click menu (`Retry Clone` / `Edit Project…` / `Dismiss` / `Remove Project from Catalog`, or `Cancel Clone` while still cloning) is the only exit from the failed state and the only UI anywhere that can delete a catalog project — the catalog row is deliberately never auto-rolled-back, because the catalog is shared across Solutions and the usual recovery is "fix the URL, then retry", so removal is offered explicitly in the same gesture and only when nothing references the row; remote URLs are normalised and sanity-checked once in `SolutionStore::{add,edit}_catalog_project` via `solutions::normalize_remote_url`, which strips URL fragments (the `…/repo#` typo that motivated all of this) and refuses forge *browse* URLs while naming the clone URL they should have been.

## 6. Concerns / follow-ups (not done here)

- `EditCatalogProject` / `DeleteCatalogProject` still have no *general* entry point — only the failed tab reaches `EditCatalogProject`, and only the failed tab can delete a catalog row. A catalog project that was added successfully and later needs its URL changed is still uneditable from the UI. Out of scope here; worth a dedicated catalog management surface.
- `normalize_remote_url` runs on **edit** as well as add, so an existing catalog row whose URL would now be refused cannot be re-saved unchanged through the Edit modal (the user must fix the URL). That is intentional but is a behaviour change for any pre-existing bad row.
