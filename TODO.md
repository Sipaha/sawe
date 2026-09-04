# TODO — deferred work

Findings from the 2026-08-31 sessions onward that do **not** block using the editor on
Linux. Recorded here for future execution rather than chased at the time.

Each entry says what is wrong, why it was deferred, and where the evidence is.
Nothing here is a guess: every item was established by reading or measuring, and the
long-form record is in `FORK.md`, `docs/findings/2026-08-31b-session-handoff.md` and
the plan doc named by each entry.

---

## A. Disowning Zed — the remaining phases

The maintainer's rule: *a user may have both Zed and Sawe installed, and they must
not intersect in anything.* Plan: `docs/plans/2026-08-31-disown-the-zed-url-scheme.md`.
FORK.md **#115-#119**. Phases 1-4 shipped; these are what is left.

### A1. The Windows shell appx still ships under Zed's identity
`crates/zed/resources/windows/` — the shell integration appx has
`Name="ZedIndustries.Zed"`, `<DisplayName>Zed</DisplayName>` and the verb
`OpenWithZed`. **Independently of the rename, its identity does not match the
`Sipaha.Sawe_…` our own uninstaller removes** — so the uninstaller cannot remove it.
That is a live bug, not cosmetics. Windows-only; unverifiable from this machine.

### A2. `MentionUri`'s `zed:///` namespace needs a migration, not a rename
`crates/acp_thread/` uses `zed:///…` (empty authority) in 19 places and **persists it
in thread history**. A rename in place loses mentions in existing threads: it needs a
reader tolerant of both spellings, or a migration. Verified safe to defer — it never
reaches `cx.open_url`, so it is not an OS-level intersection.

### A3. The `/tmp` and shared-namespace family is half-done
Renamed already: `/tmp/zed-mcp*`, the Linux keyring label `zed-github-account`.
Still Zed-spelled: `zed-askpass*`, `zed-ssh-session*`, `zed-agent-terminal-*`, and
the **macOS pasteboard types `zed-text-hash` / `zed-metadata`** — that last pair a
genuine shared-OS-namespace intersection of the same kind as the keyring label.
All-or-none; right now the family is inconsistent.

**When sweeping, search escaped, backslash and `$env:`-prefixed spellings.** An
escaped space in `Application\ Support` hid a live site for a whole review round.

### A4. The release-artifact name family
`.github/workflows/*` and `tooling/xtask/src/tasks/workflows/vars.rs`. Deliberately
excluded from phase 4 on three verified premises: `EXPECTED_ASSETS` still lists
`Zed-aarch64.dmg` / `zed-linux-x86_64.tar.gz` while the bundle scripts emit `sawe-*`
(so the manifest is *already* out of sync for every family); `release.yml` and
`release_nightly.yml` are owner-gated to `zed-industries`; and FORK.md #118 forbids
regenerating these files. Note `run_bundling.yml` is **not** owner-gated and is
already broken on its own terms — `if-no-files-found: error` on
`target/zed-linux-aarch64.tar.gz` against a `sawe-*`-emitting script. Needs one
deliberate pass, not a row.

### A5. The remote-server rename's unproven arms
Phase 4 proved the **Docker and SSH** arms live, against real containers, with a
seeded decoy and a mutation that demonstrated the collision. **WSL, every Windows arm
and the host-side `cleanup_old_binaries()` reaper are code-verified only.** The live
recipes are in the doc comments of the two `#[ignore]`d tests in `crates/remote`;
`crates/remote/src/transport/live_remote_support.rs` is the shared scaffolding.

Since that run the scaffolding changed in ways that postdate it and are therefore
code-verified only: the decoy digest is compared **before** the path assertion (the
old order made the comparison unreachable under the very regression it was written
for), and the SSH arm passes `-F /dev/null`. Also, **the target now needs a
`.sawe-live-test-target` marker file** or the run aborts without touching it — these
tests `rm -rf` two directories on the host the env vars name, and there was nothing
stopping a typo from doing that to a real machine. Both recipes create the marker.

---

## B. Tests and gates

### B1. `cargo test -p client` has 2 failing tests
`telemetry::tests`. This fork permanently disabled `flush_events_inner`, and the
tests assert on the queue it used to drain. Same family as the 49 compiled-but-never-
run tests found earlier: a fork-local disable that left a test asserting the old
behaviour. Either fix the tests to the fork's behaviour or mark them with a reason.

### B2. `cargo test -p remote_server` has 3 failing tests
`test_remote_lsp`, `test_remote_settings`, `test_remote_external_agent_server`.
Confirmed pre-existing (checked out the parent commit's sources and got identical
`29/3/0` with identical assertions). They concern settings resolution and
LSP/agent-server name lists and touch no path.

### B3. `cargo check -p component_preview --all-targets` cannot compile
Its `gpui_platform` dev-dependency enables `screen-capture` without `x11`/`wayland`,
so `zed-scap` refuses to build. It is green in the workspace check only because other
members supply a display backend — the reverse of the four crates fixed by the sweep.
**A gate that cannot run reads exactly like a gate that passed.** Two candidate fixes
are written up in `docs/findings/2026-08-31-cfg-universes-and-the-warning-gate.md`.

### B4. `cargo-machete` entries for the feature-forcing dev-dependencies
`clock`, `call` and `remote_connection` carry self- or foreign dev-dependencies that
exist only to force a `test-support` feature and have no code use. If machete is ever
run, all of them need `[package.metadata.cargo-machete] ignored = [...]` together —
currently only `remote_connection`'s foreign one is listed. Impact today is nil:
machete is not installed and `run_tests.yml` is `workflow_dispatch:` and owner-gated.

---

## C. Editor behaviour, non-blocking

### C1. A passive desktop indicator for an unreadable session
The backend is done: the wire returns `transcript_unavailable` honestly in both
regimes. But it has **no reader in `session_view`**, so such a tab renders as an
ordinary empty conversation and the only signal reaches the user on *send*. The
divergence points the wrong way — the phone user, who can do nothing, gets the
error; the desktop user, who is the only one who can act, gets nothing.

**Implementation trap:** `status_row` renders `is_cold` ("Sleeping") *ahead of*
`Errored`, so naively reusing `Errored` is invisible on exactly the tab that needs
it. Needs a third state that outranks "Sleeping". This is a product decision about
what the user sees; a distinct status line plus a retry affordance was the
recommendation.

### C2. Forwarding hand-off arguments to `zed-<channel>.sock`
Four failure routes now *report* that they dropped the user's paths but still do not
open them. The canonical instance already listens on that socket and
`OpenRequest::parse` already handles it — this is what `crates/cli` does. It is a
second hand-off mechanism on the most-trafficked exit, deliberately not built inside
a fix round. It would also close C3.

### C3. The orphaned handshake hangs the user's `sawe` command
In the default CLI mode the CLI `join`s its receiver thread, so a hand-off that is
never answered blocks the terminal until interrupted. Currently fixed only in **what
it says**, not in what it does. C2 is the real fix.

### C4. The give-up exit codes are asymmetric
`failed_single_instance_check` returns **0** after losing the user's arguments —
including after the full 30 s `READ_TIMEOUT`, the worst case in the family — while
its 4 s sibling `LockBusyButUnreachable` exits **1** for strictly less harm. Both
codes are inherited and were preserved deliberately; FORK.md #114 documents the
asymmetry and warns that aligning them is an interface change. Decide it alongside
C2, which removes the loss entirely.

### C5. `:line:column` is not deliverable across the hand-off
The suffix is now split off at the sender and reported as dropped, instead of being
sent on as part of the filename (which made the instance open a folder-kind window
rooted at a path that does not exist). Delivering it properly needs
`navigate_to_positions`, which lives in `recent_projects` — above `workspace`, where
the tool is registered. Moving the tool needs a manifest change.

### C6. Splitting a diff pane twice trips a debug assertion in `display_map`
`SplittableEditor::unsplit`, running on a clone that shares its multibuffer with the
original, reaches `crates/editor/src/display_map.rs:303` with an empty patch list and
fails `"patches_for_*_in_range is only allowed to return an empty vec if the multibuffer
is empty"`. **It predates this work**: `CommitView` has had `can_split = true` and a
multibuffer-sharing `clone_on_split` all along
(`20c695d7fb:crates/git_ui/src/commit_view.rs:1233,1259`). What the diff-view unification
newly *exposed* is the **working-tree** diff, which had no `can_split` before
`b26c6bd0ea` and now has the same gesture.

Deferred rather than gated: the assertion is `cfg!(any(test, debug_assertions))` and
`release-fast` inherits `release`, so a user-facing binary degrades to a possibly
misaligned scroll/selection mapping (`Point::zero()..max_point()` is the fallback) rather
than aborting — and gating the gesture in `git_ui` would be working around an `editor`
split-machinery bug at the wrong layer. Cost of leaving it: an agent driving a **debug**
build that splits a diff pane twice hits an assertion. The fix belongs in
`SplittableEditor`'s split/unsplit bookkeeping, not in the views that call it.
FORK.md #136 / `docs/plans/2026-09-02-unify-the-two-diff-views.md`.

### C7. `SoloDiffView` never reports itself dirty
`Item::is_dirty` is not overridden (`crates/git_ui/src/solo_diff_view.rs`), so it takes the
trait default `false`, while `can_save` returns true for the working-tree source. An edit
in the Changes tab's diff therefore shows no dirty dot on the tab and does not prompt on
close. The commit source is unaffected — since `5a3b279610` `can_save` is
`self.source.is_editable() && …`, so one of the view's two sources is now genuinely
read-only and only the other one has the bug.

Deferred because the obvious fix is a trap. The view implements
`EventEmitter<EditorEvent>` and `to_item_events` but never subscribes to its editor and
never emits, so **no `ItemEvent` ever reaches the pane** — and that is load-bearing: an
`ItemEvent::Edit` runs `Pane::handle_item_edit` → `unpreview_item_if_preview`
(`crates/workspace/src/{item,pane}.rs`), promoting the shared preview tab out of the slot,
which is exactly the pinning that FORK.md #136's gesture model forbids. That is also why
#125's "editing a preview" is listed there as a promotion gesture this item does **not**
have. A correct fix reports dirtiness without emitting a promoting event, or teaches the
pane to distinguish the two.

### C8. The Commit tab's merge-parent toggle does not change the diff
`CommitView::select_parent_index` (`crates/git_ui/src/commit_view.rs`) loads
`load_commit_diff_against_parent` and then assigns the result to `self.diff_files` and
calls `cx.notify()` — nothing else. `diff_files` feeds only the affected-files list and
`clone_on_split`; the multibuffer's excerpts are built once in `CommitView::open` and are
never rebuilt. So on a merge commit, flipping "diff vs parent" re-renders the file list
against the other parent while the diff editor below keeps showing the first parent's
diff.

Deferred: out of the unification's scope (that work deleted `CommitView`'s *single-file*
mode and left the whole-commit view alone), and the fix is a re-excerpting pass, not a
one-liner — it has to rebuild the multibuffer through `commit_blob::load_commit_file_blob`
for every file of the new diff, the way `open` does, and decide what happens to scroll
position and to any split clone sharing that multibuffer.

### C9. A split pane has no preview item, so the git panel cannot retarget into it
After splitting a diff pane, the new pane holds a permanent tab and no preview item.
`SoloDiffView::resolve_gesture` declines a `DiffOpen::Retarget`
when the **active** pane's preview slot holds no `SoloDiffView`, so single-clicking files
in the git panel silently does nothing while the split pane is active — until the user
activates the original pane again, or double-clicks to summon.

Ordinary pane semantics, and `CommitView` behaved the same way before, but it is newly
reachable for the working-tree source now that it can split. Deferred because the
alternative — summoning into a pane that has no preview slot — is the pinning the gesture
model forbids; the real question is whether a split should inherit a preview slot at all,
which is a `workspace::Pane` decision.

### C11. The S-ANN blame-options layer has no producer, so it is permanently default
`BlameOptions` (`crates/editor/src/git/blame.rs:41` — `ignore_whitespace`, `follow_renames`,
`color_mode`, `author_filter`), its setter `GitBlame::set_options` (`:402`), the enumeration
helpers `all_entries` / `date_range` (`:416`, `:424`), the whole of
`crates/editor/src/git/blame_colors.rs` and `crates/editor/src/git/blame_filters.rs`, and the
options-aware renderer `render_blame_entry_with_options` (`crates/git_ui/src/blame_ui.rs:92`,
which really does implement author-filter dimming and the ByAuthor / ByDate colouring) are all
present and all unreachable with anything but defaults.

Two independent breaks, both verified rather than assumed:

- **`set_options` has zero callers repo-wide.** The only `.set_options(` hit in `crates/` is
  `crates/terminal/src/alacritty.rs:150`, an unrelated alacritty call. So `GitBlame::options()`
  is `BlameOptions::default()` for the life of the process.
- **The paint path never asks for them anyway.** `element.rs:2179` calls the free
  `render_blame_entry` (`:7150`), which calls `renderer.render_blame_entry` (`:7179`) — the
  non-options trait method. `git_ui`'s implementation (`blame_ui.rs:63`) then forwards to its
  own `render_blame_entry_with_options` with a literal `&BlameOptions::default()` and
  `date_range: None` (`:76-88`). `ColorMode::ByDate` needs that `date_range` and so could not
  work even if a producer set the mode.

There is also **no annotate toolbar in the tree**. `blame.rs`'s own doc comments describe "v1
of the toolbar" tracking this state and an "author-filter dropdown" enumerating contributors;
nothing renders either — `ColorMode` and `AuthorFilter` appear only in their defining modules,
in `blame_ui.rs`'s match arms, and in `blame_filters.rs`'s tests. The layer looks lost in the
refork transplant: the renderer half survived, the UI half did not.

Recorded, not fixed, because the decision is a product one — build the toolbar (an action or
gutter-menu surface that calls `set_options`, plus threading `blame.options()` and
`blame.date_range()` through `element.rs` into the options-aware trait method), or delete the
layer and the doc comments that promise it. Note the second half is not optional in either
direction: wiring only `set_options` changes nothing while the paint path still hands over
defaults.

*Done when:* colour modes / author filter / date toggle are reachable and actually change the
gutter, or the dead layer and its doc comments are gone.

### C12. The Uncommitted-Changes view re-targets by emptying itself first
`GitGraphPanel` no longer blanks on a project switch — it holds the previous project's graph
until the incoming `git log` can paint (commit `<this one>`). Its neighbour on the same event
does the opposite: `ProjectDiff`'s `ActiveMemberChanged` subscription
(`crates/git_ui/src/project_diff.rs:693`) calls `BranchDiff::set_repo`
(`crates/project/src/git_store/branch_diff.rs:117`), which drops `tree_diff`, clears both
commits and emits `FileListChanged`; `ProjectDiff::refresh` (`:1034`) then removes every
excerpt whose path the incoming repo does not also have, before the new buffers load.

Whether that is *visible* depends on how much of the incoming repository's status is already
in the git store's snapshot — for `DiffBase::Head` it usually is, so this may swap without a
blank. **Unmeasured**: read from the code while fixing the graph, not observed in a running
editor. Left alone because the fix is not the graph's: the graph could hold a whole
alternative view off screen and swap entities, while the diff view owns one multibuffer whose
excerpts are the content, so "keep the old rows until the new ones are ready" there means
staging excerpt edits in `refresh`, not deferring a view swap.

The git panel's Changes list is fine either way, and is worth recording so nobody "fixes" it:
`update_visible_entries` (`crates/git_ui/src/git_panel/changes_list.rs:100`) clears and
refills within one synchronous call after `UPDATE_DEBOUNCE`, so the old list stays painted
until the new one is complete — the same shape the graph now has.

*Done when:* a measurement says whether the Changes view blanks on a member switch, and if it
does, `refresh` swaps excerpts instead of emptying and refilling.

---

## D. Tooling gaps

### D1. `workspace.read_clipboard` MCP primitive
The headless platform has no clipboard backend, so a Copy Link round trip can only be
verified in halves. CLAUDE.md's rule is to **add the missing primitive** rather than
hand the gap to the operator. Small, and it unblocks a class of UI checks.

### D2. `cargo xtask workflows` is destructive here — guard it in code
Running it deletes `retag_release.yml` and strips 25 `if: false # sawe: not
applicable` guards plus 5 `workflow_dispatch:` gates, silently re-enabling CI this
fork deliberately disabled. FORK.md #118 records it and the generated files now carry
a warning line, but **nothing stops the command itself**. An xtask-side guard would.

---

## E. Documentation

### E1. Two further `.rules` §3 drifts
Reported during the paths correction, not fixed, neither urgent. §3's other locked
identifiers were not audited line by line against the code.

### E2. `docs/src/**` remains a largely un-rebranded upstream corpus
Deliberate. Only two classes were corrected: **destructive commands aimed at paths
this fork does not own**, and **statements a fork change made false**. The rule that
emerged and should govern any future pass: *retarget an uninstall instruction when it
has no antecedent, or a false one; leave it when its antecedent is explicit and true.*

### E3. Per-project `.zed/settings.json` in the language docs
Deliberately left. Different pattern from the user-level config directories, and not
an intersection with an installed Zed's own configuration.

---

## F. Mobile client (`spk-editor-mobile`)

Audited 2026-08-31 against the desktop wire: **no breakages**, full record in
`docs/findings/2026-08-31-mobile-wire-audit.md`. Nothing here is urgent; all four are
latent, i.e. they bite only when someone next touches the code in question.

### F1. `RemoteClient.getSessionEntry` is dead code with a stale signature
`core/.../RemoteClient.kt:535`. Zero call sites. It sends a Main-stream `index` with
no `stream_id`, so whoever revives it for a teammate tab lands on the wrong entry or
gets `entry_index_out_of_range`. Add `stream_id` before wiring it up.

### F2. `RemoteClient.unsubscribe` would fail against the current desktop
`core/.../RemoteClient.kt:513` sends `{kinds:[...]}`; `editor.unsubscribe` takes
`subscription_id` and is `deny_unknown_fields`. Zero production call sites (one test).
Fix or delete — do not leave it as a trap.

### F3. `EntryRoleDto` has no `ContextCompaction` variant
The desktop emits `context_compaction` (`crates/solution_agent/src/mcp/dto.rs:313`).
The client's tolerant serializer maps it to `Unknown`, so a `/compact` marker renders
as a generic plaque rather than failing — cosmetic, and it predates the sync point.

### F4. The `LastSeenIndex` watermark is written and never read
`getCached` and `readFromDisk` have no callers in `app/src/main`; only the three
`recordIfNewer` writes and `primeFromDisk` run. Either an unread badge was removed
and the writes were left, or it was never finished. Worth a decision, not a fix —
and it is why the `entry_index` semantic change is currently inert. Also stale:
`SessionDetailScreen.kt:205` still calls `agent_session_message_appended` "id-only".
