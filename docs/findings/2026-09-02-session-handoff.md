# Session handoff — 2026-09-02

Supersedes `2026-08-31b-session-handoff.md`. That session's three
carry-overs are all closed (see § Closed from the previous handoff).

This was a **UI session**, driven entirely by the maintainer testing the
running editor and reporting what was wrong. Almost every item arrived as
a screenshot. Fifteen commits, all on `origin/main` except the last two at
the time of writing.

## Commit chain

`620470e69e` → `bfd266a04f`. In order:

| sha | what |
|---|---|
| `02cf1ba9e7` | Panel toggles moved to the side their panel opens on |
| `c6829f9fc9` | Commit tab: reorder, font, branches line, shared diff tab |
| `688738e57b` | Remote branch delete by full ref; fetch prunes |
| `7fd6d0b557` | AI badge counts what the chat strip counts |
| `620470e69e` | docs: decisions 120-126 |
| `afbb97a8fc` | Tab strips' `+` dropdowns get a keyboard |
| `95bacf35b7` | Graph filter popovers get a cursor and a focused field |
| `66dcd04c37` | Conflict resolver's `Continue` made able to run |
| `294c622e78` | Commit tab finished; merge conflict made completable |
| `9c4501f0a2` | docs: decisions 128-134 |
| `d589918fc6` | Compose box yields to the conversation |
| `bfd266a04f` | Blame gutter compacted; open diff marked in the panel |

Earlier in the same chain (`0cbc78dafd`, `6622dd4d71`, `cf6a0a5421`,
`ef213d77b4`, `0d547c5059`) closed the previous session's carry-overs.

## Closed from the previous handoff

- **`release-fast` rebuilt.** The failure was `earlyoom` SIGTERMing a
  7.9 GB rustc with swap at 0 — not a code failure. `CARGO_BUILD_JOBS=4`
  fixes it. **Verify by content, never by exit code**: the binary must
  contain `sawe_server` and not `zed_server`.
- **Mobile client needed no changes** —
  `docs/findings/2026-08-31-mobile-wire-audit.md`, latent items in
  `TODO.md` § F.
- **The three unpushed commits** were re-reviewed and pushed with a fix
  round; the review found a live hazard (the live remote tests `rm -rf`'d
  a directory on whatever host the env vars named).

## What shipped, by area

**Panel geometry.** Right-dock toggles moved to the project toolbar's
trailing edge; the band's utility buttons to the status bar's right
group. Both are bound to a `Dock` *entity*, so dragging a panel between
docks moves its button with no filter code (#120, #121).

**Commit tab.** Files above the message, mirroring Changes (#122);
per-file `+N −M` (#127); the message in the Changes tab's typography —
which also fixed a real bug, `MarkdownStyle`'s size was set only on
`base_text_style` and a `TextRun` carries no font size, so the message
had been painting at the ambient UI size and any size change was a
silent no-op; containing branches with a clickable `Show all` (#123); a
drag divider (#128); the commit's own tags via `--points-at` (#134); file
rows at the Changes tab's size, which required pinning both row kinds to
a shared height because `ButtonLike` and `Label` disagree by a pixel and
`uniform_list` measures only item 0 (#129).

**Diff tab.** Commit-tab file diffs share the pane's preview slot: double
click summons, single click retargets, never summons from nothing (#125).
Fixed a live dedupe bug — the index was found by `(sha, file)` and the
item to remove by sha alone, so re-opening a file could close a full
commit view.

**Git correctness.** A remote branch delete now pushes `refs/heads/<b>`,
which is idempotent — the failure was client-side *name resolution*, not
a server refusal (#124). Fetch prunes. The AI badge counts
`can_be_active_dialog()` like every other surface (#126).

**Merge conflicts.** The flow worked and said nothing. Now: an
in-progress banner with the unresolved count and a confirming Abort;
"Mark Resolved" instead of "Stage" for a conflicted path; the resolver
reachable from the row menu and from the status bar; a conflicted row
opens the resolver rather than a diff whose index side cannot exist
(#132). The resolver's `Continue` was broken twice over and is fixed
(#133).

**Keyboard.** Both `+` dropdowns are `Picker`s now (#130); the graph's
three filter popovers got a cursor and a focused search field (#131).

**Blame + panel mark.** Gutter compacted; the open diff is marked in both
tabs (`bfd266a04f`).

## Outstanding pool

1. **Unify the two diff views — the maintainer has asked for this and it
   is the next task.** `SoloDiffView` (working tree) and single-file
   `CommitView` (historical) are two types. The map is in the
   `explore-diff-unification` findings, summarised: **two** essential
   differences, not three — editability and hunk staging, both following
   from "the right side is a live project buffer vs a detached historic
   blob". Blame is **not** essential: the base is just a parameter
   (`HEAD` vs `<sha>^`), and the only obstacle is that
   `sync_lhs_blame_sources` resolves the repository through the *right*
   buffer, which a detached blob cannot answer. FORK.md #59 records this
   as "deliberately unwired", not impossible.

   **The maintainer's gesture model, agreed and superseding #125:** double
   click never pins on *either* tab. Tab closed → double click opens it;
   tab open → any single click in Changes *or* Commit retargets it. One
   shared diff tab across both tabs. Pinning stays available through
   Zed's own double-click-on-the-tab gesture. This dissolves the
   asymmetry that #125 documents — **amend #125 when the refactor lands.**

   Use a diff *source* (working tree / commit / range) rather than a bare
   `editable` bool, so the blame base is derived rather than passed
   separately. The remaining ~70% of the difference is drift: hunk arrows,
   Unified/Split, soft-wrap, memoised count, first-hunk jump, tab icon,
   tooltip shape, `can_split`, breadcrumbs.

2. **Blame: consecutive-commit grouping.** The biggest remaining
   readability win, and deferred deliberately — the renderer does not know
   its neighbours, but `element.rs::layout_blame_entries` does. Needs a
   run-position argument on both `BlameRenderer` methods. Recorded in #135.

3. **Two live bugs found in passing, neither fixed.** `SoloDiffView` does
   not override `is_dirty` while `can_save` is true, so an edit shows no
   dirty dot and does not prompt on close. `CommitView::select_parent_index`
   refreshes only the affected-files list, never the multibuffer, so the
   "diff vs parent" toggle does not change the diff.

4. **Dead S-ANN plumbing.** `GitBlame::set_options` has no callers and
   `render_blame_entry_with_options` is never reached with anything but the
   default, so the annotate toolbar's colour modes, author filter and date
   toggle are inert. Looks lost in the refork transplant.

5. **Everything in `TODO.md`** — six sections. Do not start any of it
   without the maintainer asking.

## Open architectural decisions

- Whether the merged diff view keeps `CommitView`'s whole-commit mode in
  the same type. `single_file` is a flag on a much larger view sharing
  `new`, `open_internal`, the blob loader, `clone_on_split`, `Render` and
  the `Item` impl. Extracting the loader into a shared function both call
  is the sane route; duplicating it is not.
- Whether the Commit tab keeps a click cursor at all once the open-diff
  mark exists. Both are rendered today and distinguishable (wash vs
  wash+bold), but it may be one state too many.

## Active gotchas — the ones that cost time this session

- **A drag divider must not cap itself at what the last frame granted.**
  `bounds` is the previous paint's hitbox, and X11 and Wayland deliver a
  batch of motion events with no draw between them, so two moves in one
  frame read the same stale bounds and cancel each other. Neither a hand
  drag nor `windows.drag_at` reproduces it — both yield a frame per step.
- **Flexbox: a sub-1 `flex_shrink` does not mean "shrinks a little".**
  The spec multiplies free space by the sum of unfrozen flex factors when
  that sum is below one, so it means "absorbs almost nothing". Express a
  lopsided priority by raising the *other* item's factor.
- **A floor is not always a `min_h`.** In the Solution band the two floors
  exceed `MIN_BAND_HEIGHT`, so a hard min-height pushes the compose box
  and its drag handle off the bottom of the band — no input, and no handle
  to drag it back. A `flex_basis` acts as a priority threshold instead.
- **A `TextRun` carries no font size or line height.** Setting them on
  `MarkdownStyle::base_text_style` alone is a no-op; the container needs
  them too.
- **Enter doing nothing in a popover is not a focus bug if Escape works.**
  Both travel the same propagation path.
- `git tag --points-at` and `--contains` differ by an order of magnitude
  on any repo that tags releases.
- `git push origin :<short-name>` fails on an absent ref;
  `:refs/heads/<name>` succeeds. Not a server refusal — client-side name
  resolution.
- `git merge --continue` needs `GIT_EDITOR`; `--no-edit` is rejected.
- **`rustfmt` descends into `mod` declarations**, so formatting one file
  can reformat another agent's in-flight hunks in a submodule.
- `pkill -f` matching `sawe` kills the issuing shell (exit 144) — the
  harness now blocks the pattern; kill by PID, or launch a second probe in
  a different `--runtime-dir` instead of killing the first.
- `target/debug/incremental` reached **176 GB** this session. Deleting it
  reclaimed that and forced no rebuild of fresh crates.

## Process notes

Controller plus subagents throughout: a fresh implementer per task, an
explore pass before anything non-trivial, a task review naming explicit
surfaces, mutation tables rather than "I added a test", and
controller-verified gates before believing any report.

**Implementers and reviewers overturned the controller seven times, and
were right every time.** The load-bearing ones: `git` *does* have an
idempotent branch delete (qualify the refspec); `-o ControlPath=none`
would have disabled the transport's own multiplexing; "unrelated staged
changes" is wrong because a merge auto-stages the incoming side;
`Picker::render_header` vanishes exactly when its create row is needed;
`cd && pwd` returns the working directory when `HOME` is unset; a
`min_h` on the band's transcript pushes the compose box off-screen; and
the drag divider's own ceiling clause oscillates. **Give every agent
explicit permission to come back with a documented negative result.**

Live verification via `script/run-mcp --debug --headless --runtime-dir`
and `windows.{click_at,drag_at,send_text,send_keystroke,screenshot}`
caught things tests could not, and *missed* the divider oscillation for a
structural reason worth remembering (see gotchas).
