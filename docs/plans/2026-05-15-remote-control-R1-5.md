# R-1.5: QR code rendering for authorized clients

**Status:** complete (2026-05-15). Sub-agent landed `d9fa51c0dd` —
clean single commit, 9/9 tests in `remote_control_ui` (7 new QR tests
+ 2 pre-existing). Sub-agent caught the worktree-staleness trap
(R-1's commits weren't in their base) and **rebased onto current
main before starting** — this is the canonical workaround; future
dispatches should expect sub-agents to do this proactively. Two
implementation deviations from plan: (1) `urlencoding` crate
preferred over `percent-encoding` (already in workspace deps, simpler
API); (2) `gpui::svg()` only loads SVG from a file path, not an
inline string, so QR is rendered as a `div`-per-module grid instead.
**Estimated:** 1 sub-agent session, ~45 min, worktree-isolated
**Goal:** Replace the "TODO R-1.5: QR rendering" toast from R-1 with an
actual QR code popover. Each authorized client's "Show QR" button opens
a popover containing a rendered QR encoding the
`spk-remote://<addr>:<port>?secret=<base64>&client=<name>` URL.

## Context

R-1 (commit `5735a4c4dc` finalize) shipped the Remote Control panel with
"Show QR" buttons that fire a `TODO` toast. This phase puts a real QR
behind those buttons. R-2 (server listener) doesn't depend on this — but
adding it now keeps the UI surface coherent for screenshots and lets the
user actually test the auth-by-secret flow end-to-end once R-2 ships.

## Scope

### A. Add `qrcodegen` dep to `crates/remote_control_ui`

`qrcodegen` (no-std-friendly, BSD-licensed, well-maintained,
upstream-Zed already uses it in some crates — check first). Run
`cargo add qrcodegen -p remote_control_ui --features <whatever>`. If
upstream-Zed already has it, lift the version from there.

### B. QR popover view

New file `crates/remote_control_ui/src/qr_popover.rs`:
- `QrPopover { client_name, secret_base64, address, port }` — held by
  the `RemoteControlModal` while it's showing a QR.
- Render: `v_flex` with the client name as header, the QR rendered as
  inline SVG (qrcodegen produces module booleans; emit an `svg()`
  GPUI element with viewBox = qr.size() and `<rect>` per dark module);
  below the QR show the encoded URL as monospaced selectable text.
- Open as a `PopoverMenu` / nested modal — pick whichever pattern is
  smaller. The `solution_picker_dropdown` uses `ModalView`; reuse.

### C. Wire into `RemoteControlModal`

In `crates/remote_control_ui/src/modal.rs` — replace the existing "Show
QR" `on_click` Toast with a `cx.toggle_modal(...)` (or
`workspace.toggle_modal`) call that mounts the new `QrPopover`.

The encoded URL is built from `RemoteControlSettings`: if `address` or
`port` are empty/invalid, show a small dimmed message in the popover
explaining "Set the server address first" instead of generating a QR
of a half-formed URL.

### D. Tests

- `qr_popover.rs` — unit test: encoding the URL produces a non-empty
  QR module grid; round-trip the URL through a base64-decode of the
  secret param (the secret IS base64 inside the URL — make sure the
  URL escaping doesn't break it; if it does, switch to URL-safe
  base64 (`base64::engine::general_purpose::URL_SAFE_NO_PAD`)).

### E. Documentation

- Tick acceptance items in the plan doc (inline below if the path
  isn't in your worktree per the staleness trap).
- No new ADR.
- FORK.md: no new rows (both crates already listed).

## Out of scope

- Click-to-copy on the URL — polish, defer.
- QR-resolution config — qrcodegen picks size; fine.
- Real client-revocation flow — R-3 polish.

## Verification

```bash
cargo build --bin sawe 2>&1 | tee /tmp/build.txt
grep -E "^error|could not compile" /tmp/build.txt   # must be empty
cargo clippy -p remote_control_ui --all-targets -- -D warnings
cargo test -p remote_control_ui --no-fail-fast
```

Supervisor § H:
- `script/run-mcp --debug --headless &`
- Open AlphaSol, open the Remote Control modal, add a client, click
  Show QR → popover with rendered QR appears in `dump_visual_structure`
  + screenshot shows actual QR pattern (not a Toast).

## When done

- [ ] cargo build / clippy / test clean.
- [ ] `qrcodegen` dep added.
- [ ] `QrPopover` renders a QR + the URL.
- [ ] "Show QR" replaces the Toast stub.
- [ ] Plan doc ticked + final SHA appended.
