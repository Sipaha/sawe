# R-1: Remote Control settings + status-bar widget + modal panel (UI only)

**Status:** complete (2026-05-15). Sub-agent landed `ee50a9508e`,
`5faad00546`, `6c97a48a8a` (merged as `<this-finalize>`). 16 unit
tests passing across the 2 new fork-owned crates. Supervisor's § H
smoke-test: status-bar item "Remote Control" + red LED renders in
the bottom-right; clicking opens the modal with all 4 sections
(Server address + Detect/Save, Port 7777, Status row, Authorized
clients with inline Add). Screenshot:
[`2026-05-15-remote-control-R1-screenshot.png`](2026-05-15-remote-control-R1-screenshot.png).
Sub-agent deviated from plan in three (acceptable) ways: inline
Add-client flow instead of sub-modal; `OsRng.try_fill_bytes` via
rand 0.9's `TryRngCore` (plan said `RngCore`); UI guard blocks
enabling toggle until address is set (modal-level, not store-
level).
**Estimated:** 1 sub-agent session, ~1.5–2 h, worktree-isolated
**Goal:** First phase of the Remote Control arc — settings model +
persistence + status-bar widget + modal panel UI scaffolding. **No
network listener yet** (R-2), **no QR codes yet** (R-2 or R-3), **no
encryption** (R-2). Just the local UI surface + state-management
plumbing so the user can interact with the empty model.

## Context

Multi-phase arc per [`docs/plans/2026-05-15-remote-control.md`](2026-05-15-remote-control.md).
This file is R-1's plan, the smallest viable first-phase. R-2 onward
adds the actual server + auth + QR.

## Scope

### A. New crate `crates/remote_control` (settings + state model)

Pattern after the existing `crates/run_config` crate (fork-owned).

- `RemoteControlSettings { server_address: Option<String>, server_port: u16, enabled: bool, clients: Vec<AuthorizedClient> }`.
- `AuthorizedClient { name: String, secret_base64: String, created_at: chrono::DateTime<chrono::Utc> }`.
- JSON persistence at `~/.config/sawe/remote-control.json` (use the existing `paths::remote_control_settings_file()` helper if it exists; else add one to `crates/paths`).
- Defaults: `server_port: 7777`, `enabled: false`, empty clients list.
- `RemoteControlStore` GPUI global (mirrors `SolutionStore` / `RunConfigStore`) with `update_settings`, `add_client(name) -> Result<&AuthorizedClient>` (generates a random 32-byte secret via `rand::rngs::OsRng`), `remove_client(name)`, `set_enabled(bool)`, `set_address(...)`, `set_port(u16)`.
- Emits `RemoteControlStoreEvent::Changed` on every mutation.
- NO server listener — `set_enabled(true)` just sets the bit.
- NO MCP tools yet (those land in R-2 / R-4 alongside the protocol).

### B. New crate `crates/remote_control_ui` (status bar + modal)

Pattern after `crates/run_config_ui`.

- **Status bar widget** (`RemoteControlStatusItem`): right-aligned, colored
  dot (red when `!enabled`, green when `enabled`) + text "Remote Control".
  Click opens the modal panel.
- **Modal panel** (`RemoteControlModal`): a `ModalView` with sections:
  1. **Server address** row: text input + "Detect" button. Detect's
     `on_click` calls `fetch_public_ip()` (HTTP GET `https://ifconfig.me`
     via the existing HTTP client in the editor; the
     `http_client::HttpClient` already in use by other crates is fine).
     Result populates the input.
  2. **Port** row: numeric input, default 7777.
  3. **On/Off toggle** row: a styled button that flips
     `RemoteControlStore::set_enabled`. Disabled when settings are
     incomplete (no address).
  4. **Clients** section: list of `AuthorizedClient`s with name + secret
     (display secret as monospaced 16-char prefix + "…"). "Add client"
     button below: opens a small text-input sub-modal asking for the
     name; on confirm calls `add_client(name)`. **No QR code** in R-1 —
     the "Show QR" button is rendered but its `on_click` shows a
     "TODO R-1.5: QR rendering" toast. NO remove-client UI in R-1 (the
     store method exists but isn't surfaced — that's a polish item).
- Wire `RemoteControlStatusItem` into the workspace status bar in
  `crates/zed/src/zed.rs::initialize_status_bar` (or wherever the
  existing status items are registered — look for `SolutionsStatusItem`
  as a sibling).

### C. CLAUDE.md / FORK.md / paths

- `FORK.md` rows for the two new crates (`crates/remote_control`,
  `crates/remote_control_ui`) under § "Fork-only crates".
- `FORK.md` row for `crates/zed/src/zed.rs` if not already listed (it
  is — the agent_panel toggle row covers it).
- `FORK.md` row for `crates/paths/src/paths.rs` if you add the path
  helper there (already listed — `.sawe` rename + run_configurations_file
  + local_run_configurations_file_relative_path).
- `.rules`: no new tool catalog entries yet (R-1 doesn't ship MCP tools).

### D. Tests

- Unit-test `RemoteControlSettings` round-trip (JSON serde).
- Unit-test `AuthorizedClient` secret generation (length, base64
  decodability, non-collision across two calls).
- Unit-test the status item's color flipping based on `enabled`.
- DO NOT inline end-to-end MCP tests — supervisor handles § H.

### E. Documentation

- Tick acceptance items in `docs/plans/2026-05-15-remote-control-R1.md`
  if it exists in your worktree (it may not — that's the staleness
  trap; the plan is inlined in the dispatch prompt).
- No new ADR for R-1 (architectural decisions land at R-2 when the
  protocol is picked).

## Out of scope

- **Server listener** — R-2.
- **QR code rendering** — R-1.5 or R-2.
- **Client removal UI** — polish, defer.
- **MCP `remote.*` tools** — R-2 / R-4.
- **Android client** — R-5 onward.
- **uPnP / NAT traversal** — undecided, R-2 design call.
- **Settings imported via VSCode** — out of scope; not relevant to a
  local-only remote-control surface.

## Verification

```bash
cd <worktree>
cargo build --bin sawe 2>&1 | tee /tmp/build.txt
grep -E "^error|could not compile" /tmp/build.txt   # must be empty
cargo clippy -p remote_control -p remote_control_ui --all-targets -- -D warnings
cargo test -p remote_control -p remote_control_ui --no-fail-fast
```

Supervisor § H smoke-test post-merge:
- `script/run-mcp --debug --headless &`
- `workspace.dump_visual_structure` should now show a new
  `RemoteControlStatusItem` in the StatusBar children (red dot,
  "Remote Control" label).
- Click the status item via `windows.click_id` → modal panel appears
  in the dump.
- `workspace.screenshot` confirms the modal layout (4 sections + "Add
  client" button + empty clients list).

## When done

- [ ] `cargo build --bin sawe` clean.
- [ ] `cargo clippy` on touched crates clean.
- [ ] `cargo test` on touched crates green.
- [ ] `RemoteControlStatusItem` visible in the status bar.
- [ ] Clicking the status item opens `RemoteControlModal`.
- [ ] Modal renders all 4 sections (address / port / on-off / clients).
- [ ] "Detect" button populates address with the result of
  `https://ifconfig.me` (network-permitting — log + leave empty on
  failure rather than panic).
- [ ] Add-client by name works; secret is generated + stored.
- [ ] Settings persist across editor restarts
  (`~/.config/sawe/remote-control.json`).
- [ ] FORK.md updated with the two new crates.
- [ ] Plan doc ticked + final SHA appended.
