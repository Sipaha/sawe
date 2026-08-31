# Session handoff — 2026-08-31, second session

Supersedes `findings/2026-08-31-session-handoff.md` for everything after
`31176f2143`. That file remains the record for the cold-read repair, the `/clear`
blob bug, the destructive-rewrite class and the `rebuild_streams` performance work,
all still shipped and untouched.

**73 commits, all on `origin/main`. Working tree clean. No UI shipped.**

The session had two halves. The first is the correctness-and-cost work below. The
second began when the maintainer read a line in my summary — that `sawe://` had been
made an alias of `zed://` — and reversed it: **`zed://` is a different product's
contract and this fork disowns it.** Asked whether that extended to `.zed_server`,
they answered *"распространяется на все. это два разных продукта."* That became a
five-phase disown series, recorded separately below.

That previous handoff said the actionable pool was drained — "every item this
session identified has shipped; what is left either needs the maintainer or is
policy-blocked" — and, in the same breath, said to treat a thin pool as an
invitation to dig. Digging its three one-line entries produced: **a 359× write-path
win, a CLI that did not open your file, five crates whose own quality gate could not
execute, three repo gates that had never passed, a tool catalog wrong in every
number it gave, and two dead tests recovered.**

---

## What shipped

### 1. The agent transcript database was on sqlite's bare defaults

`SolutionAgentDb` — every AI session transcript — opened with a plain
`Connection::open_file` and issued no pragmas: `journal_mode=delete`,
`synchronous=FULL`, `busy_timeout=0`. The only **fork-owned** database in that
state; `crates/db/src/db.rs` has issued WAL + `busy_timeout=500` +
`synchronous=NORMAL` since upstream. (`crates/agent`'s `threads.db`,
`copilot_chat` and `edit_prediction_cli` are still bare — named in #111, not fixed.)

`store::persist_main_stream` runs once per `AcpThreadEvent::EntryUpdated` with **no
coalescing anywhere in the chain** — the throttle beside it gates only the MCP emit
— and `acp_thread`'s reveal timer fires every `TASK_UPDATE_MS = 16 ms`, each event
issuing three separate transactions. Measured at production scale (29,052 rows /
114.8 MiB): **48.5 ms per event, 3,032 ms of database time per 1,000 ms of
streaming** — a 3× overrun on a chain serialized per session that holds the shared
connection mutex throughout. After: **0.135 ms (359×)**, write amplification
56.0 → 12.9 KiB per 3.6 KiB row, largest-session flush 74.4 → 8.1 ms. FORK.md **#111**.

WAL+`FULL` was measured too — 17.0 ms/event, still a 1.1× overrun. It does not fix
the problem, it survives it. That is what retired the safer-looking option, not
taste.

**The half that is not a one-liner:** WAL adds a `-wal` sidecar, so every site that
copies or opens that file by path became a data-loss hazard. Six were found; two
were not in the plan. The worst was `script/migrate-from-spk-editor.sh`'s post-boot
`rm -f …-wal`, which under WAL deletes committed transactions — including the
schema the script had just booted an editor to create.

### 2. `sawe <path>` did not open your file — five defects, each hiding the next

1. **`editor.handle_cli_args` was absent from `GLOBAL_TOOLS`**, so it was served
   only from per-solution sockets while its only caller dials the *global* one,
   which answered `-32601 Tool not found`. `solutions.switch` was absent too, and
   worse: its `solution_id` is a **target**, not a scope, so per-solution injection
   overwrote it with the bound id and it could only ever self-switch.
2. **The reply reader took the first frame off the socket** regardless of JSON-RPC
   id, and opening a path makes the server emit `buffer_opened` first. Intermittent
   by construction — reopening an already-open path emits no notification, so it
   worked.
3. **A failed hand-off told the user nothing**: a `log::warn!`, then one line,
   `sawe is already running`, with no reason and no file. That silence is what kept
   (1) and (2) invisible.
4. **A failed MCP-server start left the editor permanently unreachable.** Four
   routes drop the flock while the process stays alive owning
   `zed-<channel>.sock`, so *every* later `sawe <path>` gave up silently until
   restart. Not a race — a persistent state.
5. **`:line:column` was not merely dropped, it was sent on as part of the
   filename**, so the instance opened a folder-kind window rooted at a nonexistent
   `…/probe.rs:3:2` and never opened the file. And `sawe://` — a **locked rebrand
   identifier** — was accepted by `is_url_scheme`, promised in the help text,
   registered as a desktop and Windows URL handler, and matched by
   `OpenRequest::parse` **never**.

All fixed; the reader is bounded by a 30 s deadline with ten tests; failures name
themselves on stderr with what was lost. FORK.md **#112**, **#113**, **#114**.

### 3. Gates that read green while blind — the session's real theme

- **Two `script/check-*` had never passed** (`check-keymaps`, `check-todos`), both
  on fork-introduced false positives in comments and prose. Both now exit 0 and
  both were proven to still catch a real violation.
- **`cargo check --workspace --all-targets` cannot see the shipping build's
  warnings.** It compiles a different cfg universe, and neither is a superset. The
  checks ritual already built the shipping universe and greped only for errors —
  **the grep was the blind spot**, fixed with one alternation at both sites.
  `docs/findings/2026-08-31-cfg-universes-and-the-warning-gate.md`.
- **Five crates could not compile their own `--all-targets` test build at all**, so
  `cargo clippy -p <crate> --all-targets` — the command the workflow doc tells every
  sub-agent to run — could not execute. A 260-member sweep found four beyond the one
  found by accident. A sixth, `component_preview`, is green in the workspace only
  because other members supply a display backend; left deliberately, recorded.
- **CLAUDE.md's MCP catalog was wrong in every number**: "89 tools", global "~70",
  per-solution "~133" (70 + 133 ≠ 89). Real, enumerated live and statically with the
  two agreeing exactly: **171 / 79 / 132** debug, 40 shared. A whole namespace
  (`run_config.*`) was missing, `solution_agent.*` listed ten of thirty-one, and one
  entry named a tool that has never existed.
- **Five `#[ignore]`s stated no reason.** Two pass and are live tests again (one
  proven non-flaky at 160/160 under concurrent load, one half-written and now real);
  three fail and say why. **All five came from upstream Zed; none was added by this
  fork** — the fork-local `split.rs` one, the most suspicious-looking, is an upstream
  aspiration never implemented.
- **`rebuild_streams`' decoration property test was tautological** for the five
  helpers its reference delegated to; three mutations were crate-wide holes no test
  anywhere caught. The two decision-logic helpers no longer delegate — in the
  reference *or* in the anti-vacuity census — and their mutations now fail on field
  comparisons rather than on the census floor.

---

## Disproved — three leads that were dead, and cost less to kill than to act on

1. **`cargo test -p collab` does not silently skip its integration tests.** `collab`
   dev-depends on *itself* with `features = ["test-support"]`, so the resolver
   satisfies `required-features` regardless of the CLI flag; `project`, `worktree`
   and `fs` do the same. `required-features` is a visible cargo mechanism, and
   `tooling/test_target_guard` is correctly scoped to `test = false` only.
2. **The fork's doc bookshelf has no link rot.** 162 files, 234 local links, all 40
   non-resolving ones false positives — upstream doc-authoring templates, an example
   inside a blockquote, and a `spk-image://` URI mistaken for a path. No gate built
   for a failure that is not occurring.
3. **Fork-local keybindings are consistent** across all three keymaps; zero
   fork-local actions bound on fewer than three platforms.

---

## What a future session must not re-derive

### 1. `docs/superpowers/` is only PARTLY gitignored

Seventeen files under it are **tracked**. "It's scratch, skip it" is wrong in the
way that hides things: a task sweeping the tree for `cp <database>` sites excluded
that directory on exactly this belief and missed a tracked one. Check `git ls-files`
on the specific path. (Committed HEAVY-track plans still belong in `docs/plans/`;
that half of the old rule is unchanged.)

### 2. `server.add_tool(` is not how you count the MCP catalog

`register_typed_tool_with_tier` and `register_typed_tool_with_protection` call
`add_tool` *inside* `editor_mcp`, so `git_ui` (51), `git_graph` (2),
`git_conflict_ui` (5) and `solution_git` (8) — **66 tools** — are invisible to that
grep. A count built on it was off by ~105. Walk every `impl McpServerTool` and read
each `const NAME`, or ask a running instance for `tools/list`. `GLOBAL_TOOLS`'
literal count is not its entry count either: commented-out lines inflate a grep.

### 3. `failed_single_instance_check` IS reachable on a debug build

A report claimed it is hard-coded `false` on the Dev channel and that
`ZED_RELEASE_CHANNEL=stable` is needed. **False, disproved twice.**
`crates/zed/RELEASE_CHANNEL` contains `stable`; the env var is only an *override*; a
plain debug build reports `channel=Stable` and binds `zed-stable.sock`. Dev *is*
special differently: `main.rs:569-572` forces the single-instance check false there,
so on Dev the flock is the only gate. Do not conflate the two.

### 4. `cargo fmt --all` crosses agent boundaries

`git commit <path>` respects file ownership; `cargo fmt --all` does not — it is a
whole-tree write, and it reformatted another implementer's uncommitted work
mid-session. With more than one agent live, `cargo fmt -p <package>`.

### 5. A tool's socket placement is part of its contract

`GLOBAL_TOOLS` is fail-*safe* for leaks and fail-*silent* for reachability, and this
has now shipped broken **four** times. FORK.md #112 has the rule; the short form is
to check which socket the real caller dials, and what per-solution injection does to
the params. A `solution_id` that is a *target* is corrupted by injection; a tool with
no `solution_id` property gets no injection at all, so scoping it buys zero
isolation while removing it from the global socket.

### 6. The two single-instance gates are different locks taken at different times

`zed-<channel>.sock` is bound before `app.run`; the flock is acquired in the *last*
statement of the `app.run` closure. Everything in cluster 2 above follows from that
gap. FORK.md #113.

---

## Process: what actually caught things

Controller + subagents, a fresh implementer per task, a task review naming explicit
surfaces, a scoped re-review per fix round, controller-verified gates before
believing any report, and a push only after review.

- **Every agent had explicit permission to return a documented negative result, and
  it paid three times** — the collab claim, the doc-link rot and the keymap
  consistency were disproved rather than "fixed".
- **Six times a reviewer overturned something already treated as established**,
  including twice against the controller's own evidence (an `add_tool` count off by
  66 tools; a `GLOBAL_TOOLS` count that included commented-out lines) and three times
  against an implementer's headline claim. The sharpest: an implementer justified a
  fix with "the desktop entry is `Exec=%U`, so 'Open with Sawe' arrives as a
  `file://` URL" — the reviewer showed the desktop entry runs the **CLI**, which
  never passes that URL to the editor. **FORK.md #113's own last bullet already said
  so.** A citation is not a reading.
- **A re-reviewer's measurement was itself wrong**, and a later task caught it by
  measuring again. Distrust `task-R-report.md`'s Finding 4 paragraph specifically.
- **Ask for the mutation table, not "I added a test."** Every reviewer re-ran it
  rather than reading it; one reproduced a table exactly *including the seeds* and
  then added five mutations of its own. The mutation that matters in a fix round is
  the one that restores the old assertion **and** its expectation, so only the newly
  added assertion can fire.
- **Several defects were claims rather than code** — FORK.md #111 saying "the only
  database in the fork", a test header saying each delegated helper had its own unit
  tests, an `#[ignore]` borrowing upstream's "flaky" label for a failure that is
  deterministic here, and three CLI messages that overstated what they knew. The
  `#[ignore]` one is the subtlest: the mechanism named was right, the *intermittency*
  implied was not.

---

## Active gotchas

- **Disk.** Opened at 268 GB, hit **119 GB** mid-session; `target/debug/incremental`
  alone had regrown to 184 GB. Deleting it and `target/release` returned 187 GB.
  **Never** delete `target/release-fast` (the maintainer's running binary). Reviewers'
  worktree target dirs live *outside* `target/`. **Arm a disk alert at session
  start** — one backgrounded `until` loop caught this before anything failed.
- **`git push origin <sha>:main` pushes that sha's whole ancestry.** When some tasks
  are cleared and others are not, push the sha of the *last reviewed* commit. I did
  this correctly all session and then slipped once, putting an unreviewed task on
  `origin` (fixed forward).
- **`git commit <path>` commits that path's working-tree content**, so two agents
  dirty in one file means whoever commits second sweeps the other's work. Serialise
  on the **file**, not the crate.
- **`| tail` and `| head` mask exit codes.** Still true, still caught agents.
- The harness's `<new-diagnostics>` blocks remain stale mid-edit snapshots.
- `mcp__sawe__*` drives the maintainer's **live** editor. `script/run-mcp
  --debug --headless --runtime-dir <dir>` for your own; `SAWE_HOME` is the only
  variable that moves this fork's paths.
- `script/clippy` forces `--release`; scope clippy to the package on dev instead.

---

## Outstanding pool

1. **A passive desktop indicator for an unreadable session** — backend done,
   `transcript_unavailable` still has no reader in `session_view`, so the phone user
   who can do nothing gets the error and the desktop user who can act gets nothing.
   `status_row` renders `is_cold` ahead of `Errored`, so reusing `Errored` would be
   invisible on exactly the tab that needs it. **Product decision, raised with the
   maintainer.**
2. **`sawe://` is parsed but not registered with macOS.**
   `install_cli::register_zed_scheme` registers only `ZED_URL_SCHEME = "zed"`. Linux
   (`sawe.desktop.in:16`) and Windows (`sawe.iss:1260`) do register it. Untestable
   from this machine.
3. **Forwarding arguments to `zed-<channel>.sock`** so the four hand-off failure
   routes actually open the files instead of only reporting the loss. The canonical
   instance already listens there and `OpenRequest::parse` already handles it — this
   is what `crates/cli` does. A second hand-off mechanism on the most-trafficked
   exit; deliberately not built inside a fix round.
4. **The orphaned handshake still hangs** the user's `sawe` command in the default
   CLI mode (it `join`s its receiver thread). Fixed only in what it *says*.
5. **`component_preview`'s per-crate gate cannot compile.** Two candidate fixes
   written up; choosing between them is open.
6. **The debugger's still-pending-startup server at quit** — two reviewers agreed to
   defer *harder*; publishing the starting server's `Arc` would let the quit hook
   `shutdown()` a server the startup task still owns and is mid-`initialize` on.
   **Do not build it without evidence that a real server survives a quit.**

**Do not** clean up the ~18 legacy orphan sessions in the maintainer's database —
still their call, and the GC that would purge them is deliberately gated on liveness
with cold orphans logged instead. If you ever copy that database: it is on WAL now,
so `PRAGMA wal_checkpoint(TRUNCATE)` first, or copy `.db` **and** `.db-wal`. Never
`-shm`.

---

## Resume recipe

Read this file, then `docs/INDEX.md`, then `git log --oneline -50` to confirm the
chain ends at `fde9ac75cf`. Pick from the pool per `docs/workflow/supervisor-mode.md` § 7.
The pool above is genuinely thinner than what this session started with — but so was
the last one's, and it hid five real defects. Dig.

---

## Final gate, controller-verified at `fde9ac75cf`

`cargo check --workspace --all-targets` exit 0, zero errors, zero warnings ·
`cargo build --bin sawe` exit 0, zero warnings · `cargo fmt --all --check` clean ·
`script/check-{licenses,keymaps,todos}` all exit 0.

The whole-branch review returned **coherent**: the contended regions in
`crates/zed/src/main.rs` (four tasks), `crates/editor_mcp/` (three) and `FORK.md`
(five) read as one design. It swept the tree itself for `solution_agent.db` by path
and found no seventh site the branch missed, and confirmed there is no
double-reporting on any CLI exit and no case that exits non-zero having lost nothing.

Its three findings were all seams where a comment or a durable record contradicted
the code the branch itself changed — fixed in one wave and re-reviewed.

**One asymmetry is documented rather than resolved, deliberately:**
`failed_single_instance_check` returns **0** after losing the user's arguments,
including after the full 30 s `READ_TIMEOUT`, while its 4 s sibling
`LockBusyButUnreachable` exits **1** for strictly less harm. Both codes are
inherited and were preserved on purpose; FORK.md #114 now says so and warns that
aligning them is an interface change. Whoever forwards arguments to
`zed-<channel>.sock` should decide it, since forwarding removes the loss entirely.

---

# Part two — disowning Zed

`docs/plans/2026-08-31-disown-the-zed-url-scheme.md` is the plan and carries the
maintainer's rulings verbatim. FORK.md **#115-#119**.

**The rule:** a user may have both editors installed and **they must not intersect
in anything**.

## What was actually intersecting

- **`File → Install CLI` deleted a real Zed's CLI.** It targeted
  `/usr/local/bin/zed` with an unconditional `remove_file` before symlinking,
  escalating through `osascript` with admin privileges if that failed — live on
  macOS, dead code on Linux. It also registered `zed://` with the OS
  (`NSWorkspace.setDefaultApplication` on macOS ≥12), and the error was swallowed by
  `.log_err()`, so the dialog appeared on no platform.
- **`zed://` was accepted, promised in the help text, and registered as a desktop and
  Windows URL handler — and `OpenRequest::parse` matched only `zed://` while the
  fork's own `sawe://` was inert.** Disowning turned out to be free: `zed://extension/`,
  the one arm that looked like it carried the *kept* zed.dev registry feature, has no
  in-repo producer — extensions install over HTTP and never see a URL.
- **Our uninstall documentation destroyed a real Zed**, instructing a Sawe user to
  `rm -rf ~/Library/Application Support/Zed`, `~/Library/Caches/Zed` and
  `/usr/local/bin/zed`. And `script/uninstall.sh` ran `rm -rf $HOME/.zed_server`
  **on both platforms**.
- **Both products wrote byte-identical filenames into one shared remote directory.**
  `.zed_server` held `zed-remote-server-{channel}-{version}` from either client.

## The three things that were worse than they looked

1. **My own fix made an instruction more dangerous.** I ordered the uninstall docs
   retargeted from `~/Library/Application Support/Zed` to `~/.spk/sawe` — and
   `~/.spk/sawe/ss` holds the user's **Solution project checkouts**, including the
   repository this fork is developed in. Two reviews passed it. Phase 3's implementer
   caught it. The docs now name the five children (`config`, `data`, `state`,
   `cache`, `logs`) and never the root, exactly as the script does, because
   *"delete X but keep X/ss" is not an executable instruction.*
2. **`.rules` §3 described directories the binary never creates.** It locked
   `~/.config/sawe/`, `~/Library/Application Support/Sawe/` and `%APPDATA%\Sawe\`;
   `paths::base_dir()` is `home_dir()/.spk/<channel>` on **every** platform. The
   maintainer ruled *"правим документацию"* — the code stays, since the single root
   is what makes `SAWE_HOME` / `--user-data-dir` isolation work, and every
   agent-driven MCP check depends on that.
3. **`cargo xtask workflows` is destructive in this fork.** An implementer ran it,
   read the diff before committing, and refused: regeneration deletes
   `retag_release.yml` and strips **25** `if: false # sawe: not applicable` guards
   plus 5 `workflow_dispatch:` gates — silently re-enabling CI this fork deliberately
   disabled. The files now say so under their own `# Rebuild with…` header. FORK.md #118.

## The bar the last phase set

Phase 4 renamed `.zed_server` → `.sawe_server` and the uploaded binary to
`sawe-remote-server-…`. It proved the change against a **real `ubuntu:24.04`
container**: seed a decoy `~/.zed_server/zed-remote-server-dev-build`, run the real
upload path, confirm the client writes `~/.sawe_server/` and leaves the decoy
byte-identical — **then revert `paths.rs` and run it again, watching the decoy's md5
change.** It demonstrated the collision rather than arguing it.

It was equally exact about what it could not prove: the **SSH** arm — the one users
actually use — is code-verified only, because standing up `sshd` would mean touching
the maintainer's system service and authorized keys. So are WSL, every Windows arm,
and the host-side reaper.

**Why the rename is safe:** the name is in no proto message and no handshake.
*Version negotiation **is** the file name* — the client runs the expected path with
`version` and reads only the exit status — so a rename can only fail into the
re-upload branch, never desynchronise a check.

## Still to do in this series

- **Phase 5**: the Windows shell appx (`Name="ZedIndustries.Zed"`,
  `<DisplayName>Zed</DisplayName>`, verb `OpenWithZed`, and an identity that does not
  match what our own uninstaller removes — a live bug); and `MentionUri`'s `zed:///`
  namespace in `crates/acp_thread`, which is **persisted in thread history** and so
  needs a migration or a tolerant reader, not a rename in place.
- **The `/tmp` family**: `zed-askpass*`, `zed-ssh-session*`, `zed-agent-terminal-*`,
  and the macOS pasteboard types `zed-text-hash` / `zed-metadata` — that last pair a
  shared-OS-namespace intersection of the same kind as the keyring label, which was
  renamed. Search for **escaped, backslash and `$env:`-prefixed spellings**: an
  escaped space in `Application\ Support` hid a live site for a whole round.
- **The release-artifact name family** in `.github/workflows` and
  `tooling/xtask/.../vars.rs`. Deliberately excluded: that manifest is already out of
  sync for *every* artifact family (`EXPECTED_ASSETS` lists `Zed-aarch64.dmg` while
  the bundle scripts emit `sawe-*`), every job is gated on
  `repository_owner == 'zed-industries'`, and #118 forbids regeneration. It needs one
  deliberate pass, not a row.
- **`~/.zed_server` is still shared by design nowhere** — but per-project
  `.zed/settings.json` was deliberately left, as a different pattern that does not
  intersect an installed Zed's user config.
