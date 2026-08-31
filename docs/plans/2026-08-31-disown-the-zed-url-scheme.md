# Plan — disown `zed://`, and stop shipping another product's names

Status: in progress (2026-08-31)
Owner: autonomous supervisor session 2026-08-31b
Ruling by: the maintainer

## The ruling

**`zed://` is a different product's external contract and this fork disowns it.**
Sawe must not try to open `zed://` links and must not register itself with the OS as
a handler for another editor's scheme.

CLAUDE.md §3 locks `sawe://` as the fork's URL scheme and names `.zed_server` /
`.zed_wsl_server` as the *only* preserved upstream identifiers. Earlier in this
session `915bd2b73f` made `sawe://` a **alias of** `zed://` on the reasoning that
`zed://` was preserved too. That reasoning was invented, not read, and this plan
reverses its direction: `sawe://` becomes canonical and `zed://` stops being ours.

## What the recon established

Full inventory: `.superpowers/sdd/s2026-08-31b/recon-zed-identifiers.md`.

**Disowning is free.** `zed://extension/` — the one arm that looked like it might
carry a *kept* feature, since the extension registry on zed.dev is deliberately
retained — has **no in-repo producer**. The registry flow installs over HTTP
(`extension_host.rs:603/848/893`, `/extensions/{id}`, `/extensions/{id}/download`)
and never sees a URL. Same for `zed://file`, `ssh`, `git/clone`, `git/commit/`,
bare/`open` and `agent`. The arms we actually feed are ours end to end: `schemas/`
(round-trip), `settings/` (Copy Link → clipboard), `skill` (share link), and
`agent/shared/`, which is already unreachable — it needs the unregistered AgentPanel
*and* the disabled collab RPC.

**The OS registration is live and reaches further than the palette.**
`install_cli::register_zed_scheme` registers `ZED_URL_SCHEME = "zed"` and is called
from two places: the command-palette action `cli: register zed scheme`, and —
unconditionally — the last line of **`File → Install CLI`**
(`install_cli_binary.rs:97`). On macOS ≥12 that is
`NSWorkspace.setDefaultApplication`: Sawe takes `zed://` system-wide. On Linux and
Windows GPUI returns `Err("register_url_scheme unimplemented")`, so the menu item
routinely shows the user **"Error registering zed:// scheme"** — an error message
about another product, from a menu item that otherwise succeeded.

The same code path symlinks **`/usr/local/bin/zed`** and toasts ``Installed `zed`
to …``, while CLAUDE.md §3 locks the CLI binary name as `sawe`. We put another
product's binary name in the user's `PATH`.

**Packaging is otherwise clean**: `sawe.iss` registers only
`HKCU\Software\Classes\sawe`, `sawe.desktop.in` only `x-scheme-handler/sawe`, and
cargo-bundle gets `osx_url_schemes = ["sawe"]` on all four channels.

**No migration concern.** Everything on disk under a `zed-` name is ephemeral: one
live socket, stale per-pid crash sockets, a `/tmp` symlink. The one off-disk orphan
is the Linux keyring label `zed-github-account` — one re-authentication.

**There is already a guard for this policy, with a hole.** `paths.rs:626/639`
asserts that the data and config directories must not mention `zed` — and
`zed-<channel>.sock` escapes it because the name is appended by a caller in another
crate.

## Decisions

**An incoming `zed://` link is named, not swallowed.** The fork tells the user it is
a Zed link rather than silently ignoring it. Silent loss is the exact defect class
this session spent itself removing. *Reversible in one branch if it proves noisy.*

**Generation and parsing move in the same change.** We still generate
`zed://schemas/…` ourselves; deleting the parse arms alone would break settings
schemas at the halfway point. The recon lists nine sites that move together —
`main.rs:1679`, `settings_ui.rs:1428`, `agent_skills.rs:803`,
`open_url_modal.rs:30,57`, `cli/main.rs:37`, `hyperlinks.rs:22` and the rest.

**On-disk names are renamed, not aliased.** `zed-<channel>.sock`, `zed-cli://` and
`zed-crash-handler-<pid>` are ours at both ends and nothing outside can know them,
so there is no compatibility window to keep. Close the `paths.rs` guard hole in the
same change so the class cannot recur.

**Out of scope, recorded not fixed:** the Windows shell appx ships under
`Name="ZedIndustries.Zed"` / `<DisplayName>Zed</DisplayName>` / verb `OpenWithZed`,
and its identity does not match the `Sipaha.Sawe_…` the uninstaller removes — a live
bug, Windows-only, untestable from this machine. Likewise
`util::get_zed_cli_path()`, which looks for `../bin/zed` this fork never ships and
is therefore already broken rather than merely misnamed.

## Phases

1. **Contract.** Stop registering the `zed` scheme; remove the palette action and the
   unconditional call from `Install CLI`; symlink `sawe`, not `zed`; fix the toast
   and the "links will now open in" message.
2. **Routing.** Make `sawe://` canonical — reverse `915bd2b73f`'s direction — moving
   generation and parsing together; drop `zed://` from `is_url_scheme`; add the
   spoken message for an incoming `zed://`.
3. **On-disk names.** Rename the socket, the ipc handshake url and the crash-handler
   socket; close the `paths.rs` guard hole.

## Verification

Every phase verified live on a runtime-isolated headless instance, not by reading:
the CLI hand-off must still work end to end after the socket rename, and
`sawe://settings`, `sawe://schemas/…` and the Copy Link round trip must still route.
