use anyhow::{Context as _, Result};
use gpui::{AppContext as _, AsyncApp, Context, PromptLevel, Window, actions};
use release_channel::ReleaseChannel;
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use util::ResultExt;
use workspace::notifications::{DetachAndPromptErr, NotificationId};
use workspace::{Toast, Workspace};

actions!(
    cli,
    [
        /// Installs the Sawe CLI tool to the system PATH.
        InstallCliBinary,
    ]
);

/// `/usr/local/bin` is a namespace shared with every other program on the
/// machine — including a real Zed, whose CLI is named `zed`. This fork claims
/// exactly one name in it and never touches anything else there.
const CLI_LINK_PATH: &str = "/usr/local/bin/sawe";

const LINUX_PROMPT_DETAIL: &str = "The CLI is installed alongside the app. If you used the official install script, add ~/.local/bin to your PATH to get the `sawe` command.\n\nIf you installed some other way, symlink the `cli` binary next to the app into a directory that is already on your PATH.";

/// What is sitting at the path we want the CLI symlink to occupy.
#[derive(Debug, PartialEq, Eq)]
enum LinkPathState {
    /// Nothing is there, so the path is ours to create.
    Vacant,
    /// A symlink to this build's CLI binary — one we created ourselves.
    AlreadyOurs,
    /// Something this fork did not create. Reported to the user, never removed.
    /// The payload completes the sentence "refusing to replace <path> because …".
    Foreign(String),
}

async fn inspect_link_path(cli_path: &Path, link_path: &Path) -> LinkPathState {
    match smol::fs::read_link(link_path).await {
        Ok(target) if target == cli_path => LinkPathState::AlreadyOurs,
        Ok(target) => LinkPathState::Foreign(format!("it points to {}", target.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => LinkPathState::Vacant,
        // `read_link` on an existing non-symlink fails with `EINVAL`, and any
        // other failure leaves us unable to prove the entry is ours — either
        // way something we did not create may be there, so refuse.
        Err(error) => LinkPathState::Foreign(format!(
            "it is not a symlink this installation created ({error})"
        )),
    }
}

fn refusal_message(link_path: &Path, reason: &str) -> String {
    format!(
        "Refusing to replace {} because {}. Sawe only replaces a symlink it created itself — remove that entry yourself if you want the `sawe` CLI there.",
        link_path.display(),
        reason
    )
}

/// Points `link_path` at `cli_path`, without ever removing a filesystem entry
/// this fork did not create: a machine may have both Sawe and another editor
/// installed, and deleting whatever happens to sit at the target path would
/// destroy the other product's CLI.
async fn install_symlink(cli_path: &Path, link_path: &Path) -> Result<PathBuf> {
    match inspect_link_path(cli_path, link_path).await {
        LinkPathState::AlreadyOurs => return Ok(link_path.into()),
        LinkPathState::Foreign(reason) => anyhow::bail!(refusal_message(link_path, &reason)),
        LinkPathState::Vacant => {}
    }

    // Nothing is in the way, so try to create the symlink without escalating.
    if smol::fs::unix::symlink(cli_path, link_path)
        .await
        .log_err()
        .is_some()
    {
        return Ok(link_path.into());
    }

    // The symlink could not be created, so use osascript with admin privileges
    // to create it. `ln -s`, never `ln -sf`: we established above that nothing
    // is at `link_path`, and the non-forcing form refuses to clobber an entry
    // that appeared in the meantime rather than deleting it.
    let bin_dir_path = link_path
        .parent()
        .context("CLI symlink path has no parent directory")?;
    let status = smol::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(
                "do shell script \" \
                    mkdir -p \'{}\' && \
                    ln -s \'{}\' \'{}\' \
                \" with administrator privileges",
                bin_dir_path.to_string_lossy(),
                cli_path.to_string_lossy(),
                link_path.to_string_lossy(),
            ),
        ])
        .stdout(smol::process::Stdio::inherit())
        .stderr(smol::process::Stdio::inherit())
        .output()
        .await?
        .status;
    anyhow::ensure!(status.success(), "error running osascript");
    Ok(link_path.into())
}

async fn install_script(cx: &AsyncApp) -> Result<PathBuf> {
    let cli_path = cx.update(|cx| cx.path_for_auxiliary_executable("cli"))?;
    install_symlink(&cli_path, Path::new(CLI_LINK_PATH)).await
}

fn installed_toast_message(link_path: &Path, display_name: &str) -> String {
    format!(
        "Installed `sawe` to {}. You can launch {} from your terminal.",
        link_path.display(),
        display_name
    )
}

pub fn install_cli_binary(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |workspace, cx| {
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            let prompt = cx.prompt(
                PromptLevel::Warning,
                "CLI should already be installed",
                Some(LINUX_PROMPT_DETAIL),
                &["OK"],
            );
            cx.background_spawn(prompt).detach();
            return Ok(());
        }
        let path = install_script(cx.deref())
            .await
            .context("error creating CLI symlink")?;

        workspace.update_in(cx, |workspace, _, cx| {
            struct InstalledSaweCli;

            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<InstalledSaweCli>(),
                    installed_toast_message(&path, ReleaseChannel::global(cx).display_name()),
                ),
                cx,
            )
        })?;
        Ok(())
    })
    .detach_and_prompt_err("Error installing sawe cli", window, cx, |_, _, _| None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Both a locked-identifier guard (CLAUDE.md §3 names the CLI binary `sawe`)
    /// and the reason this fork may not clobber whatever is already at the path:
    /// a real Zed's CLI lives in the same directory.
    #[test]
    fn cli_link_path_claims_only_our_own_name() {
        assert_eq!(CLI_LINK_PATH, "/usr/local/bin/sawe");
    }

    #[test]
    fn user_facing_text_never_names_another_product() {
        let toast = installed_toast_message(Path::new(CLI_LINK_PATH), "Sawe");
        assert!(toast.contains("`sawe`"), "{toast}");
        for text in [toast.as_str(), LINUX_PROMPT_DETAIL] {
            assert!(
                !text.to_lowercase().contains("zed"),
                "user-facing CLI text names another product: {text}"
            );
        }
    }

    #[test]
    fn creates_the_symlink_when_nothing_is_in_the_way() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli_path = dir.path().join("cli");
        fs::write(&cli_path, b"cli").expect("write cli");
        let link_path = dir.path().join("sawe");

        let installed =
            smol::block_on(install_symlink(&cli_path, &link_path)).expect("install should succeed");

        assert_eq!(installed, link_path);
        assert_eq!(fs::read_link(&link_path).expect("read_link"), cli_path);
    }

    #[test]
    fn is_a_no_op_when_the_link_is_already_ours() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli_path = dir.path().join("cli");
        fs::write(&cli_path, b"cli").expect("write cli");
        let link_path = dir.path().join("sawe");
        std::os::unix::fs::symlink(&cli_path, &link_path).expect("symlink");

        let installed =
            smol::block_on(install_symlink(&cli_path, &link_path)).expect("install should succeed");

        assert_eq!(installed, link_path);
        assert_eq!(fs::read_link(&link_path).expect("read_link"), cli_path);
    }

    #[test]
    fn refuses_to_replace_a_symlink_owned_by_something_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli_path = dir.path().join("cli");
        fs::write(&cli_path, b"cli").expect("write cli");
        let other_cli_path = dir.path().join("other-editor-cli");
        fs::write(&other_cli_path, b"other").expect("write other cli");
        let link_path = dir.path().join("sawe");
        std::os::unix::fs::symlink(&other_cli_path, &link_path).expect("symlink");

        let error = smol::block_on(install_symlink(&cli_path, &link_path))
            .expect_err("install must refuse a link it does not own");

        assert!(error.to_string().contains("Refusing to replace"), "{error}");
        assert_eq!(
            fs::read_link(&link_path).expect("the foreign symlink must survive"),
            other_cli_path
        );
        assert!(other_cli_path.exists());
    }

    #[test]
    fn refuses_to_replace_a_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli_path = dir.path().join("cli");
        fs::write(&cli_path, b"cli").expect("write cli");
        let link_path = dir.path().join("sawe");
        fs::write(&link_path, b"someone else's binary").expect("write occupant");

        let error = smol::block_on(install_symlink(&cli_path, &link_path))
            .expect_err("install must refuse a path it does not own");

        assert!(error.to_string().contains("Refusing to replace"), "{error}");
        assert_eq!(
            fs::read(&link_path).expect("the occupant must survive"),
            b"someone else's binary"
        );
    }
}
