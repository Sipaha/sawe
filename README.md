# Sawe

**Sawe — Solution-Aware Workspace Editor.** An AI-native, multi-project IDE forked from [Zed](https://zed.dev). Free, open-source, no telemetry, no cloud sign-in. Maintained by **Simonov Pavel** ([@Sipaha](https://github.com/Sipaha)).

*"Zed" is a trademark of Zed Industries; Sawe is not affiliated with or endorsed by Zed Industries. Internal identifiers (binary name `spk-editor`, config dirs, app bundle name) are mid-migration from the previous brand and still reflect the old name in code paths below.*

Sawe is built around two ideas:

- **Solutions** — a multi-project workspace abstraction: group N git repos as worktrees in one editor window (like an "IDEA Solution" spanning multiple projects).
- **Solution-scoped AI** — multiple [Claude Code](https://claude.ai/code) agent sessions per Solution (via the Agent Client Protocol), each a first-class pane item that understands the whole Solution rather than individual files. AI auth uses your existing Claude subscription via `~/.claude/`; no API keys.

Sawe does **not** operate any of the Zed Industries cloud services that upstream Zed uses by default:

- No telemetry is sent.
- No auto-update channel — the binary is built from source.
- No Zed account / sign-in.
- No collab / channels / chat / voice.
- No Sentry crash uploads (panics are still logged locally).
- No native Zed cloud LLM provider — AI features go through the external `claude` subprocess.
- The Zed extension registry on `zed.dev` **is** still used for browsing and installing extensions.

<<<<<<< ours
- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))
=======
## What this fork adds beyond rebrand
>>>>>>> theirs

- **Solutions** — multi-project workspace abstraction. Group N remote git projects into a single editor window with all members mounted as worktrees. Daily work on tightly-related repos (e.g. a parent + microservices) without juggling windows. Catalog of remote URLs is shareable across machines via `~/.config/spk-editor/solutions.json`.
- **Solution-scoped AI sessions** — N parallel Claude Code-style chat sessions per Solution, each a first-class pane item that can sit next to the code being changed (split view). Long tasks keep running after you close the window — the editor pings you with an OS notification when a turn completes (5 min threshold). Auth uses your `claude` subscription via `~/.claude/`; no API keys.
- **Embedded MCP server** — running `spk-editor` exposes a Unix-socket JSON-RPC API at `~/.config/spk-editor/mcp.sock` (58 tools across `editor.*`, `windows.*`, `solutions.*`, `catalog.*`, `solution_agent.*`, `workspace.*`, `project.*`, `diagnostics.*`). Lets external agents drive the editor for end-to-end automation without a human in the loop.

For the full list of fork-local crates and architectural decisions, see [`FORK.md`](./FORK.md).

## Building from source

Same toolchain requirements as upstream Zed (recent stable Rust, OS-specific dependencies — see upstream's README for the current list).

**Linker requirement (fork-local):** this fork pins [`mold`](https://github.com/rui314/mold) for `x86_64-unknown-linux-gnu` and `lld` for `aarch64-unknown-linux-gnu` in `.cargo/config.toml` — install before first build:

```sh
# Debian / Ubuntu
sudo apt install mold      # or `lld` on aarch64
# Other distros: prebuilt binaries at https://github.com/rui314/mold/releases
```

<<<<<<< ours
Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.
=======
See [`FORK.md`](./FORK.md) decision #15 for rationale (~5-10× faster link, lower peak RAM vs system `ld`). After cloning:
>>>>>>> theirs

```sh
cargo build --release
```

The binary lands at `target/release/spk-editor` (the cargo crate name is `zed` for upstream-merge friendliness, but the bin name is overridden to `spk-editor`).

Bundling helpers per platform:

```sh
script/bundle-linux         # produces a tarball
script/bundle-mac           # produces SpkEditor.app (display name "SPK Editor")
script/bundle-windows.ps1   # produces the Inno Setup installer
```

## Running unsigned binaries

Sawe binaries are **not signed or notarized**. To run on each OS:

- **Linux**: no extra step.
- **macOS**: Gatekeeper will refuse to launch. Right-click the app → Open, or run `xattr -dr com.apple.quarantine /Applications/SpkEditor.app`.
- **Windows**: SmartScreen will warn. Click "More info" → "Run anyway".

If you want signing, set up your own certificates and wire them through `script/bundle-mac` / `script/bundle-windows.ps1` (see `SPK_EDITOR_SIGN` env var).

## Icon

The shipped icon is a placeholder ('S' on a blue background). To regenerate after editing the geometry / colors in `script/generate-placeholder-icons.sh`, run (requires ImageMagick):

```sh
script/generate-placeholder-icons.sh
```

Replace with proper artwork by overwriting the files at `crates/zed/resources/app-icon*.png`, `crates/zed/resources/Document.icns`, and `crates/zed/resources/windows/app-icon*.ico`.

## Issues

Bug reports, feature requests, and questions: <https://github.com/Sipaha/spk-editor/issues>.

For upstream Zed bugs (anything not specific to this fork), please file directly at <https://github.com/zed-industries/zed>.

## License

Sawe inherits Zed's licensing unchanged:

- The editor (`crates/zed`) is licensed under **GPL-3.0-or-later**.
- The collab server (`crates/collab*`) is licensed under **AGPL-3.0** (kept in the tree but not built / run by default in spk-editor).
- The shared libraries (`gpui`, etc.) are licensed under **Apache-2.0**.

See `LICENSE-GPL`, `LICENSE-AGPL`, `LICENSE-APACHE`. All `Copyright Zed Industries, Inc.` notices are preserved per GPL §5(a). The legal documents inherited from upstream Zed are in `legal/upstream-zed/`; they describe Zed Industries' hosted services and **do not apply to Sawe builds** (which operate no service infrastructure).

License-compliance for third-party dependencies is enforced by `cargo-about` (see `script/licenses/`). To re-check locally:

```sh
cargo install cargo-about
cargo about generate -c script/licenses/zed-licenses.toml templates/about.hbs > /dev/null
```

## Upstream

This fork is periodically merged from <https://github.com/zed-industries/zed>. Internal identifiers (cargo crate `zed`, modules, types) are kept unchanged from upstream to minimize merge friction; only user-visible identity (binary name, app bundle id, URL scheme, config directories, About dialog) is rebranded.

## Acknowledgements

All credit for the editor core — rendering, buffer, language services, GPUI — goes to **Zed Industries, Inc.** and the upstream Zed contributors. Sawe builds a substantial workflow layer on top of that core: multi-project Solutions, first-class AI sessions, embedded MCP server, headless platform, run configurations, remote control, and the surrounding service-detachment.
