# ADR-0002: Native headless GPUI platform for autonomous agent driving

**Status:** accepted
**Date:** 2026-05-15
**Deciders:** Pavel Simonov (@Sipaha)
**Related:** [`docs/findings/2026-05-headless-screenshot-blank.md`](../../findings/2026-05-headless-screenshot-blank.md) (resolves), [`docs/plans/2026-05-15-headless-platform-real.md`](../../plans/2026-05-15-headless-platform-real.md), [ADR-0001](0001-fork-philosophy.md)

---

## Context

`script/run-mcp` exists so an autonomous agent can drive a live spk-editor
window over the embedded MCP socket without a human attached. From day one
that's worked by wrapping the editor in `xvfb-run`, which spins up a virtual
X server and presents the swap-chain frames to it. Two pain points
accumulated:

1. **`workspace.screenshot` returns blank under Xvfb.** The offscreen
   `WgpuRenderer::render_to_image` path renders into a private texture (not
   the swap chain), but on machines where Xvfb has no DRI3 the wgpu adapter
   selection silently picks NVIDIA's proprietary stack, which then fails the
   `RENDER_ATTACHMENT` pass for that offscreen texture without surfacing an
   error. The PNG comes out a uniform clear-color slate. Documented in
   `docs/findings/2026-05-headless-screenshot-blank.md`. Existing
   workarounds (`ZED_DEVICE_ID`, `WGPU_ADAPTER_NAME`, `WGPU_BACKEND=gl`)
   either don't apply, get ignored, or trade the bug for a different one
   (AMD `try_adapter_with_surface` failing under Xvfb without DRI3).

2. **Xvfb is a fragile system dep.** `xvfb-run` is ~3 MB plus a transient
   process per launch, breaks under `flatpak` / `docker` without extra
   work, and the "is xvfb-run installed?" precheck is a non-trivial
   chunk of `script/run-mcp` that fails at the worst possible time (after
   someone's already configured everything else).

The fork already had most of the pieces for a fully native headless
platform: `gpui::PlatformHeadlessRenderer` trait, `gpui::TestWindow`
(a complete `PlatformWindow` impl), `gpui_macos::MetalHeadlessRenderer`,
`gpui_linux::headless::HeadlessClient` stub, `gpui_platform::current_platform(headless)`.
All of it was test-support-gated and the Linux pieces stubbed out. So the
work was "finish wiring what's already there" rather than "design a new
platform."

---

## Options considered

### Option A — Keep Xvfb, harden adapter selection

Stay on `xvfb-run` and add env-var workarounds (`VK_DRIVER_FILES`
forcing lavapipe, `MESA_VK_DEVICE_SELECT` pinning the AMD RADV adapter,
explicit `WGPU_BACKEND=gl` fallback) until offscreen rendering works
reliably on the agent's box.

- **Pro:** smallest diff. No new code paths.
- **Con:** each workaround is per-machine. Lavapipe is software-rasterised
  so the screenshot path becomes 5-10× slower. AMD RADV under Xvfb has a
  `try_adapter_with_surface` failure that's a wgpu-side bug we'd have to
  carry around.
- **Con:** doesn't kill the Xvfb dep, just papers over its current failure
  mode. Future Xvfb / driver / wgpu interactions will surface new bugs in
  the same place.

### Option B — Native `HeadlessClient` via offscreen wgpu (THIS)

Wire `gpui_linux::headless::HeadlessClient::open_window` to construct a
real `gpui::HeadlessWindow` backed by a new `gpui_wgpu::WgpuHeadlessRenderer`
(offscreen `WgpuRenderer` — no `wgpu::Surface`, just device+queue and the
existing `render_to_image` path). Ungate `gpui::PlatformHeadlessRenderer`
and `gpui_platform::current_headless_renderer()`. Add a `--headless` CLI
flag on the editor binary; pass it through `current_platform(headless)`.
Drop the Xvfb wrapper in `script/run-mcp --headless` mode.

- **Pro:** zero X dependency. Works under Wayland-only systems, Docker
  with no X11, headless CI, etc. No new system packages to install.
- **Pro:** adapter selection is deterministic and surface-free — the
  offscreen path biases toward integrated GPUs (AMD RADV > NVIDIA's
  proprietary stack for offscreen rendering on Linux today) and never
  consults a compositor hint, so the "discrete-NVIDIA-with-no-Xvfb-DRI3"
  failure mode is gone.
- **Pro:** `workspace.screenshot` is exercising the exact same code path
  as the on-screen `--display` mode (`Window::render_to_image` →
  `WgpuRenderer::render_to_image`), just without the Surface. If
  screenshots work in `--display` they work in `--headless`.
- **Con:** more code surface in `gpui_wgpu` and `gpui_linux`. ~150 LOC
  of additive wiring + a small refactor of `WgpuRenderer` to make
  `surface` optional.
- **Con:** macOS still relies on `MetalHeadlessRenderer`, which is
  `test-support`-gated upstream. Out of scope for this phase — see
  Consequences.

### Option C — Open the X11 window, then hide it after first paint

Use the existing X11 backend, override the window's `_NET_WM_STATE` to add
`_NET_WM_STATE_HIDDEN` right after `open_window`. Screenshots would
exercise the live swap chain.

- **Pro:** smallest delta to the existing platform code.
- **Con:** still requires `$DISPLAY`. Doesn't solve the "no X server"
  case at all — pure regression of agent-from-Docker / agent-in-CI use
  cases.
- **Con:** WMs handle hidden windows inconsistently (some skip render
  invalidation; some unmap the window entirely). Painting into a
  WM-managed window we don't want shown is fighting the WM.

### Option D — Per-element headless mock

Bypass GPUI's window layer entirely. Build a parallel rendering pipeline
that walks the element tree and produces synthetic frames without going
through `Scene` / wgpu at all.

- **Pro:** completely decoupled from GPU drivers.
- **Con:** what the agent then sees is no longer "what users see" — the
  text-rendering pipeline alone is a thousand lines of `cosmic-text` /
  glyph atlas / subpixel positioning that we'd have to either duplicate
  or stub. The screenshot would lie about what the editor actually
  renders.

---

## Decision

We picked **Option B** — native headless platform via offscreen wgpu.

Load-bearing reasons:

1. **Zero X dependency** removes a whole class of failures (no Xvfb
   missing, no display server incompatibility, no DRI3-flavour-of-the-week
   bugs).
2. **Adapter selection is deterministic** — the offscreen path enumerates
   adapters once, sorts by `(IntegratedGpu > Discrete > ...)`, and never
   takes a compositor hint. The "NVIDIA-discrete-fails-headless" bug
   disappears structurally.
3. **Screenshot path is identical to on-screen**. `Window::render_to_image`
   → `WgpuRenderer::render_to_image` runs the same pipelines, the same
   atlas, the same paths. If `--display` mode renders correctly, so does
   `--headless`. No more "works in `--display`, blank in `--headless`"
   asymmetry to debug.
4. **Single concrete `WgpuHeadlessRenderer`** keeps the surface tight —
   no enum dispatch, no trait-object inside-the-adapter. macOS keeps its
   own `MetalHeadlessRenderer`; the `PlatformHeadlessRenderer` trait is
   the only abstraction needed.
5. **CLI flag, not env var**. `--headless` shows up in `ps aux` and
   `--help`. Env vars are hidden state that ages badly.

The decision is **load-bearing** for the supervisor-mode MCP smoke-test
recipe (the two-mode table in `docs/workflow/supervisor-mode.md`
simplifies — `--headless` is now the default for all agent work,
`--display` only when a human wants to watch the window).

---

## Consequences

### Positive
- `workspace.screenshot` works identically in `--headless` and `--display`
  modes. The two-mode supervisor recipe collapses to one mode for the
  90%-case (`--headless`).
- `script/run-mcp` drops the Xvfb precheck + the `xvfb-run` wrapper, both
  failure modes go away.
- `windows.list` / `windows.dispatch_action` / `send_keystroke` /
  `send_text` / `click_at` work uniformly. Active-window is now
  deterministic (the last `open_window`ed handle) — `HeadlessClient`
  was returning `None` before, silently dropping any
  `App::dispatch_action` call that depended on `active_window`.
- Linux + FreeBSD symmetric — both use `WgpuHeadlessRenderer`. macOS keeps
  its existing `MetalHeadlessRenderer`. Windows TBD.

### Negative
- `gpui` proper gains a small amount of surface: `HeadlessWindow`,
  `HeadlessDisplay` (~250 LOC). `WgpuRenderer::resources::surface` is now
  `Option<wgpu::Surface<'static>>` instead of `wgpu::Surface<'static>`;
  six call-sites in `wgpu_renderer.rs` had to be `if let Some(surface)`.
- The macOS `MetalHeadlessRenderer` impl is still `test-support`-gated.
  A non-test macOS build's `current_headless_renderer()` returns
  `None`. A future session that needs a native headless macOS path has
  to ungate `MetalHeadlessRenderer` (no other blocker, just a `cfg`
  removal — see `crates/gpui_macos/src/metal_renderer.rs` line ~1682).
- Single-window only today. The fork's UX opens one Solution window at
  a time, so `HeadlessClient` tracks a single-element window stack.
  Multi-window headless would need the stack semantics extended (z-order,
  active-on-focus-cycle, etc.) — straightforward but out of scope.

### Reversibility
- **Reverting to Xvfb** is one revert commit per code commit + `git
  revert` on `script/run-mcp`. The pre-existing `xvfb-run` infrastructure
  isn't deleted from the build host. `--headless` CLI flag can stay (the
  binary defaults to on-screen platform when the flag is off).
- **Adding a Windows arm** is one branch in `gpui_platform::current_headless_renderer()`:
  build a `WindowsHeadlessRenderer` (wgpu-DX12-backed) following
  `WgpuHeadlessRenderer`'s pattern. No data format / contract changes.

---

## How to apply

For **future sessions touching the platform layer**:

- The `PlatformHeadlessRenderer` trait abstracts platform-specific
  offscreen rendering. New platforms (Windows, etc.) implement the trait
  and slot into `gpui_platform::current_headless_renderer()`.
- Don't re-add Xvfb wrapping to `script/run-mcp`. The Xvfb path was the
  *workaround*; the native headless platform is the fix.
- When extending `HeadlessClient::open_window` for multi-window: keep
  `WgpuHeadlessRenderer` instances per-window (each `Box::new` is cheap
  — adapter selection runs once, device/queue/pipelines per renderer).
  Don't share a single renderer across windows; the atlas state is
  per-window.
- If you need `workspace.screenshot` to work on macOS-non-test builds,
  ungate `MetalHeadlessRenderer` in `crates/gpui_macos/src/metal_renderer.rs`
  (~line 1682). The trait is already ungated.
- `gpui_linux::headless::HeadlessClient` directly constructs
  `WgpuHeadlessRenderer` (avoids `gpui_linux → gpui_platform` dep
  cycle that would happen if it called
  `gpui_platform::current_headless_renderer`). Keep this pattern; the
  alternative inversion would force `gpui_platform` to be split.

Files that encode this decision:

- `crates/gpui/src/platform.rs` — `PlatformHeadlessRenderer` trait
  (ungated), `headless` module re-exports.
- `crates/gpui/src/platform/headless/{window,display}.rs` — `HeadlessWindow`
  + `HeadlessDisplay` primitives.
- `crates/gpui_wgpu/src/headless_renderer.rs` — `WgpuHeadlessRenderer`.
- `crates/gpui_wgpu/src/wgpu_renderer.rs` — `WgpuRenderer::new_offscreen`
  + `surface: Option<wgpu::Surface<'static>>`.
- `crates/gpui_wgpu/src/wgpu_context.rs` — `WgpuContext::new_offscreen` +
  `instance_offscreen`.
- `crates/gpui_linux/src/linux/headless/{client,display}.rs` —
  `HeadlessClient` wiring.
- `crates/gpui_platform/src/gpui_platform.rs` —
  `current_headless_renderer()` (Linux arm).
- `crates/zed/src/main.rs` — `--headless` CLI flag.
- `script/run-mcp` — drops `xvfb-run` from the `--headless` path.

---

## Notes

- The single-window assumption is intentional, not lazy: the fork's
  workflow opens one Solution window at a time. Multi-window headless
  would be a follow-up plan (separate spec doc) when a workflow actually
  needs it.
- Adapter selection ordering (`Integrated > Discrete`) is the **inverse**
  of the on-screen ordering. For on-screen we want a discrete GPU when
  there is one (better perf for the swap chain). For offscreen we want
  the integrated GPU because it's typically more reliable on Linux today
  (AMD RADV is a less rough Vulkan stack than NVIDIA's proprietary one
  for offscreen `RENDER_ATTACHMENT` operations). This bias is local to
  `WgpuContext::new_offscreen` — the on-screen `select_adapter_and_device`
  is unchanged.
