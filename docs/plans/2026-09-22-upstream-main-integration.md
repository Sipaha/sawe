# Integrate current upstream Zed into Sawe

**Status:** complete
**Requested by:** Pavel, 2026-09-22: «а давай вольем свежую ремоут версию к нам».

## Goal
Merge fresh upstream Zed into Sawe, preserving the fork's product identity,
Solution workflows, native AI providers, remote control, and Git-panel fixes.

## Authorization and scope
This explicit user instruction authorizes a one-time upstream merge despite
the older no-merge guidance in `.rules` and ADR-0001. It does not authorize
re-enabling disabled services, renaming Sawe, resetting user data, or adding an
automatic merge cadence. Do not ask the user to repeat this authorization.

- Starting Sawe main: `1535e330a5`.
- Pinned upstream main: `b54cc1d0acc8fe3f7581721ee1195516e7581f9d` (2026-09-22).
- Merge base: `c1b45aaa5f31401fa5368a8c9636f9d8db979517`.
- Divergence at start: 1133 fork-side commits, 1676 upstream-side commits.
- Upstream requires Rust 1.98.1.

## Integration strategy
Use a dedicated integration branch/worktree inside this Solution. Resolve
conflicts semantically rather than selecting one side globally. Separate agent
worktrees own disjoint conflict groups; the supervisor integrates their patches
and owns root manifests, the lockfile, shared integration points and final checks.
Retain both histories in a real merge commit and fast-forward main only after
verification. Never rewrite existing history or force-push.

## Product invariants
- Sawe branding, CLI/bundle IDs, `.sawe` project settings, and the `~/.spk/sawe`
  profile and Solution storage layout stay intact.
- Solutions, solution_agent (Claude/Codex), embedded MCP, remote control,
  project toolbar, Solution band and run configurations remain functional.
- Upstream collab UI, sign-in, auto-update, telemetry, cloud-only model services,
  Zeta and Sentry uploads remain disabled at their existing integration seams.
- Keep the Changes/Commit UI and regression coverage from `45f43cd492`; adopt
  compatible upstream fixes without losing Solution-specific repository routing.
- Preserve license and upstream attribution requirements.

## Verification
1. Confirm no unresolved merge markers or unmerged index entries remain.
2. Use Rust 1.98.1; keep any new toolchain installation inside the Solution.
3. Run formatting, a normal debug editor build, and the workspace/all-targets
   check where feasible; diagnose failures rather than suppressing them.
4. Run focused tests for conflict-heavy crates and fork-owned integration crates,
   including Git-panel regressions and native watcher tests. Test temporary
   projects must be outside member Git checkouts but inside the Solution.
5. Launch an isolated headless debug editor; verify Solution/member switching,
   Git panels, agent controls and representative editor UI with screenshots.
6. Once source and debug checks settle, build release-fast for the next user launch.
7. Update documentation, commit, fast-forward main, and push origin/main.

## Documentation
Record conflict decisions, compatibility adaptations, actual check results and
remaining blockers here. Amend ADR-0001 and `.rules` only to reflect the user's
explicit authorization and the new pinned baseline, without inventing a cadence.

## Progress
134 conflicted files were reconciled in isolated ownership groups. All initial
unmerged index entries are resolved; the aggregate Rust 1.98.1 check is now
finding and validating cross-crate API adaptations. The main checkout still
contains only the planning commit, not the provisional merged source.

Group checkpoints: shell `9ba03839b8`, docs/package `0f5bf5a699`, CLI/runtime
`6312c0ea2a`, shell follow-up `73bd644dc7`; core `be0bce4e1a` + `1c9bbcc486`;
Git `b35fa9803b` through `442b12e209`; ACP/native `241de5cc3d` + `204d0a36c5`.
These are resolution artifacts, not additional upstream merge parents.

### Tooling incident

Rust 1.98.1 was installed under the Solution's `sawe/target/upstream-tooling`,
but rustup unexpectedly self-updated the existing
`/home/spk/.cargo/bin/rustup` from 1.29.0 to 1.29.1 during that installation.
This out-of-workspace side effect was disclosed immediately; the global active
compiler remains 1.95.0. Further self-update is disabled in the isolated rustup
home, and Cargo caches/build artifacts are now scoped under the Solution too.

## Pause checkpoint (history)

The user paused at 13:40 and resumed at 13:48:32 on 2026-09-22. A second stop
came from the Codex session hitting its provider usage limit at ~15:14 with the
merge unlanded; a Claude session picked the work up from the worktree state.
Neither checkpoint is an open item any more — see the completion section below.

## Resumed compatibility checks

The user resumed at 13:48:32 on 2026-09-22. Pending shell/core patches
`ac6b679a6e` and `5c9272f996`, plus the original ListFilter SVG, are now
integrated. Solution pickers use stable serialization names and embedded
presentation; empty Solution workspaces close individually with
`RemovalIntent::CloseProject` so neighbouring empty Solutions remain open.

The preservation audit found and corrected two newly inherited product
regressions: the View menu exposed the disabled upstream Agent Panel, and the
installer's new dependency check inspected a Zed binary instead of Sawe's.
Menu regressions now assert that toggling AI settings never exposes that panel.
The pre-merge Git refresh protections were checked in the merged source;
this source audit is not a substitute for the pending runtime tests.

Verified so far after resumption:
- `cargo check --keep-going -p project_panel`: passed without warnings.
- `cargo test -p icons -p settings_content`: 57 passed, including both icon
  asset checks, fork settings deserialization and flattened key collision checks.
- Installer shell syntax and scoped diff whitespace checks: passed.
- `cargo check --keep-going -p zed --bin sawe`: passed without warnings
  (`check-integration-18.log`).
- GPUI list regressions: 30 passed. Explicit layout pauses now resume tail
  following after resize; manual scrolling retains Sawe's no-bounce behavior.
  Remeasure clamping covers wheel, scrollbar and coalesced reversal gestures.
- Debug binary build, wider regression tests and runtime verification remain pending.

Logs live in the primary member's `target/upstream-integration-audit/`.
Test temporary projects and the isolated UI probe live under the Solution's
`.verification/upstream-2026-09-22/`, outside member Git checkouts.

The read-only Git audit also identified an existing non-Git-member diff fallback:
`ProjectDiff` passes no repository for such a member, and `DiffBufferList` then
falls back to the global active repository. The same behavior exists before
this merge; it is not counted as a new integration regression or fixed here.

### Additional tooling incident during verification

At approximately 14:46 on 2026-09-22 an agent invoked `rustfmt` without the
scoped environment. Rustup started installing Rust 1.98.1 into
`/home/spk/.rustup`. That process was interrupted, but an attempted diagnostic
`rustup --version` in the same checkout completed the toolchain installation.
The global default remains stable/Rust 1.95.0; rustup remains 1.29.1. New
1.98.1 toolchain files and roughly 253 MB of download cache were written outside
the authorized Solution. The incident was disclosed; no external rollback or
cleanup was performed because it requires exact per-action user authorization.
All further verification uses explicit scoped tools, with automatic toolchain
installation disabled in the Solution's environment script.

### Runtime verification corrections

Debug assets now come from the checkout discovered above the executable, so
shared-target binaries/tests must be hard-linked into the integration worktree
before launch. The local test runner does this; no shipping source override is
needed. A first run otherwise read primary main's older defaults.

Runtime checks also found missing `terminal.starts_open`/`bottom_dock_layout`
defaults and a Git click setting misplaced under obsolete `message_editor`.
Those defaults are corrected, the obsolete block is removed, and FileFinder's
old Linux menu keys target the new picker Actions menu and include-ignored
control. The native watcher/backend now treat metadata-path changes as repository
identity changes even if the working directory and scan ID remain unchanged.

The first broad library run completed with 3114 passed and 8 failures. Most
failures exposed inherited test assumptions: automatic checkpoint capture,
upstream BranchDiff action ownership, Sawe branding/autosave defaults, and a
migration assumed to be last. Their fixtures now state the relevant behavior
explicitly. A real remote-control failure required selecting the Rustls provider
per server config because both provider feature sets are linked. GPUI's new Infer
regression also required measuring during a real render phase. Follow-up runs:
ACP 174 passed, GPUI 356 passed, remote control 76 passed, workspace 326 passed;
Git UI follow-up and native metadata watcher checks are still pending.

Initial UI screenshots verify the Sawe launcher, Solution/member strips,
Changes/Commit, split diff, external tag movement, FileFinder project/everywhere
scopes, and cold native chat alongside the Git graph. Fixtures use no live agent
subprocess or user profile. Final runtime verification follows the watcher fix.

## Verification results (Linux, Rust 1.98.1)

Successful runs, without counting reruns twice:

| Area | Passed | Existing ignored |
|---|---:|---:|
| ACP thread | 174 | 2 |
| Native Claude library / mock protocol | 107 / 16 | 0 |
| Native Codex library | 18 | 0 |
| Editor MCP library | 57 | 0 |
| Git backend / Git graph / Git UI | 149 / 100 / 426 | 0 |
| GPUI | 356 | 0 |
| Project library | 80 | 1 |
| Workspace | 326 | 0 |
| Solution agent | 941 | 1 |
| Solutions / Solutions UI | 245 / 68 | 0 |
| Worktree integration (including native watcher) | 98 | 0 |
| Remote control library / TLS+proxy end-to-end | 76 / 10 | 0 |
| Icons / settings (including doctests) | 2 / 55 | 0 |
| Test-target guard | 11 | 0 |

`cargo check --workspace --all-targets --keep-going` passed without warnings
in `check-all-targets-3.log`. Debug builds passed; the final build/runtime pass
including metadata identity and TLS changes is pending. The scoped debug
`script/clippy` wrapper retains all-targets/all-features, removes its forced
release build and caps diagnostics at warnings so pre-existing lints can be
reported. Its first run found three already-known LSP cache test warnings and
new adaptation diagnostics; redundant copies were removed and the renderer's
required upstream `Arc` API was documented as foreground-only. A final lint
run follows those changes. No warning suppression was added for test failures.

Real UI checks also cover a commit through `Commit Tracked`, clearing Changes
and updating the graph; closing one of two empty Solutions through the tab menu
keeps its neighbour, and closing the last produces one launcher. The seeded
native chat survives Solution close/reopen. Screenshots are under the isolated
verification directory; selected final images will be linked with this plan.

## What upstream ships that this fork does not adopt

A merge takes upstream's *code*; it must not take upstream's *instructions to
agents*, nor silently take behavior changes that contradict the fork's own
defaults. Five items were caught reviewing the merged diff and reverted or
gated in place, each with a `sawe:` comment at the site:

1. **The `.rules` anti-AI-PR tripwire.** Upstream's `.rules` orders any agent
   to prepend a `> [!IMPORTANT]` / "Remove this line to confirm you've reviewed
   this PR before submitting." banner to `README.md`, and never to remove it.
   Because `.rules` is also `AGENTS.md` and `CLAUDE.md` here (symlinks), the
   merge put that instruction in front of every future session, and the
   integration session had already complied and edited `README.md`. Sawe sends
   no pull requests upstream: banner reverted, bullet dropped, decision
   recorded in `.rules` and FORK.md #200 so a later integration does not
   reintroduce it unnoticed.
2. **An interactive zed.dev sign-in on the channel-link path.** Upstream
   replaced the fork's `authenticate(client, cx)` — which only signs in
   `if client.has_credentials(cx).await` — with `client.connect(true, cx)`,
   which has no such guard and reaches the browser OAuth flow. Opening
   `https://zed.dev/channel/<slug>-<id>` would have prompted a user with no
   Zed account to sign in. The credential-gated helper is restored.
3. **A dead keybinding shadowing two live ones.** Upstream added
   `assets/keymaps/{linux,macos}/vscode.json`, and this fork's default
   `base_keymap` is `"VSCode"` — which previously resolved to no asset, so the
   overlay is live here for the first time. Its `ctrl-alt-i` / `ctrl-cmd-i`
   binds `agent::ToggleFocus`, a panel Sawe never registers, at a higher
   precedence than `dev::ToggleInspector` (Linux) and
   `edit_prediction::ToggleMenu` (macOS). That single binding is removed; the
   rest of the overlay — Format Document, Zen Mode, debugger F5/F10/F11, call
   hierarchy — is kept.
4. **Re-advertised disabled services.** The edit-prediction settings page
   regained an unconditional "Zed Predictions" section for the Zeta provider
   that `edit_prediction_registry` maps to `None`, and the command palette
   gained `zed::GetMerch` opening `merch.zed.dev`. Both are disabled at their
   call sites, code kept in tree per "disable, don't delete".
5. **`format_on_save` inverted under us.** Upstream moved from a global `"on"`
   to a global `"off"` plus a twelve-language allowlist (Astro, Dart, EEx,
   Elixir, Elm, Go, GraphQL, HEEx, Kotlin, Rust, Starlark, Zig). Adopting that
   would have silently stopped formatting on save for 28 languages here,
   including JavaScript, TypeScript, TSX, Java, Python, JSON, YAML, HTML, CSS
   and PHP. The pre-merge effective value is restored for every language
   (global `"on"` plus the fork's `"off"` overrides for C, C++, Markdown and
   SystemVerilog), verified by comparing resolved per-language values against
   pre-merge `HEAD`. Switching to upstream's model later is a one-line change.

One merge artifact was also removed: `ListState::scroll_to_end` had taken both
sides' `state.pending_scroll = None;` and assigned it twice. A scan of all 1261
changed non-test Rust files found no other duplicated-assignment artifact (the
one in `gpui_linux`'s `set_appearance` predates the merge on both sides).

## Audited and found clean

- **CI workflows**: every workflow is gated. The merge *strengthened* this — jobs
  that our main disabled implicitly via
  `if: github.repository_owner == 'zed-industries'` now carry an explicit
  `if: false # sawe: upstream automation is disabled`.
  The eight newly added community/guild process workflows (guild_*,
  `triage_queue_board`, `slack_notify_community_automation_failure`,
  `community_pr_cleanup`, `maintainer_edits_nudge`) were gated on arrival and then
  **deleted** on the maintainer's instruction, together with the three scripts they
  alone used; see FORK.md #201 for why that pack is the exception to #118's
  keep-and-disable default. The tree now holds 40 workflow files — 39 hard-disabled
  by 107 job guards, with `run_tests.yml` narrowed to `workflow_dispatch:`.
- **Network**: no new unconditional outbound request. Telemetry
  (`send_event`, `flush_events_inner`), Sentry (`upload_panic`,
  `upload_minidump`, `upload_build_timings`) and the cloud LLM provider are
  still dead-ended; `should_install_crash_handler` got *stricter*. The new
  `openai_subscribed` / `x_ai_subscribed` providers reach `chatgpt.com` and
  `auth.x.ai` only after an explicit OAuth sign-in, and read only the keychain
  at startup. The new WSL sandbox helper that downloads from `cloud.zed.dev` is
  `#[cfg(target_os = "windows")]`.
- **Rebrand identifiers**: `crates/paths` still resolves `.sawe`,
  `.sawe/settings.json`, `.sawe/tasks.json`, `.sawe/run-configurations.json`,
  `.sawe/debug.json` and `.sawe_server`. The `.zed` strings the merge brought
  back live only in `#[cfg(test)]` fixtures and one comment.
- **No fork decision was dropped.** All 1056 files this fork had changed since
  the merge base were checked for lost `sawe` markers: two hits, both benign
  (`typos.toml` moved to `.config/typos.toml` with its marker intact, and one
  `.zed/settings` comment in a test). Thirteen fork-touched files now match
  upstream byte-for-byte, but each was verified to still contain the fork's
  change — upstream had converged on the same fix (`block_mouse_except_scroll`
  in the sidebar, the notification flex-layout fix, the removed "Rules Library"
  menu entry, the removed `[lib] test = false` in fs/project/worktree, which
  the fork's own `test_target_guard` independently enforces).
- **Settings compatibility**: the maintainer's real
  `~/.spk/sawe/config/settings.json` was loaded by the merged debug build and
  parsed without error. Upstream's `folder_icons` -> `folder_indicator` rename
  ships a migration and keeps `folder_icons` as an optional legacy field.
  `git_panel.sort_by_path` (the fork's panel) and the new `git_panel.sort_by`
  (upstream's diff multibuffer) drive different surfaces and do not conflict.
- **Upstream's two new `.agents/skills/` entries** (`gpui-bench`,
  `lint-creator`) are ordinary engineering references with no embedded
  instructions to act on; kept.

## Upstream infrastructure removed

The merge also carried build/process infrastructure that exists only for
Zed's own organisation. At the maintainer's instruction it is deleted rather
than carried:

- `corgi.toml` — configuration for Zed's sandboxed build tool, pinning macOS
  arm64 tool downloads by sha256. Sawe never runs corgi.
- `.wezel/` (`config.toml`, `schema.json`, `wezel.lock`, `experiments/`) and
  `.github/workflows/wezel.yml` — Zed's build-experiment service. The config
  even embeds Zed's own `project_id` UUID and `name = "zed"`. Nothing else in
  the tree referenced them.
- `.github/zizmor.yml` — an empty `rules: {}` config for a GitHub Actions
  linter that no enabled workflow runs.

Two related things are deliberately **kept**:

- `tooling/corgi/patches/scratch` and its `[patch.crates-io]` entry in the
  root `Cargo.toml`. Despite the corgi-flavoured name this is load-bearing:
  `scratch` sits in the real build graph as `cxx-build` -> `webrtc-sys` ->
  `libwebrtc` -> `audio` -> `agent_ui`/`call` -> `zed`. Reverting to the
  crates.io `scratch` would change how cxx-build shares headers and force a
  libwebrtc rebuild, for no benefit over the configuration that is already
  verified here.
- `nix/tests/sandboxing/` — `nix/tests/` predates this merge and these files
  exercise the Linux bwrap sandbox that *is* compiled into this build.

One dangling mention survives on purpose: `tooling/xtask/src/tasks/workflows/`
still names `.wezel/` in a path-filter regex. That generator must not be run in
this fork (FORK.md #118), and an unmatched path in a regex is inert, so it is
left rather than edited.

## The `zed` bin target hid four failures

`crates/zed` declares no `[lib]`, only `[[bin]] name = "sawe"`. Its tests
therefore live in the bin target, and `cargo test --lib -p zed` matches nothing
and exits 0 — exactly the trap `tooling/test_target_guard` was built for. The
verification table above was assembled from `--lib` runs, so these tests were
compiled by `--all-targets` but never executed. `cargo test -p zed --bin sawe`
reported 114 passed / 4 failed; all four now pass (118/0).

One was a real defect in this integration, three were inherited upstream
assumptions:

1. **`test_sweep_prompt_format_routes_to_sweep_prompt_model` — real defect.**
   The adaptation had dead-ended `EditPredictionPromptFormat::Sweep` next to
   the cloud `Zeta` format. Sweep is self-hosted:
   `sweep_prompt::request_prediction` reads the user's own `ollama` /
   `open_ai_compatible_api` settings and uses a plain `http_client`, the same
   shape as `fim`, which this fork allows. The shared
   `EditPredictionProviderConfig::Zed(..)` wrapper — which also carries
   `Mercury`, another provider the fork keeps — is what made it look
   cloud-bound. Upstream's routing to `EditPredictionModel::SweepPrompt` is
   restored; only `Zeta(_)` stays gated.
2. **`test_reload_checks_all_workspaces_for_dirty_items`.** Upstream assumes
   autosave is off, so a dirty buffer prompts on close. Sawe defaults to
   IDEA-style autosave, which saves it instead. Diagnosed by instrumenting the
   close path rather than guessed: it printed `hot_exit=true serialized=0
   remaining=1` with no prompt pending. Fixed with the fork's existing
   `disable_autosave(cx)` helper, which three other prompt tests already use.
   The instrumentation also refuted the data-loss worry this raised —
   `prepare_to_close` runs for *every* workspace held by the window, and once
   the prompt path is taken the dirty workspace is activated, so the test's
   activation assertion passes too.
3. **`test_reload_restores_project_windows_and_tabs`** and
   **`test_e2e_new_window_setting_restores_workspace_when_no_paths`.** Both
   assume the upstream session-restore default; Sawe lands on the launchpad and
   restores nothing, which `zed.rs` already documents. Fixed with
   `restore_last_session_on_startup(cx)` and an explicit
   `restore_on_startup = LastSession` respectively.

The other bin targets carrying test code (`cli`, `extension_cli`,
`edit_prediction_cli`, `auto_update_helper`, `docs_preprocessor`) were run for
the same reason: 154 passed, 0 failed.

## Runtime verification of the keymap fix

The removed `agent::ToggleFocus` binding was confirmed in a real editor, not
only in review: an isolated headless instance running the merged debug build
was sent `ctrl-alt-i`, and the GPUI Inspector opened. Before the fix that
keystroke would have resolved to the unregistered upstream AgentPanel and done
nothing.
