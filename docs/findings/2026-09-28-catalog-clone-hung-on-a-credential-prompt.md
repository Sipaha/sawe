# A catalog clone hung forever on a credential prompt — and Cancel didn't stop it

**Date:** 2026-09-28 · **Status:** fixed

## Symptom

Adding `spk-mm-client` and `spk-cockpit` (both private GitHub repos, registered in the
catalog with `https://` URLs) to a Solution left both project tabs spinning with no end.
"Cancel Clone" removed the tab, but re-adding refused, and the catalog project could not be
fixed: there was no Edit anywhere except on a *failed* add's tab, and this add never failed.

## Mechanism

1. **git prompted on the editor's tty.** `solutions::git` spawned `git clone --bare <https-url>`
   with inherited stdin, no `GIT_TERMINAL_PROMPT`, in the editor's own session. GitHub answers
   a private repo with 401, so git asked `Username for 'https://github.com':` — on the
   controlling tty of the process that launched Sawe, where nobody can see it. `/proc/<pid>/fd`
   of `git-remote-https` showed fds 6/7 on `/dev/tty`, and its socket to GitHub sat in
   `CLOSE-WAIT`: the server had long gone, git was waiting on a keyboard.
2. **Cancel was a flag, not a kill.** `cancel_add_member` removed the in-flight entry and set a
   flag that the task checked only *between* git steps. A step that never ends never reaches the
   check, so the git process lived on — holding the store-wide `fs_lock`, which every add takes.
   The second project's add had been queued behind that lock from the start, which is why it
   also spun forever.
3. **A killed bare clone looked like a good cache.** `git clone --bare` writes `HEAD` in its
   first milliseconds, and `is_usable_mirror` accepted any bare dir with `HEAD`. A clone that
   died mid-way (SIGKILL, crash) would have been reused as the project's mirror from then on.

## Fix

- `git_command()` (`crates/solutions/src/git.rs`) is the only way this crate spawns git:
  `GIT_TERMINAL_PROMPT=0`, stdin `/dev/null`, and `setsid` — so git's credential prompt errors
  out and `ssh` has no tty for a passphrase / host-key question either (without overriding the
  user's `core.sshCommand`, which forcing `ssh -o BatchMode=yes` would). A private https repo
  now fails in about a second with `could not read Username … terminal prompts disabled`.
- `drain_command` SIGTERMs the child's **process group** when its future is dropped (the
  `setsid` makes git the group leader; the network work happens in a `git-remote-https` / `ssh`
  grandchild). SIGTERM, not SIGKILL: git's handler removes its half-written clone directory.
- `InFlightAdd` holds an abort `Sender` whose *drop* is the signal; the add task races its git
  steps against it. Removing the entry for any reason (Cancel, catalog row deleted mid-clone)
  therefore stops git and frees `fs_lock` at once.
- `ensure_cache` clones into `<key>.partial` and renames into place only on success.
- Edit is reachable without a failure first: a pencil on every catalog row of both add-project
  pickers, `secondary-enter` on the selected row, and "Edit Project…" on the in-flight tab.

## Gotcha for anyone spawning git elsewhere

`smol::process::Command::from(std::process::Command)` **forgets the std command's stdio**: it
spawns with inherited stdin unless stdin is set again on the async wrapper. Set stdio after the
conversion.

Tests: `git::tests::background_git_cannot_prompt`,
`git::tests::dropping_a_git_step_terminates_its_process_group` (both fail with the fix
reverted), `cache::tests::a_failed_clone_leaves_no_cache_behind`,
`cache::tests::ensure_cache_clears_a_stale_partial_clone`,
`add_project_picker::tests::secondary_enter_edits_the_catalog_row_instead_of_adding_it`.
