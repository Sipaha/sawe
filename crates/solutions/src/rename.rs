//! Filesystem-level rename primitives shared by `rename_solution` and
//! `rename_member`: collision checks (DB + disk + the symlink an unfinished
//! rename leaves behind), the same-filesystem preflight, and the
//! `rename(2)` + compat-symlink move itself.

use crate::folder_name::FolderNameError;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// A folder name already spoken for by a row in the DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakenFolder {
    pub folder: String,
    pub owner: String,
}

/// Case-insensitive on purpose: on macOS/Windows `Sawe` and `sawe` are the
/// same directory, so allowing both would produce a rename that silently
/// works on Linux and destroys data elsewhere.
fn same_folder_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

pub fn ensure_folder_available(
    parent: &Path,
    folder: &str,
    source: Option<&Path>,
    taken: &[TakenFolder],
) -> Result<PathBuf, FolderNameError> {
    let target = parent.join(folder);

    if let Some(conflict) = taken
        .iter()
        .find(|candidate| same_folder_name(&candidate.folder, folder))
    {
        return Err(FolderNameError::TakenBySolution {
            folder: folder.to_string(),
            owner: conflict.owner.clone(),
        });
    }

    match std::fs::symlink_metadata(&target) {
        Err(_) => Ok(target),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(FolderNameError::HeldByLink {
                    folder: folder.to_string(),
                });
            }
            // A case-only rename on a case-insensitive filesystem "finds" the
            // source directory at the target path. Same inode ⇒ same
            // directory ⇒ not a collision.
            if let Some(source) = source
                && is_same_dir(source, &target)
            {
                return Ok(target);
            }
            Err(FolderNameError::ExistsOnDisk {
                folder: folder.to_string(),
            })
        }
    }
}

#[cfg(unix)]
fn is_same_dir(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A cross-device move changes the inode, which orphans every live process
/// holding the directory as its cwd — the premise of the whole hot-rename
/// design. Callers treat `false` as a hard error, never as "copy instead".
#[cfg(unix)]
pub fn same_filesystem(source: &Path, target_parent: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    Ok(std::fs::metadata(source)?.dev() == std::fs::metadata(target_parent)?.dev())
}

#[cfg(not(unix))]
pub fn same_filesystem(_source: &Path, _target_parent: &Path) -> std::io::Result<bool> {
    // Windows: `rename` across volumes fails with ERROR_NOT_SAME_DEVICE, which
    // surfaces as an error from `move_dir_with_compat_link` anyway.
    Ok(true)
}

/// `rename(2)` + a `symlink(target → source)` compatibility shim. The shim is
/// what keeps the *strings* held by live processes valid (a spawned `claude`'s
/// cwd string, its `~/.claude/projects/<enc(cwd)>` bucket key, git `gitdir`
/// pointers, absolute paths in model context). It is deleted by the cold
/// reconcile on the next start.
pub fn move_dir_with_compat_link(source: &Path, target: &Path) -> Result<()> {
    let case_only = source != target && is_same_dir_name_ignoring_case(source, target);
    if case_only {
        // A direct rename is a no-op (or an error) on a case-insensitive
        // filesystem, so go through a temporary name.
        let temp = temp_sibling(target)?;
        std::fs::rename(source, &temp)
            .with_context(|| format!("renaming {} to {}", source.display(), temp.display()))?;
        if let Err(err) = std::fs::rename(&temp, target) {
            std::fs::rename(&temp, source).ok();
            return Err(err)
                .with_context(|| format!("renaming {} to {}", temp.display(), target.display()));
        }
    } else {
        std::fs::rename(source, target)
            .with_context(|| format!("renaming {} to {}", source.display(), target.display()))?;
    }

    if let Err(err) = symlink_dir(target, source) {
        // Roll the move back so the caller's DB stays consistent with disk.
        std::fs::rename(target, source).ok();
        return Err(err).with_context(|| {
            format!("linking {} back to {}", source.display(), target.display())
        });
    }
    Ok(())
}

fn is_same_dir_name_ignoring_case(source: &Path, target: &Path) -> bool {
    match (source.file_name(), target.file_name()) {
        (Some(a), Some(b)) => {
            source.parent() == target.parent()
                && same_folder_name(&a.to_string_lossy(), &b.to_string_lossy())
        }
        _ => false,
    }
}

fn temp_sibling(target: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .context("target has no file name")?
        .to_string_lossy()
        .into_owned();
    let parent = target.parent().context("target has no parent")?;
    for attempt in 0..1_000u32 {
        let candidate = parent.join(format!(".{name}.rename-tmp.{attempt}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("no free temporary name next to {}", target.display())
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_name_is_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = ensure_folder_available(dir.path(), "Sawe", None, &[]).expect("available");
        assert_eq!(target, dir.path().join("Sawe"));
    }

    #[test]
    fn db_owned_name_collides_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let taken = [TakenFolder {
            folder: "citeck-forge".into(),
            owner: "Citeck Forge".into(),
        }];
        let err =
            ensure_folder_available(dir.path(), "Citeck-Forge", None, &taken).expect_err("collides");
        assert_eq!(
            err,
            FolderNameError::TakenBySolution {
                folder: "Citeck-Forge".into(),
                owner: "Citeck Forge".into()
            }
        );
    }

    #[test]
    fn plain_directory_on_disk_collides() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("citeck-forge")).expect("mkdir");
        let err =
            ensure_folder_available(dir.path(), "citeck-forge", None, &[]).expect_err("collides");
        assert_eq!(
            err,
            FolderNameError::ExistsOnDisk {
                folder: "citeck-forge".into()
            }
        );
    }

    #[test]
    fn symlink_from_an_unfinished_rename_collides_with_its_own_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("new-name");
        std::fs::create_dir(&real).expect("mkdir");
        std::os::unix::fs::symlink(&real, dir.path().join("citeck-forge")).expect("symlink");
        let err =
            ensure_folder_available(dir.path(), "citeck-forge", None, &[]).expect_err("collides");
        assert_eq!(
            err,
            FolderNameError::HeldByLink {
                folder: "citeck-forge".into()
            }
        );
    }

    #[test]
    fn renaming_a_directory_to_itself_is_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("sawe");
        std::fs::create_dir(&source).expect("mkdir");
        // On a case-sensitive fs `Sawe` simply doesn't exist; on a
        // case-insensitive one it resolves to `source` — both are fine.
        let target =
            ensure_folder_available(dir.path(), "Sawe", Some(&source), &[]).expect("available");
        assert_eq!(target, dir.path().join("Sawe"));
    }

    #[test]
    fn same_filesystem_is_true_within_one_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("a");
        std::fs::create_dir(&source).expect("mkdir");
        assert!(same_filesystem(&source, dir.path()).expect("stat"));
    }

    #[test]
    fn move_leaves_a_symlink_behind_and_old_paths_resolve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("old");
        std::fs::create_dir(&source).expect("mkdir");
        std::fs::write(source.join("file.txt"), b"hello").expect("write");
        let target = dir.path().join("new");

        move_dir_with_compat_link(&source, &target).expect("move");

        assert!(target.join("file.txt").is_file());
        assert!(
            std::fs::symlink_metadata(&source)
                .expect("stat old")
                .file_type()
                .is_symlink()
        );
        // The old path — and every path *under* it — still resolves.
        assert_eq!(
            std::fs::read(source.join("file.txt")).expect("read via link"),
            b"hello"
        );
        std::fs::write(source.join("file.txt"), b"written via link").expect("write via link");
        assert_eq!(
            std::fs::read(target.join("file.txt")).expect("read new"),
            b"written via link"
        );
    }

    #[test]
    fn case_only_move_goes_through_a_temp_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("sawe");
        std::fs::create_dir(&source).expect("mkdir");
        std::fs::write(source.join("file.txt"), b"x").expect("write");
        let target = dir.path().join("Sawe");

        move_dir_with_compat_link(&source, &target).expect("move");

        assert!(target.join("file.txt").is_file());
        // No leftover temp directory.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rename-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {leftovers:?}");
    }
}
