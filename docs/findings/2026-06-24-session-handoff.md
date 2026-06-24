# Session handoff — 2026-06-24

## Shipped + committed + pushed this session (branch `status-bar-model-selector`)

Four commits, pushed to `origin/status-bar-model-selector`
(`1d437f0dbf..31358a10b4`):

1. **`solution_agent`** — close-to-DB + reopen picker, status-bar Stop, compose-row
   cleanup, New Chat/Terminal open in active project folder, and the **bubble timestamp
   fix** (hover time now floats *above* the bubble via `bottom_full` instead of painting
   over the bubble's own text — `conversation_render.rs::render_message_time`).
2. **`solutions_ui`, `title_bar`** — tab-strip + toolbar alignment polish; quit when no
   `MultiWorkspace` window remains.
3. **`title_bar`, `git_ui`** — relocated **Update Project** + **Push** buttons to the
   project toolbar (Push shows only with unpushed commits, count beside the icon; dropped
   the ↑ahead indicator from the branch dropdown). Branch popup is now branch-only
   (removed Update/Update-All/Commit/Push rows). `git::Branch` (New Branch / Checkout)
   now resolves the **active Solution member's** repo via `active_member_repository`, so
   the picker lists only the selected project's branches.
4. **`workspace`, `.rules`** — new MCP primitives **`windows.hover_at` / `windows.hover_id`**
   (dispatch a MouseMove so a following `workspace.screenshot` shows hover-only UI).
   Tool catalog bumped to **84**. Added the `.rules` rule: *extend the MCP surface rather
   than push the gap onto the operator*.

All verified on the headless MCP harness (incl. hovering a real bubble to confirm the
timestamp no longer overlaps; the Update/Push buttons + right-click; cleaned branch popup).
A fresh `target/release-fast/spk-editor` was built (≈16:38) for the maintainer to test.

## Uncommitted (intentionally left in working tree — DO NOT commit)

`crates/gpui/src/elements/list.rs` — TEMP `spk_scroll` `log::info!` instrumentation only
(two probes + a `will_reengage` rename, behaviour unchanged). For the deferred scroll-jump
diagnosis. Never `git add` this; never `git add -A`.

## Next big thing — RE-FORK + REBRAND (planned, not started)

The maintainer approved re-forking onto a recent upstream stable tag (to restore real git
ancestry — our history is disjoint from upstream's after 2021, so `git merge upstream/main`
is infeasible; only cherry-pick works today) **combined with the `spk-editor → sawe`
rebrand**. Execution happens in a **fresh context**.

**Plan doc:** `docs/superpowers/plans/2026-06-24-refork-and-rebrand-sawe.md`
(NOTE: `docs/superpowers` is gitignored — the plan is on local disk only, not in git. Read
it from disk.) It contains the evidence, the single-patch 3-way transplant strategy, the
divergence inventory (18 net-new crates, 911 modified files), the full `sawe` identifier
map, the `zed`-strip scope (§6a), and the verification ritual.

**All decisions RESOLVED (maintainer, 2026-06-24)** — don't re-ask:
- Target = the clean **`sawe` member repo** (`solutions/spk-solutions/sawe`), commit
  straight to **`main`**, push to `Sipaha/sawe`. Configured this session: `origin` =
  `git@github.com-sipaha:Sipaha/sawe.git` (pushes as Sipaha, ssh-verified),
  `user.email=sipahabk@gmail.com`, `user.name=Pavel Simonov`. The `spk-editor` repo is the
  donor of fork code. NOT a branch in spk-editor; no GitHub repo rename needed.
- `.spke` → `.sawe`; `SPK_EDITOR_*` → `SAWE_*`; root `~/.spk/sawe`; target = latest stable
  upstream tag (confirm exact at start).
- **Strip `zed`/`Zed` from brand/user-visible surfaces**, keep only license-required
  mentions (LICENSE-*, copyright, `Fork of Zed…` attribution, `legal/upstream-zed/`,
  `.zed_server`) AND the internal `zed` cargo-crate / shared upstream identifiers (renaming
  those would conflict on every future merge — see plan §6a).
- `paths.rs` doc-comments + `.rules` claim config lives at `~/.config/spk-editor` — STALE;
  real root is `~/.spk/<kebab>` on all platforms. Fix during rebrand.

## Upstream comparison context (already done)

`upstream` remote = `https://github.com/zed-industries/zed.git` is configured (blobless
fetch of `main` present). Our base = Zed v0.235 (`b7a6783f99`, in object DB but not a HEAD
ancestor). The scroll-jump bug the maintainer reported maps to upstream fixes already in
recent stable: **#59002** `rebase_pending_scroll` (scroll reverted by pending scroll during
remeasure) and **#53378** (scrollbar drag position inversion when content grows) — both
ABSENT in our forked `list.rs`. They resolve for free after the re-fork; cherry-pick them
into the current branch only if interim relief is wanted before the re-fork.

## Smaller pending items raised but NOT done

- **Panel layout persistence across solution switches** (visibility + sizes). Infra exists
  (`SolutionStore::dock_snapshots`, `capture/apply_dock_snapshot` in `solutions_ui::switch`)
  and is wired to the MCP `solutions.switch` path, but **NOT to the tab-click path**
  (`MultiWorkspace::activate`, `multi_workspace.rs:1495-1497` detaches a non-retained
  leaving workspace → its docks are lost on switch-back). Fix = wire capture/apply around
  the `activate` switch from `solutions_ui` (cross-crate: `workspace` can't depend on
  `solutions`).
- **Scroll-jump** desktop bug (see #59002/#53378 above; `spk_scroll` instrumentation is in
  place for diagnosis).
