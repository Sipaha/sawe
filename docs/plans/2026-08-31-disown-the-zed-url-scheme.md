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

**The maintainer's rule, in their words: a user may have both Zed and Sawe
installed, and they must not intersect in any way.** That is broader than the URL
scheme, and it puts two shared-namespace items into scope that the scheme question
alone would not have reached (below). Installing extensions *from inside Sawe*
stays; what goes is the "Install in Zed" click-from-the-website path.

**`Install CLI` currently destroys a real Zed installation.**
`install_cli_binary.rs:21` targets `/usr/local/bin/zed`, and line 31 is an
unconditional `remove_file(link_path)` before the symlink — so on a machine with
both editors, `File → Install CLI` **deletes Zed's CLI and puts Sawe's binary at its
path**, escalating through `osascript` with admin privileges (line 40) if the plain
symlink fails. It then toasts ``Installed `zed` to …``, and its Linux help text
talks about "Zed from our official release". The target becomes `sawe`, and nothing
in this fork may remove a path it does not own.

**No special case for an incoming `zed://`.** An earlier draft of this plan had the
fork *name* such a link out loud rather than swallow it. The maintainer asked the
question that dissolves it: **how would one arrive?** Once we stop registering the
scheme with the OS, the only routes left are the user typing `sawe zed://…`
themselves or pasting into our own "Open URL" modal — whose placeholder currently
*suggests* `zed://…`, i.e. we are the one proposing it. Fix the placeholder and the
case has no producer. `zed://` therefore becomes an ordinary unrecognised URL,
handled by whatever we already do with one. No branch, no message.

**Generation and parsing move in the same change.** We still generate
`zed://schemas/…` ourselves; deleting the parse arms alone would break settings
schemas at the halfway point. The recon lists nine sites that move together —
`main.rs:1679`, `settings_ui.rs:1428`, `agent_skills.rs:803`,
`open_url_modal.rs:30,57`, `cli/main.rs:37`, `hyperlinks.rs:22` and the rest.

**On-disk names are renamed, not aliased.** `zed-<channel>.sock`, `zed-cli://` and
`zed-crash-handler-<pid>` are ours at both ends, live under `paths::data_dir()` /
`cache_dir()`, and nothing outside can know them — so there is no compatibility
window to keep and no collision with a real Zed either. Rename them for hygiene, and
close the `paths.rs:626/639` guard hole in the same change so the class cannot recur.

**Two genuine intersections in shared namespaces, now in scope:**
- the Linux keyring label `zed-github-account`
  (`crates/gpui_linux/src/linux/platform.rs:44`) — one slot, two products. Costs the
  user a single re-authentication.
- the Windows shell appx, which ships as `Name="ZedIndustries.Zed"` /
  `<DisplayName>Zed</DisplayName>` / verb `OpenWithZed`. Its identity also fails to
  match the `Sipaha.Sawe_…` our uninstaller removes, so it is a live bug on its own
  terms. Windows-only and untestable from this machine — **fix the names, record
  that it could not be verified here.**

**Out of scope, recorded not fixed:** `util::get_zed_cli_path()` looks for
`../bin/zed` this fork never ships, so it is already broken rather than merely
misnamed.


## Maintainer rulings, 2026-08-31 (widening the plan)

**The non-intersection rule extends to everything, including `.zed_server`.**
Verbatim: *"распространяется на все. это два разных продукта."* So the "preserved
upstream identifier" carve-out in `.rules` §3 for `.zed_server` / `.zed_wsl_server`
is withdrawn — they become this fork's own names, and with them the remote-server
binary, which `ssh.rs:800`, `wsl.rs:182` and `docker.rs:196` currently spell
`zed-remote-server-{channel}-{version}`, i.e. byte-identical to what upstream writes.

*Consequence, accepted:* a Sawe client will no longer reuse a server binary uploaded
by Zed, and vice versa — that is the point. A one-time re-upload to each remote host
this fork already connects to is the whole migration cost.

*Consequence, needing care:* `crates/acp_thread`'s `MentionUri` namespace uses
`zed:///…` (empty authority, 19 sites) and is **persisted in thread history**.
Renaming it needs a migration or a reader tolerant of both spellings, or old threads
lose their mentions. That is its own task, not a rename-in-place.

**`.rules` §3 is the side that is wrong about paths — correct the documentation.**
Verbatim: *"По п.1 - правим документацию."* §3 locks `~/.config/sawe/` (Linux),
`~/Library/Application Support/Sawe/` (macOS) and `%APPDATA%\Sawe\` (Windows);
`paths::base_dir()` is `home_dir()/.spk/<channel-kebab>` on **every** platform
(`crates/paths/src/paths.rs:133-141`), and its own doc comment says the
`Application Support` / `AppData` split was removed deliberately. The single root is
also what makes `SAWE_HOME` and `--user-data-dir` isolation work, which every
agent-driven MCP check depends on. §3 is corrected to the code; the code does not move.

*Follows immediately:* `script/uninstall.sh` deletes `~/.config/sawe`,
`~/.local/share/sawe` and `~/Library/Application Support/Sawe` — none of which the
binary creates — and never removes `~/.spk/sawe`, so "keep your preferences?" has no
consequence either way. It is repointed at `base_dir()`.

## Phases

1. **Contract.** Stop registering the `zed` scheme; remove the palette action and
   the unconditional call from `Install CLI`; symlink `sawe`, not `zed`, and never
   remove a path this fork does not own; fix the toast, the Linux help text and the
   "links will now open in" message.
2. **Routing.** Make `sawe://` canonical — reverse `915bd2b73f`'s direction — moving
   generation and parsing together; drop `zed://` from `is_url_scheme`; fix the
   "Open URL" placeholder so we stop suggesting the scheme we just disowned. **No**
   special branch for an incoming `zed://`.
3. **Identity and on-disk names.** Correct `.rules` §3 to the code; repoint
   `script/uninstall.sh` at `base_dir()`; close the `paths.rs:626/639` guard hole
   through which `zed-<channel>.sock` escapes today; rename that socket, the
   `zed-cli://` ipc handshake url and the `zed-crash-handler-<pid>` socket; rename
   the Linux keyring label `zed-github-account`.
4. **The remote server.** `.zed_server` / `.zed_wsl_server` and the
   `zed-remote-server-{channel}-{version}` binary name become ours. Verify against a
   real remote target before and after, since this is the one change with a
   compatibility consequence rather than a cosmetic one.
5. **The long tails.** The Windows shell appx (`Name="ZedIndustries.Zed"`,
   `<DisplayName>Zed</DisplayName>`, verb `OpenWithZed`, and an identity that does
   not match what our own uninstaller removes — a live bug); and `MentionUri`'s
   `zed:///` namespace, which needs a migration or a tolerant reader because it is
   persisted in thread history.

## Verification

Every phase verified live on a runtime-isolated headless instance, not by reading:
the CLI hand-off must still work end to end after the socket rename, and
`sawe://settings`, `sawe://schemas/…` and the Copy Link round trip must still route.
