//! S-PCH patches — `git format-patch` / `git apply` / `git am` plumbing.
//!
//! Three flavours of patch are recognised, each with a corresponding apply
//! strategy:
//!
//! * `Mbox` — the mailbox produced by `git format-patch`. Apply via
//!   `git am --3way --keep-cr`. Carries author + commit metadata.
//! * `UnifiedWithIndex` — `git diff` output with `index <hash>..<hash>`
//!   lines. Apply via `git apply --3way` (the index hashes give git a
//!   common base for 3-way merge).
//! * `UnifiedNoIndex` — `git diff` output without index hashes. Apply via
//!   plain `git apply` (no 3-way; rejected hunks become `.rej` files).
//!
//! Detection is done in [`detect_patch_format`] from the first ~4KB of the
//! file. Patches are not [`super::AtomicGitOp`] — they're filesystem-effecting
//! but the rollback story is `git status` + `git restore`, and `git apply`
//! never moves HEAD so backup-refs don't apply.

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

use super::direct::{list_conflicted_paths, run_git, run_git_with_envs};
use git2::Repository;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchFormat {
    /// `git format-patch` mailbox. Apply via `git am`.
    Mbox,
    /// `git diff` output that contains `index <hash>..<hash>` lines.
    /// Apply via `git apply --3way`.
    UnifiedWithIndex,
    /// `git diff` output without index hashes. Apply via `git apply`
    /// (3-way merge isn't possible without the base blob).
    UnifiedNoIndex,
}

impl PatchFormat {
    pub fn label(self) -> &'static str {
        match self {
            PatchFormat::Mbox => "mbox",
            PatchFormat::UnifiedWithIndex => "unified-with-index",
            PatchFormat::UnifiedNoIndex => "unified-no-index",
        }
    }
}

/// Detect the format of `bytes` using the first ~4KB.
///
/// Algorithm (per S-PCH spec):
/// 1. Strip a UTF-8 BOM if present.
/// 2. If the first non-empty line matches `^From [0-9a-f]{40} ` → `Mbox`.
/// 3. Else if the body contains a line matching `^index [0-9a-f]+\.\.[0-9a-f]+`
///    → `UnifiedWithIndex`.
/// 4. Else if the body contains a line matching `^diff --git a/`
///    → `UnifiedNoIndex`.
/// 5. Else → error.
pub fn detect_patch_format(bytes: &[u8]) -> Result<PatchFormat> {
    let scan_limit = bytes.len().min(4096);
    let head = &bytes[..scan_limit];

    let head_str =
        std::str::from_utf8(head).map_err(|err| anyhow!("patch is not valid UTF-8: {err}"))?;
    let head_str = head_str.strip_prefix('\u{feff}').unwrap_or(head_str);

    let first_non_empty = head_str
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .find(|line| !line.is_empty());

    if let Some(line) = first_non_empty
        && is_mbox_from_line(line)
    {
        return Ok(PatchFormat::Mbox);
    }

    let mut has_index = false;
    let mut has_diff_git = false;
    for raw in head_str.lines() {
        let line = raw.trim_end_matches('\r');
        if !has_index && is_index_line(line) {
            has_index = true;
        }
        if !has_diff_git && line.starts_with("diff --git a/") {
            has_diff_git = true;
        }
        if has_index && has_diff_git {
            break;
        }
    }

    if has_index {
        return Ok(PatchFormat::UnifiedWithIndex);
    }
    if has_diff_git {
        return Ok(PatchFormat::UnifiedNoIndex);
    }
    Err(anyhow!("Unrecognized patch format"))
}

fn is_mbox_from_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("From ") else {
        return false;
    };
    let mut chars = rest.chars();
    let mut count = 0;
    for ch in chars.by_ref() {
        if ch == ' ' {
            break;
        }
        if !ch.is_ascii_hexdigit() {
            return false;
        }
        count += 1;
        if count > 40 {
            return false;
        }
    }
    count == 40
}

fn is_index_line(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("index ") else {
        return false;
    };
    // Form: index <hex>..<hex>[ <mode>]
    let Some((left, right_and_mode)) = rest.split_once("..") else {
        return false;
    };
    if left.is_empty() || !left.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    let right = match right_and_mode.split_once(' ') {
        Some((r, _mode)) => r,
        None => right_and_mode,
    };
    !right.is_empty() && right.chars().all(|c| c.is_ascii_hexdigit())
}

/// Tunables for [`apply_patch`].
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Use `--3way` (for `git apply`) / `--3way` (for `git am`). Defaults to
    /// `false`. Only meaningful for `Mbox` and `UnifiedWithIndex`.
    pub three_way: bool,
    /// Pass `--keep-cr` to `git am` (mbox flow). Preserves CRLF line
    /// endings the patch encodes; harmless on Unix-only patches.
    pub keep_cr: bool,
    /// On apply failure (after a 3-way attempt has been made), retry with
    /// `git apply --reject`, leaving `.rej` files for failed hunks.
    pub apply_with_reject: bool,
}

/// Result of [`apply_patch`].
#[derive(Debug, Clone)]
pub enum ApplyOutcome {
    /// Patch applied cleanly.
    Clean,
    /// 3-way apply / am produced unmerged paths. Caller is expected to
    /// open the conflict resolver UI and finish via `git am --continue`
    /// (mbox) or by committing the fixed working tree (unified).
    Conflict {
        /// Repo-relative paths reported by `git status --porcelain` as
        /// unmerged.
        conflicted_files: Vec<PathBuf>,
    },
    /// `--reject` fallback succeeded with rejected hunks. The listed
    /// `.rej` files contain the unapplied chunks.
    RejectedHunks { reject_files: Vec<PathBuf> },
}

/// Create one or more patch files from `repo_path` covering the range
/// `sha_from..sha_to` (when `sha_to` is `Some`) or just `sha_from` (when
/// `sha_to` is `None`).
///
/// When `out_dir` is `None`, runs `git format-patch --stdout` and writes the
/// resulting bytes to a single file in a temp directory; the returned
/// `Vec<PathBuf>` has one entry. When `out_dir` is `Some`, runs
/// `git format-patch -o <out_dir>` and returns the per-commit files git
/// produces.
pub fn create_patch(
    repo_path: &Path,
    sha_from: &str,
    sha_to: Option<&str>,
    out_dir: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    if sha_from.trim().is_empty() {
        return Err(anyhow!("create_patch: sha_from is empty"));
    }

    let range_arg = match sha_to {
        Some(to) if !to.trim().is_empty() => format!("{sha_from}..{to}"),
        _ => sha_from.to_string(),
    };

    if let Some(out_dir) = out_dir {
        std::fs::create_dir_all(out_dir)
            .map_err(|err| anyhow!("create_patch: mkdir {}: {err}", out_dir.display()))?;
        let out_str = out_dir.to_string_lossy().to_string();
        let mut args: Vec<&str> = vec!["format-patch"];
        if sha_to.is_none() || sha_to.map(str::trim).unwrap_or("").is_empty() {
            args.push("-1");
        }
        args.push("-o");
        args.push(&out_str);
        args.push(&range_arg);
        let output = run_git(repo_path, &args)?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut paths = Vec::new();
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            paths.push(PathBuf::from(trimmed));
        }
        return Ok(paths);
    }

    // No out_dir → `--stdout`, write to a temp file rooted under the
    // repo's `.git/spke-patches/` directory so the resulting path is
    // stable and discoverable.
    let mut args: Vec<&str> = vec!["format-patch", "--stdout"];
    if sha_to.is_none() || sha_to.map(str::trim).unwrap_or("").is_empty() {
        args.push("-1");
    }
    args.push(&range_arg);
    let output = run_git(repo_path, &args)?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let scratch_dir = repo_path.join(".git").join("spke-patches");
    std::fs::create_dir_all(&scratch_dir)
        .map_err(|err| anyhow!("create_patch: mkdir {}: {err}", scratch_dir.display()))?;
    let short = sha_from.chars().take(12).collect::<String>();
    let filename = match sha_to {
        Some(to) if !to.trim().is_empty() => {
            let to_short: String = to.chars().take(12).collect();
            format!("range-{short}-{to_short}.patch")
        }
        _ => format!("commit-{short}.patch"),
    };
    let out_path = scratch_dir.join(filename);
    std::fs::write(&out_path, &output.stdout)
        .map_err(|err| anyhow!("create_patch: write {}: {err}", out_path.display()))?;
    Ok(vec![out_path])
}

/// Apply `patch_path` to `repo_path`. Detects the patch format, picks the
/// appropriate git verb, and translates the result into [`ApplyOutcome`].
pub fn apply_patch(
    repo_path: &Path,
    patch_path: &Path,
    options: ApplyOptions,
) -> Result<ApplyOutcome> {
    let bytes = std::fs::read(patch_path)
        .map_err(|err| anyhow!("apply_patch: read {}: {err}", patch_path.display()))?;
    let format = detect_patch_format(&bytes)?;

    match format {
        PatchFormat::Mbox => apply_mbox(repo_path, patch_path, &options),
        PatchFormat::UnifiedWithIndex => {
            apply_unified(
                repo_path, patch_path, &options, /*three_way_default*/ true,
            )
        }
        PatchFormat::UnifiedNoIndex => {
            apply_unified(
                repo_path, patch_path, &options, /*three_way_default*/ false,
            )
        }
    }
}

fn apply_mbox(repo_path: &Path, patch_path: &Path, options: &ApplyOptions) -> Result<ApplyOutcome> {
    let path_arg = patch_path.to_string_lossy().to_string();
    let mut args: Vec<&str> = vec!["am"];
    if options.three_way {
        args.push("--3way");
    }
    if options.keep_cr {
        args.push("--keep-cr");
    }
    args.push(&path_arg);
    let envs = super::direct::no_editor_envs();
    let output = run_git_with_envs(repo_path, &args, &envs)?;
    if output.status.success() {
        return Ok(ApplyOutcome::Clean);
    }

    // `git am` failure: check for unmerged paths (3-way conflict path).
    let conflicts = list_conflicted_paths(repo_path).unwrap_or_default();
    if !conflicts.is_empty() {
        return Ok(ApplyOutcome::Conflict {
            conflicted_files: conflicts,
        });
    }

    if options.apply_with_reject {
        return apply_unified_reject(repo_path, patch_path);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let abort_envs = super::direct::no_editor_envs();
    let _ = run_git_with_envs(repo_path, &["am", "--abort"], &abort_envs);
    Err(anyhow!("git am failed: {}", stderr.trim()))
}

fn apply_unified(
    repo_path: &Path,
    patch_path: &Path,
    options: &ApplyOptions,
    three_way_default: bool,
) -> Result<ApplyOutcome> {
    let path_arg = patch_path.to_string_lossy().to_string();
    let want_three_way = options.three_way && three_way_default;
    let mut args: Vec<&str> = vec!["apply"];
    if want_three_way {
        args.push("--3way");
    }
    args.push(&path_arg);
    let output = run_git(repo_path, &args)?;
    if output.status.success() {
        // 3-way may have left unmerged paths even when the command exits
        // 0 — git apply --3way uses index 1/2/3 stages. Surface them as
        // a conflict so the caller invokes the resolver.
        if want_three_way {
            let conflicts = list_conflicted_paths(repo_path).unwrap_or_default();
            if !conflicts.is_empty() {
                return Ok(ApplyOutcome::Conflict {
                    conflicted_files: conflicts,
                });
            }
        }
        return Ok(ApplyOutcome::Clean);
    }

    if want_three_way {
        let conflicts = list_conflicted_paths(repo_path).unwrap_or_default();
        if !conflicts.is_empty() {
            return Ok(ApplyOutcome::Conflict {
                conflicted_files: conflicts,
            });
        }
    }

    if options.apply_with_reject {
        return apply_unified_reject(repo_path, patch_path);
    }

    Err(anyhow!(
        "git apply failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn apply_unified_reject(repo_path: &Path, patch_path: &Path) -> Result<ApplyOutcome> {
    let path_arg = patch_path.to_string_lossy().to_string();
    let args: Vec<&str> = vec!["apply", "--reject", &path_arg];
    // `git apply --reject` exits non-zero when there are rejected hunks,
    // but still writes the .rej files. Treat that as success and
    // enumerate the .rej files via libgit2's status walk.
    let _output = run_git(repo_path, &args)?;
    let reject_files = list_reject_files(repo_path).unwrap_or_default();
    Ok(ApplyOutcome::RejectedHunks { reject_files })
}

fn list_reject_files(repo_path: &Path) -> Result<Vec<PathBuf>> {
    let repo = Repository::discover(repo_path).map_err(|err| anyhow!("discover repo: {err}"))?;
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|err| anyhow!("statuses: {err}"))?;
    let mut out = Vec::new();
    for entry in statuses.iter() {
        let Some(path) = entry.path() else { continue };
        if path.ends_with(".rej") {
            out.push(PathBuf::from(path));
        }
    }
    Ok(out)
}

/// Parse hunk-affected paths + per-file +/- counts from a patch's bytes.
/// Returns one entry per file with `(path, additions, deletions)`. Used by
/// the apply-confirm modal to render a preview without invoking git.
pub fn parse_patch_summary(bytes: &[u8]) -> Vec<(String, u32, u32)> {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut summary: Vec<(String, u32, u32)> = Vec::new();
    let mut current: Option<(String, u32, u32)> = None;
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some(prev) = current.take() {
                summary.push(prev);
            }
            // `a/<path> b/<path>` — take the `b/` side as the canonical
            // post-image path; falls back to the `a/` side for deletes.
            let path = rest
                .split_once(" b/")
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| rest.to_string());
            current = Some((path, 0, 0));
            continue;
        }
        let Some((_, add, del)) = current.as_mut().map(|t| (&t.0, &mut t.1, &mut t.2)) else {
            continue;
        };
        if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("@@") {
            continue;
        }
        if let Some(first) = line.chars().next() {
            match first {
                '+' => *add += 1,
                '-' => *del += 1,
                _ => {}
            }
        }
    }
    if let Some(prev) = current {
        summary.push(prev);
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_format_mbox() {
        let body = b"From cafef00dcafef00dcafef00dcafef00dcafef00d Mon Sep 17 00:00:00 2001\n\
                     From: Test <t@example>\n\
                     Date: Mon, 1 Jan 2024 00:00:00 +0000\n\
                     Subject: change\n\
                     ---\n";
        assert_eq!(
            detect_patch_format(body).expect("detect"),
            PatchFormat::Mbox
        );
    }

    #[test]
    fn detect_format_unified_with_index() {
        let body = b"diff --git a/foo b/foo\n\
                     index 0123456789abcdef..fedcba9876543210 100644\n\
                     --- a/foo\n\
                     +++ b/foo\n\
                     @@ -1,1 +1,1 @@\n\
                     -old\n\
                     +new\n";
        assert_eq!(
            detect_patch_format(body).expect("detect"),
            PatchFormat::UnifiedWithIndex
        );
    }

    #[test]
    fn detect_format_unified_no_index() {
        let body = b"diff --git a/foo b/foo\n\
                     --- a/foo\n\
                     +++ b/foo\n\
                     @@ -1,1 +1,1 @@\n\
                     -old\n\
                     +new\n";
        assert_eq!(
            detect_patch_format(body).expect("detect"),
            PatchFormat::UnifiedNoIndex
        );
    }

    #[test]
    fn detect_format_rejects_unrecognized() {
        let body = b"this is not a patch\nrandom\n";
        let err = detect_patch_format(body).unwrap_err();
        assert!(format!("{err}").contains("Unrecognized"));
    }

    #[test]
    fn detect_format_handles_bom_and_crlf() {
        // BOM + mbox header + CRLF endings.
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        body.extend_from_slice(
            b"From cafef00dcafef00dcafef00dcafef00dcafef00d Mon Sep 17 00:00:00 2001\r\n\
              From: Test <t@example>\r\n\
              Subject: change\r\n",
        );
        assert_eq!(
            detect_patch_format(&body).expect("detect"),
            PatchFormat::Mbox
        );

        let crlf_unified = b"diff --git a/foo b/foo\r\n\
                              index 1234abcd..5678ef00 100644\r\n";
        assert_eq!(
            detect_patch_format(crlf_unified).expect("detect"),
            PatchFormat::UnifiedWithIndex
        );
    }

    #[test]
    fn detect_format_skips_leading_blank_lines() {
        let body =
            b"\n\n\nFrom cafef00dcafef00dcafef00dcafef00dcafef00d Mon Sep 17 00:00:00 2001\n";
        assert_eq!(
            detect_patch_format(body).expect("detect"),
            PatchFormat::Mbox
        );
    }

    #[test]
    fn parse_patch_summary_counts_hunks() {
        let body = b"diff --git a/foo b/foo\n\
                     index 1234..5678 100644\n\
                     --- a/foo\n\
                     +++ b/foo\n\
                     @@ -1,2 +1,2 @@\n\
                     -old\n\
                     +new1\n\
                     +new2\n";
        let summary = parse_patch_summary(body);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].0, "foo");
        assert_eq!(summary[0].1, 2);
        assert_eq!(summary[0].2, 1);
    }
}
