# Session handoff — mobile chat polish + project-registry feature (2026-05-20)

Continuation snapshot for a fresh context. This session was **user-driven
mobile work** (sawe-mobile + supporting server changes), NOT the
autonomous-supervisor track.

## State at pause

Both repos are **clean and pushed**:
- `sawe` @ `fc73594c5d` (main, pushed).
- `sawe-mobile` @ `9c39678` (main, pushed).

Running editor binary: built **10:59 (2026-05-20)** from `fc73594c5d`.
Verify any time without bothering the user:
```bash
python3 - <<'EOF'
import socket,os,json
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(8)
s.connect(os.path.expanduser("~/.spk/sawe/config/mcp.sock"))
s.sendall((json.dumps({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"editor.capabilities","arguments":{}}})+"\n").encode())
b=b""
while True:
    c=s.recv(65536); b+=c
    for ln in b.split(b"\n"):
        ln=ln.strip()
        if ln:
            try: m=json.loads(ln)
            except: continue
            if m.get("id")==1: print(m["result"]["content"][0]["text"]); raise SystemExit
EOF
```
`editor.capabilities` now reports `binary_built_at` (mtime of the running
exe) — added this session precisely so the agent self-checks the running
build instead of accusing the user of not restarting.

Phone: adb device `A3SQUT5902000367`. Latest debug APK (commit `9c39678`)
installed. Mobile-only changes need just an APK reinstall; server changes
need a manual editor restart by the user.

## What shipped this session (committed)

**sawe `fc73594c5d`** (one commit):
- `solution_agent.reset_context` MCP tool (mobile "Reset context" — clears
  conversation, keeps id+title; cold sessions resolve a headless project).
- upload resolver preserves ResourceLink `_meta` → queued image keeps its
  csid → fixes the duplicate image bubble.
- db `column_exists` fix (was reading PRAGMA col 0 `cid` not `name` → every
  ALTER re-ran + logged a spurious WARN each startup).
- remote_control allowlist: `reset_context`.
- reqwest_client: normalize bare `socks://` → `socks5://` proxy scheme.
- editor.capabilities: `binary_path` + `binary_built_at`.

**sawe-mobile `ae87530..9c39678`**:
- DIY AnnotatedString markdown renderer (replaced multiplatform-markdown-
  renderer — its internal Loading-state caused per-emit flicker).
- `animateHeightOnly` modifier: animates content-size HEIGHT only (plain
  animateContentSize squeezed bubbles horizontally). On assistant + tool
  bubbles.
- Server-authoritative queue: drop local optimistic when its csid lands in
  a server bundle; render all bundles; namespaced synthetic key.
- Pending text send persists + rehydrates across navigation.
- Full UX-audit batch: insets/landscape safe-areas, resetSwitch Channel,
  reconnect banner (ticking countdown, status-bar inset, tappable
  re-pair), top snackbar, delete-confirm dialogs, error-message cleanup.
- Dropped server-side paths from solutions list / detail / cwd picker.

## NEXT (resume here)

Implement the **mobile project registry + member management** feature.
- Spec: `docs/superpowers/specs/2026-05-20-mobile-project-registry-design.md`
- Plan: `docs/superpowers/plans/2026-05-20-mobile-project-registry.md`
  (both LOCAL — `docs/superpowers` is gitignored; same machine so fine).

Design was brainstormed + approved with the user. Execution mode chosen:
**inline** (needs the agent's socket / build / device access; subagent
coordination is awkward here). Start at **Task 1** (server
`solutions.add_empty_member` MCP tool) and go task-by-task per the plan,
building + verifying on device between UI tasks.

Feature in one line: mobile can create a Solution with multi-selected
registry projects (optional — empty solution allowed), add registry
projects to an existing Solution, and create new empty (non-git) projects
— mirroring the desktop "+" picker. Git-add stays desktop-only. The
"clean project" path is the existing sync `SolutionStore::add_empty_member`
(empty dir, no git init), just needs an MCP wrapper + allowlist + UI.

## Gotchas / lessons (don't relearn these)

- **A new `solution_agent.*` / `solutions.*` MCP tool the phone calls needs
  TWO registrations: the MCP tool itself AND a `remote_control`
  `allow_list::translate` entry** (+ round-trip test row). The phone routes
  through remote_control's allowlist, not the local MCP socket. Missing the
  allowlist entry → "method not found" even though the tool exists. This
  burned a full rebuild cycle this session.
- Events the phone consumes also need `allow_list::should_forward_event`.
- Server release builds: `cargo build --bin sawe --profile
  release-fast` (~5 min). The user runs `target/release-fast/sawe`;
  socket `~/.spk/sawe/config/mcp.sock` (NOT `-dev`). Build in the
  background; verify via the capabilities snippet above.
- Mobile build+install: `cd sawe-mobile && ./gradlew :app:assembleDebug`
  then `adb -s A3SQUT5902000367 install -r app/build/outputs/apk/debug/app-debug.apk`.
  The phone drops off adb frequently — re-ask the user to reconnect.
- Inspect live session/queue state over the socket with
  `solution_agent.list_sessions` / `get_session` (this session's chat is
  session `8l84aoiw`, title "sawe").
- `git push` in the mobile repo once reported "up-to-date" with a stale
  `@{u}`; use `git push origin main` explicitly.
- No `Co-Authored-By` in commits (user rule). Fork lands directly on `main`.
