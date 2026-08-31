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

/// Plants the decoy, wiping whatever a previous run left behind so that the
/// digest is meaningful. Returns a shell script, because the two transports
/// reach their target by different means (`docker exec` and `ssh`).
pub(crate) fn seed_decoy_script() -> String {
    format!(
        "rm -rf ~/{DECOY_DIR} ~/{OURS_DIR} && mkdir -p ~/{DECOY_DIR} && \
         printf '#!/bin/sh\\necho another-editors-server\\n' > ~/{DECOY_DIR}/{DECOY_NAME} && \
         chmod +x ~/{DECOY_DIR}/{DECOY_NAME} && md5sum < ~/{DECOY_DIR}/{DECOY_NAME}"
    )
}

pub(crate) fn decoy_digest_script() -> String {
    format!("md5sum < ~/{DECOY_DIR}/{DECOY_NAME}")
}

/// Lists our own directory, so the test can assert the upload really arrived
/// under the new name rather than only that the client said it would.
pub(crate) fn list_our_dir_script() -> String {
    format!("ls -1 ~/{OURS_DIR}")
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
