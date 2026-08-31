//! Shared scaffolding for the `#[ignore]`d live-target tests in the transports.
//!
//! These tests exist because the disown pass (FORK.md #119) made a claim that a
//! unit test cannot check: that this fork's remote server no longer shares a
//! directory or a file name with another editor installed for the same user on
//! the same host. That is a statement about a real filesystem on a real remote
//! target, so it is checked against one.
//!
//! Every helper here works through the *decoy*: a file planted at the path
//! upstream Zed uploads to, whose digest must be identical before and after we
//! connect. Asserting only that our own upload landed correctly would pass just
//! as happily on code that clobbers the neighbour on its way there.

use anyhow::{Context as _, Result};
use askpass::EncryptedPassword;
use futures::channel::oneshot;
use gpui::{AsyncApp, Task};
use release_channel::ReleaseChannel;
use semver::Version as SemanticVersion;

use crate::{RemoteClientDelegate, RemotePlatform};

/// The directory and file name upstream Zed uses, which this fork used to use
/// too. Nothing in this crate may write, move or remove either of them.
pub(crate) const DECOY_DIR: &str = ".zed_server";
pub(crate) const DECOY_NAME: &str = "zed-remote-server-dev-build";

/// The directory and file name this fork uses now.
pub(crate) const OURS_DIR: &str = ".sawe_server";
pub(crate) const OURS_NAME: &str = "sawe-remote-server-dev-build";

/// The marker a target must carry before any of these scripts will delete
/// anything on it. `seed_decoy_script` wipes two directories, and the targets
/// are named by environment variables (`SAWE_TEST_SSH_HOST` and friends) that a
/// typo can point at a real machine — where the directory we would delete is
/// precisely the neighbouring editor's install this test exists to protect.
/// The marker is the target volunteering, and nothing here creates it.
pub(crate) const MARKER_NAME: &str = ".sawe-live-test-target";

/// Plants the decoy, wiping whatever a previous run left behind so that the
/// digest is meaningful. Returns a shell script, because the two transports
/// reach their target by different means (`docker exec` and `ssh`).
///
/// `home` must be the home directory of the *same* user the client uploads as,
/// either resolved or as a shell expression. It is a parameter rather than a
/// literal `~` because the two transports resolve `~` differently: the SSH arm
/// runs as the connection's user, but a `docker exec` without `-u` runs as the
/// image's default user, which for a non-root `USER` image is not the user the
/// client writes as — seeding the decoy in the wrong home would leave the digest
/// comparison passing over a file no code path under test can reach.
/// `target` names the target in the abort message and is not otherwise used.
pub(crate) fn seed_decoy_script(home: &str, target: &str) -> String {
    let home = checked_home(home);
    format!(
        "if [ ! -f \"{home}\"/{MARKER_NAME} ]; then \
           echo \"refusing to touch {target}: it has no disposability marker at \
           {home}/{MARKER_NAME}. This test deletes {home}/{DECOY_DIR} and {home}/{OURS_DIR} \
           outright. If that is acceptable, run 'touch {home}/{MARKER_NAME}' on that target \
           and try again; if it is not, you are pointed at the wrong host.\" >&2; \
           exit 1; \
         fi && \
         rm -rf \"{home}\"/{DECOY_DIR} \"{home}\"/{OURS_DIR} && mkdir -p \"{home}\"/{DECOY_DIR} && \
         printf '#!/bin/sh\\necho another-editors-server\\n' > \"{home}\"/{DECOY_DIR}/{DECOY_NAME} && \
         chmod +x \"{home}\"/{DECOY_DIR}/{DECOY_NAME} && \
         md5sum < \"{home}\"/{DECOY_DIR}/{DECOY_NAME}"
    )
}

pub(crate) fn decoy_digest_script(home: &str) -> String {
    let home = checked_home(home);
    format!("md5sum < \"{home}\"/{DECOY_DIR}/{DECOY_NAME}")
}

/// Lists our own directory, so the test can assert the upload really arrived
/// under the new name rather than only that the client said it would.
pub(crate) fn list_our_dir_script(home: &str) -> String {
    let home = checked_home(home);
    format!("ls -1 \"{home}\"/{OURS_DIR}")
}

/// The single place that decides whether a resolved home is usable, so that no
/// renderer can be added that skips it.
///
/// Unquoted, an empty `home` aborts naming `/` (misdirecting an operator whose
/// real problem is an unset `HOME`) and, if `/` happened to carry the marker,
/// would `rm -rf /{DECOY_DIR} /{OURS_DIR}`; a `home` with whitespace word-splits
/// the `[` into four arguments, so the test evaluates false, skips the abort
/// branch, and hands `rm -rf` a truncated prefix (`/mnt/shared data/bob` deletes
/// `/mnt/shared`). Quoting the interpolations stops the splitting, but a
/// whitespace-bearing home is still not a target these tests can honestly cover:
/// the transport under test builds its own remote command lines from that same
/// path, so a pass here would not mean the product works there. Refuse it at
/// setup rather than half-work.
fn checked_home(home: &str) -> &str {
    assert!(
        !home.is_empty(),
        "the target reported an empty home directory (`{HOME_SCRIPT}` returned nothing, which is \
         what an unset $HOME looks like — e.g. `docker exec -u` with a uid that has no passwd \
         entry). These tests need the real home of the user the client uploads as."
    );
    assert!(
        !home.contains(char::is_whitespace),
        "the target's home directory contains whitespace ({home:?}); these tests do not support \
         such a target, because the transport under test builds its own remote command lines from \
         this path and quoting it here would not make that work."
    );
    home
}

/// Resolves the target's home directory the same way the script helpers will
/// use it. Kept as one place so both transports spell it identically.
///
/// Quoted, because unquoted `echo $HOME` word-splits and glob-expands the value
/// before Rust ever sees it. Deliberately *not* `cd && pwd`: with `HOME` unset,
/// dash's `cd` succeeds and stays put, so `pwd` reports the shell's working
/// directory — a plausible absolute path that is not the home the client uploads
/// to, which is worse than the empty string `checked_home` can recognise.
pub(crate) const HOME_SCRIPT: &str = "echo \"$HOME\"";

/// The live tests must be handed a prebuilt server binary. Without a
/// `ZED_COPY_REMOTE_SERVER` that names an existing file,
/// `build_remote_server_from_source` falls through to `ZED_BUILD_REMOTE_SERVER`,
/// which defaults to `"nocompress"` rather than `"never"` — so an `--ignored`
/// test that was only meant to copy a file starts a musl cross-compile of
/// `remote_server` and may `cargo install` `cargo-zigbuild` first. Fail before
/// connecting instead.
///
/// The existence check is the half that matters in practice: a stale or
/// mistyped path is only a `log::warn!` there, so it takes the same fall-through
/// as an unset variable — and forgetting to `cargo build -p remote_server` is
/// the likelier mistake than forgetting the variable.
pub(crate) fn require_prebuilt_server_binary() {
    let path = std::env::var_os("ZED_COPY_REMOTE_SERVER").unwrap_or_else(|| {
        panic!(
            "set ZED_COPY_REMOTE_SERVER to a prebuilt Linux `remote_server` binary before \
             running this test; omitting it starts a multi-gigabyte cross-compile inside the \
             test. The test's doc comment has the full incantation."
        )
    });
    let path = std::path::Path::new(&path);
    assert!(
        path.exists(),
        "ZED_COPY_REMOTE_SERVER points at {}, which does not exist; the transport only logs a \
         warning for that and then starts the same multi-gigabyte cross-compile as if the \
         variable were unset. Build it first (`cargo build -p remote_server`) or fix the path.",
        path.display()
    );
}

pub(crate) async fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = util::command::new_command(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("spawning {program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn init_release_channel(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        release_channel::init_test(SemanticVersion::new(0, 1, 0), ReleaseChannel::Dev, cx)
    });
}

/// A delegate that answers nothing. The live tests supply the server binary via
/// `ZED_COPY_REMOTE_SERVER` and authenticate with a key, so every method that
/// would reach the network or the user is unreachable by construction — and
/// says so loudly rather than silently returning a default.
pub(crate) struct SilentDelegate;

impl RemoteClientDelegate for SilentDelegate {
    fn ask_password(
        &self,
        prompt: String,
        _tx: oneshot::Sender<EncryptedPassword>,
        _cx: &mut AsyncApp,
    ) {
        panic!("the live tests authenticate with a key; asked for a password: {prompt}")
    }

    fn get_download_url(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<SemanticVersion>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        panic!("the server binary is supplied via ZED_COPY_REMOTE_SERVER")
    }

    fn download_server_binary_locally(
        &self,
        _platform: RemotePlatform,
        _release_channel: ReleaseChannel,
        _version: Option<SemanticVersion>,
        _cx: &mut AsyncApp,
    ) -> Task<Result<std::path::PathBuf>> {
        panic!("the server binary is supplied via ZED_COPY_REMOTE_SERVER")
    }

    fn set_status(&self, status: Option<&str>, _cx: &mut AsyncApp) {
        if let Some(status) = status {
            eprintln!("live remote status: {status}");
        }
    }
}
