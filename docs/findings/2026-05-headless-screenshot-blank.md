# Finding: `workspace.screenshot` returns a blank dark slate under `--headless` (Xvfb)

**Date:** 2026-05-15
**Status:** resolved — replaced Xvfb with native headless platform (see Resolution below + ADR-0002)

## Symptom

Launch `script/run-mcp --debug --headless` (Xvfb-backed) → open a Solution via
`solutions.open` → call `workspace.screenshot` → response is a valid PNG of
the correct dimensions (e.g. 1379x852), but visually it's a **uniform dark
slate** (Zed's `ClearColor`), no UI rendered. The same flow in `--display`
mode (or default mode with a real `$DISPLAY`) produces a fully-rendered
screenshot. The MCP `workspace.dump_visual_structure` returns the full
populated tree (TitleBar / Dock / PaneArea / Pane / Tab etc.) in **both**
modes — so the layout and element tree are correct, only the rasterised
pixels are missing in headless.

## What's known

- The editor process boots correctly under Xvfb. Log line "Rendered first
  frame" fires (from `Workspace::render`) → the gpui `Window::draw` cycle
  is invoked → `next_frame.scene` is built and swapped into
  `rendered_frame.scene`.
- `workspace.screenshot` finds the correct window
  (`find_window_for_solution` uses `cx.windows()` directly) and calls
  `Window::render_to_image()`, which forwards to
  `WgpuRenderer::render_to_image(scene)` — passing `rendered_frame.scene`.
- `WgpuRenderer::render_to_image` re-renders that scene into an offscreen
  RENDER_ATTACHMENT texture, then `copy_texture_to_buffer` + readback. No
  reliance on the on-screen swap chain having succeeded.
- Adapter selection differs between modes:
  - **`--display`** (X session with compositor GPU hint): wgpu picks
    `AMD Radeon 780M Graphics (RADV PHOENIX) (Vulkan)`. Screenshot works.
  - **`--headless`** (Xvfb, no compositor GPU hint): wgpu picks
    `NVIDIA GeForce RTX 5070 Laptop GPU (Vulkan)` (discrete > integrated by
    rule 3 of `select_adapter_and_device`). Screenshot is blank.
- Tried as workarounds — none helped:
  - `ZED_DEVICE_ID=0x1900` (force AMD) — AMD's `try_adapter_with_surface`
    **fails** under Xvfb (no DRI3); wgpu falls back to NVIDIA.
  - `WGPU_ADAPTER_NAME=llvmpipe` — wgpu's `enumerate_adapters` doesn't
    return llvmpipe in this case (log shows only 1 adapter tested).
  - `WGPU_BACKEND=gl` — ignored; still picks NVIDIA Vulkan.
  - Triggering a fresh paint via `windows.dispatch_action
    project_panel::ToggleFocus` between open and screenshot — no change.

## Hypothesis (not verified)

NVIDIA's Vulkan ICD under Xvfb (no DRI3) silently fails the
RENDER_ATTACHMENT pass for `render_to_image`'s offscreen target — the scene
is submitted but never rasterised, leaving the texture at its clear value.
The on-screen swap chain is irrelevant here (we don't present); but the
adapter itself may be miscompiled for headless use. The `libEGL warning:
DRI3 error: Could not get DRI3 device` line in startup is the only visible
breadcrumb.

A clean fix probably requires one of:
1. **Force software Vulkan (lavapipe / llvmpipe)** in headless mode —
   `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json`. Slow
   (CPU rasterisation) but reliable and side-effect-free.
2. **Implement `HeadlessRenderer`** in `gpui_linux::linux::headless::client`
   so headless mode bypasses X11 entirely (the current stub bails in
   `open_window`).
3. **Force AMD RADV** in headless via `MESA_VK_DEVICE_SELECT` or per-display
   ICD env — but AMD failed surface configuration under Xvfb, so this
   needs deeper wgpu work.
4. **Investigate why AMD RADV's `try_adapter_with_surface` fails** under
   Xvfb — if it's a wgpu surface-format mismatch, fixing it would also
   resolve the screenshot issue.

## Current workaround

Two-mode supervisor workflow:
- **`--headless`** for state ops (open/close solutions, run MCP actions,
  drive `dump_visual_structure`, dispatch keystrokes, modify buffers).
  No window pops on the user's desktop.
- **`--display`** when a screenshot is genuinely required for visual
  verification. The editor window flashes briefly on the user's desktop.

Document the trade-off in `docs/workflow/supervisor-mode.md` MCP recipe.

## Adjacent fixes that LANDED in the same session

- `script/run-mcp`: stale socket symlink + stale lock are stripped BEFORE
  any precheck, so a failing precheck doesn't leave a half-ready state
  fooling follow-up `until [ -S socket ]` waits.
- `crates/workspace/src/mcp/windows.rs`: `windows.list` and
  `find_window_by_id` treat `cx.window_stack() == Some(empty)` the same as
  `None`. Xvfb returns an empty `_NET_CLIENT_LIST_STACKING` (no WM), and
  the old `unwrap_or_else(|| cx.windows())` only fired on `None` — leaving
  `windows.list` reporting "0 windows" while editor windows were actually
  open.

## Next steps

A dedicated HEAVY phase to fix the offscreen-render path under headless
adapter selection — most likely path is option 1 (force lavapipe via env)
in `script/run-mcp --headless` when no AMD/Intel adapter is available, or
expose `--gpu-adapter=lavapipe` flag for explicit selection. ETA: 1 sub-
agent session if the env-var approach holds; longer if the wgpu surface
issue with AMD RADV needs proper investigation.

Until that lands, the two-mode workflow above is the canonical recipe.

---

## Resolution (2026-05-15)

Resolved by going option **2** from the original hypothesis: a native
`HeadlessClient` that bypasses X11 entirely. See
[ADR-0002](../architecture/decisions/0002-native-headless-platform.md)
for the full decision rationale.

Concretely:

- `gpui::PlatformHeadlessRenderer` trait ungated (no longer
  `cfg(test|test-support)`).
- New `gpui::HeadlessWindow` + `gpui::HeadlessDisplay` in
  `crates/gpui/src/platform/headless/{window,display}.rs`.
- New `gpui_wgpu::WgpuHeadlessRenderer` (`crates/gpui_wgpu/src/headless_renderer.rs`)
  + `WgpuRenderer::new_offscreen` constructor + surface-less
  `WgpuContext::new_offscreen` path with integrated-GPU adapter bias.
- `gpui_linux::headless::HeadlessClient::open_window` builds a real
  `HeadlessWindow` backed by `WgpuHeadlessRenderer`. `displays()` /
  `primary_display()` / `active_window()` / `window_stack()` all now
  return real values.
- `gpui_platform::current_headless_renderer()` ungated; Linux arm added.
- `crates/zed/src/main.rs` parses `--headless` CLI flag and passes it
  through `current_platform(headless)`.
- `script/run-mcp --headless` drops the `xvfb-run` wrapper; passes
  `--headless` to the binary instead.

The "NVIDIA Vulkan adapter under Xvfb fails the offscreen pass" failure
mode is gone — adapter selection for the offscreen renderer never consults
a compositor and biases toward integrated GPUs. No Xvfb. No DRI3.
`workspace.screenshot` exercises the same `Window::render_to_image` →
`WgpuRenderer::render_to_image` path as `--display` mode.

Plan doc: [`docs/plans/2026-05-15-headless-platform-real.md`](../plans/2026-05-15-headless-platform-real.md).
Commit SHAs: see plan-doc bottom (filled at finalize).
