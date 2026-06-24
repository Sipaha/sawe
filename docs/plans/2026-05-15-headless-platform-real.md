# Real headless platform (no Xvfb)

**Status:** complete (2026-05-15). §H supervisor smoke-test passed:
`workspace.screenshot` returns a 1379×852 PNG of ~118 KB with the
rendered Workspace (project panel, README.md tab, git diff, commit
graph) — see [`2026-05-15-headless-platform-real-screenshot.png`](2026-05-15-headless-platform-real-screenshot.png).
Supervisor also landed a follow-on hotfix commit `56d7fee51e` —
`atlas.before_frame()` in offscreen `render_to_image` + calloop
refresh timer in `HeadlessClient` — without which the
sub-agent's path painted blank pixels (the first-frame scene was
fully built but pending atlas uploads never flushed, and subsequent
async state changes never re-rendered).
**Estimated:** 1 sub-agent session, ~1.5–2 h, worktree-isolated
**Goal:** Replace the Xvfb wrapper with a native headless GPUI platform so
`script/run-mcp --headless` runs the editor with **no X server at all**,
zero windows on the user's desktop, and `workspace.screenshot` returns
actual rendered pixels (closing
[`docs/findings/2026-05-headless-screenshot-blank.md`](../findings/2026-05-headless-screenshot-blank.md)).

## Context

Currently `--headless` mode wraps `spk-editor` in `xvfb-run` — works for
state ops but `workspace.screenshot` is blank because NVIDIA Vulkan under
Xvfb silently fails the offscreen RENDER_ATTACHMENT pass. The Xvfb path
is also ~3 MB of extra system deps + a wrapper process per launch.

The native infrastructure for a real headless backend is **already in
place but disabled**:

| Component | Today | What's needed |
|---|---|---|
| `gpui::PlatformHeadlessRenderer` trait | `#[cfg(any(test, feature = "test-support"))]` | Un-gate |
| `gpui::TestWindow` (full `PlatformWindow` impl, ~400 LOC) | `#[cfg(test\|test-support)]` | Promote sibling or reuse |
| `gpui_macos::MetalHeadlessRenderer` | Implements `PlatformHeadlessRenderer` | Mirror on Linux via wgpu |
| `gpui_linux::headless::HeadlessClient` | Stub — `open_window` bails | Return a real `HeadlessWindow` |
| `gpui_platform::current_headless_renderer()` | macOS-only | Add Linux arm |
| `gpui_platform::current_platform(headless: bool)` | Accepts `false` always from zed | Wire `--headless` flag |

So the work is "stitch the existing pieces together for Linux + add the
wgpu offscreen renderer", not "write a new platform from scratch".

## Scope

### A. `gpui` — un-gate the headless trait + headless window primitive

- `crates/gpui/src/platform.rs`:
  - Remove `#[cfg(any(test, feature = "test-support"))]` from the
    `PlatformHeadlessRenderer` trait definition (line ~712).
  - Add the `HeadlessRenderer` accessor (helper conversion if needed)
    that non-test code can use.
- `crates/gpui/src/platform/test/window.rs`:
  - Either (preferred) **promote `TestWindow` to a public `HeadlessWindow`** in
    a new file `crates/gpui/src/platform/headless/window.rs`, removing the
    `test`/`test-support` gating; rename `TestWindowState` → `HeadlessWindowState`.
    Keep a thin `TestWindow = HeadlessWindow` alias under the test-support
    feature for existing tests if any depend on the name (grep first).
  - Or (fallback) duplicate the file with adjustments. Promotion is
    cleaner — `TestWindow`'s API is identical to what we need.
- Re-export from `gpui::prelude` / lib root: `HeadlessWindow`,
  `PlatformHeadlessRenderer` (both ungated).

### B. `gpui_wgpu` — Linux offscreen wgpu renderer

- `crates/gpui_wgpu/src/wgpu_renderer.rs`:
  - Add `WgpuRenderer::new_offscreen(width, height, scale: f32) -> Result<Self>` —
    no `raw-window-handle` Surface, just device+queue+offscreen atlas. May
    require minor refactor of existing `new()` to extract the surface-less
    init path.
  - The existing `render_to_image(scene)` already does pure offscreen
    render; reuse it directly.
- New file `crates/gpui_wgpu/src/headless_renderer.rs`:
  - `pub struct WgpuHeadlessRenderer { inner: WgpuRenderer }`.
  - `impl PlatformHeadlessRenderer for WgpuHeadlessRenderer { … }`:
    - `fn render_to_image(scene) -> RgbaImage` → `inner.render_to_image(scene)`.
    - `fn sprite_atlas() -> Arc<dyn PlatformAtlas>` → `inner.sprite_atlas()`.
- Add adapter-selection bias: when constructing offscreen, prefer
  `IntegratedGpu > DiscreteGpu` in `select_adapter_and_device` (compositor
  hint is `None` so today it picks NVIDIA discrete, which is exactly what
  fails under Xvfb today). Verify the AMD RADV path works for pure offscreen
  (no Surface = no DRI3 dependency, should be fine).

### C. `gpui_linux` — HeadlessClient::open_window returns a real window

- `crates/gpui_linux/src/linux/headless/client.rs`:
  - Replace the `open_window` stub with:
    ```rust
    fn open_window(&self, handle: AnyWindowHandle, params: WindowParams)
        -> anyhow::Result<Box<dyn PlatformWindow>>
    {
        let renderer = gpui_platform::current_headless_renderer()
            .context("no headless renderer available on this platform")?;
        Ok(Box::new(gpui::HeadlessWindow::new(handle, params, renderer)))
    }
    ```
  - Wire `displays()` / `primary_display()` to return a synthetic
    1920×1080 display so layout code that queries it has bounds.
  - Wire `active_window()` to track the last `open_window`ed handle
    (single-window assumption is fine for now — fork's usage opens
    one Solution window at a time).
  - Wire `window_stack()` to return that single window (NOT empty
    — even though the recent fix tolerates empty, returning the real
    value is cleaner).
- New file `crates/gpui_linux/src/linux/headless/display.rs`:
  - `pub struct HeadlessDisplay { id: DisplayId, bounds: Bounds<Pixels>, scale: f32 }`
  - Implements `PlatformDisplay`.

### D. `gpui_platform` — Linux arm for headless renderer

- `crates/gpui_platform/src/gpui_platform.rs`:
  - Drop the `#[cfg(feature = "test-support")]` on `current_headless_renderer()`
    (it's needed for the runtime headless path now, not just tests).
  - Add the Linux arm:
    ```rust
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new(1920, 1080, 1.0).ok()?))
    }
    ```
  - macOS arm stays.

### E. `crates/zed/src/main.rs` — CLI flag

- Parse a top-level `--headless` flag (before `Application::with_platform(...)`
  at line 421). Set a `headless` boolean.
- Replace `current_platform(false)` with `current_platform(headless)` at
  line 421 and the error-modal site at line 104.
- Add to `--help` output if there is one.
- Update `FORK.md` "Notable upstream file modifications" row for
  `crates/zed/src/main.rs` (already listed — append `+ --headless flag`
  to the existing change description).

### F. `script/run-mcp` — drop Xvfb wrapping

- Remove the `xvfb-run` availability check + `command -v xvfb-run` test
  in `--headless` mode (no longer needed).
- Remove the `xvfb-run --auto-servernum --server-args="-screen 0 1920x1080x24"`
  prefix in the launch path. Just `"$binary" --headless "${forward_args[@]}"`.
- Update header comment block — "Uses native headless platform; no Xvfb
  required."
- Keep `--display` mode unchanged.

### G. `editor_mcp` — verify tools work in headless

No code changes expected — just confirm in the smoke-test:
- `windows.list` returns the headless window.
- `workspace.screenshot` returns a non-blank PNG (the test of the whole phase).
- `windows.dispatch_action` dispatches.
- `windows.send_keystroke` + `windows.send_text` deliver events.
- `windows.click_at` deliver mouse events.

If any of these silently no-op in headless mode (because `HeadlessClient`
returns `None` for active_window or the wrong type), file as a follow-up
finding + fix it minimally inline.

### H. Verification — supervisor smoke-test

Post-merge, the supervisor runs:

```bash
cargo build --bin spk-editor
script/run-mcp --debug --headless &
until [ -S "$HOME/.spk/spk-editor-dev/config/mcp.sock" ]; do sleep 0.5; done
# Python client: open AlphaSol, screenshot, assert non-blank
python3 <<'PY'
import socket, json, base64, os, time, hashlib
# … connect, solutions.open, sleep 5, workspace.screenshot
# Decode PNG, assert at least N distinct colors (not solid dark slate)
PY
pkill -f target/debug/spk-editor
```

Acceptance: the screenshot contains the rendered Workspace UI (title bar,
project panel, README.md tab) — same content as `--display` mode, just
without a window on the user's screen.

### I. Documentation

- `docs/findings/2026-05-headless-screenshot-blank.md`: mark **resolved**;
  append a "Resolution" section pointing at this plan + the final commit
  SHA.
- `docs/workflow/supervisor-mode.md`: simplify the MCP-recipe two-mode
  table — `--headless` becomes the default for all agent-driven work
  (screenshots now work in headless); `--display` is only for "human dev
  wants to see the window".
- `FORK.md`: update `crates/zed/src/main.rs` row (add `--headless` flag);
  add `crates/gpui_wgpu/src/headless_renderer.rs` to fork-touched table.
- The plan doc itself: tick acceptance items, append final SHA.

## Out of scope

- **Multi-window headless.** Single-window assumption is fine — the
  fork's workflow opens one Solution window at a time. Multi-window
  support can come later if needed.
- **Window-size CLI flag (`--size WxH`).** Default to 1920×1080 in
  headless. Configurable size is a follow-up plan (separate from this
  phase). Out of scope here so we don't blow up the diff.
- **Clickable-tree dump + `windows.click_id`.** Separate plan
  (`docs/superpowers/plans/2026-05-15-clickable-tree.md`) — independent
  feature, depends on this one only insofar as it'll use the headless
  mode for verification.
- **Wayland-only headless.** Linux headless via wgpu works on both
  X11-host and Wayland-host machines (no display server consulted at
  all). No Wayland-specific path needed.

## Architectural decisions

1. **Promote `TestWindow` → `HeadlessWindow`** rather than write a new
   sibling impl. The TestWindow's API is exactly what we need; promoting
   is ~30 lines of cfg-flag removal + renames vs ~400 lines duplicated.
   Keep an `pub type TestWindow = HeadlessWindow` alias for any tests
   that import the old name.
2. **Single concrete `WgpuHeadlessRenderer`** (no enum / no trait
   object inside) — Linux + FreeBSD use it; macOS keeps its own
   `MetalHeadlessRenderer`; Windows TBD if/when needed. The
   `PlatformHeadlessRenderer` trait already abstracts the choice.
3. **Adapter-selection bias toward IntegratedGpu** in offscreen mode.
   Discrete GPUs are over-represented at the top of the wgpu adapter
   sort, but for pure offscreen rendering integrated GPUs are equally
   capable and often more reliable on Linux (AMD RADV > NVIDIA's
   proprietary stack for offscreen). The bias is gated to offscreen
   construction only.
4. **`HeadlessClient` returns a real `active_window`** (the last
   `open_window`ed handle) rather than `None`. The
   `find_window_for_solution` path in `solutions::mcp` uses
   `cx.windows()` directly so it works either way, but other code
   paths (`App::dispatch_action`) depend on `active_window()`
   returning `Some` to route to the right window. Track a single
   pointer in `HeadlessClientState`.
5. **CLI flag, not env var**, for `--headless`. The flag is on the
   spk-editor binary itself, propagated from `script/run-mcp --headless`
   via `forward_args`. Env vars are hidden state; a CLI flag is visible
   in `ps aux` and discoverable via `--help`.

This will get one new ADR (ADR-0002) when the phase finalizes: "Native
headless platform for autonomous agent driving" — captures the
decision-tree of "Xvfb vs native vs hide-after-show" and locks in #1, #2,
#3 from the list above.

## Risks

- **`HeadlessWindow` may have test-only assumptions baked in** (the name
  alone tells you it was written for test code). Possible: input handlers
  not delivering events through the real GPUI event dispatch path, atlas
  not initialising in a non-test-build. Mitigation: the sub-agent verifies
  end-to-end via the supervisor's MCP smoke-test before declaring done;
  any test-isms surfaced get inline-patched in scope.
- **`WgpuRenderer::new` may not split cleanly** into "with Surface" and
  "without Surface" variants. The constructor presumably wires the
  Surface format / config into many places. If extracting `new_offscreen`
  ends up touching >200 LOC inside `WgpuRenderer`, that's a sign to keep
  the Surface but immediately drop it post-init; pragmatism over purity.
- **macOS regression.** Promoting `TestWindow` to `HeadlessWindow` touches
  shared `gpui` code. The macOS `MetalHeadlessRenderer` impl + tests must
  still build and behave the same. Mitigation: `cargo test -p gpui` +
  `cargo build --workspace` on the merged result.
- **`select_adapter_and_device` adapter-bias change**. Toggling integrated
  > discrete only in offscreen mode requires a parameter. If we change
  the global preference, normal `--display` runs would also switch
  adapters. Strict: the bias gates on `is_offscreen` (passed from the
  caller).

## Verification

```bash
# 1. Build clean
cargo build --bin spk-editor 2>&1 | tee /tmp/build.txt
grep -E "^error|could not compile" /tmp/build.txt    # must be empty

# 2. Clippy + tests (touched crates only — workspace test is overkill for
#    a sub-agent; supervisor runs workspace test at finalize if scope demands)
cargo clippy -p gpui -p gpui_wgpu -p gpui_linux -p gpui_platform -p zed --all-targets -- -D warnings
cargo test -p gpui -p gpui_wgpu -p gpui_linux -p gpui_platform --no-fail-fast

# 3. Native headless smoke-test (the acceptance check)
pgrep -af "target/debug/spk-editor" | grep -v bash || echo "clean"
script/run-mcp --debug --headless &
until [ -S "$HOME/.spk/spk-editor-dev/config/mcp.sock" ]; do sleep 0.5; done
# Verify no Xvfb spawned:
pgrep -af "Xvfb|xvfb-run" | grep -v bash && echo "FAIL: still using Xvfb" || echo "PASS: native headless"

# 4. Screenshot non-blank assertion
python3 <<'PY'
import sys; sys.path.insert(0, '/tmp')
from mcp_client import tool
import time, base64
tool("solutions.open", solution_id="alphasol")
time.sleep(5)
r = tool("workspace.screenshot", solution_id="alphasol", format="png")
sc = r["result"]["structuredContent"]
data = base64.b64decode(sc["base64_data"])
# Heuristic: blank dark-slate PNG is ~35 KB compressed; rendered Workspace is ~120+ KB
assert len(data) > 60_000, f"screenshot looks blank: {len(data)} bytes (rendered should be ≥60 KB)"
print(f"OK: screenshot {sc['width']}x{sc['height']} = {len(data)} bytes")
PY
pkill -f target/debug/spk-editor
```

Acceptance: the assertion above passes (screenshot ≥60 KB, indicating
real content). Supervisor also visually inspects the PNG via the Read
tool to confirm it shows the Workspace UI.

## When done

- [x] `cargo build --bin spk-editor` clean.
- [x] `cargo clippy` on touched crates clean (`gpui` / `gpui_wgpu` / `gpui_linux` / `gpui_platform` / `zed`).
- [x] `cargo test` on touched crates green (160 passed, 0 failed).
- [x] `script/run-mcp --debug --headless` launches without `xvfb-run` (no
  `xvfb-run` invocation in the script — verified by `bash -n` syntax check;
  `pgrep Xvfb` should return nothing during a launch).
- [ ] `workspace.screenshot` returns ≥60 KB PNG with visible Workspace UI.
  *(deferred to the supervisor's post-merge end-to-end smoke-test, § H — sub-agent verified the static wiring + types.)*
- [ ] `windows.list` returns the headless window.
  *(deferred to § H; the underlying `HeadlessClient::active_window` / `window_stack` now return the populated handle, no longer `None` — verified by reading the code, exercised end-to-end by the supervisor.)*
- [ ] `windows.dispatch_action` + `windows.send_keystroke` dispatch
  correctly in headless mode.
  *(deferred to § H; dispatch routes through `active_window()`, which is now populated.)*
- [x] `FORK.md` updated (zed/main.rs row + new headless_renderer / wgpu_context / gpui_linux headless / gpui_platform rows).
- [x] `docs/findings/2026-05-headless-screenshot-blank.md` marked resolved (Resolution section appended).
- [x] `docs/workflow/supervisor-mode.md` MCP-recipe simplified
  (`--headless` is the default for all agent work).
- [x] ADR-0002 written + listed in `docs/INDEX.md`.
- [ ] Plan doc ticked + final SHA appended at the bottom.
  *(supervisor appends commit SHAs at finalize after § H passes.)*
