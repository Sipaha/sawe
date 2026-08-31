# TODO — deferred work

Findings from the 2026-08-31 sessions that do **not** block using the editor on
Linux. Recorded here for future execution rather than chased at the time.

Each entry says what is wrong, why it was deferred, and where the evidence is.
Nothing here is a guess: every item was established by reading or measuring, and the
long-form record is in `FORK.md` and `docs/findings/2026-08-31b-session-handoff.md`.

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
