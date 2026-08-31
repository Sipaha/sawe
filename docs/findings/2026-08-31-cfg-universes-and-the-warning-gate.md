# The standing check and the shipping binary compile different cfg universes

**Date:** 2026-08-31
**Status:** confirmed — nine configurations measured, facts reproduced independently by two agents
**Crates:** `remote`, `remote_connection`, `recent_projects`, `project`, `title_bar`

`cargo check --workspace --all-targets` runs continuously under the
rust-analyzer flycheck and is this fork's de facto quality gate. It reported
zero warnings while `cargo build --bin sawe` — the binary that actually ships —
emitted five. Neither configuration is a superset of the other, so each can
carry diagnostics the other structurally cannot see.

## The mechanism

`RemoteConnectionOptions` (`crates/remote/src/remote_client.rs:1274`) has a
variant that only exists under **`remote`'s** `test-support`:

```rust
pub enum RemoteConnectionOptions {
    Ssh(SshConnectionOptions),
    Wsl(WslConnectionOptions),
    Docker(DockerConnectionOptions),
    #[cfg(any(test, feature = "test-support"))]
    Mock(crate::transport::mock::MockConnectionOptions),
}
```

Each consumer used to gate both its `Mock` arm and its `_` catchall on **its
own** crate's `test-support`. Those are two different crates' features, and
cargo can drive them apart. Three configurations result:

| consumer's `test-support` | `remote/test-support` | arms compiled | result |
|---|---|---|---|
| off | off | Ssh/Wsl/Docker + `_` | the three concrete arms are already exhaustive → **`_` unreachable → warning** |
| on | on | Ssh/Wsl/Docker/Mock | exhaustive, no `_` → clean |
| off | on | Ssh/Wsl/Docker + `_` | `_` covers `Mock` → **load-bearing**, clean |

`cargo build --bin sawe` builds no test targets, so no dev-dependency enters
the graph and nothing turns `test-support` on anywhere: **row 1**, five
warnings.

`cargo check --workspace --all-targets` builds test targets, dev-dependencies
enter the graph, and feature unification turns `test-support` on across it
(`recent_projects`' dev-deps take `remote` and `remote_connection` with
`test-support`; `remote_connection/test-support` forwards to
`remote/test-support`): **row 2**. Note the intuitive story is backwards — the
catchall does not become *reachable* under `--all-targets`, it **is not
compiled at all** there. That is why the gate cannot see the warning: not
"it looked at the code and judged it fine", but "that source text was never
part of the translation unit".

Row 3 is real and is why the catchall exists at all. `cargo check -p
recent_projects --all-targets` produces it: the crate's own dev-deps enable
`remote/test-support`, but nothing can enable the *package's own* feature from
its own dev-deps. `crates/title_bar/Cargo.toml` is the one manifest in the tree
that enables `remote/test-support` without `remote_connection/test-support`.
Deleting the catchall fails `E0004` in that configuration.

A crate cannot `cfg` on a dependency's feature, so there is no cfg expression
that says "compile this arm exactly when `remote` has `Mock`". The fix is
therefore to compile the catchall unconditionally and silence it:

```rust
// Reachable only when `remote/test-support` is enabled by feature
// unification without this crate's own `test-support`; the arms above
// are exhaustive in every other configuration.
#[allow(unreachable_patterns)]
_ => unreachable!("RemoteConnectionOptions::Mock requires remote/test-support"),
```

## The mirror-image trap: a gate that cannot run reads like a gate that passed

`cargo check -p remote_connection --all-targets` did not compile at all:

```
error[E0599]: no variant or associated item named `Mock` found for enum
`RemoteConnectionOptions` in the current scope
error: could not compile `remote_connection` (lib test) due to 1 previous error
```

The crate had **no `[dev-dependencies]` section whatsoever**. Its lib-test unit
is compiled with `--cfg test`, which switches the `Mock` arm on, while nothing
in that invocation switches `remote/test-support` on — row 1 inverted, and a
hard error rather than a warning. Both standing checks miss it for different
reasons: `--bin sawe` builds no test target, and `--workspace --all-targets`
has `remote_connection/test-support` unified in from other crates' dev-deps.

The concrete cost: `docs/workflow/supervisor-mode.md` § CHECKS tells every
sub-agent to run `cargo clippy -p <crate> --all-targets -- -D warnings` and
`cargo test -p <crate>`. For `remote_connection` neither command could execute
— they died on E0599 before clippy or the test harness said anything. **A gate
that cannot run reads exactly like a gate that passed** unless someone checks
the exit code.

Fixed with the self-dev-dependency idiom `crates/project/Cargo.toml` already
uses, which keeps a package's own `test-support` on whenever its test target is
built, so arm and variant stay in lockstep:

```toml
[dev-dependencies]
project = { workspace = true, features = ["test-support"] }
remote_connection = { workspace = true, features = ["test-support"] }
```

The `project` line is needed for the same reason one level down:
`crates/project/src/trusted_worktrees.rs:175` gates its `Mock` arm on
`#[cfg(feature = "test-support")]` alone, so with `remote/test-support` on and
`project/test-support` off that match is non-exhaustive (E0004).

## The sweep, and the one member left broken on purpose

`cargo check -p <member> --all-targets` was afterwards run over **all 260
workspace members**, one at a time. It found four more instances of the shape
above, all fixed in `ce9fb1e327`: `clock`, `call` and `language_extension` take
the self / dependency `test-support` dev-dep idiom, and `git_hosting_providers`
is fixed one level up, in `git`'s own `test-support` feature list, so that every
consumer of that feature gets it rather than just this one. Each is a row in
FORK.md's touched-files table.

A fifth member, **`component_preview`, still cannot compile standalone, and is
left that way deliberately**:

```
error: 'wayland' or 'x11' feature must be enabled.   (x2)
error: could not compile `zed-scap` (lib) due to 2 previous errors
```

This is not a `test-support` problem at all. `component_preview`'s only
dev-dependency is `gpui_platform = { features = ["screen-capture"] }`;
`screen-capture` fans out to `gpui_linux/screen-capture`, which pulls
`zed-scap`, which requires one of `wayland` / `x11` — features `gpui_platform`
exposes *separately* (`wayland = ["gpui_linux/wayland"]`, `x11 =
["gpui_linux/x11"]`) and that `screen-capture` does not imply. Checked alone,
the crate enables a capture backend with no display backend. It is the
**reverse direction** of everything above: green in `cargo check --workspace
--all-targets` purely because other members enable `x11` and `wayland`, i.e. a
crate that compiles only because the workspace supplies a feature it never
asked for.

The consequence is the same as the mirror-image trap: `cargo clippy -p
component_preview --all-targets -- -D warnings`, the per-crate gate
`docs/workflow/supervisor-mode.md` § CHECKS tells every sub-agent to run,
**cannot execute** on this member. Expect it, and read the exit code rather
than the empty diagnostic list.

Two candidate fixes, neither taken:

- make `gpui_linux`'s `screen-capture` require a display backend on Linux —
  the `git` treatment, fixing it for every consumer; or
- add `"x11", "wayland"` to `component_preview`'s `gpui_platform`
  dev-dependency — local, and leaves the next consumer to rediscover it.

It was reported rather than fixed because the cause is a display-backend
feature edge, not the `test-support` idiom: choosing between those two is a
decision about `gpui_linux`'s feature contract, not a mechanical application of
the fix above.

## What to do about it

- The verification ritual already compiles the shipping universe — it just
  threw the warnings away. `grep -E "^error|could not compile|^warning"`, not
  `^error` alone. `cargo check --bin sawe` emits a byte-identical warning set to
  `cargo build --bin sawe` when a cheaper probe is wanted.
- `cargo check --workspace` (no `--all-targets`) adds nothing: measured
  identical to the `--bin sawe` set.
- **Swept once, not guarded.** "package X's `--all-targets` build depends on
  package Y's `test-support` being enabled by X's dev-deps" is a whole-workspace
  property; all 260 members were checked individually (see the section above),
  so the tree is clean as of `ce9fb1e327` apart from `component_preview`. That
  is a snapshot, not a check that reruns: a new crate, or a new
  `#[cfg(any(test, feature = "test-support"))]` block in an existing one, can
  reintroduce it and neither standing gate will notice. There is no cheap static
  equivalent either, because the shape depends on how feature unification
  resolves — the sweep costs roughly one full check per member.
  `tooling/test_target_guard` catches a *sibling* instance of this family (a
  `[lib] test = false` package whose `#[cfg(test)]` modules are never compiled)
  but nothing about this one.
